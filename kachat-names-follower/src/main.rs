//! `.kachat` registry follower (KACHAT_NAMES_INDEXER.md §B, §3, §4; app contract §1).
//!
//! Reads the node's virtual chain from the manifest's `genesis.scanFrom` (or the stored
//! checkpoint), applies every accepted transaction to the kachat-names engine, and keeps the
//! registry in Postgres for kachat-webserver's `/names/*` API. It only marks itself `synced`
//! once it is within a minute of the node's virtual DAA **and** the latest self-test found
//! every served UTXO still unspent on the node — the app switches to this indexer on that
//! signal, so nothing it serves may be refutable by the chain.

mod node;
mod store;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use kachat_names::follower::{ChainSource, Follower, VccBatch};
use kachat_names::ingest::{Outpoint, Templates, Tracked};
use kachat_names::GapState;
use kaspa_addresses::{Address, Prefix, Version};
use kaspa_rpc_core::api::rpc::RpcApi;
use sqlx::postgres::PgPoolOptions;
use tracing::{info, warn};

use crate::node::Node;
use crate::store::UtxoMeta;

#[derive(Parser, Debug)]
#[command(about = ".kachat names registry follower")]
struct Args {
    /// Names manifest (kachat-domains/manifests/kachat-names-<network>.json).
    #[arg(long, env = "KACHAT_NAMES_MANIFEST")]
    manifest: String,
    /// The node's wRPC Borsh endpoint (Toccata-capable, --utxoindex for the self-test), or
    /// `resolver` for a public node of the manifest's network.
    #[arg(long, env = "KACHAT_NAMES_NODE_URL", default_value = "ws://127.0.0.1:17210")]
    node_url: String,
    /// Follow the chain in memory from scanFrom to the tip, print every registry event and the
    /// self-test result, then exit. No database.
    #[arg(long)]
    probe: bool,
    #[arg(long, env = "DB_HOST", default_value = "localhost")]
    db_host: String,
    #[arg(long, env = "DB_PORT", default_value_t = 5432)]
    db_port: u16,
    #[arg(long, env = "DB_NAME", default_value = "")]
    db_name: String,
    #[arg(long, env = "DB_USER", default_value = "")]
    db_user: String,
    #[arg(long, env = "DB_PASSWORD", default_value = "")]
    db_password: String,
    /// Chain blocks held back from the tip, so a shallow reorg never reaches indexed state.
    #[arg(long, default_value_t = 10)]
    min_confirmations: u64,
    /// Undo-journal entries kept for deeper reorgs (registry txs, not blocks).
    #[arg(long, default_value_t = 10_000)]
    journal_keep: usize,
    /// Caught up = within this many DAA of the node's virtual DAA (10 BPS: 600 ≈ 1 min).
    #[arg(long, default_value_t = 600)]
    synced_within_daa: u64,
    #[arg(long, default_value_t = 2_000)]
    poll_ms: u64,
    #[arg(long, default_value_t = 300)]
    self_test_secs: u64,
}

/// Static facts read from the manifest.
struct Manifest {
    registry: String,
    network: String,
    prefix: Prefix,
    genesis_txid: String,
    genesis_outpoint: Outpoint,
    genesis_gap: GapState,
    scan_from: [u8; 32],
    grace_ms: i64,
    templates: Templates,
}

fn hex32(v: &serde_json::Value, what: &str) -> Result<[u8; 32]> {
    let s = v.as_str().ok_or_else(|| anyhow!("manifest: {what} missing"))?;
    hex::decode(s)?.try_into().map_err(|_| anyhow!("manifest: {what} is not 32 bytes"))
}

fn read_manifest(path: &str) -> Result<Manifest> {
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?)?;
    let registry = raw["registryCovenantId"].as_str().ok_or_else(|| anyhow!("manifest: no registryCovenantId"))?;
    let network = raw["network"].as_str().unwrap_or("testnet-10").to_string();
    let prefix = if network == "mainnet" { Prefix::Mainnet } else { Prefix::Testnet };
    let genesis = &raw["genesis"];
    let gap0 = genesis["authorizedOutputs"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["contract"] == "KachatGap"))
        .ok_or_else(|| anyhow!("manifest: no genesis KachatGap output"))?;
    let genesis_txid = genesis["txid"].as_str().ok_or_else(|| anyhow!("manifest: no genesis.txid"))?;
    let grace_ms = raw["params"]["graceMs"].as_i64().unwrap_or(0);
    Ok(Manifest {
        registry: registry.to_lowercase(),
        prefix,
        genesis_outpoint: (hex32(&genesis["txid"], "genesis.txid")?, gap0["index"].as_u64().unwrap_or(0) as u32),
        genesis_gap: GapState {
            lo: hex32(&gap0["state"]["lo"], "genesis gap lo")?,
            hi: hex32(&gap0["state"]["hi"], "genesis gap hi")?,
        },
        scan_from: hex32(&genesis["scanFrom"], "genesis.scanFrom")?,
        genesis_txid: genesis_txid.to_lowercase(),
        network,
        grace_ms,
        templates: Templates::from_manifest(&raw).ok_or_else(|| anyhow!("manifest: incomplete artifacts"))?,
    })
}

/// A batch already fetched from the node, handed to the (sync) engine loop.
struct Prefetched(Option<VccBatch>);
impl ChainSource for Prefetched {
    type Error = anyhow::Error;
    fn next_batch(&mut self, _from: Option<[u8; 32]>) -> Result<VccBatch> {
        self.0.take().ok_or_else(|| anyhow!("batch already consumed"))
    }
}

/// The address a script pays to (P2PK schnorr / ECDSA / P2SH), as the API names identities.
fn spk_address(prefix: Prefix, spk: &[u8]) -> Option<String> {
    let (version, payload) = match spk {
        [0x20, rest @ .., 0xac] if rest.len() == 32 => (Version::PubKey, rest),
        [0x21, rest @ .., 0xab] if rest.len() == 33 => (Version::PubKeyECDSA, rest),
        [0xaa, 0x20, rest @ .., 0x87] if rest.len() == 32 => (Version::ScriptHash, rest),
        _ => return None,
    };
    Some(Address::new(prefix, version, payload).to_string())
}

fn tracked_spk(t: &Templates, tracked: &Tracked) -> Vec<u8> {
    match tracked {
        Tracked::Gap(g) => t.gap.spk(&g.encode()),
        Tracked::Name(n) => t.name.spk(&n.encode()),
        Tracked::Offer(o) => t.offer.spk(&o.encode()),
    }
}

/// §4.2: every live row must still be an unspent output on the node. Returns the outpoints
/// the node does not have.
async fn self_test(node: &Node, m: &Manifest, reg: &kachat_names::ingest::Registry) -> Result<HashSet<Outpoint>> {
    let mut by_address: HashMap<String, Vec<Outpoint>> = HashMap::new();
    for (op, tracked) in &reg.utxos {
        let spk = tracked_spk(&m.templates, tracked);
        let addr = spk_address(m.prefix, &spk).ok_or_else(|| anyhow!("unaddressable registry script"))?;
        by_address.entry(addr).or_default().push(*op);
    }
    let mut on_node: HashSet<Outpoint> = HashSet::new();
    let addresses: Vec<String> = by_address.keys().cloned().collect();
    for chunk in addresses.chunks(200) {
        let addrs = chunk.iter().map(|a| Address::try_from(a.as_str())).collect::<Result<Vec<_>, _>>()?;
        let entries = node.client.get_utxos_by_addresses(addrs).await.map_err(|e| anyhow!("getUtxosByAddresses: {e}"))?;
        for e in entries {
            on_node.insert((e.outpoint.transaction_id.as_bytes(), e.outpoint.index));
        }
    }
    Ok(reg.utxos.keys().filter(|op| !on_node.contains(*op)).copied().collect())
}

/// Batch-size control: uncapped normally; after a timeout, cap to a blue-score window,
/// halving on every further failure and doubling back to uncapped on success.
#[derive(Default)]
struct Window(Option<u64>);
impl Window {
    const FIRST: u64 = 1_000;
    const MIN: u64 = 50;
    const UNCAPPED_ABOVE: u64 = 64_000;
    fn failed(&mut self) {
        self.0 = Some(self.0.map(|w| (w / 2).max(Self::MIN)).unwrap_or(Self::FIRST));
    }
    fn succeeded(&mut self) {
        self.0 = self.0.and_then(|w| (w * 2 <= Self::UNCAPPED_ABOVE).then_some(w * 2));
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let m = read_manifest(&args.manifest)?;
    info!("[names] registry {} on {} (scanFrom {})", m.registry, m.network, hex::encode(m.scan_from));
    if args.probe {
        return probe(&args, &m).await;
    }

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&format!(
            "postgres://{}:{}@{}:{}/{}",
            args.db_user, args.db_password, args.db_host, args.db_port, args.db_name
        ))
        .await
        .context("connecting to Postgres")?;
    store::create_schema(&pool).await?;

    // A different registry (new genesis / other network) starts from scratch.
    if store::stored_registry(&pool).await?.as_deref() != Some(m.registry.as_str()) {
        info!("[names] fresh registry tables for {}", m.registry);
        store::reset(&pool, &m.registry, &m.network, &m.genesis_txid, m.grace_ms).await?;
    }
    let (registry, mut meta, mut profile_times, mut status) = store::load(&pool).await?;
    let mut follower = Follower::new(args.journal_keep);
    follower.registry = registry;
    match status.checkpoint {
        Some(cp) => {
            follower.checkpoint = Some(cp);
            info!("[names] resuming from {} ({} live rows)", hex::encode(cp), follower.registry.utxos.len());
        }
        None => {
            follower.seed(m.scan_from, m.genesis_outpoint, m.genesis_gap);
            // The genesis gap's value is in the manifest; its creation time isn't known here.
            meta.insert(m.genesis_outpoint, UtxoMeta { value: 0, created_at: 0, created_daa: 0 });
            info!("[names] seeded genesis gap at {}:{}", hex::encode(m.genesis_outpoint.0), m.genesis_outpoint.1);
        }
    }
    // The live set has never been written (fresh seed): write it with the first batch.
    let mut dirty = status.checkpoint.is_none();

    let node = Node::connect(&args.node_url, &m.network).await?;
    info!("[names] connected to {}", args.node_url);

    let mut refuted: HashSet<Outpoint> = HashSet::new();
    let mut suspects: HashSet<Outpoint> = HashSet::new();
    let mut last_self_test = Instant::now() - Duration::from_secs(args.self_test_secs);
    // Names by key, so a release/reclaim event (the name is gone after apply) keeps its name.
    let mut names_by_key: HashMap<[u8; 32], String> = HashMap::new();
    let mut window = Window::default();

    loop {
        let from = follower.checkpoint.unwrap_or(m.scan_from);
        let (batch, last_daa) = match node.next_batch(from, args.min_confirmations, window.0).await {
            Ok(b) => {
                window.succeeded();
                b
            }
            Err(e) => {
                window.failed();
                warn!("[names] fetch failed ({e:#}); next batch capped to {:?} blue score", window.0);
                tokio::time::sleep(Duration::from_millis(args.poll_ms * 5)).await;
                continue;
            }
        };
        let fetched_blocks = batch.tip.is_some();
        let removed = batch.removed_blocks.clone();
        let accepted_ids: Vec<_> = batch.accepted.iter().map(|t| t.id).collect();

        for (_, n) in follower.registry.names() {
            names_by_key.insert(n.key, n.name_str());
        }
        let before: HashSet<Outpoint> = follower.registry.utxos.keys().copied().collect();
        let profiles_before = follower.registry.profiles.clone();
        let (batch, events) = follower.step(&m.templates, &mut Prefetched(Some(batch)))?;

        // Metadata for the outputs this batch started tracking.
        let txs: HashMap<_, _> = batch.accepted.iter().map(|t| (t.id, t)).collect();
        for op in follower.registry.utxos.keys() {
            if !before.contains(op)
                && let Some(tx) = txs.get(&op.0)
            {
                let value = tx.outputs.get(op.1 as usize).map(|o| o.value).unwrap_or(0);
                meta.insert(*op, UtxoMeta { value, created_at: tx.block_time, created_daa: tx.accepting_daa });
            }
        }
        for (spk, (_, (_, txid))) in &follower.registry.profiles {
            if profiles_before.get(spk).map(|p| &p.1.1) != Some(txid)
                && let Some(tx) = txs.get(txid)
            {
                profile_times.insert(spk.clone(), tx.block_time);
            }
        }
        for (_, n) in follower.registry.names() {
            names_by_key.insert(n.key, n.name_str());
        }

        let changed = dirty
            || !removed.is_empty()
            || !events.is_empty()
            || follower.registry.utxos.len() != before.len()
            || follower.registry.utxos.keys().any(|op| !before.contains(op))
            || follower.registry.profiles != profiles_before;

        if let Some(daa) = last_daa {
            status.indexed_daa = daa;
        }
        status.checkpoint = follower.checkpoint;
        status.virtual_daa = node.virtual_daa().await.unwrap_or(status.virtual_daa);
        let caught_up = status.virtual_daa.saturating_sub(status.indexed_daa)
            <= args.synced_within_daa + args.min_confirmations * 10;

        // Self-test once caught up (and periodically after): a row missing from the node on
        // two consecutive runs is refuted and withheld; synced needs a clean run.
        if caught_up && last_self_test.elapsed() >= Duration::from_secs(args.self_test_secs) {
            last_self_test = Instant::now();
            match self_test(&node, &m, &follower.registry).await {
                Ok(missing) => {
                    let confirmed: HashSet<Outpoint> = missing.intersection(&suspects).copied().collect();
                    if !confirmed.is_empty() {
                        warn!("[names] self-test: {} row(s) refuted by the node; withholding them", confirmed.len());
                    }
                    if confirmed != refuted {
                        store::set_refuted(&pool, &confirmed).await?;
                    }
                    refuted = confirmed;
                    suspects = missing;
                    status.self_test_ok = suspects.is_empty();
                    status.self_test_at = now_ms();
                }
                Err(e) => warn!("[names] self-test failed to run: {e:#}"),
            }
        }
        status.synced = caught_up && status.self_test_ok;

        if changed {
            let mut profiles = Vec::new();
            for (spk, (json, (daa, txid))) in &follower.registry.profiles {
                let Some(address) = spk_address(m.prefix, spk) else { continue };
                let at = profile_times.get(spk).copied().unwrap_or(0);
                profiles.push((spk.clone(), address, json.clone(), *daa, *txid, at));
            }
            let named: Vec<_> = events.iter().map(|e| (e.clone(), names_by_key.get(&e.key).cloned())).collect();
            store::persist(&pool, &follower.registry, &meta, &refuted, &profiles, &removed, &named, true, &status)
                .await?;
            dirty = false;
            for e in &events {
                info!(
                    "[names] {} {} tx {}",
                    e.op,
                    names_by_key.get(&e.key).map(String::as_str).unwrap_or("?"),
                    hex::encode(e.tx_id)
                );
            }
            if !removed.is_empty() {
                info!("[names] reorg: undid {} chain block(s)", removed.len());
            }
        } else {
            store::save_status(&pool, &status).await?;
        }
        if accepted_ids.is_empty() && !fetched_blocks {
            tokio::time::sleep(Duration::from_millis(args.poll_ms)).await;
        }
        if follower.checkpoint.is_none() {
            bail!("lost the checkpoint");
        }
    }
}

/// `--probe`: the same engine path as the service, in memory, from scanFrom to the tip.
async fn probe(args: &Args, m: &Manifest) -> Result<()> {
    let node = Node::connect(&args.node_url, &m.network).await?;
    info!("[probe] connected to {}", args.node_url);
    let mut follower = Follower::new(args.journal_keep);
    follower.seed(m.scan_from, m.genesis_outpoint, m.genesis_gap);
    let mut names_by_key: HashMap<[u8; 32], String> = HashMap::new();
    let (mut blocks, mut txs, mut indexed_daa) = (0usize, 0usize, 0u64);
    let mut window = Window::default();
    let mut failures = 0;
    loop {
        let (batch, last_daa) = match node.next_batch(follower.checkpoint.unwrap(), args.min_confirmations, window.0).await {
            Ok(b) => {
                window.succeeded();
                failures = 0;
                b
            }
            Err(e) => {
                failures += 1;
                window.failed();
                warn!("[probe] fetch failed ({e:#}); retrying capped to {:?} blue score", window.0);
                if failures > 20 {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let added = batch.tip.is_some();
        for (_, n) in follower.registry.names() {
            names_by_key.insert(n.key, n.name_str());
        }
        let (batch, events) = follower.step(&m.templates, &mut Prefetched(Some(batch)))?;
        txs += batch.accepted.len();
        blocks += usize::from(added);
        if let Some(d) = last_daa {
            indexed_daa = d;
        }
        for (_, n) in follower.registry.names() {
            names_by_key.insert(n.key, n.name_str());
        }
        for e in &events {
            info!(
                "[probe] daa {} {:<14} {:<24} tx {}",
                e.daa,
                e.op,
                names_by_key.get(&e.key).map(String::as_str).unwrap_or("?"),
                hex::encode(e.tx_id)
            );
        }
        let virtual_daa = node.virtual_daa().await?;
        info!(
            "[probe] batch {blocks}: indexed DAA {indexed_daa} / virtual {virtual_daa} ({} behind), {txs} txs so far, {} live rows",
            virtual_daa.saturating_sub(indexed_daa),
            follower.registry.utxos.len()
        );
        if !added || virtual_daa.saturating_sub(indexed_daa) <= args.synced_within_daa + args.min_confirmations * 10 {
            break;
        }
    }
    let names: Vec<String> = follower.registry.names().map(|(_, n)| n.name_str()).collect();
    let (gaps, offers) = follower.registry.utxos.values().fold((0, 0), |(g, o), t| match t {
        Tracked::Gap(_) => (g + 1, o),
        Tracked::Offer(_) => (g, o + 1),
        Tracked::Name(_) => (g, o),
    });
    info!(
        "[probe] caught up at DAA {indexed_daa} after {blocks} batch(es), {txs} accepted txs: {} names {:?}, {gaps} gaps, {offers} offers, {} profiles",
        names.len(),
        names,
        follower.registry.profiles.len()
    );
    let missing = self_test(&node, m, &follower.registry).await?;
    if missing.is_empty() {
        info!("[probe] self-test: every live row is unspent on the node");
    } else {
        warn!("[probe] self-test: {} live row(s) not on the node: {:?}", missing.len(),
            missing.iter().map(|o| format!("{}:{}", hex::encode(o.0), o.1)).collect::<Vec<_>>());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_shrinks_on_failure_and_recovers() {
        let mut w = Window::default();
        assert_eq!(w.0, None);
        w.failed();
        assert_eq!(w.0, Some(1_000));
        w.failed();
        assert_eq!(w.0, Some(500));
        for _ in 0..10 {
            w.failed();
        }
        assert_eq!(w.0, Some(Window::MIN));
        for _ in 0..20 {
            w.succeeded();
        }
        assert_eq!(w.0, None, "back to uncapped");
    }

    #[test]
    fn spk_address_matches_the_builder_vector() {
        // kachat-domains' p2pk vector: script 0x20 <x-only> 0xac.
        let spk = hex::decode("206dece92abd087978562b0e47943d859bd444672f89bc68fc8bfa03a3d0b27ee8ac").unwrap();
        assert_eq!(
            spk_address(Prefix::Testnet, &spk).as_deref(),
            Some("kaspatest:qpk7e6f2h5y8j7zk9v8y09paskdag3r897ymc68u30aq8g7skflwsaxs0p99v")
        );
        // A registry P2SH (aa 20 <hash> 87) is addressable; a bare/odd script is not.
        let p2sh = hex::decode("aa2091e1c42572eec31a4bdfab6f4fe298fe51cb0934ac6f2cdb87e1052082744cc987").unwrap();
        assert!(spk_address(Prefix::Testnet, &p2sh).unwrap().starts_with("kaspatest:p"));
        assert_eq!(spk_address(Prefix::Testnet, &[0x51]), None);
    }

    #[test]
    fn reads_the_live_testnet_manifest() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../kachat-domains/manifests/kachat-names-testnet-10.json");
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: no live manifest");
            return;
        }
        let m = read_manifest(path).unwrap();
        assert_eq!(m.registry, "9444187f09a3e77450e125d448b21eb79b3c54b692a5b3f3e8af38343b9a7a51");
        assert_eq!(m.network, "testnet-10");
        assert_eq!(hex::encode(m.genesis_outpoint.0), m.genesis_txid);
        assert_eq!(m.genesis_gap.lo, [0u8; 32]);
        assert_eq!(m.genesis_gap.hi, [0xffu8; 32]);
        // The genesis gap state hashes to the deployed genesis output script.
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let spk = raw["genesis"]["authorizedOutputs"][0]["scriptPublicKey"].as_str().unwrap();
        assert_eq!(hex::encode(m.templates.gap.spk(&m.genesis_gap.encode())), spk);
    }
}

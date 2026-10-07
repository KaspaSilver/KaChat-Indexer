//! `.kachat` registry follower (KACHAT_NAMES_INDEXER.md §B, §3, §4; app contract §1).
//!
//! Reads the node's virtual chain from the manifest's `genesis.scanFrom` (or the stored
//! checkpoint), applies every accepted transaction to the kachat-names engine, and keeps the
//! registry in Postgres for kachat-webserver's `/names/*` API. It only marks itself `synced`
//! once it is within a minute of the node's virtual DAA **and** the latest self-test found
//! every served UTXO still unspent on the node — the app switches to this indexer on that
//! signal, so nothing it serves may be refutable by the chain.

mod bootstrap;
mod node;
mod profiles;
mod pushes;
mod store;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use kachat_names::follower::{ChainSource, Follower, VccBatch};
use kachat_names::ingest::{Outpoint, Templates, Tracked};
use kachat_names::{GapState, PriceState};
use kaspa_addresses::{Address, Prefix, Version};
use kaspa_rpc_core::api::rpc::RpcApi;
use sqlx::postgres::PgPoolOptions;
use tracing::{info, warn};

use crate::node::Node;
use crate::store::UtxoMeta;

#[derive(Parser, Debug)]
#[command(about = ".kachat names registry follower")]
struct Args {
    /// Names manifest (kachat-domains/manifests/kachat-names-<network>.json). Required for
    /// the names follower; in `--profiles` mode only its scanFrom is used, as a start block.
    #[arg(long, env = "KACHAT_NAMES_MANIFEST")]
    manifest: Option<String>,
    /// Run the address-profiles follower instead (docs/KACHAT_PROFILES.md): every network,
    /// no manifest needed, sole writer of `names_profiles`.
    #[arg(long)]
    profiles: bool,
    /// The network this stack follows (`mainnet`, `testnet-10`); used by `--profiles`.
    #[arg(long, env = "NETWORK", default_value = "mainnet")]
    network: String,
    /// `--profiles` first-run start block (hex hash). Default: the manifest's scanFrom if
    /// there is one, else the node's pruning point. The stored checkpoint wins afterwards.
    #[arg(long, env = "KACHAT_PROFILES_SCAN_FROM")]
    profiles_scan_from: Option<String>,
    /// The node's wRPC Borsh endpoint (Toccata-capable, --utxoindex for the self-test), or
    /// `resolver` for a public node of the manifest's network.
    #[arg(long, env = "KACHAT_NAMES_NODE_URL", default_value = "ws://127.0.0.1:17210")]
    node_url: String,
    /// Follow the chain in memory from scanFrom to the tip, print every registry event and the
    /// self-test result, then exit. No database.
    #[arg(long)]
    probe: bool,
    /// Rebuild the registry from the REST API (no node history, no database), print every
    /// live name and the walk's report, then exit. Checks §3 of KACHAT_NAMES_PRUNED_START.md.
    #[arg(long)]
    bootstrap_probe: bool,
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
    /// The push service's internal route base (Part E name pushes). Same box, never public.
    #[arg(long, env = "PUSH_INTERNAL_URL", default_value = "http://127.0.0.1:8600/internal/push")]
    push_url: String,
    #[arg(long, env = "INTERNAL_PUSH_SECRET", default_value = "")]
    push_secret: String,
    /// When the start block is below the node's pruning point, rebuild the registry from the
    /// Kaspa REST API (`auto`, the default) or only report it (`off`).
    /// KACHAT_NAMES_REST_URL overrides the API base (docs/KACHAT_NAMES_PRUNED_START.md §3).
    #[arg(long, env = "KACHAT_NAMES_REST_BOOTSTRAP", default_value = "auto")]
    rest_bootstrap: String,
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
    renew_window_ms: i64,
    templates: Templates,
    /// 2, 3 or 4 (docs/KACHAT_NAMES_REGISTRY_V3.md, _V4.md).
    version: i32,
    /// Registry v3: the price covenant id and the K genesis shards.
    price_covenant_id: Option<String>,
    price_shards: Vec<(Outpoint, PriceState)>,
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
    let templates = Templates::from_manifest(&raw).ok_or_else(|| anyhow!("manifest: incomplete artifacts"))?;
    let version = templates.version as i32;

    // Verify both geneses against the templates (as the app's manifest check does): the
    // genesis gap, and v3's K price shards, must each be exactly their deployed scripts.
    let gap_out_spk = gap0["scriptPublicKey"].as_str().unwrap_or("");
    let genesis_gap = GapState { lo: hex32(&gap0["state"]["lo"], "genesis gap lo")?, hi: hex32(&gap0["state"]["hi"], "genesis gap hi")? };
    if !gap_out_spk.is_empty() && hex::encode(templates.gap_spk(&genesis_gap)) != gap_out_spk {
        bail!("manifest: the genesis gap does not hash to its deployed script");
    }
    let mut price_shards = Vec::new();
    let mut price_covenant_id = None;
    if templates.has_price_record() {
        let pg = &raw["priceGenesis"];
        let txid = hex32(&pg["txid"], "priceGenesis.txid")?;
        for o in pg["authorizedOutputs"].as_array().ok_or_else(|| anyhow!("manifest: no priceGenesis.authorizedOutputs"))? {
            let st = &o["state"];
            let prices: Vec<i64> = st["prices"].as_array().map(|a| a.iter().filter_map(|p| p.as_i64()).collect()).unwrap_or_default();
            let shard = PriceState {
                shard: st["shard"].as_i64().ok_or_else(|| anyhow!("manifest: shard without a number"))?,
                authority: hex32(&st["authority"], "shard authority")?,
                prices: prices.try_into().map_err(|_| anyhow!("manifest: a shard without 5 prices"))?,
            };
            if hex::encode(templates.price_spk(&shard)) != o["scriptPublicKey"].as_str().unwrap_or("") {
                bail!("manifest: price shard {} does not hash to its deployed script", shard.shard);
            }
            price_shards.push(((txid, o["index"].as_u64().unwrap_or(0) as u32), shard));
        }
        let expected = raw["params"]["priceShards"].as_u64().unwrap_or(8) as usize;
        if price_shards.len() != expected {
            bail!("manifest: {} price shards, expected {expected}", price_shards.len());
        }
        price_covenant_id = raw["priceCovenantId"].as_str().map(str::to_lowercase);
    }
    Ok(Manifest {
        registry: registry.to_lowercase(),
        prefix,
        genesis_outpoint: (hex32(&genesis["txid"], "genesis.txid")?, gap0["index"].as_u64().unwrap_or(0) as u32),
        genesis_gap,
        // v3: the price genesis comes first, and an authority price change may follow it
        // before the registry genesis, so scanning starts from the earlier of the two.
        scan_from: if templates.has_price_record() && raw["priceGenesis"]["scanFrom"].is_string() {
            hex32(&raw["priceGenesis"]["scanFrom"], "priceGenesis.scanFrom")?
        } else {
            hex32(&genesis["scanFrom"], "genesis.scanFrom")?
        },
        genesis_txid: genesis_txid.to_lowercase(),
        network,
        grace_ms,
        // Registry v2: renewing opens this long before expiry (10 days).
        renew_window_ms: raw["params"]["renewWindowMs"].as_i64().unwrap_or(864_000_000),
        templates,
        version,
        price_covenant_id,
        price_shards,
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

/// §4.2: every live row must still be an unspent output on the node. Returns the outpoints
/// the node does not have.
async fn self_test(node: &Node, m: &Manifest, reg: &kachat_names::ingest::Registry) -> Result<HashSet<Outpoint>> {
    let mut by_address: HashMap<String, Vec<Outpoint>> = HashMap::new();
    for (op, tracked) in &reg.utxos {
        let spk = m.templates.tracked_spk(tracked);
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

/// Whether a stored registry id is the manifest's (case and whitespace don't count).
fn same_registry(stored: Option<&str>, manifest: &str) -> bool {
    stored.is_some_and(|s| s.trim().eq_ignore_ascii_case(manifest.trim()))
}

/// A fetch failed: is it because `from` is a block the node no longer has, below its pruning
/// point? Then it will never come back. Anything else (a timeout, a busy node) is retried.
async fn start_block_pruned(node: &Node, from: [u8; 32], err: &anyhow::Error) -> bool {
    if !format!("{err:#}").contains("cannot find header") {
        return false;
    }
    // The node no longer knows the header at all: pruned, provided it has a pruning point
    // (an archival node keeps everything and would have found it).
    match node.client.get_block(kaspa_rpc_core::model::RpcHash::from_bytes(from), false).await {
        Ok(_) => false,
        Err(_) => node.client.get_block_dag_info().await.is_ok_and(|i| i.pruning_point_hash.as_bytes() != from),
    }
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

/// Deliver one name push to its recipient (owner/seller/buyer key → their address).
async fn send_push(http: &reqwest::Client, base: &str, secret: Option<&str>, prefix: Prefix, p: &pushes::NamePush) {
    let to = Address::new(prefix, Version::PubKey, &p.to_key).to_string();
    match pushes::send(http, base, secret, &to, p).await {
        Ok(()) => info!("[names] push {} {} -> {}", p.event, p.name, to),
        Err(e) => warn!("[names] push {} {} failed: {e:#}", p.event, p.name),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[tokio::main]
async fn main() {
    // No ANSI colour: the panel filters this container's output on the `[names]` tag
    // (docs/KACHAT_NAMES_PANEL_LOGS.md).
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    // A crash must be visible in the panel's log: tag it before supervisord restarts us.
    let args = Args::parse();
    let tag = if args.profiles { "[profiles]" } else { "[names]" };
    let result = if args.profiles { profiles::run(&args).await } else { run(args).await };
    if let Err(e) = result {
        tracing::error!("{tag} fatal: {e:#}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<()> {
    let manifest = args.manifest.as_deref().ok_or_else(|| anyhow!("KACHAT_NAMES_MANIFEST is not set"))?;
    let m = read_manifest(manifest)?;
    info!(
        "[names] registry v{} {} on {} (scanFrom {}){}",
        m.version,
        m.registry,
        m.network,
        hex::encode(m.scan_from),
        m.price_covenant_id.as_deref().map(|p| format!(", price covenant {p}, {} shards", m.price_shards.len())).unwrap_or_default()
    );
    if args.probe {
        return probe(&args, &m).await;
    }
    if args.bootstrap_probe {
        let node = Node::connect(&args.node_url, &m.network).await?;
        let base = bootstrap::rest_base(&m.network);
        info!("[names] [bootstrap-probe] connected to {}; REST {base}", args.node_url);
        let mut follower = Follower::new(args.journal_keep);
        follower.seed(m.scan_from, m.genesis_outpoint, m.genesis_gap);
        follower.seed_shards(&m.price_shards);
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
        let report = bootstrap::walk(&node, &http, &base, m.prefix, &m.templates, &mut follower.registry).await?;
        for e in &report.events {
            info!("[names] [bootstrap-probe] {:<14} key {}… tx {}", e.op, &hex::encode(e.key)[..12], hex::encode(e.tx_id));
        }
        let names: Vec<String> = follower.registry.names().map(|(_, n)| n.name_str()).collect();
        info!(
            "[names] [bootstrap-probe] {} round(s), {} tx(s), {} live rows, unresolved {}; names {:?}",
            report.rounds,
            report.applied.len(),
            follower.registry.utxos.len(),
            report.unresolved,
            names
        );
        let missing = self_test(&node, &m, &follower.registry).await?;
        info!("[names] [bootstrap-probe] self-test: {} live row(s) not on the node", missing.len());
        for op in &missing {
            let kind = match follower.registry.utxos.get(op) {
                Some(Tracked::Gap(_)) => "gap",
                Some(Tracked::Name(_)) => "name",
                Some(Tracked::Offer(_)) => "offer",
                Some(Tracked::Shard(p)) => if p.shard == 0 { "shard 0" } else { "shard" },
                None => "?",
            };
            info!("[names] [bootstrap-probe]   {kind} {}:{}", hex::encode(op.0), op.1);
        }
        return Ok(());
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
    // Reset only for a genuinely different registry (KACHAT_NAMES_PRUNED_START.md §1):
    // compared case- and whitespace-insensitively, and both ids logged when it happens.
    let stored = store::stored_registry(&pool).await?;
    if !same_registry(stored.as_deref(), &m.registry) {
        info!(
            "[names] fresh registry tables: stored {}, manifest {}",
            stored.as_deref().unwrap_or("(none)"),
            m.registry
        );
        store::reset(
            &pool,
            &store::RegistryInfo {
                registry: &m.registry,
                network: &m.network,
                genesis_txid: &m.genesis_txid,
                grace_ms: m.grace_ms,
                price_covenant_id: m.price_covenant_id.as_deref(),
                version: m.version,
                period_ms: m.templates.period_ms,
            },
        )
        .await?;
    }
    let (registry, mut meta, mut status) = store::load(&pool).await?;
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
            // Registry v3: the price genesis (sent first) seeds the K shards.
            follower.seed_shards(&m.price_shards);
            for (op, _) in &m.price_shards {
                meta.insert(*op, UtxoMeta { value: 0, created_at: 0, created_daa: 0 });
            }
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
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let push_secret = (!args.push_secret.is_empty()).then_some(args.push_secret.as_str());
    let mut last_reminder_scan = Instant::now() - Duration::from_secs(60);
    let mut last_heartbeat = Instant::now() - Duration::from_secs(60);
    let mut last_fatal_log = Instant::now() - Duration::from_secs(60);
    // REST bootstrap (§3): when it last ran, and the transactions it applied (the node's
    // chain after the checkpoint may carry them again; they are skipped, not re-applied).
    let mut last_bootstrap: Option<Instant> = None;
    let mut bootstrapped_txids: HashSet<[u8; 32]> = HashSet::new();

    loop {
        let from = follower.checkpoint.unwrap_or(m.scan_from);
        let (mut batch, last_daa) = match node.next_batch(from, args.min_confirmations, window.0).await {
            Ok(b) => {
                window.succeeded();
                b
            }
            Err(e) => {
                // A start block the node has pruned never comes back (KACHAT_NAMES_PRUNED_START.md
                // §2): say so, once a minute, as an error, and in /names/status, instead of
                // shrinking the batch and retrying forever as if just started.
                let pruned = start_block_pruned(&node, from, &e).await;
                // §3: rebuild from the REST API instead, then follow the node from its sink.
                // Tried at most every 10 minutes (the API may be down, or not caught up).
                if pruned
                    && args.rest_bootstrap != "off"
                    && last_bootstrap.is_none_or(|t| t.elapsed() >= Duration::from_secs(600))
                {
                    last_bootstrap = Some(Instant::now());
                    let base = bootstrap::rest_base(&m.network);
                    info!("[names] start block {} is pruned: rebuilding the registry from {base}", hex::encode(from));
                    let sink = node.client.get_sink().await.map_err(|e| anyhow!("getSink: {e}"))?.sink;
                    let sink_daa = node.virtual_daa().await.unwrap_or(0);
                    match bootstrap::walk(&node, &http, &base, m.prefix, &m.templates, &mut follower.registry).await {
                        Ok(report) => {
                            for tx in &report.applied {
                                for (i, o) in tx.outputs.iter().enumerate() {
                                    let op = (tx.id, i as u32);
                                    if follower.registry.utxos.contains_key(&op) {
                                        meta.insert(op, UtxoMeta { value: o.value, created_at: tx.block_time, created_daa: tx.accepting_daa });
                                    }
                                }
                            }
                            for (_, n) in follower.registry.names() {
                                names_by_key.insert(n.key, n.name_str());
                            }
                            let named: Vec<_> = report.events.iter().map(|e| (e.clone(), names_by_key.get(&e.key).cloned())).collect();
                            follower.checkpoint = Some(sink.as_bytes());
                            bootstrapped_txids = report.applied.iter().map(|t| t.id).collect();
                            status.checkpoint = follower.checkpoint;
                            status.fatal_reason = None;
                            status.start_block = None;
                            status.synced = false;
                            status.bootstrapped_at = sink_daa as i64;
                            store::persist(&pool, &follower.registry, &meta, &refuted, &[], &named, true, &status).await?;
                            dirty = false;
                            info!(
                                "[names] bootstrap done: {} transaction(s) in {} round(s), {} live rows; following the node from its sink {}",
                                report.applied.len(),
                                report.rounds,
                                follower.registry.utxos.len(),
                                hex::encode(sink.as_bytes())
                            );
                            if report.unresolved > 0 {
                                warn!("[names] bootstrap: {} spent output(s) not yet in the REST API; the node will carry them", report.unresolved);
                            }
                            continue;
                        }
                        Err(err) => warn!("[names] bootstrap failed ({err:#}); retrying in 10 minutes"),
                    }
                }
                if pruned {
                    let first = status.fatal_reason.is_none();
                    status.fatal_reason = Some("start_block_pruned".into());
                    status.start_block = Some(from);
                    status.synced = false;
                    store::save_status(&pool, &status).await?;
                    if first || last_fatal_log.elapsed() >= Duration::from_secs(60) {
                        last_fatal_log = Instant::now();
                        tracing::error!(
                            "[names] start block {} is below the node's pruning point; this node can't replay the registry from it (see docs/KACHAT_NAMES_PRUNED_START.md)",
                            hex::encode(from)
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                window.failed();
                warn!("[names] fetch failed ({e:#}); next batch capped to {:?} blue score", window.0);
                tokio::time::sleep(Duration::from_millis(args.poll_ms * 5)).await;
                continue;
            }
        };
        if status.fatal_reason.take().is_some() {
            status.start_block = None;
            info!("[names] the start block is reachable again; following");
        }
        // Transactions the REST bootstrap already applied are not applied again.
        if !bootstrapped_txids.is_empty() {
            batch.accepted.retain(|t| !bootstrapped_txids.contains(&t.id));
        }
        let fetched_blocks = batch.tip.is_some();
        let removed = batch.removed_blocks.clone();
        let accepted_ids: Vec<_> = batch.accepted.iter().map(|t| t.id).collect();

        for (_, n) in follower.registry.names() {
            names_by_key.insert(n.key, n.name_str());
        }
        let before: HashSet<Outpoint> = follower.registry.utxos.keys().copied().collect();
        let utxos_before = follower.registry.utxos.clone();
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
        for (_, n) in follower.registry.names() {
            names_by_key.insert(n.key, n.name_str());
        }

        let changed = dirty
            || !removed.is_empty()
            || !events.is_empty()
            || follower.registry.utxos.len() != before.len()
            || follower.registry.utxos.keys().any(|op| !before.contains(op));

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
        let was_synced = status.synced;
        status.synced = caught_up && status.self_test_ok;
        // Progress heartbeat: once a minute and whenever `synced` flips, with the same
        // numbers /names/status reports, so a long catch-up or a quiet registry is not silent.
        if status.synced != was_synced || last_heartbeat.elapsed() >= Duration::from_secs(60) {
            last_heartbeat = Instant::now();
            info!(
                "[names] at DAA {} (tip {}, {} behind, synced={})",
                status.indexed_daa,
                status.virtual_daa,
                status.virtual_daa.saturating_sub(status.indexed_daa),
                status.synced
            );
        }

        if changed {
            let named: Vec<_> = events.iter().map(|e| (e.clone(), names_by_key.get(&e.key).cloned())).collect();
            store::persist(&pool, &follower.registry, &meta, &refuted, &removed, &named, true, &status)
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

        // Part E. Only once caught up, and only for events from the last few minutes: the
        // first sync replays the whole registry history, which must never notify anyone.
        if caught_up && !events.is_empty() {
            let fresh: Vec<_> = events.iter().filter(|e| e.at >= now_ms() - 15 * 60_000).cloned().collect();
            let values: HashMap<Outpoint, u64> = meta.iter().map(|(op, m)| (*op, m.value)).collect();
            let inputs: HashMap<[u8; 32], Vec<Outpoint>> = batch
                .accepted
                .iter()
                .map(|t| (t.id, t.inputs.iter().map(|i| i.previous_outpoint).collect()))
                .collect();
            for p in pushes::event_pushes(&fresh, &utxos_before, &follower.registry.utxos, &values, &names_by_key, &inputs) {
                send_push(&http, &args.push_url, push_secret, m.prefix, &p).await;
            }
        }
        if status.synced && last_reminder_scan.elapsed() >= Duration::from_secs(60) {
            last_reminder_scan = Instant::now();
            let now = now_ms();
            let due: Vec<_> = follower
                .registry
                .utxos
                .iter()
                .filter(|(op, _)| !refuted.contains(*op))
                .filter_map(|(_, t)| match t {
                    Tracked::Name(n) => pushes::due_reminder(n, now, m.renew_window_ms, m.grace_ms).map(|r| (*n, r)),
                    _ => None,
                })
                .collect();
            for (n, (event, days, kind)) in due {
                if store::claim_reminder(&pool, &n.key, n.expires_at, kind, now).await? {
                    let p = pushes::NamePush {
                        to_key: n.owner,
                        event,
                        name: n.name_str(),
                        tx_id: String::new(),
                        amount: None,
                        days,
                        dedup: format!("{}:{}:{kind}", hex::encode(n.key), n.expires_at),
                    };
                    send_push(&http, &args.push_url, push_secret, m.prefix, &p).await;
                }
            }
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
    info!("[names] [probe] connected to {}", args.node_url);
    let mut follower = Follower::new(args.journal_keep);
    follower.seed(m.scan_from, m.genesis_outpoint, m.genesis_gap);
    follower.seed_shards(&m.price_shards);
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
                warn!("[names] [probe] fetch failed ({e:#}); retrying capped to {:?} blue score", window.0);
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
                "[names] [probe] daa {} {:<14} {:<24} tx {}",
                e.daa,
                e.op,
                names_by_key.get(&e.key).map(String::as_str).unwrap_or("?"),
                hex::encode(e.tx_id)
            );
        }
        let virtual_daa = node.virtual_daa().await?;
        info!(
            "[names] [probe] batch {blocks}: indexed DAA {indexed_daa} / virtual {virtual_daa} ({} behind), {txs} txs so far, {} live rows",
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
        Tracked::Name(_) | Tracked::Shard(_) => (g, o),
    });
    info!(
        "[names] [probe] caught up at DAA {indexed_daa} after {blocks} batch(es), {txs} accepted txs: {} names {:?}, {gaps} gaps, {offers} offers, {} profiles",
        names.len(),
        names,
        follower.registry.profiles.len()
    );
    let missing = self_test(&node, m, &follower.registry).await?;
    if missing.is_empty() {
        info!("[names] [probe] self-test: every live row is unspent on the node");
    } else {
        warn!("[names] [probe] self-test: {} live row(s) not on the node: {:?}", missing.len(),
            missing.iter().map(|o| format!("{}:{}", hex::encode(o.0), o.1)).collect::<Vec<_>>());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_compare_loosely() {
        assert!(same_registry(Some("90F56BD1 "), "90f56bd1"));
        assert!(!same_registry(None, "90f56bd1"));
        assert!(!same_registry(Some("82f4315c"), "90f56bd1"));
    }

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
    fn reads_the_v3_vectors_manifest() {
        // Frozen v3 vectors (KaChat 0ed15e9^): the current app file is v4.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../kachat-names/testdata/vectors-v3.json");
        let text = std::fs::read_to_string(path).expect("kachat-names/testdata/vectors-v3.json");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        // The dry run has no chain, so no scan blocks: give it two.
        let mut manifest = v["manifest"].clone();
        manifest["genesis"]["scanFrom"] = serde_json::json!("22".repeat(32));
        manifest["priceGenesis"]["scanFrom"] = serde_json::json!("11".repeat(32));
        let dir = std::env::temp_dir().join("kachat-names-v3-manifest.json");
        std::fs::write(&dir, manifest.to_string()).unwrap();
        let m = read_manifest(dir.to_str().unwrap()).expect("the v3 manifest reads and both geneses verify");
        assert_eq!(m.version, 3);
        assert_eq!(m.price_shards.len(), 8);
        assert_eq!(m.templates.period_ms, 600_000);
        assert_eq!(m.price_covenant_id.as_deref(), v["manifest"]["priceCovenantId"].as_str());
        assert_eq!(hex::encode(m.scan_from), "11".repeat(32), "v3 scans from the price genesis");
    }

    #[test]
    fn reads_the_live_testnet_manifest() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../kachat-domains/manifests/kachat-names-testnet-10.json");
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping: no live manifest");
            return;
        }
        let m = read_manifest(path).unwrap();
        // Registry v4, deployed 2026-10-07 (kachat-domains e774c86); v3 90f56bd1… is retired.
        assert_eq!(m.registry, "bff185546af1940ec70d74143e23b5f018fdb864bd02e15ca9b4c8d8ede40e2f");
        assert_eq!(m.version, 4);
        assert_eq!(m.price_covenant_id, None, "v4 has no price record");
        assert!(m.price_shards.is_empty());
        assert_eq!(m.templates.period_ms, 600_000, "a 10-minute period on testnet");
        assert_eq!(m.grace_ms, 1_800_000, "v4 testnet grace is 30 minutes");
        assert_eq!(m.network, "testnet-10");
        assert_eq!(hex::encode(m.genesis_outpoint.0), m.genesis_txid);
        assert_eq!(m.genesis_gap.lo, [0u8; 32]);
        assert_eq!(m.genesis_gap.hi, [0xffu8; 32]);
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(hex::encode(m.scan_from), raw["genesis"]["scanFrom"].as_str().unwrap());
        let spk = raw["genesis"]["authorizedOutputs"][0]["scriptPublicKey"].as_str().unwrap();
        assert_eq!(hex::encode(m.templates.gap.spk(&m.genesis_gap.encode())), spk);
    }
}

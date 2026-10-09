//! REST bootstrap: rebuild the registry without the node's old blocks
//! (docs/KACHAT_NAMES_PRUNED_START.md §3).
//!
//! A pruned node keeps about a day of blocks, so a follower that starts later than that (a new
//! install, or one that was down too long) can never replay the registry from its start block.
//! This rebuilds it the way the KaChat app's chain walker does
//! (`KachatNamesRegistryState.swift` `walk`), from the tracked set alone:
//!
//! 1. the registry is seeded from the manifest (done by the caller);
//! 2. ask the node which tracked outputs are still unspent (`getUtxosByAddresses`, the same
//!    call the self-test uses);
//! 3. for every spent one, find the spending transaction in the REST API's history of its
//!    P2SH address (`GET {rest}/addresses/{p2sh}/full-transactions`);
//! 4. check the REST transaction against its id (IDX-013): recomputed from its contents with
//!    rusty-kaspa's hashing, filling in what REST leaves out (sequence, lock time, gas) from
//!    the node's copy of the block when the node still has it;
//! 5. apply it with the same engine (`Registry::apply`), which verifies every derived state
//!    against its output's script and covenant binding, exactly as when following the node;
//! 6. repeat until no tracked output is spent; only then does the caller checkpoint at the
//!    node's sink and continue over `getVirtualChainFromBlockV2`. A walk that ends with spent
//!    outputs it could not follow is never checkpointed: the node's chain after the sink would
//!    not carry those spends again.
//!
//! The one gap (as for the app): an offer created before the switch-over is not discoverable
//! (its creating transaction touches no registry output, and REST can't search payloads).
//! Offers live 7 days at most; each is picked up once it is accepted, declined or refunded.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow};
use kachat_names::ingest::{CovenantBinding, Event, Outpoint, Registry, Templates, Tx, TxInput, TxOutput};
use kaspa_addresses::{Address, Prefix};
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::model::RpcHash;
use serde_json::Value;
use tracing::{info, warn};

use crate::node::Node;
use crate::spk_address;

/// The public Kaspa REST API for a network, unless `KACHAT_NAMES_REST_URL` overrides it.
pub fn rest_base(network: &str) -> String {
    if let Ok(url) = std::env::var("KACHAT_NAMES_REST_URL") {
        let url = url.trim().trim_end_matches('/').to_string();
        if !url.is_empty() {
            return url;
        }
    }
    if network == "mainnet" { "https://api.kaspa.org".into() } else { "https://api-tn10.kaspa.org".into() }
}

/// What a walk did.
#[derive(Debug, Default)]
pub struct Report {
    pub rounds: usize,
    /// Every transaction applied, in the order it was applied.
    pub applied: Vec<Tx>,
    pub events: Vec<Event>,
    /// Spent tracked outputs whose spender the REST API did not (yet) have, plus transactions
    /// still waiting for an earlier one. The walk is complete only when this is 0.
    pub unresolved: usize,
    /// Applied on the REST API's word: their id could not be recomputed from the REST data and
    /// the node no longer has their block. The walk's end state is still checked by the node.
    pub unverified: usize,
    /// REST transactions rejected because their contents do not match their id.
    pub rejected: usize,
}

fn hex32(v: &Value) -> Option<[u8; 32]> {
    hex::decode(v.as_str()?).ok()?.try_into().ok()
}

fn num(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

/// A REST transaction plus, per input, the covenant id of the output it spends (REST reports
/// it). The walk uses them to wait until every registry input of a transaction is tracked.
pub struct RestTx {
    pub tx: Tx,
    pub input_covenants: Vec<Option<[u8; 32]>>,
    /// The blocks that contain it (REST `block_hash`), to find the node's copy.
    pub blocks: Vec<[u8; 32]>,
    /// Its contents hash to `tx.id` with REST's own fields (sequence, lock time and gas 0).
    /// Registry transactions usually set a lock time, so most need the node's copy instead.
    pub id_checked: bool,
}

/// One REST transaction (`full-transactions`, `resolve_previous_outpoints=no`) in the engine's
/// shape. `None` for a transaction that is not accepted, or one missing what the engine needs.
/// `id_checked` says whether its contents hash to its `transaction_id` as served; when they
/// don't, the walk asks the node (`verify`) before using it.
pub fn parse_rest_tx(j: &Value) -> Option<RestTx> {
    if j.get("is_accepted").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let id = hex32(&j["transaction_id"])?;
    let mut inputs: Vec<(u64, TxInput, Option<[u8; 32]>, u64)> = Vec::new();
    for (k, i) in j["inputs"].as_array().map(Vec::as_slice).unwrap_or_default().iter().enumerate() {
        let prev = hex32(&i["previous_outpoint_hash"])?;
        let idx = num(&i["previous_outpoint_index"])? as u32;
        let sig = hex::decode(i["signature_script"].as_str().unwrap_or("")).ok()?;
        let cov = i["covenant_id"].as_str().filter(|c| !c.is_empty()).and_then(|c| hex::decode(c).ok()?.try_into().ok());
        inputs.push((
            num(&i["index"]).unwrap_or(k as u64),
            TxInput { previous_outpoint: (prev, idx), signature_script: sig, spent_script: Vec::new() },
            cov,
            // Not served by the REST API today: assume 0 (a wallet's default).
            num(&i["sequence"]).unwrap_or(0),
        ));
    }
    let mut outputs: Vec<(u64, TxOutput, u16)> = Vec::new();
    for (k, o) in j["outputs"].as_array().map(Vec::as_slice).unwrap_or_default().iter().enumerate() {
        let covenant = match o["covenant_id"].as_str().filter(|s| !s.is_empty()) {
            Some(cid) => Some(CovenantBinding {
                authorizing_input: num(&o["covenant_authorizing_input"])? as u16,
                covenant_id: hex::decode(cid).ok()?.try_into().ok()?,
            }),
            None => None,
        };
        outputs.push((
            num(&o["index"]).unwrap_or(k as u64),
            TxOutput { script_public_key: hex::decode(o["script_public_key"].as_str()?).ok()?, value: num(&o["amount"])?, covenant },
            num(&o["script_public_key_version"]).unwrap_or(0) as u16,
        ));
    }
    inputs.sort_by_key(|(k, ..)| *k);
    outputs.sort_by_key(|(k, ..)| *k);
    let input_covenants = inputs.iter().map(|(_, _, c, _)| *c).collect();
    let sequences: Vec<u64> = inputs.iter().map(|(.., s)| *s).collect();
    let spk_versions: Vec<u16> = outputs.iter().map(|(.., v)| *v).collect();
    let tx = Tx {
        id,
        inputs: inputs.into_iter().map(|(_, i, ..)| i).collect(),
        outputs: outputs.into_iter().map(|(_, o, _)| o).collect(),
        payload: hex::decode(j["payload"].as_str().unwrap_or("")).ok()?,
        accepting_block: hex32(&j["accepting_block_hash"]).unwrap_or(id),
        // REST gives the accepting block's blue score, not its DAA score: close enough for the
        // history's ordering column, and nothing derives state from it.
        accepting_daa: num(&j["accepting_block_blue_score"]).unwrap_or(0),
        block_time: j["accepting_block_time"].as_i64().or_else(|| j["block_time"].as_i64()).unwrap_or(0),
    };
    let blocks = match &j["block_hash"] {
        Value::Array(a) => a.iter().filter_map(hex32).collect(),
        v => hex32(v).into_iter().collect(),
    };
    let fields = IdFields {
        version: num(&j["version"]).unwrap_or(0) as u16,
        subnetwork: match j["subnetwork_id"].as_str() {
            Some(s) => hex::decode(s).ok()?.try_into().ok()?,
            None => [0; 20],
        },
        lock_time: num(&j["lock_time"]).unwrap_or(0),
        gas: num(&j["gas"]).unwrap_or(0),
        sequences,
    };
    let id_checked = transaction_id(&tx, &fields, &spk_versions) == id;
    Some(RestTx { tx, input_covenants, blocks, id_checked })
}

/// What a transaction id commits to besides the engine's `Tx`. The REST API serves the version
/// and subnetwork but not `sequence`, `lock_time` or `gas`.
struct IdFields {
    version: u16,
    subnetwork: [u8; 20],
    lock_time: u64,
    gas: u64,
    sequences: Vec<u64>,
}

/// The consensus transaction id of `tx` with `fields`, by rusty-kaspa's own hashing (signature
/// scripts and mass are not part of it).
fn transaction_id(tx: &Tx, fields: &IdFields, spk_versions: &[u16]) -> [u8; 32] {
    use kaspa_consensus_core::tx as ktx;
    let inputs = tx
        .inputs
        .iter()
        .zip(&fields.sequences)
        .map(|(i, seq)| {
            ktx::TransactionInput::new(
                ktx::TransactionOutpoint::new(kaspa_consensus_core::Hash::from_bytes(i.previous_outpoint.0), i.previous_outpoint.1),
                Vec::new(),
                *seq,
                0,
            )
        })
        .collect();
    let outputs = tx
        .outputs
        .iter()
        .zip(spk_versions)
        .map(|(o, v)| {
            ktx::TransactionOutput::with_covenant(
                o.value,
                ktx::ScriptPublicKey::from_vec(*v, o.script_public_key.clone()),
                o.covenant.map(|c| ktx::CovenantBinding::new(c.authorizing_input, kaspa_consensus_core::Hash::from_bytes(c.covenant_id))),
            )
        })
        .collect();
    let ktx = ktx::Transaction::new(
        fields.version,
        inputs,
        outputs,
        fields.lock_time,
        kaspa_consensus_core::subnets::SubnetworkId::from_bytes(fields.subnetwork),
        fields.gas,
        tx.payload.clone(),
    );
    ktx.id().as_bytes()
}

/// How a REST transaction checked out against the node.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Its id is recomputed from its contents (with the node's sequence/lock time/gas when
    /// REST lacks them) and the node's copy carries the same signature scripts.
    Verified,
    /// The node no longer has any block containing it (pruned): taken on REST's word.
    Unverifiable,
    /// Contents and id disagree, or the node's block does not contain it.
    Forged,
}

async fn verify(node: &Node, rt: &RestTx) -> Verdict {
    if rt.id_checked && rt.blocks.is_empty() {
        return Verdict::Verified;
    }
    for block in &rt.blocks {
        let Ok(b) = node.client.get_block(RpcHash::from_bytes(*block), true).await else { continue };
        // A pruned block may still answer with its header alone (every real body has a coinbase).
        if b.transactions.is_empty() {
            continue;
        }
        let Some(ntx) = b.transactions.into_iter().find_map(|t| {
            let t = kaspa_consensus_core::tx::Transaction::try_from(t).ok()?;
            (t.id().as_bytes() == rt.tx.id).then_some(t)
        }) else {
            return Verdict::Forged;
        };
        let fields = IdFields {
            version: ntx.version,
            subnetwork: *AsRef::<[u8; 20]>::as_ref(&ntx.subnetwork_id),
            lock_time: ntx.lock_time,
            gas: ntx.gas,
            sequences: ntx.inputs.iter().map(|i| i.sequence).collect(),
        };
        let spk_versions: Vec<u16> = ntx.outputs.iter().map(|o| o.script_public_key.version()).collect();
        let same_inputs = ntx.inputs.len() == rt.tx.inputs.len()
            && ntx.inputs.iter().zip(&rt.tx.inputs).all(|(n, r)| n.signature_script == r.signature_script);
        return if same_inputs && spk_versions.len() == rt.tx.outputs.len() && transaction_id(&rt.tx, &fields, &spk_versions) == rt.tx.id {
            Verdict::Verified
        } else {
            Verdict::Forged
        };
    }
    if rt.id_checked { Verdict::Verified } else { Verdict::Unverifiable }
}

/// The outpoints the node still has unspent, for these addresses.
async fn unspent(node: &Node, addresses: &[String]) -> Result<HashSet<Outpoint>> {
    let mut out = HashSet::new();
    for chunk in addresses.chunks(200) {
        let addrs = chunk.iter().map(|a| Address::try_from(a.as_str())).collect::<Result<Vec<_>, _>>()?;
        for e in node.client.get_utxos_by_addresses(addrs).await.map_err(|e| anyhow!("getUtxosByAddresses: {e}"))? {
            out.insert((e.outpoint.transaction_id.as_bytes(), e.outpoint.index));
        }
    }
    Ok(out)
}

/// The transactions in an address's REST history that spend any of `wanted`.
///
/// A registry P2SH address holds one state, so the covenant only ever creates and spends it
/// once; but anyone can pay to a P2SH address, and the API lists newest first, so the spender
/// can sit behind any number of later payments (IDX-013). Pages until every wanted outpoint's
/// spender is seen or the history ends, up to `MAX_PAGES`. A transaction that shifts onto a
/// later page while paging is seen twice (deduplicated), never skipped.
async fn address_history(http: &reqwest::Client, base: &str, address: &str, wanted: &HashSet<Outpoint>) -> Result<Vec<RestTx>> {
    const PAGE: usize = 50;
    const MAX_PAGES: usize = 100;
    let mut out: Vec<RestTx> = Vec::new();
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut found: HashSet<Outpoint> = HashSet::new();
    for page in 0..MAX_PAGES {
        let url = format!(
            "{base}/addresses/{address}/full-transactions?limit={PAGE}&offset={}&resolve_previous_outpoints=no",
            page * PAGE
        );
        let body: Value = http.get(&url).send().await?.error_for_status()?.json().await.with_context(|| format!("GET {url}"))?;
        let rows = body.as_array().map(Vec::as_slice).unwrap_or_default();
        for rt in rows.iter().filter_map(parse_rest_tx) {
            let hits: Vec<Outpoint> = rt.tx.inputs.iter().map(|i| i.previous_outpoint).filter(|op| wanted.contains(op)).collect();
            if hits.is_empty() || !seen.insert(rt.tx.id) {
                continue;
            }
            found.extend(hits);
            out.push(rt);
        }
        if rows.len() < PAGE || wanted.iter().all(|op| found.contains(op)) {
            return Ok(out);
        }
    }
    warn!("[names] bootstrap: {address}: gave up after {} transactions without finding every spender", MAX_PAGES * PAGE);
    Ok(out)
}

/// Walk the registry forward from its tracked set to the present, applying every spend.
///
/// Progress goes into `report` as it happens, so a walk that fails or stops short can be
/// resumed by calling again with the same registry and report (the REST API may lag behind
/// a recent spend). Complete when it returns `Ok` with `report.unresolved == 0`.
pub async fn walk(
    node: &Node,
    http: &reqwest::Client,
    base: &str,
    prefix: Prefix,
    templates: &Templates,
    reg: &mut Registry,
    report: &mut Report,
) -> Result<()> {
    const MAX_ROUNDS: usize = 512;
    let mut applied_ids: HashSet<[u8; 32]> = report.applied.iter().map(|t| t.id).collect();
    let mut rejected_ids: HashSet<[u8; 32]> = HashSet::new();
    report.unresolved = 0;
    for _ in 0..MAX_ROUNDS {
        report.rounds += 1;
        let mut by_address: HashMap<String, Vec<Outpoint>> = HashMap::new();
        for (op, tracked) in &reg.utxos {
            let spk = templates.tracked_spk(tracked);
            let a = spk_address(prefix, &spk).ok_or_else(|| anyhow!("unaddressable registry script"))?;
            by_address.entry(a).or_default().push(*op);
        }
        let addresses: Vec<String> = by_address.keys().cloned().collect();
        let live = unspent(node, &addresses).await?;
        let spent: Vec<(String, Outpoint)> =
            by_address.iter().flat_map(|(a, ops)| ops.iter().filter(|op| !live.contains(*op)).map(|op| (a.clone(), *op))).collect();
        if spent.is_empty() {
            report.unresolved = 0;
            return Ok(());
        }

        let wanted: HashSet<Outpoint> = spent.iter().map(|(_, op)| *op).collect();
        let mut wanted_at: HashMap<String, HashSet<Outpoint>> = HashMap::new();
        for (a, op) in &spent {
            wanted_at.entry(a.clone()).or_default().insert(*op);
        }
        let mut candidates: HashMap<[u8; 32], RestTx> = HashMap::new();
        let mut found: HashSet<Outpoint> = HashSet::new();
        for (a, at) in &wanted_at {
            for rt in address_history(http, base, a, at).await? {
                if applied_ids.contains(&rt.tx.id) || rejected_ids.contains(&rt.tx.id) || candidates.contains_key(&rt.tx.id) {
                    continue;
                }
                // IDX-013: only a transaction whose contents match its id is used.
                match verify(node, &rt).await {
                    Verdict::Verified => {}
                    Verdict::Unverifiable => report.unverified += 1,
                    Verdict::Forged => {
                        warn!("[names] bootstrap: REST transaction {} does not match its id on the node; ignored", hex::encode(rt.tx.id));
                        rejected_ids.insert(rt.tx.id);
                        report.rejected += 1;
                        continue;
                    }
                }
                found.extend(rt.tx.inputs.iter().map(|i| i.previous_outpoint).filter(|op| wanted.contains(op)));
                candidates.insert(rt.tx.id, rt);
            }
        }
        report.unresolved = wanted.iter().filter(|op| !found.contains(*op)).count();

        // Apply a transaction only once every input it takes from the registry (or price)
        // covenant is tracked: it may also spend an output an earlier transaction of this
        // walk is still to create (a renew of a name whose listing is pending). Applying it
        // early would apply only half of it. Several passes per round, so one transaction
        // can make the next one ready.
        let ours = |c: &Option<[u8; 32]>| c.is_some() && (*c == templates.registry_id || *c == templates.price_id);
        let mut pending: Vec<RestTx> = candidates.into_values().collect();
        pending.sort_by(|a, b| (a.tx.accepting_daa, a.tx.block_time, a.tx.id).cmp(&(b.tx.accepting_daa, b.tx.block_time, b.tx.id)));
        let mut applied_this_round = 0;
        loop {
            let mut rest = Vec::new();
            let mut progressed = false;
            for rt in pending {
                let ready = rt
                    .tx
                    .inputs
                    .iter()
                    .zip(&rt.input_covenants)
                    .all(|(i, c)| !ours(c) || reg.utxos.contains_key(&i.previous_outpoint));
                if !ready {
                    rest.push(rt);
                    continue;
                }
                let events = reg.apply(templates, &rt.tx);
                applied_ids.insert(rt.tx.id);
                report.events.extend(events);
                report.applied.push(rt.tx);
                applied_this_round += 1;
                progressed = true;
            }
            pending = rest;
            if !progressed || pending.is_empty() {
                break;
            }
        }
        if applied_this_round == 0 {
            // Nothing could move: what is left waits for a transaction the REST API does not
            // have yet (or never will). The walk is incomplete; the caller must not checkpoint.
            report.unresolved += pending.len();
            warn!(
                "[names] bootstrap: stopped with {} spent output(s) whose spender the REST API does not have and {} transaction(s) waiting for an earlier one",
                report.unresolved - pending.len(),
                pending.len()
            );
            return Ok(());
        }
        info!("[names] bootstrap: round {}: {} transaction(s) applied so far", report.rounds, report.applied.len());
    }
    Err(anyhow!("bootstrap did not settle after {MAX_ROUNDS} rounds"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_rest_transaction() {
        // The shape api-tn10.kaspa.org serves (v4 genesis, trimmed).
        let j: Value = serde_json::from_str(
            r#"{"transaction_id":"b1f28a5f3ff917dc567fa038c80dc50539d42fa6f088bb2e008d5dad82f685a1",
                "is_accepted":true,"payload":"6b",
                "accepting_block_hash":"7f8e2f8aa1994eb33f2901a58e4875e6720bc90c24c73823b6f23366fcffcc75",
                "accepting_block_blue_score":578870159,"accepting_block_time":1791374408034,"block_time":1791374408000,
                "inputs":[{"index":0,"previous_outpoint_hash":"11111111111111111111111111111111111111111111111111111111111111aa",
                           "previous_outpoint_index":"2","signature_script":"0102"}],
                "outputs":[{"index":1,"amount":5,"script_public_key":"20aa","covenant_id":null},
                           {"index":0,"amount":"100000000","script_public_key":"aa2087",
                            "covenant_id":"bff185546af1940ec70d74143e23b5f018fdb864bd02e15ca9b4c8d8ede40e2f","covenant_authorizing_input":0}]}"#,
        )
        .unwrap();
        let rt = parse_rest_tx(&j).expect("parses");
        assert!(!rt.id_checked, "the trimmed sample's id is not its contents' id");
        assert_eq!(rt.blocks, Vec::<[u8; 32]>::new());
        let tx = &rt.tx;
        assert_eq!(tx.inputs[0].previous_outpoint.1, 2);
        assert_eq!(tx.inputs[0].signature_script, vec![1, 2]);
        assert_eq!(tx.outputs[0].value, 100_000_000, "outputs in index order");
        assert_eq!(tx.outputs[0].covenant.unwrap().authorizing_input, 0);
        assert!(tx.outputs[1].covenant.is_none());
        assert_eq!(tx.accepting_daa, 578_870_159);
        assert_eq!(tx.block_time, 1_791_374_408_034);
        let mut rejected = j.clone();
        rejected["is_accepted"] = Value::Bool(false);
        assert!(parse_rest_tx(&rejected).is_none(), "an unaccepted transaction is skipped");
    }

    #[test]
    fn rest_transaction_ids_are_recomputed() {
        // Live testnet-10 transactions as api-tn10.kaspa.org serves them (the v4 registry
        // genesis and its first register). Signature scripts are not part of the id, so they
        // are trimmed; everything else is verbatim.
        let genesis: Value = serde_json::from_str(GENESIS).unwrap();
        let rt = parse_rest_tx(&genesis).unwrap();
        assert!(rt.id_checked, "the genesis (sequence, lock time and gas 0) hashes to its id from REST data alone");

        // Any change to what the id commits to is caught: an output's amount, its script, its
        // covenant binding, an outpoint, the payload, the version.
        let tampered = [
            ("/outputs/1/amount", serde_json::json!(799_393_044u64)),
            ("/outputs/0/script_public_key", serde_json::json!("aa201192c1f723ef3e6b3d206b3346167a6c04ee371966a98ce99f1dadcc2f23704688")),
            ("/outputs/0/covenant_authorizing_input", serde_json::json!(1)),
            ("/inputs/0/previous_outpoint_index", serde_json::json!("0")),
            ("/payload", serde_json::json!("6b")),
            ("/version", serde_json::json!(0)),
        ];
        for (path, v) in tampered {
            let mut j = genesis.clone();
            *j.pointer_mut(path).unwrap() = v;
            assert!(!parse_rest_tx(&j).unwrap().id_checked, "{path} changed but still hashes to the id");
        }
        // The signature script is not committed to by the id; `verify` compares it with the
        // node's copy instead.
        let mut j = genesis.clone();
        j["inputs"][0]["signature_script"] = serde_json::json!("0102");
        assert!(parse_rest_tx(&j).unwrap().id_checked);

        // A register sets a lock time (and its commit input a sequence), which REST does not
        // serve: its id can't be recomputed from REST alone, so `verify` asks the node.
        let register: Value = serde_json::from_str(REGISTER).unwrap();
        let rt = parse_rest_tx(&register).unwrap();
        assert!(!rt.id_checked);
        assert_eq!(rt.blocks, vec![hex32(&serde_json::json!("d0455f09afcc2f8b4842d1fcd26c78c59aeaa9349eece44d861bc52ae92a5625")).unwrap()]);

        // With the fields the node supplies, the REST contents hash to the id again.
        let mut j = genesis.clone();
        let mut fields = IdFields { version: 1, subnetwork: [0; 20], lock_time: 1_791_404_400_000, gas: 0, sequences: vec![7] };
        let id = transaction_id(&parse_rest_tx(&genesis).unwrap().tx, &fields, &[0, 0]);
        j["transaction_id"] = serde_json::json!(hex::encode(id));
        let rt = parse_rest_tx(&j).unwrap();
        assert!(!rt.id_checked);
        assert_eq!(transaction_id(&rt.tx, &fields, &[0, 0]), id);
        fields.lock_time += 1;
        assert_ne!(transaction_id(&rt.tx, &fields, &[0, 0]), id);
        // And a REST source that does serve them is checked with them.
        j["lock_time"] = serde_json::json!(1_791_404_400_000u64);
        j["inputs"][0]["sequence"] = serde_json::json!(7);
        assert!(parse_rest_tx(&j).unwrap().id_checked);
    }

    const GENESIS: &str = r#"{"subnetwork_id":"0000000000000000000000000000000000000000","transaction_id":"5ffdd006230bcba0ee52c3ce7b69fba2b9a4d489b57fee93e2b103eb1622a777","payload":null,"version":1,"is_accepted":true,"accepting_block_hash":"524c136ebbb87ffd94ed580553dc2f102a0c04892ebde75e09918c4ddfb0444a","accepting_block_blue_score":579170588,"accepting_block_time":1791404475022,"inputs":[{"index":0,"previous_outpoint_hash":"b1f28a5f3ff917dc567fa038c80dc50539d42fa6f088bb2e008d5dad82f685a1","previous_outpoint_index":"1","signature_script":"00","covenant_id":null}],"outputs":[{"index":0,"amount":100000000,"script_public_key":"aa201192c1f723ef3e6b3d206b3346167a6c04ee371966a98ce99f1dadcc2f23704687","covenant_authorizing_input":0,"covenant_id":"e6b7244831004e1db928458bce570347317b50ff124c010d342d73a6c2017f0d"},{"index":1,"amount":799393043,"script_public_key":"20a866cf597e3e681324adbc115ec34ca7746813cf70f6bfe4f9c36f2c9dd30848ac","covenant_authorizing_input":null,"covenant_id":null}]}"#;
    const REGISTER: &str = r#"{"subnetwork_id":"0000000000000000000000000000000000000000","transaction_id":"0847d5c9dc2f4ce3fb427d304ca404593184594ecd29fcefdc7aa193012139b5","block_hash":["d0455f09afcc2f8b4842d1fcd26c78c59aeaa9349eece44d861bc52ae92a5625"],"payload":"6b636861743a313a6e616d653a72656769737465723a6b","version":1,"is_accepted":true,"accepting_block_hash":"45738d628b7aed95cf857ce4995cbfe10e5d129506b6719c6d59b05ab0cd3880","accepting_block_blue_score":579226604,"accepting_block_time":1791409658754,"inputs":[{"index":0,"previous_outpoint_hash":"5ffdd006230bcba0ee52c3ce7b69fba2b9a4d489b57fee93e2b103eb1622a777","previous_outpoint_index":"0","signature_script":"00","covenant_id":"e6b7244831004e1db928458bce570347317b50ff124c010d342d73a6c2017f0d"},{"index":1,"previous_outpoint_hash":"21a9dd8475e157706fea674c033eb25427ad386c88337b107c6536ce88b1ae2c","previous_outpoint_index":"0","signature_script":"00","covenant_id":null},{"index":2,"previous_outpoint_hash":"21a9dd8475e157706fea674c033eb25427ad386c88337b107c6536ce88b1ae2c","previous_outpoint_index":"1","signature_script":"00","covenant_id":null}],"outputs":[{"index":0,"amount":100000000,"script_public_key":"aa20e0a93fb4d5447b0580a32fd00b1742d5d1f171862925f9f6b27e2def70ba877e87","covenant_authorizing_input":0,"covenant_id":"e6b7244831004e1db928458bce570347317b50ff124c010d342d73a6c2017f0d"},{"index":1,"amount":100000000,"script_public_key":"aa20fdf4f3352cea607d5782197a3a1216ac99cf6a6a99e2b7a445b29cd8b71b826187","covenant_authorizing_input":0,"covenant_id":"e6b7244831004e1db928458bce570347317b50ff124c010d342d73a6c2017f0d"},{"index":2,"amount":100000000,"script_public_key":"aa20aa3b388806d43482297d30069f626677fc761ed0b6f28c64953b03ac4902603187","covenant_authorizing_input":0,"covenant_id":"e6b7244831004e1db928458bce570347317b50ff124c010d342d73a6c2017f0d"},{"index":3,"amount":991583689898,"script_public_key":"20af1044f8c7ac523b7862ac6d6d5d91909d22d93001775ae65dce7f3541e4950dac","covenant_authorizing_input":null,"covenant_id":null}]}"#;

    #[test]
    fn rest_base_follows_the_network() {
        if std::env::var("KACHAT_NAMES_REST_URL").is_ok() {
            return;
        }
        assert_eq!(rest_base("mainnet"), "https://api.kaspa.org");
        assert_eq!(rest_base("testnet-10"), "https://api-tn10.kaspa.org");
    }
}

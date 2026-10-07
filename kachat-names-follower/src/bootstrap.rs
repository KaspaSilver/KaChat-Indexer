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
//! 4. apply it with the same engine (`Registry::apply`), which verifies every derived state
//!    against its output's script and covenant binding, exactly as when following the node;
//! 5. repeat until no tracked output is spent; the caller then checkpoints at the node's sink
//!    and continues over `getVirtualChainFromBlockV2`.
//!
//! The one gap (as for the app): an offer created before the switch-over is not discoverable
//! (its creating transaction touches no registry output, and REST can't search payloads).
//! Offers live 7 days at most; each is picked up once it is accepted, declined or refunded.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow};
use kachat_names::ingest::{CovenantBinding, Event, Outpoint, Registry, Templates, Tx, TxInput, TxOutput};
use kaspa_addresses::{Address, Prefix};
use kaspa_rpc_core::api::rpc::RpcApi;
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
    /// Spent tracked outputs whose spender the REST API did not (yet) have.
    pub unresolved: usize,
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
}

/// One REST transaction (`full-transactions`, `resolve_previous_outpoints=no`) in the engine's
/// shape. `None` for a transaction that is not accepted, or one missing what the engine needs.
pub fn parse_rest_tx(j: &Value) -> Option<RestTx> {
    if j.get("is_accepted").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let id = hex32(&j["transaction_id"])?;
    let mut inputs: Vec<(u64, TxInput, Option<[u8; 32]>)> = Vec::new();
    for (k, i) in j["inputs"].as_array().map(Vec::as_slice).unwrap_or_default().iter().enumerate() {
        let prev = hex32(&i["previous_outpoint_hash"])?;
        let idx = num(&i["previous_outpoint_index"])? as u32;
        let sig = hex::decode(i["signature_script"].as_str().unwrap_or("")).ok()?;
        let cov = i["covenant_id"].as_str().filter(|c| !c.is_empty()).and_then(|c| hex::decode(c).ok()?.try_into().ok());
        inputs.push((
            num(&i["index"]).unwrap_or(k as u64),
            TxInput { previous_outpoint: (prev, idx), signature_script: sig, spent_script: Vec::new() },
            cov,
        ));
    }
    let mut outputs: Vec<(u64, TxOutput)> = Vec::new();
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
        ));
    }
    inputs.sort_by_key(|(k, _, _)| *k);
    outputs.sort_by_key(|(k, _)| *k);
    let input_covenants = inputs.iter().map(|(_, _, c)| *c).collect();
    let tx = Tx {
        id,
        inputs: inputs.into_iter().map(|(_, i, _)| i).collect(),
        outputs: outputs.into_iter().map(|(_, o)| o).collect(),
        payload: hex::decode(j["payload"].as_str().unwrap_or("")).ok()?,
        accepting_block: hex32(&j["accepting_block_hash"]).unwrap_or(id),
        // REST gives the accepting block's blue score, not its DAA score: close enough for the
        // history's ordering column, and nothing derives state from it.
        accepting_daa: num(&j["accepting_block_blue_score"]).unwrap_or(0),
        block_time: j["accepting_block_time"].as_i64().or_else(|| j["block_time"].as_i64()).unwrap_or(0),
    };
    Some(RestTx { tx, input_covenants })
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

async fn address_history(http: &reqwest::Client, base: &str, address: &str) -> Result<Vec<RestTx>> {
    // A registry P2SH address holds one state, so it sees two transactions: the one that
    // created the output and the one that spent it. 50 is the app's page size too.
    let url = format!("{base}/addresses/{address}/full-transactions?limit=50&offset=0&resolve_previous_outpoints=no");
    let body: Value = http.get(&url).send().await?.error_for_status()?.json().await.with_context(|| format!("GET {url}"))?;
    Ok(body.as_array().map(|a| a.iter().filter_map(parse_rest_tx).collect()).unwrap_or_default())
}

/// Walk the registry forward from its tracked set to the present, applying every spend.
pub async fn walk(
    node: &Node,
    http: &reqwest::Client,
    base: &str,
    prefix: Prefix,
    templates: &Templates,
    reg: &mut Registry,
) -> Result<Report> {
    const MAX_ROUNDS: usize = 512;
    let mut report = Report::default();
    let mut applied_ids: HashSet<[u8; 32]> = HashSet::new();
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
            return Ok(report);
        }

        let wanted: HashSet<Outpoint> = spent.iter().map(|(_, op)| *op).collect();
        let mut candidates: HashMap<[u8; 32], RestTx> = HashMap::new();
        let mut found: HashSet<Outpoint> = HashSet::new();
        for a in spent.iter().map(|(a, _)| a.clone()).collect::<HashSet<_>>() {
            for rt in address_history(http, base, &a).await? {
                let hits: Vec<Outpoint> = rt.tx.inputs.iter().map(|i| i.previous_outpoint).filter(|op| wanted.contains(op)).collect();
                if !hits.is_empty() {
                    found.extend(hits);
                    candidates.insert(rt.tx.id, rt);
                }
            }
        }
        report.unresolved = wanted.iter().filter(|op| !found.contains(*op)).count();
        candidates.retain(|id, _| !applied_ids.contains(id));

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
            // have yet (or never will). The node takes over from here.
            if report.unresolved > 0 || !pending.is_empty() {
                warn!(
                    "[names] bootstrap: stopped with {} unresolved spend(s) and {} transaction(s) waiting for an earlier one",
                    report.unresolved,
                    pending.len()
                );
            }
            report.unresolved += pending.len();
            return Ok(report);
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
    fn rest_base_follows_the_network() {
        if std::env::var("KACHAT_NAMES_REST_URL").is_ok() {
            return;
        }
        assert_eq!(rest_base("mainnet"), "https://api.kaspa.org");
        assert_eq!(rest_base("testnet-10"), "https://api-tn10.kaspa.org");
    }
}

//! Replays the `kachat-names-vectors` transactions (28 steps, built by kachat-domains' own
//! builder and shipped in the iOS repo as `KaChatTests/KachatNamesVectors.json`) through the
//! applier. Every registry state the applier records must hash to the exact output script
//! the real builder produced, so this checks transitions + templates + P2SH end to end.
//!
//! The file is looked up at `$KACHAT_NAMES_VECTORS`, else next to this repo in the
//! "Everything KaChat" layout (`../KaChat/KaChatTests/KachatNamesVectors.json`); the tests
//! skip when neither exists.

use std::collections::HashSet;

use serde_json::Value;

use crate::ingest::{Outpoint, Registry, Templates, Tracked, Tx, TxInput, TxOutput};
use crate::{pad_name, GapState, NameState, OfferState};

fn vectors() -> Option<Value> {
    let path = std::env::var("KACHAT_NAMES_VECTORS").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../KaChat/KaChatTests/KachatNamesVectors.json").to_string()
    });
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn h(v: &Value) -> Vec<u8> {
    hex::decode(v.as_str().unwrap()).unwrap()
}
fn h32(v: &Value) -> [u8; 32] {
    h(v).try_into().unwrap()
}
fn outpoint(utxo: &Value) -> Outpoint {
    (h32(&utxo["txid"]), utxo["index"].as_u64().unwrap() as u32)
}

fn tracked_spk(t: &Templates, tracked: &Tracked) -> Vec<u8> {
    match tracked {
        Tracked::Gap(g) => t.gap.spk(&g.encode()),
        Tracked::Name(n) => t.name.spk(&n.encode()),
        Tracked::Offer(o) => t.offer.spk(&o.encode()),
    }
}

/// The registry UTXOs a step's `records` say exist before it (gap/above/below, name/target,
/// offer). Commits are not registry UTXOs.
fn prior_states(records: &Value) -> Vec<(Outpoint, Tracked, Vec<u8>)> {
    let mut out = Vec::new();
    for (role, r) in records.as_object().unwrap() {
        let tracked = match role.as_str() {
            "gap" | "above" | "below" => Tracked::Gap(GapState { lo: h32(&r["lo"]), hi: h32(&r["hi"]) }),
            "name" | "target" => Tracked::Name(NameState {
                key: h32(&r["key"]),
                name: pad_name(r["name"].as_str().unwrap().as_bytes()),
                owner: h32(&r["owner"]),
                price: r["price"].as_i64().unwrap(),
                period_start: r["periodStart"].as_i64().expect("v2 name record carries periodStart"),
                expires_at: r["expiresAt"].as_i64().unwrap(),
            }),
            "offer" => Tracked::Offer(OfferState {
                key: h32(&r["key"]),
                buyer: h32(&r["buyer"]),
                refund_after: r["refundAfter"].as_i64().unwrap(),
            }),
            _ => continue,
        };
        out.push((outpoint(&r["utxo"]), tracked, h(&r["utxo"]["script"])));
    }
    out
}

fn tx_of(step: &Value) -> Tx {
    let e = &step["expected"];
    let id = h32(&e["txid"]);
    Tx {
        id,
        inputs: e["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| TxInput { spent_script: Vec::new(),
                previous_outpoint: (h32(&i["txid"]), i["index"].as_u64().unwrap() as u32),
                signature_script: h(&i["signatureScript"]),
            })
            .collect(),
        outputs: e["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| TxOutput { script_public_key: h(&o["script"]), value: o["value"].as_u64().unwrap() })
            .collect(),
        payload: h(&e["payload"]),
        // One block per step: unique and good enough for the undo journal here.
        accepting_block: id,
        accepting_daa: step["env"]["blockDaa"].as_u64().unwrap(),
        block_time: step["env"]["blockTimeMs"].as_i64().unwrap(),
    }
}

/// Outputs that are registry UTXOs: covenant-bound gap/name outputs, plus the offer P2SH an
/// offer-creating tx (payload `kchat:1:offer:`) makes.
fn registry_outputs(step: &Value) -> Vec<(u32, Vec<u8>)> {
    let e = &step["expected"];
    let is_offer_tx = e["payloadText"].as_str().unwrap_or("").starts_with("kchat:1:offer:");
    e["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, o)| {
            !o["covenant"].is_null()
                || (is_offer_tx && o["label"].as_str().unwrap_or("").starts_with("offer P2SH"))
        })
        .map(|(i, o)| (i as u32, h(&o["script"])))
        .collect()
}

/// Each step on its own: seed exactly its prior registry UTXOs, apply it, and require that
/// (1) every seeded state hashes to its recorded script (templates + state encoding),
/// (2) every registry output is tracked with a state that hashes to that output's script,
/// (3) every spent registry input is gone, and (4) nothing else was added.
#[test]
fn every_vector_step_applies_exactly() {
    let Some(v) = vectors() else {
        eprintln!("skipping: no KachatNamesVectors.json");
        return;
    };
    let t = Templates::from_manifest(&v["manifest"]).expect("templates from the vectors manifest");
    let mut replayed: Vec<&str> = Vec::new();

    for step in v["steps"].as_array().unwrap() {
        let label = step["label"].as_str().unwrap();
        let mut reg = Registry::new();
        for (op, tracked, script) in prior_states(&step["records"]) {
            assert_eq!(tracked_spk(&t, &tracked), script, "[{label}] seeded state hashes to its utxo script");
            reg.utxos.insert(op, tracked);
        }
        let seeded: HashSet<Outpoint> = reg.utxos.keys().copied().collect();

        let tx = tx_of(step);
        let events = reg.apply(&t, &tx);

        // The history op the step must produce (none for commits, refunds, withdrawals).
        let want_op = match step["op"].as_str().unwrap() {
            "register" => Some("register"),
            "renew" => Some("renew"),
            "extend" => Some("extend"),
            "transfer" => Some("transfer"),
            "list" if label.starts_with("delist") => Some("delist"),
            "list" => Some("list"),
            "buy" => Some("sale"),
            "offer" => Some("offer"),
            "acceptOffer" => Some("offer_accepted"),
            "release" => Some("release"),
            "reclaim" => Some("reclaim"),
            _ => None,
        };
        let ops: Vec<&str> = events.iter().map(|e| e.op).collect();
        match want_op {
            Some(op) => assert_eq!(ops, vec![op], "[{label}] history op"),
            None => assert!(ops.is_empty(), "[{label}] no history op, got {ops:?}"),
        }

        let expected = registry_outputs(step);
        for (i, script) in &expected {
            let got = reg.utxos.get(&(tx.id, *i)).unwrap_or_else(|| panic!("[{label}] output {i} not tracked"));
            assert_eq!(&tracked_spk(&t, got), script, "[{label}] output {i} state hashes to its script");
        }
        for input in &tx.inputs {
            if seeded.contains(&input.previous_outpoint) {
                assert!(!reg.utxos.contains_key(&input.previous_outpoint), "[{label}] spent input still tracked");
            }
        }
        let added: HashSet<u32> =
            reg.utxos.keys().filter(|(id, _)| *id == tx.id).map(|(_, i)| *i).collect();
        let want: HashSet<u32> = expected.iter().map(|(i, _)| *i).collect();
        assert_eq!(added, want, "[{label}] exactly the registry outputs are tracked");
        replayed.push(step["op"].as_str().unwrap());
    }
    eprintln!("replayed {} vector transactions: {:?}", replayed.len(), replayed);
}

/// The connected lifecycle (steps up to `reclaim lapse-tn`): start from the genesis gap only
/// and apply in order; after every step the tracked set must equal the live registry UTXOs.
#[test]
fn connected_lifecycle_tracks_the_live_set() {
    let Some(v) = vectors() else {
        eprintln!("skipping: no KachatNamesVectors.json");
        return;
    };
    let t = Templates::from_manifest(&v["manifest"]).unwrap();
    let genesis = &v["manifest"]["genesis"];
    let gap0 = &genesis["authorizedOutputs"][0];
    let genesis_op: Outpoint = (h32(&genesis["txid"]), gap0["index"].as_u64().unwrap() as u32);

    let mut reg = Registry::new();
    reg.seed_genesis(genesis_op, GapState { lo: h32(&gap0["state"]["lo"]), hi: h32(&gap0["state"]["hi"]) });
    let mut live: HashSet<Outpoint> = [genesis_op].into();

    for step in v["steps"].as_array().unwrap() {
        let label = step["label"].as_str().unwrap();
        let tx = tx_of(step);
        reg.apply(&t, &tx);
        for input in &tx.inputs {
            live.remove(&input.previous_outpoint);
        }
        for (i, _) in registry_outputs(step) {
            live.insert((tx.id, i));
        }
        let tracked: HashSet<Outpoint> = reg.utxos.keys().copied().collect();
        assert_eq!(tracked, live, "[{label}] tracked set == live registry UTXOs");
        if label == "reclaim lapse-tn" {
            // The chain ends here; later steps are standalone edge cases on synthetic UTXOs.
            assert_eq!(reg.names().count(), 1, "only alpha-tn is left");
            return;
        }
    }
    panic!("lifecycle end marker not found");
}

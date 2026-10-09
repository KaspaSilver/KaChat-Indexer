//! Replays the `kachat-names-vectors` transactions (built by kachat-domains' own builder)
//! through the applier. Every registry state the applier records must hash to the exact
//! output script the real builder produced, so this checks transitions + templates + P2SH
//! (+ v3 covenant bindings) end to end.
//!
//! Every registry generation the follower supports, so each stays exactly right:
//! - **the current file** shipped in the iOS repo as `KaChatTests/KachatNamesVectors.json`
//!   (looked up at `$KACHAT_NAMES_VECTORS`, else next to this repo in the "Everything KaChat"
//!   layout). Skipped when absent.
//! - **frozen copies:** `testdata/vectors-v2.json` (KaChat `e1e3455^`), `vectors-v3.json`
//!   (KaChat `0ed15e9^`), `vectors-v4.json` (KaChat `d82dfb2`) and `vectors-v5.json`
//!   (kachat-domains `6eddc7a`, `vectors/KachatNamesVectors-v5.json`: the migration `import`).

use std::collections::HashSet;

use serde_json::Value;

use crate::ingest::{CovenantBinding, Outpoint, Registry, Templates, Tracked, Tx, TxInput, TxOutput};
use crate::{pad_name, GapState, NameState, OfferState, PriceState};

fn load(path: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// (label, vectors) for every vector file present.
fn vector_sets() -> Vec<(&'static str, Value)> {
    let v3 = std::env::var("KACHAT_NAMES_VECTORS").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../KaChat/KaChatTests/KachatNamesVectors.json").to_string()
    });
    let mut out = Vec::new();
    if let Some(v) = load(&v3) {
        out.push(("current (iOS repo)", v));
    } else {
        eprintln!("skipping the current vectors: no KachatNamesVectors.json");
    }
    let v2 = load(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/vectors-v2.json")).expect("testdata/vectors-v2.json");
    out.push(("v2 (testdata)", v2));
    let v3 = load(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/vectors-v3.json")).expect("testdata/vectors-v3.json");
    out.push(("v3 (testdata)", v3));
    let v4 = load(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/vectors-v4.json")).expect("testdata/vectors-v4.json");
    out.push(("v4 (testdata)", v4));
    let v5 = load(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/vectors-v5.json")).expect("testdata/vectors-v5.json");
    out.push(("v5 (testdata)", v5));
    out
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

fn shard_of(r: &Value) -> PriceState {
    let prices: Vec<i64> = r["prices"].as_array().unwrap().iter().map(|p| p.as_i64().unwrap()).collect();
    PriceState { shard: r["shard"].as_i64().unwrap(), authority: h32(&r["authority"]), prices: prices.try_into().unwrap() }
}

/// The tracked UTXOs a step's `records` say exist before it (gap/above/below, name/target,
/// offer, shard/shards). Commits are not registry UTXOs.
fn prior_states(records: &Value) -> Vec<(Outpoint, Tracked, Vec<u8>)> {
    let mut out = Vec::new();
    for (role, r) in records.as_object().unwrap() {
        let mut push = |r: &Value, tracked: Tracked| out.push((outpoint(&r["utxo"]), tracked, h(&r["utxo"]["script"])));
        match role.as_str() {
            "gap" | "above" | "below" => push(r, Tracked::Gap(GapState { lo: h32(&r["lo"]), hi: h32(&r["hi"]) })),
            "name" | "target" => push(
                r,
                Tracked::Name(NameState {
                    key: h32(&r["key"]),
                    name: pad_name(r["name"].as_str().unwrap().as_bytes()),
                    owner: h32(&r["owner"]),
                    price: r["price"].as_i64().unwrap(),
                    period_start: r["periodStart"].as_i64().expect("name record carries periodStart"),
                    expires_at: r["expiresAt"].as_i64().unwrap(),
                }),
            ),
            "offer" => push(
                r,
                Tracked::Offer(OfferState {
                    key: h32(&r["key"]),
                    buyer: h32(&r["buyer"]),
                    seller: r.get("seller").filter(|s| !s.is_null()).map(h32),
                    refund_after: r["refundAfter"].as_i64().unwrap(),
                }),
            ),
            "shard" => push(r, Tracked::Shard(shard_of(r))),
            "shards" => {
                for one in r.as_array().unwrap() {
                    push(one, Tracked::Shard(shard_of(one)));
                }
            }
            _ => {}
        }
    }
    out
}

/// The covenant binding of a vector output (`{"authorizingInput", "covenantId"}` or null).
fn binding(o: &Value) -> Option<CovenantBinding> {
    let c = o.get("covenant").filter(|c| !c.is_null())?;
    Some(CovenantBinding {
        authorizing_input: c["authorizingInput"].as_u64().unwrap() as u16,
        covenant_id: h32(&c["covenantId"]),
    })
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
            .map(|i| TxInput {
                spent_script: Vec::new(),
                previous_outpoint: (h32(&i["txid"]), i["index"].as_u64().unwrap() as u32),
                signature_script: h(&i["signatureScript"]),
            })
            .collect(),
        outputs: e["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| TxOutput { script_public_key: h(&o["script"]), value: o["value"].as_u64().unwrap(), covenant: binding(o) })
            .collect(),
        payload: h(&e["payload"]),
        // One block per step: unique and good enough for the undo journal here.
        accepting_block: id,
        accepting_daa: step["env"]["blockDaa"].as_u64().unwrap(),
        block_time: step["env"]["blockTimeMs"].as_i64().unwrap(),
    }
}

/// Outputs that are tracked UTXOs: covenant-bound registry/price outputs, plus the offer P2SH
/// an offer-creating tx (payload `kchat:1:offer:`) makes.
fn registry_outputs(step: &Value) -> Vec<(u32, Vec<u8>)> {
    let e = &step["expected"];
    let is_offer_tx = e["payloadText"].as_str().unwrap_or("").starts_with("kchat:1:offer:");
    e["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, o)| {
            !o["covenant"].is_null() || (is_offer_tx && o["label"].as_str().unwrap_or("").starts_with("offer P2SH"))
        })
        .map(|(i, o)| (i as u32, h(&o["script"])))
        .collect()
}

/// The history ops a step must produce (none for commits).
fn want_ops(step: &Value) -> Option<Vec<&'static str>> {
    let label = step["label"].as_str().unwrap();
    Some(vec![match step["op"].as_str().unwrap() {
        "register" => "register",
        "import" => "import",
        "renew" => "renew",
        "extend" => "extend",
        "transfer" => "transfer",
        "list" if label.starts_with("delist") => "delist",
        "list" => "list",
        "buy" => "sale",
        "offer" => "offer",
        "acceptOffer" => "offer_accepted",
        "declineOffer" => "offer_decline",
        "refundOffer" => "offer_refund",
        "withdrawOffer" => "offer_withdraw",
        "release" => "release",
        "reclaim" => "reclaim",
        "setPrices" => "prices",
        _ => return None,
    }])
}

/// Each step on its own: seed exactly its prior tracked UTXOs, apply it, and require that
/// (1) every seeded state hashes to its recorded script (templates + state encoding),
/// (2) every registry output is tracked with a state that hashes to that output's script,
/// (3) every spent tracked input is gone, and (4) nothing else was added.
#[test]
fn every_vector_step_applies_exactly() {
    for (set, v) in vector_sets() {
        let t = Templates::from_manifest(&v["manifest"]).expect("templates from the vectors manifest");
        let mut replayed = 0;
        for step in v["steps"].as_array().unwrap() {
            let label = format!("{set}: {}", step["label"].as_str().unwrap());
            let mut reg = Registry::new();
            for (op, tracked, script) in prior_states(&step["records"]) {
                assert_eq!(t.tracked_spk(&tracked), script, "[{label}] seeded state hashes to its utxo script");
                reg.utxos.insert(op, tracked);
            }
            let seeded: HashSet<Outpoint> = reg.utxos.keys().copied().collect();

            let tx = tx_of(step);
            let events = reg.apply(&t, &tx);

            let ops: Vec<&str> = events.iter().map(|e| e.op).collect();
            match want_ops(step) {
                // A price change that keeps the prices only rotates the key.
                Some(w) if w == ["prices"] => {
                    assert!(ops == ["prices"] || ops == ["price_authority"], "[{label}] history op, got {ops:?}")
                }
                Some(w) => assert_eq!(ops, w, "[{label}] history op"),
                None => assert!(ops.is_empty(), "[{label}] no history op, got {ops:?}"),
            }

            let expected = registry_outputs(step);
            for (i, script) in &expected {
                let got = reg.utxos.get(&(tx.id, *i)).unwrap_or_else(|| panic!("[{label}] output {i} not tracked"));
                assert_eq!(&t.tracked_spk(got), script, "[{label}] output {i} state hashes to its script");
            }
            for input in &tx.inputs {
                if seeded.contains(&input.previous_outpoint) {
                    assert!(!reg.utxos.contains_key(&input.previous_outpoint), "[{label}] spent input still tracked");
                }
            }
            let added: HashSet<u32> = reg.utxos.keys().filter(|(id, _)| *id == tx.id).map(|(_, i)| *i).collect();
            let want: HashSet<u32> = expected.iter().map(|(i, _)| *i).collect();
            assert_eq!(added, want, "[{label}] exactly the registry outputs are tracked");
            replayed += 1;
        }
        eprintln!("{set}: replayed {replayed} vector transactions");
    }
}

/// The connected lifecycle (steps up to `reclaim lapse-tn`): start from the genesis gap (and,
/// v3, the price genesis shards) and apply in order; after every step the tracked set must
/// equal the live registry UTXOs.
#[test]
fn connected_lifecycle_tracks_the_live_set() {
    for (set, v) in vector_sets() {
        let m = &v["manifest"];
        let t = Templates::from_manifest(m).unwrap();
        let genesis = &m["genesis"];
        let gap0 = &genesis["authorizedOutputs"][0];
        let genesis_op: Outpoint = (h32(&genesis["txid"]), gap0["index"].as_u64().unwrap() as u32);

        let mut reg = Registry::new();
        reg.seed_genesis(genesis_op, GapState { lo: h32(&gap0["state"]["lo"]), hi: h32(&gap0["state"]["hi"]) });
        let mut live: HashSet<Outpoint> = [genesis_op].into();
        if let Some(pg) = m.get("priceGenesis") {
            for s in pg["authorizedOutputs"].as_array().unwrap() {
                let op = (h32(&pg["txid"]), s["index"].as_u64().unwrap() as u32);
                let shard = shard_of(&s["state"]);
                assert_eq!(hex::encode(t.price_spk(&shard)), s["scriptPublicKey"].as_str().unwrap(), "{set}: genesis shard");
                reg.seed_shard(op, shard);
                live.insert(op);
            }
        }

        let mut done = false;
        let mut imported = 0;
        for step in v["steps"].as_array().unwrap() {
            if step["op"] == "import" {
                imported += 1; // v5: imported names stay live through the whole chain
            }
            let label = format!("{set}: {}", step["label"].as_str().unwrap());
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
            if step["label"] == "reclaim lapse-tn" {
                // The chain ends here; later steps are standalone edge cases on synthetic UTXOs.
                assert_eq!(reg.names().count(), 1 + imported, "[{set}] only alpha-tn (and any imports) is left");
                done = true;
                break;
            }
        }
        assert!(done, "[{set}] lifecycle end marker not found");
        if m.get("priceGenesis").is_some() {
            let shards = reg.shards();
            assert_eq!(shards.len(), 8, "[{set}] all 8 shards still live");
            assert!(shards.windows(2).all(|w| w[0].1.prices == w[1].1.prices), "[{set}] every shard agrees on prices");
        }
    }
}

/// Registry v3: a look-alike output (the shard's exact script, but no covenant binding)
/// placed before the real continuation is never taken for the shard.
#[test]
fn v3_ignores_a_look_alike_output_without_the_covenant() {
    let Some((_, v)) = vector_sets().into_iter().find(|(_, v)| v["manifest"].get("priceCovenantId").is_some()) else {
        eprintln!("skipping: no v3 vectors");
        return;
    };
    let t = Templates::from_manifest(&v["manifest"]).unwrap();
    let step = v["steps"].as_array().unwrap().iter().find(|s| s["op"] == "extend").expect("an extend step");
    let mut reg = Registry::new();
    for (op, tracked, _) in prior_states(&step["records"]) {
        reg.utxos.insert(op, tracked);
    }
    let mut tx = tx_of(step);
    // Copy the real shard continuation (output 1), strip its covenant, put it first.
    let mut fake = tx.outputs[1].clone();
    assert!(fake.covenant.is_some(), "the real shard output is covenant-bound");
    fake.covenant = None;
    tx.outputs.insert(0, fake);
    // Every real output moved up one, so the authorizing inputs are unchanged.
    reg.apply(&t, &tx);
    assert!(!reg.utxos.contains_key(&(tx.id, 0)), "the look-alike is not tracked");
    assert!(matches!(reg.utxos.get(&(tx.id, 2)), Some(Tracked::Shard(_))), "the real shard continuation is");
    assert!(matches!(reg.utxos.get(&(tx.id, 1)), Some(Tracked::Name(_))), "the name continuation is");
}

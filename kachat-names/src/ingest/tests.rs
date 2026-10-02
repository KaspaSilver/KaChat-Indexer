use super::*;
use crate::{name_key, pad_name, GapState, NameState, OfferState};

// --- test helpers: build synthetic spends/outputs from the codec ---------------

fn templates() -> Templates {
    // Arbitrary but self-consistent prefix/suffix (the applier only needs them to build
    // and match P2SH); real deployments load them from the manifest.
    let t = |len| ContractTemplate { prefix: vec![0x6b], suffix: vec![0xaa, 0xbb, 0xcc], state_offset: 1, state_len: len };
    Templates { gap: t(66), name: t(117), offer: t(75) }
}

/// Minimal LE sign-magnitude script number (how int args travel).
fn scriptnum(v: i64) -> Vec<u8> {
    if v == 0 {
        return vec![];
    }
    let neg = v < 0;
    let mut n = v.unsigned_abs();
    let mut out = vec![];
    while n > 0 {
        out.push((n & 0xff) as u8);
        n >>= 8;
    }
    if out.last().unwrap() & 0x80 != 0 {
        out.push(if neg { 0x80 } else { 0 });
    } else if neg {
        let l = out.len() - 1;
        out[l] |= 0x80;
    }
    out
}

fn push_bytes(d: &[u8]) -> Vec<u8> {
    let n = d.len();
    let mut s = Vec::new();
    if n == 0 {
        s.push(0x00);
    } else if n <= 75 {
        s.push(n as u8);
        s.extend_from_slice(d);
    } else if n <= 255 {
        s.push(0x4c);
        s.push(n as u8);
        s.extend_from_slice(d);
    } else {
        s.push(0x4d);
        s.extend_from_slice(&(n as u16).to_le_bytes());
        s.extend_from_slice(d);
    }
    s
}

/// `<args...> <4-byte tag> <push(redeem)>`.
fn sig_script(args: &[Vec<u8>], tag: [u8; 4], redeem: &[u8]) -> Vec<u8> {
    let mut s = Vec::new();
    for a in args {
        s.extend(push_bytes(a));
    }
    s.extend(push_bytes(&tag));
    s.extend(push_bytes(redeem));
    s
}

fn out(spk: Vec<u8>, value: u64) -> TxOutput {
    TxOutput { script_public_key: spk, value }
}

const BOND: u64 = 100_000_000;
const NOW: i64 = 1_790_000_000_000;

// --- tests ---------------------------------------------------------------------

#[test]
fn register_splits_gap_and_mints_name() {
    let t = templates();
    let mut reg = Registry::new();
    let genesis = ([0x9au8; 32], 0u32);
    let gap = GapState { lo: [0u8; 32], hi: [0xff; 32] };
    reg.seed_genesis(genesis, gap);

    let owner = [7u8; 32];
    let key = name_key(b"alice");
    let (left, right, nm) = crate::transition::register(&gap, b"alice", owner, NOW, 2);

    let tx = Tx {
        id: [0x11; 32],
        inputs: vec![TxInput {
            previous_outpoint: genesis,
            signature_script: sig_script(
                &[b"alice".to_vec(), owner.to_vec(), [3u8; 32].to_vec(), scriptnum(NOW), scriptnum(2), vec![], vec![]],
                [0x86, 0x67, 0xaf, 0x5e],
                &t.gap.redeem(&gap.encode()),
            ),
        }],
        outputs: vec![
            out(t.gap.spk(&left.encode()), 100_000_000),
            out(t.gap.spk(&right.encode()), 100_000_000),
            out(t.name.spk(&nm.encode()), BOND),
            out(vec![0xde, 0xad], 5), // change
        ],
        payload: b"kchat:1:name:register:alice".to_vec(),
        accepting_block: [0xbb; 32],
        accepting_daa: 585_800_000,
        block_time: NOW,
    };

    let events = reg.apply(&t, &tx);
    assert!(!reg.utxos.contains_key(&genesis), "genesis gap consumed");
    let (op, got) = reg.name_by_key(&key).expect("name indexed");
    assert_eq!(op, ([0x11; 32], 2));
    assert_eq!(got.owner, owner);
    assert_eq!(got.price, 0);
    assert_eq!(got.expires_at, NOW + 2 * crate::YEAR_MS);
    assert_eq!(got.name_str(), "alice");
    // both gaps tracked
    assert_eq!(reg.utxos.len(), 3); // left gap, right gap, name
    assert!(events.iter().any(|e| e.op == "register" && e.key == key));
}

fn seed_name(reg: &mut Registry, outpoint: Outpoint, owner: [u8; 32], price: i64, expires: i64) -> NameState {
    let ns = NameState { key: name_key(b"alice"), name: pad_name(b"alice"), owner, price, expires_at: expires };
    reg.utxos.insert(outpoint, Tracked::Name(ns));
    ns
}

#[test]
fn transfer_updates_owner_and_moves_utxo() {
    let t = templates();
    let mut reg = Registry::new();
    let old_op = ([0x22; 32], 2);
    let ns = seed_name(&mut reg, old_op, [1u8; 32], 0, NOW + crate::YEAR_MS);
    let new_owner = [2u8; 32];
    let cont = crate::transition::name_transfer(&ns, new_owner);

    let tx = Tx {
        id: [0x33; 32],
        inputs: vec![TxInput {
            previous_outpoint: old_op,
            signature_script: sig_script(&[new_owner.to_vec(), [9u8; 65].to_vec()], [0x79, 0x4d, 0xca, 0x54], &t.name.redeem(&ns.encode())),
        }],
        outputs: vec![out(t.name.spk(&cont.encode()), BOND)],
        payload: vec![],
        accepting_block: [0xbb; 32],
        accepting_daa: 1,
        block_time: NOW,
    };
    reg.apply(&t, &tx);
    assert!(!reg.utxos.contains_key(&old_op));
    let (op, got) = reg.name_by_key(&ns.key).unwrap();
    assert_eq!(op, ([0x33; 32], 0));
    assert_eq!(got.owner, new_owner);
}

#[test]
fn list_sets_price_then_renew_extends_expiry() {
    let t = templates();
    let mut reg = Registry::new();
    let op = ([0x44; 32], 2);
    let ns = seed_name(&mut reg, op, [1u8; 32], 0, NOW + crate::YEAR_MS);

    // list at 5 KAS
    let listed = crate::transition::name_list(&ns, 500_000_000);
    let tx = Tx {
        id: [0x45; 32],
        inputs: vec![TxInput {
            previous_outpoint: op,
            signature_script: sig_script(&[scriptnum(500_000_000), [9u8; 65].to_vec()], [0x67, 0x4a, 0x8e, 0xa4], &t.name.redeem(&ns.encode())),
        }],
        outputs: vec![out(t.name.spk(&listed.encode()), BOND)],
        payload: vec![],
        accepting_block: [0xbb; 32],
        accepting_daa: 1,
        block_time: NOW,
    };
    reg.apply(&t, &tx);
    let (op2, got) = reg.name_by_key(&ns.key).unwrap();
    assert_eq!(got.price, 500_000_000);

    // renew +2y (from op2)
    let renewed = crate::transition::name_renew(&got, 2);
    let tx2 = Tx {
        id: [0x46; 32],
        inputs: vec![TxInput {
            previous_outpoint: op2,
            signature_script: sig_script(&[scriptnum(2)], [0xb7, 0x06, 0xac, 0x38], &t.name.redeem(&got.encode())),
        }],
        outputs: vec![out(t.name.spk(&renewed.encode()), BOND)],
        payload: vec![],
        accepting_block: [0xbb; 32],
        accepting_daa: 2,
        block_time: NOW,
    };
    reg.apply(&t, &tx2);
    let (_, got3) = reg.name_by_key(&ns.key).unwrap();
    assert_eq!(got3.expires_at, (NOW + crate::YEAR_MS) + 2 * crate::YEAR_MS);
}

#[test]
fn offer_marker_tracks_the_offer() {
    let t = templates();
    let mut reg = Registry::new();
    let key = name_key(b"alice");
    let buyer = [8u8; 32];
    let offer = OfferState { key, buyer, refund_after: 600_100_000 };
    let payload = format!("kchat:1:offer:{}:{}:{}", hex::encode(key), hex::encode(buyer), offer.refund_after);

    let tx = Tx {
        id: [0x55; 32],
        inputs: vec![],
        outputs: vec![out(t.offer.spk(&offer.encode()), 3 * BOND)],
        payload: payload.into_bytes(),
        accepting_block: [0xbb; 32],
        accepting_daa: 1,
        block_time: NOW,
    };
    let events = reg.apply(&t, &tx);
    assert_eq!(reg.utxos.get(&([0x55; 32], 0)), Some(&Tracked::Offer(offer)));
    assert!(events.iter().any(|e| e.op == "offer"));

    // A marker with no matching output is ignored.
    let mut reg2 = Registry::new();
    let tx2 = Tx { outputs: vec![out(vec![0x00], 1)], ..tx.clone() };
    reg2.apply(&t, &tx2);
    assert!(reg2.utxos.is_empty());
}

#[test]
fn release_exit_merges_gaps_and_removes_name() {
    let t = templates();
    let mut reg = Registry::new();
    let key = name_key(b"alice");
    // name key sits between the two gaps.
    let lo_gap = GapState { lo: [0u8; 32], hi: key };
    let hi_gap = GapState { lo: key, hi: [0xff; 32] };
    let name = NameState { key, name: pad_name(b"alice"), owner: [1u8; 32], price: 0, expires_at: NOW };
    let lo_op = ([0x60; 32], 0);
    let name_op = ([0x60; 32], 1);
    let hi_op = ([0x60; 32], 2);
    reg.utxos.insert(lo_op, Tracked::Gap(lo_gap));
    reg.utxos.insert(name_op, Tracked::Name(name));
    reg.utxos.insert(hi_op, Tracked::Gap(hi_gap));

    let merged = crate::transition::merge_gaps(&lo_gap, &hi_gap);
    let tx = Tx {
        id: [0x61; 32],
        inputs: vec![
            TxInput { previous_outpoint: lo_op, signature_script: sig_script(&[], [0x63, 0xd2, 0x5b, 0xc2], &t.gap.redeem(&lo_gap.encode())) },
            TxInput { previous_outpoint: name_op, signature_script: sig_script(&[[9u8; 65].to_vec()], [0x38, 0x8a, 0xd0, 0xb4], &t.name.redeem(&name.encode())) },
            TxInput { previous_outpoint: hi_op, signature_script: sig_script(&[], [0xda, 0xb7, 0x63, 0x55], &t.gap.redeem(&hi_gap.encode())) },
        ],
        outputs: vec![out(t.gap.spk(&merged.encode()), 100_000_000)],
        payload: vec![],
        accepting_block: [0xbb; 32],
        accepting_daa: 1,
        block_time: NOW,
    };
    reg.apply(&t, &tx);
    assert!(reg.name_by_key(&key).is_none(), "name removed on release");
    // the merged gap is tracked, the three inputs consumed
    assert_eq!(reg.utxos.len(), 1);
    let only = reg.utxos.values().next().unwrap();
    assert_eq!(only, &Tracked::Gap(merged));
}

// --- reorg undo journal (§4.1) --------------------------------------------------

#[test]
fn undo_block_reverses_a_register() {
    let t = templates();
    let mut reg = Registry::new();
    let genesis = ([0x9au8; 32], 0u32);
    let gap = GapState { lo: [0u8; 32], hi: [0xff; 32] };
    reg.seed_genesis(genesis, gap);
    let key = name_key(b"alice");
    let (left, right, nm) = crate::transition::register(&gap, b"alice", [7u8; 32], NOW, 2);

    let block = [0xc0u8; 32];
    let tx = Tx {
        id: [0x11; 32],
        inputs: vec![TxInput {
            previous_outpoint: genesis,
            signature_script: sig_script(
                &[b"alice".to_vec(), [7u8; 32].to_vec(), [3u8; 32].to_vec(), scriptnum(NOW), scriptnum(2), vec![], vec![]],
                [0x86, 0x67, 0xaf, 0x5e],
                &t.gap.redeem(&gap.encode()),
            ),
        }],
        outputs: vec![
            out(t.gap.spk(&left.encode()), BOND),
            out(t.gap.spk(&right.encode()), BOND),
            out(t.name.spk(&nm.encode()), BOND),
        ],
        payload: vec![],
        accepting_block: block,
        accepting_daa: 1,
        block_time: NOW,
    };
    reg.apply(&t, &tx);
    assert!(reg.name_by_key(&key).is_some());
    assert_eq!(reg.utxos.len(), 3);

    // Reorg: the accepting block is removed. The registry returns to exactly genesis.
    reg.undo_block(&block);
    assert!(reg.name_by_key(&key).is_none());
    assert_eq!(reg.utxos.len(), 1);
    assert_eq!(reg.utxos.get(&genesis), Some(&Tracked::Gap(gap)));
    assert_eq!(reg.journal_len(), 0);
}

#[test]
fn undo_restores_prior_owner_and_outpoint_on_transfer() {
    let t = templates();
    let mut reg = Registry::new();
    let old_op = ([0x22; 32], 2);
    let ns = seed_name(&mut reg, old_op, [1u8; 32], 0, NOW + crate::YEAR_MS);
    let cont = crate::transition::name_transfer(&ns, [2u8; 32]);
    let block = [0xc1u8; 32];
    let tx = Tx {
        id: [0x33; 32],
        inputs: vec![TxInput {
            previous_outpoint: old_op,
            signature_script: sig_script(&[[2u8; 32].to_vec(), [9u8; 65].to_vec()], [0x79, 0x4d, 0xca, 0x54], &t.name.redeem(&ns.encode())),
        }],
        outputs: vec![out(t.name.spk(&cont.encode()), BOND)],
        payload: vec![],
        accepting_block: block,
        accepting_daa: 1,
        block_time: NOW,
    };
    reg.apply(&t, &tx);
    assert_eq!(reg.name_by_key(&ns.key).unwrap().1.owner, [2u8; 32]);

    reg.undo_block(&block);
    let (op, got) = reg.name_by_key(&ns.key).unwrap();
    assert_eq!(op, old_op, "name back at its original outpoint");
    assert_eq!(got.owner, [1u8; 32], "prior owner restored");
}

#[test]
fn undo_restores_a_replaced_profile() {
    let t = templates();
    let mut reg = Registry::new();
    let addr = vec![0xab, 0xcd];
    let out_self = TxOutput { script_public_key: addr.clone(), value: 1 };
    let mk = |id: [u8; 8], daa: u64, block: [u8; 32], handle: &str| Tx {
        id: { let mut x = [0u8; 32]; x[..8].copy_from_slice(&id); x },
        inputs: vec![],
        outputs: vec![out_self.clone()],
        payload: format!("kchat:1:profile:{{\"v\":1,\"social\":\"https://x.com/{handle}\"}}").into_bytes(),
        accepting_block: block,
        accepting_daa: daa,
        block_time: NOW,
    };
    reg.apply(&t, &mk([1; 8], 1, [0xd0; 32], "first"));
    assert!(reg.profiles.get(&addr).unwrap().0.contains("first"));

    let b2 = [0xd1u8; 32];
    reg.apply(&t, &mk([2; 8], 2, b2, "second"));
    assert!(reg.profiles.get(&addr).unwrap().0.contains("second"));

    reg.undo_block(&b2);
    assert!(reg.profiles.get(&addr).unwrap().0.contains("first"), "prior profile restored");
}

#[test]
fn prune_journal_keeps_recent() {
    let mut reg = Registry::new();
    // fabricate entries by applying no-op-ish profile txs across blocks
    let t = templates();
    let addr = vec![0x01];
    for i in 0..5u64 {
        let tx = Tx {
            id: { let mut x = [0u8; 32]; x[0] = i as u8; x },
            inputs: vec![],
            outputs: vec![TxOutput { script_public_key: addr.clone(), value: 1 }],
            payload: b"kchat:1:profile:{\"v\":1,\"social\":\"https://x.com/a\"}".to_vec(),
            accepting_block: { let mut b = [0u8; 32]; b[0] = i as u8; b },
            accepting_daa: i,
            block_time: NOW,
        };
        reg.apply(&t, &tx);
    }
    assert_eq!(reg.journal_len(), 5);
    reg.prune_journal(2);
    assert_eq!(reg.journal_len(), 2);
}

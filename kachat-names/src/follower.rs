//! The follower loop (KACHAT_NAMES_INDEXER.md §3) — drive the registry from the node's
//! virtual chain, reorg-safe.
//!
//! The chain is abstracted behind [`ChainSource`] so the loop is unit-testable with a fake
//! source; the real implementation is a thin wRPC client calling
//! `getVirtualChainFromBlockV2(start, includeAcceptedTransactions=true)` and mapping each
//! accepted tx into [`crate::ingest::Tx`]. The loop applies accepted txs, undoes removed
//! chain blocks (reorgs), advances the checkpoint, and prunes the undo journal past finality.

use crate::ingest::{Event, Registry, Templates, Tx};

/// One virtual-chain batch: blocks removed by a reorg (undo these first), the transactions
/// newly accepted since the last call (in chain order), and the new tip to checkpoint at.
#[derive(Debug, Default)]
pub struct VccBatch {
    pub removed_blocks: Vec<[u8; 32]>,
    pub accepted: Vec<Tx>,
    pub tip: Option<[u8; 32]>,
}

/// Source of virtual-chain batches. `from` is the last checkpointed chain block (`None` =
/// start at the manifest's `genesis.scanFrom`).
pub trait ChainSource {
    type Error;
    fn next_batch(&mut self, from: Option<[u8; 32]>) -> Result<VccBatch, Self::Error>;
}

/// The follower: the tracked registry, the current checkpoint (last processed chain block),
/// and how many transactions of undo history to retain (past finality a reorg can't reach).
pub struct Follower {
    pub registry: Registry,
    pub checkpoint: Option<[u8; 32]>,
    finality_keep: usize,
}

impl Follower {
    pub fn new(finality_keep: usize) -> Self {
        Self { registry: Registry::new(), checkpoint: None, finality_keep }
    }

    /// Seed the genesis gap and set the checkpoint to the manifest's scan-from block.
    pub fn seed(&mut self, scan_from: [u8; 32], genesis_outpoint: crate::ingest::Outpoint, genesis_gap: crate::GapState) {
        self.registry.seed_genesis(genesis_outpoint, genesis_gap);
        self.checkpoint = Some(scan_from);
    }

    /// Registry v3: seed the K price shards from the price genesis (sent before the
    /// registry genesis, so the scan start covers both).
    pub fn seed_shards(&mut self, shards: &[(crate::ingest::Outpoint, crate::PriceState)]) {
        for (op, shard) in shards {
            self.registry.seed_shard(*op, *shard);
        }
    }

    /// Pull one batch and apply it. Reorg-safe order: undo removed blocks first, then apply
    /// the newly-accepted txs, then advance the checkpoint and prune. Returns the batch (so
    /// the caller can persist what it touched) and the registry events it produced.
    pub fn step<S: ChainSource>(&mut self, templates: &Templates, src: &mut S) -> Result<(VccBatch, Vec<Event>), S::Error> {
        let batch = src.next_batch(self.checkpoint)?;
        for block in &batch.removed_blocks {
            self.registry.undo_block(block);
        }
        let mut events = Vec::new();
        for tx in &batch.accepted {
            events.extend(self.registry.apply(templates, tx));
        }
        if let Some(tip) = batch.tip {
            self.checkpoint = Some(tip);
        }
        self.registry.prune_journal(self.finality_keep);
        Ok((batch, events))
    }
}

impl Follower {
    /// [`Self::step`] for the profiles follower: the same reorg-safe order, applying only the
    /// profile rules (no registry templates needed). Returns the batch and the ids of the
    /// transactions whose profile record was accepted.
    pub fn step_profiles<S: ChainSource>(&mut self, src: &mut S) -> Result<(VccBatch, Vec<[u8; 32]>), S::Error> {
        let batch = src.next_batch(self.checkpoint)?;
        for block in &batch.removed_blocks {
            self.registry.undo_block(block);
        }
        let applied = batch.accepted.iter().filter(|tx| self.registry.apply_profile_only(tx)).map(|tx| tx.id).collect();
        if let Some(tip) = batch.tip {
            self.checkpoint = Some(tip);
        }
        self.registry.prune_journal(self.finality_keep);
        Ok((batch, applied))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{Outpoint, TxInput, TxOutput};
    use crate::{name_key, GapState};

    fn templates() -> Templates {
        use crate::ingest::ContractTemplate;
        let t = |len| ContractTemplate { prefix: vec![0x6b], suffix: vec![0xaa, 0xbb, 0xcc], state_offset: 1, state_len: len };
        Templates::new(t(66), t(126), t(75))
    }

    fn scriptnum(v: i64) -> Vec<u8> {
        if v == 0 {
            return vec![];
        }
        let mut n = v.unsigned_abs();
        let mut out = vec![];
        while n > 0 {
            out.push((n & 0xff) as u8);
            n >>= 8;
        }
        if out.last().unwrap() & 0x80 != 0 {
            out.push(0);
        }
        out
    }
    fn push_bytes(d: &[u8]) -> Vec<u8> {
        let mut s = Vec::new();
        if d.is_empty() {
            s.push(0x00);
        } else if d.len() <= 75 {
            s.push(d.len() as u8);
            s.extend_from_slice(d);
        } else {
            s.push(0x4d);
            s.extend_from_slice(&(d.len() as u16).to_le_bytes());
            s.extend_from_slice(d);
        }
        s
    }
    fn sig(args: &[Vec<u8>], tag: [u8; 4], redeem: &[u8]) -> Vec<u8> {
        let mut s = Vec::new();
        for a in args {
            s.extend(push_bytes(a));
        }
        s.extend(push_bytes(&tag));
        s.extend(push_bytes(redeem));
        s
    }

    /// A scripted source: hands out queued batches in order.
    struct Fake {
        batches: std::collections::VecDeque<VccBatch>,
        seen_from: Vec<Option<[u8; 32]>>,
    }
    impl ChainSource for Fake {
        type Error = ();
        fn next_batch(&mut self, from: Option<[u8; 32]>) -> Result<VccBatch, ()> {
            self.seen_from.push(from);
            Ok(self.batches.pop_front().unwrap_or_default())
        }
    }

    fn register_tx(t: &Templates, gap: &GapState, genesis: Outpoint, block: [u8; 32]) -> Tx {
        let (left, right, nm) = crate::transition::register(gap, b"alice", [7u8; 32], 1_790_000_000_000, 2);
        Tx {
            id: [0x11; 32],
            inputs: vec![TxInput { spent_script: Vec::new(),
                previous_outpoint: genesis,
                signature_script: sig(
                    &[b"alice".to_vec(), [7u8; 32].to_vec(), [3u8; 32].to_vec(), scriptnum(1_790_000_000_000), scriptnum(2), vec![], vec![]],
                    [0x86, 0x67, 0xaf, 0x5e],
                    &t.gap.redeem(&gap.encode()),
                ),
            }],
            outputs: vec![
                TxOutput { script_public_key: t.gap.spk(&left.encode()), value: 100_000_000, covenant: None },
                TxOutput { script_public_key: t.gap.spk(&right.encode()), value: 100_000_000, covenant: None },
                TxOutput { script_public_key: t.name.spk(&nm.encode()), value: 100_000_000, covenant: None },
            ],
            payload: vec![],
            accepting_block: block,
            accepting_daa: 1,
            block_time: 1_790_000_000_000,
        }
    }

    #[test]
    fn follows_then_survives_a_reorg() {
        let t = templates();
        let scan_from = [0x00; 32];
        let genesis = ([0x9a; 32], 0u32);
        let gap = GapState { lo: [0u8; 32], hi: [0xff; 32] };

        let mut f = Follower::new(100);
        f.seed(scan_from, genesis, gap);

        let block_a = [0xa0; 32];
        let mut src = Fake {
            batches: std::collections::VecDeque::from(vec![
                // batch 1: block A accepts the register
                VccBatch { removed_blocks: vec![], accepted: vec![register_tx(&t, &gap, genesis, block_a)], tip: Some(block_a) },
                // batch 2: reorg — block A is removed, nothing re-accepted
                VccBatch { removed_blocks: vec![block_a], accepted: vec![], tip: Some(scan_from) },
            ]),
            seen_from: vec![],
        };

        let key = name_key(b"alice");
        let (batch, events) = f.step(&t, &mut src).unwrap();
        assert_eq!(batch.accepted.len(), 1);
        assert_eq!(events.iter().map(|e| e.op).collect::<Vec<_>>(), vec!["register"]);
        assert!(f.registry.name_by_key(&key).is_some(), "name indexed after block A");
        assert_eq!(f.checkpoint, Some(block_a));
        assert_eq!(src.seen_from[0], Some(scan_from), "first pull starts at scanFrom");

        // Reorg batch: the register is undone, registry back to the genesis gap only.
        f.step(&t, &mut src).unwrap();
        assert!(f.registry.name_by_key(&key).is_none(), "name undone on reorg");
        assert_eq!(f.registry.utxos.get(&genesis), Some(&crate::ingest::Tracked::Gap(gap)));
        assert_eq!(f.checkpoint, Some(scan_from));
        assert_eq!(src.seen_from[1], Some(block_a), "second pull resumes from the checkpoint");
    }

    fn profile_tx(id: u8, block: [u8; 32], daa: u64, from: &[u8], to: &[u8], json: &str) -> Tx {
        Tx {
            id: [id; 32],
            inputs: vec![TxInput { previous_outpoint: ([id ^ 0xff; 32], 0), signature_script: vec![], spent_script: from.to_vec() }],
            outputs: vec![TxOutput { script_public_key: to.to_vec(), value: 1, covenant: None }],
            payload: format!("kchat:1:profile:{json}").into_bytes(),
            accepting_block: block,
            accepting_daa: daa,
            block_time: daa as i64,
        }
    }

    #[test]
    fn profiles_follow_newest_wins_and_roll_back() {
        let alice = b"alice-spk".to_vec();
        let bob = b"bob-spk".to_vec();
        let v1 = r#"{"v":1,"avatar":"https://x.com/alice"}"#;
        let v2 = r#"{"v":1,"avatar":"https://github.com/alice"}"#;
        let (a, b) = ([0xa1; 32], [0xb2; 32]);
        let mut src = Fake {
            batches: std::collections::VecDeque::from(vec![
                VccBatch {
                    removed_blocks: vec![],
                    accepted: vec![
                        profile_tx(1, a, 10, &alice, &alice, v1),
                        // Paid *to* alice by bob: not a self-send, ignored.
                        profile_tx(2, a, 11, &bob, &alice, v2),
                        // Not a v1 record: ignored.
                        profile_tx(3, a, 12, &bob, &bob, r#"{"avatar":"https://x.com/b"}"#),
                    ],
                    tip: Some(a),
                },
                VccBatch { removed_blocks: vec![], accepted: vec![profile_tx(4, b, 20, &alice, &alice, v2)], tip: Some(b) },
                // Reorg: block b goes, alice is back to v1.
                VccBatch { removed_blocks: vec![b], accepted: vec![], tip: Some(a) },
            ]),
            seen_from: vec![],
        };
        let mut f = Follower::new(100);
        f.checkpoint = Some([0; 32]);

        let (_, applied) = f.step_profiles(&mut src).unwrap();
        assert_eq!(applied, vec![[1; 32]]);
        assert_eq!(f.registry.profiles.get(&alice).map(|p| p.0.as_str()), Some(v1));
        assert!(!f.registry.profiles.contains_key(&bob));

        let (_, applied) = f.step_profiles(&mut src).unwrap();
        assert_eq!(applied, vec![[4; 32]]);
        assert_eq!(f.registry.profiles.get(&alice).map(|p| p.0.as_str()), Some(v2));

        f.step_profiles(&mut src).unwrap();
        assert_eq!(f.registry.profiles.get(&alice).map(|p| p.0.as_str()), Some(v1), "reorg restores the prior record");
        assert_eq!(f.checkpoint, Some(a));
    }
}

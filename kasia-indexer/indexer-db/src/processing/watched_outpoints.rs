//! Address Activity self-send filter (docs/KACHAT_WALLET_PUSH.md §1): which watched address each
//! output paid, so a later spend can be traced back to its owner. A tx that spends from a device's
//! own watch-only address is not "received" for that device.
//!
//! - `watched_outpoints`: outpoint -> the watched address the output paid. Written for every
//!   output paying an address some device watches.
//! - `watched_outpoint_spends`: (spent_at_ms, outpoint) -> []. A spent outpoint is kept for a while
//!   rather than removed at once: the same tx can sit in several parallel blocks, and each sighting
//!   must still see its inputs. Entries past the retention are pruned from both partitions.

use crate::AddressPayload;
use fjall::{PartitionCreateOptions, WriteTransaction};
use zerocopy::big_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Unaligned,
    Immutable,
    IntoBytes,
    FromBytes,
    KnownLayout,
)]
pub struct WatchedOutpointKey {
    pub tx_id: [u8; 32],
    pub index: U32,
}

#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Unaligned,
    Immutable,
    IntoBytes,
    FromBytes,
    KnownLayout,
)]
pub struct WatchedOutpointSpendKey {
    pub spent_at_ms: U64,
    pub outpoint: WatchedOutpointKey,
}

#[derive(Clone)]
pub struct WatchedOutpointPartition(fjall::TxPartition);

impl WatchedOutpointPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> anyhow::Result<Self> {
        Ok(Self(keyspace.open_partition(
            "watched_outpoints",
            PartitionCreateOptions::default(),
        )?))
    }

    pub fn len(&self) -> anyhow::Result<usize> {
        Ok(self.0.inner().len()?)
    }

    pub fn is_empty(&self) -> anyhow::Result<bool> {
        Ok(self.0.inner().is_empty()?)
    }

    pub fn insert_wtx(
        &self,
        wtx: &mut WriteTransaction,
        key: &WatchedOutpointKey,
        address: &AddressPayload,
    ) {
        wtx.insert(&self.0, key.as_bytes(), address.as_bytes())
    }

    /// Read through the write transaction, so an output created earlier in the same block is seen.
    pub fn get_wtx(
        &self,
        wtx: &mut WriteTransaction,
        key: &WatchedOutpointKey,
    ) -> anyhow::Result<Option<AddressPayload>> {
        wtx.get(&self.0, key.as_bytes())?
            .map(|bytes| {
                AddressPayload::read_from_bytes(bytes.as_ref())
                    .map_err(|_| anyhow::anyhow!("db corrupted, failed to read watched outpoint"))
            })
            .transpose()
    }

    pub fn remove_wtx(&self, wtx: &mut WriteTransaction, key: &WatchedOutpointKey) {
        wtx.remove(&self.0, key.as_bytes())
    }

    /// Up to `limit` rows after `after` (from the start when None), in key order. Reads the
    /// committed state, so a sweep adds no read-conflict tracking.
    pub fn scan_after(
        &self,
        after: Option<&WatchedOutpointKey>,
        limit: usize,
    ) -> anyhow::Result<Vec<(WatchedOutpointKey, AddressPayload)>> {
        let read = |item: fjall::Result<(fjall::Slice, fjall::Slice)>| {
            let (key, value) = item?;
            let key = WatchedOutpointKey::read_from_bytes(key.as_ref())
                .map_err(|_| anyhow::anyhow!("db corrupted, failed to read watched outpoint"))?;
            let address = AddressPayload::read_from_bytes(value.as_ref())
                .map_err(|_| anyhow::anyhow!("db corrupted, failed to read watched outpoint"))?;
            Ok((key, address))
        };
        let inner = self.0.inner();
        match after {
            None => inner.iter().take(limit).map(read).collect(),
            Some(after) => {
                let start = after.as_bytes().to_vec();
                inner
                    .range((std::ops::Bound::Excluded(start), std::ops::Bound::Unbounded))
                    .take(limit)
                    .map(read)
                    .collect()
            }
        }
    }
}

#[derive(Clone)]
pub struct WatchedOutpointSpendPartition(fjall::TxPartition);

impl WatchedOutpointSpendPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> anyhow::Result<Self> {
        Ok(Self(keyspace.open_partition(
            "watched_outpoint_spends",
            PartitionCreateOptions::default(),
        )?))
    }

    pub fn insert_wtx(&self, wtx: &mut WriteTransaction, key: &WatchedOutpointSpendKey) {
        wtx.insert(&self.0, key.as_bytes(), [])
    }

    pub fn remove_wtx(&self, wtx: &mut WriteTransaction, key: &WatchedOutpointSpendKey) {
        wtx.remove(&self.0, key.as_bytes())
    }

    /// Up to `limit` spends recorded before `cutoff_ms`, oldest first. Reads the committed state
    /// (not the write transaction), so pruning adds no read-conflict tracking.
    pub fn spent_before(
        &self,
        cutoff_ms: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<WatchedOutpointSpendKey>> {
        let end = U64::new(cutoff_ms);
        self.0
            .inner()
            .range(..end.as_bytes())
            .take(limit)
            .map(|item| {
                let (key, _) = item?;
                WatchedOutpointSpendKey::read_from_bytes(key.as_ref()).map_err(|_| {
                    anyhow::anyhow!("db corrupted, failed to read watched outpoint spend")
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outpoint(b: u8, index: u32) -> WatchedOutpointKey {
        WatchedOutpointKey {
            tx_id: [b; 32],
            index: index.into(),
        }
    }

    #[test]
    fn outpoints_resolve_in_the_same_block_and_expired_spends_are_found() {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let keyspace = fjall::Config::new(dir.path())
            .open_transactional()
            .expect("transactional keyspace");
        let outpoints = WatchedOutpointPartition::new(&keyspace).expect("outpoints");
        let spends = WatchedOutpointSpendPartition::new(&keyspace).expect("spends");
        let addr = AddressPayload::from_xonly_pubkey([7; 32]);

        // An output written earlier in the same write transaction (same block) is visible.
        let mut wtx = keyspace.write_tx().expect("wtx");
        outpoints.insert_wtx(&mut wtx, &outpoint(1, 0), &addr);
        assert_eq!(
            outpoints.get_wtx(&mut wtx, &outpoint(1, 0)).expect("get"),
            Some(addr)
        );
        assert_eq!(
            outpoints.get_wtx(&mut wtx, &outpoint(1, 1)).expect("get"),
            None
        );
        for (at, op) in [
            (300u64, outpoint(1, 0)),
            (100, outpoint(2, 5)),
            (200, outpoint(3, 1)),
        ] {
            spends.insert_wtx(
                &mut wtx,
                &WatchedOutpointSpendKey {
                    spent_at_ms: at.into(),
                    outpoint: op,
                },
            );
        }
        wtx.commit().expect("commit").expect("no conflict");

        // Oldest first, strictly before the cutoff, at most `limit`.
        let expired = spends.spent_before(300, 10).expect("range");
        let got: Vec<(u64, WatchedOutpointKey)> = expired
            .iter()
            .map(|k| (k.spent_at_ms.get(), k.outpoint))
            .collect();
        assert_eq!(got, vec![(100, outpoint(2, 5)), (200, outpoint(3, 1))]);
        assert_eq!(spends.spent_before(300, 1).expect("range").len(), 1);

        let mut wtx = keyspace.write_tx().expect("wtx");
        for spend in &expired {
            spends.remove_wtx(&mut wtx, spend);
        }
        outpoints.remove_wtx(&mut wtx, &outpoint(1, 0));
        wtx.commit().expect("commit").expect("no conflict");
        assert_eq!(spends.spent_before(u64::MAX, 10).expect("range").len(), 1);
        assert!(outpoints.is_empty().expect("empty"));
    }

    #[test]
    fn a_sweep_walks_every_outpoint_in_batches() {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let keyspace = fjall::Config::new(dir.path())
            .open_transactional()
            .expect("transactional keyspace");
        let outpoints = WatchedOutpointPartition::new(&keyspace).expect("outpoints");
        let addr = AddressPayload::from_xonly_pubkey([7; 32]);
        let mut wtx = keyspace.write_tx().expect("wtx");
        for (b, index) in [(1, 0), (1, 3), (2, 0), (3, 1), (3, 2)] {
            outpoints.insert_wtx(&mut wtx, &outpoint(b, index), &addr);
        }
        wtx.commit().expect("commit").expect("no conflict");

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let batch = outpoints.scan_after(cursor.as_ref(), 2).expect("scan");
            seen.extend(batch.iter().map(|(key, address)| {
                assert_eq!(*address, addr);
                (key.tx_id[0], key.index.get())
            }));
            if batch.len() < 2 {
                break;
            }
            cursor = batch.last().map(|(key, _)| *key);
        }
        assert_eq!(seen, vec![(1, 0), (1, 3), (2, 0), (3, 1), (3, 2)]);
    }
}

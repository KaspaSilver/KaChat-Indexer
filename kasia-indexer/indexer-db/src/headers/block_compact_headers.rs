use anyhow::Result;
use fjall::{PartitionCreateOptions, ReadTransaction, WriteTransaction};
use zerocopy::little_endian::U64;
use zerocopy::{FromBytes, Immutable, IntoBytes, TryFromBytes, Unaligned};

#[derive(Clone)]
pub struct BlockCompactHeaderPartition(fjall::TxPartition);

/// Compact header data containing blue work and DAA score
#[derive(Clone, Copy, Debug, PartialEq, Eq, Immutable, FromBytes, IntoBytes, Unaligned)]
#[repr(C)]
pub struct CompactHeader {
    pub blue_work: [u8; 24], // BlueWorkType serialized as 24 bytes LE
    pub daa_score: U64,      // u64 DAA score serialized as 8 bytes LE
}

impl BlockCompactHeaderPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> Result<Self> {
        Ok(Self(
            keyspace.open_partition(
                "block_compact_header",
                PartitionCreateOptions::default()
                    .block_size(64 * 1024)
                    .compaction_strategy(fjall::compaction::Strategy::SizeTiered(
                        fjall::compaction::SizeTiered {
                            base_size: 8 * 1024 * 1024,
                            level_ratio: 6,
                        },
                    )),
            )?,
        ))
    }

    /// Records the block's header inside the block's own write transaction, so "already
    /// processed" commits together with the block's content. Committed on its own, a block whose
    /// write then hit a conflict, or a crash, was skipped on the retry and its txs were lost.
    pub fn insert_compact_header_wtx(
        &self,
        wtx: &mut WriteTransaction,
        block_hash: &[u8; 32],
        blue_work: [u8; 24],
        daa_score: impl Into<U64>,
    ) -> Result<AlreadyExist> {
        let daa_score: U64 = daa_score.into();
        Ok(wtx
            .fetch_update(&self.0, block_hash, move |_| {
                Some(
                    CompactHeader {
                        blue_work,
                        daa_score,
                    }
                    .as_bytes()
                    .into(),
                )
            })?
            .is_some())
    }

    pub fn get_compact_header_rtx(
        &self,
        rtx: &ReadTransaction,
        block_hash: &[u8; 32],
    ) -> Result<Option<CompactHeader>> {
        rtx.get(&self.0, block_hash.as_bytes())?
            .map(|bytes| {
                CompactHeader::try_read_from_bytes(bytes.as_bytes())
                    .map_err(|_| anyhow::anyhow!("Invalid CompactHeader length"))
            })
            .transpose()
    }

    pub fn get_compact_header_wtx(
        &self,
        wtx: &mut WriteTransaction,
        block_hash: &[u8; 32],
    ) -> Result<Option<CompactHeader>> {
        wtx.get(&self.0, block_hash.as_bytes())?
            .map(|bytes| {
                CompactHeader::try_read_from_bytes(bytes.as_bytes())
                    .map_err(|_| anyhow::anyhow!("Invalid CompactHeader length"))
            })
            .transpose()
    }

    pub fn get_compact_header(&self, block_hash: &[u8; 32]) -> Result<Option<CompactHeader>> {
        self.0
            .get(block_hash.as_bytes())?
            .map(|bytes| {
                CompactHeader::try_read_from_bytes(bytes.as_bytes())
                    .map_err(|_| anyhow::anyhow!("Invalid CompactHeader length"))
            })
            .transpose()
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self.0.inner().len()?)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.0.inner().is_empty()?)
    }

    pub fn remove(&self, block_hash: &[u8; 32]) -> Result<()> {
        self.0.remove(block_hash.as_bytes())?;
        Ok(())
    }
}

type AlreadyExist = bool;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_whose_write_is_dropped_is_not_marked_processed() {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let keyspace = fjall::Config::new(dir.path())
            .open_transactional()
            .expect("transactional keyspace");
        let headers = BlockCompactHeaderPartition::new(&keyspace).expect("headers");
        let hash = [9u8; 32];

        // The block's write never commits (a conflict or a crash): the retry must process it.
        let mut wtx = keyspace.write_tx().expect("wtx");
        assert!(!headers.insert_compact_header_wtx(&mut wtx, &hash, [0; 24], 5u64).expect("insert"));
        drop(wtx);
        let mut wtx = keyspace.write_tx().expect("wtx");
        assert!(!headers.insert_compact_header_wtx(&mut wtx, &hash, [0; 24], 5u64).expect("insert"));
        wtx.commit().expect("commit").expect("no conflict");

        // Once it commits with the block, a second sighting is skipped.
        let mut wtx = keyspace.write_tx().expect("wtx");
        assert!(headers.insert_compact_header_wtx(&mut wtx, &hash, [0; 24], 5u64).expect("insert"));
    }
}

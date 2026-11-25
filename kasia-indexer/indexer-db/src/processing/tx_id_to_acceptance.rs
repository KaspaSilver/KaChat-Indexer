use crate::{AddressPayload, PartitionId, SharedImmutable};
use fjall::{PartitionCreateOptions, ReadTransaction, WriteTransaction};
use smallvec::SmallVec;
use std::ops::Deref;
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

#[derive(Clone)]
pub struct TxIDToAcceptancePartition(fjall::TxPartition);

#[repr(C)]
#[derive(Clone, Copy, Debug, Immutable, IntoBytes, FromBytes, Unaligned, KnownLayout)]
pub struct AcceptanceKey {
    pub tx_id: [u8; 32],
    pub receiver: AddressPayload,
}

#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Immutable,
    IntoBytes,
    FromBytes,
    Unaligned,
    KnownLayout,
    Eq,
    PartialEq,
)]
pub struct AcceptanceValueHeader {
    pub accepting_block_hash: [u8; 32],
    pub accepting_daa: zerocopy::U64<zerocopy::LE>,
}

#[repr(C)]
#[derive(Debug, Immutable, Unaligned, KnownLayout, FromBytes)]
pub struct AcceptanceValueHeaderWithSuffix {
    pub header: AcceptanceValueHeader,
    pub suffix: [u8],
}

impl Deref for AcceptanceValueHeaderWithSuffix {
    type Target = AcceptanceValueHeader;

    fn deref(&self) -> &Self::Target {
        &self.header
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'a> {
    pub partition_id: PartitionId,
    pub action: Action,
    pub key: &'a [u8],
}

#[repr(u8)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, TryFromBytes, IntoBytes, Unaligned, Immutable, KnownLayout,
)]
pub enum Action {
    UpdateValueSender,
    InsertByKeySender,
}

pub struct InsertionEntry<const N: usize = 160> {
    pub partition_id: PartitionId,
    pub action: Action,
    pub partition_key: SmallVec<[u8; N]>,
}

impl TxIDToAcceptancePartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> anyhow::Result<Self> {
        Ok(Self(keyspace.open_partition(
            "tx_id_to_acceptance",
            PartitionCreateOptions::default(),
        )?))
    }

    /// Create the record, or append `entries` to it. Returns what the record was before (None
    /// when it is new).
    pub fn insert_wtx(
        &self,
        wtx: &mut WriteTransaction,
        k: &AcceptanceKey,
        entries: &[InsertionEntry],
    ) -> anyhow::Result<Option<PreviousAcceptance>> {
        let entries_size: usize = entries
            .iter()
            .map(|entry| entry.partition_key.len() + 2)
            .sum();
        let old_value = wtx.fetch_update(&self.0, k.as_bytes(), |old_value| match old_value {
            None => {
                let mut v = Vec::with_capacity(size_of::<AcceptanceValueHeader>() + entries_size);
                v.extend_from_slice(AcceptanceValueHeader::new_zeroed().as_bytes());
                entries.iter().for_each(|entry| {
                    v.extend_from_slice(entry.partition_id.as_bytes());
                    v.extend_from_slice(entry.action.as_bytes());
                    v.extend_from_slice(entry.partition_key.as_slice());
                });
                Some(v.into())
            }
            Some(old) => {
                let mut v = Vec::with_capacity(old.len() + entries_size);
                v.extend_from_slice(old.as_ref());
                entries.iter().for_each(|entry| {
                    v.extend_from_slice(entry.partition_id.as_bytes());
                    v.extend_from_slice(entry.action.as_bytes());
                    v.extend_from_slice(entry.partition_key.as_slice());
                });
                Some(v.into())
            }
        })?;
        Ok(old_value.and_then(|old| {
            AcceptanceValueHeader::read_from_prefix(old.as_ref())
                .ok()
                .map(|(header, rest)| PreviousAcceptance {
                    header,
                    has_entries: !rest.is_empty(),
                })
        }))
    }

    pub fn remove_wtx(&self, wtx: &mut WriteTransaction, k: &AcceptanceKey) {
        wtx.remove(&self.0, k.as_bytes());
    }

    pub fn key_by_tx_id_rtx(
        &self,
        rtx: &ReadTransaction,
        tx_id: &[u8; 32],
    ) -> anyhow::Result<Option<SharedImmutable<AcceptanceKey>>> {
        let mut iter = rtx.prefix(&self.0, tx_id);
        let r = iter.next().transpose()?;
        warn_if_duplicate(tx_id, iter.next().is_some());
        Ok(r.map(|(k, _)| SharedImmutable::new(k)))
    }

    pub fn key_by_tx_id(
        &self,
        tx_id: &[u8; 32],
    ) -> anyhow::Result<Option<SharedImmutable<AcceptanceKey>>> {
        let mut iter = self.0.inner().prefix(tx_id);
        let r = iter.next().transpose()?;
        warn_if_duplicate(tx_id, iter.next().is_some());
        Ok(r.map(|(k, _)| SharedImmutable::new(k)))
    }

    /// The record for `tx_id`, read through the write transaction so a record written earlier in
    /// it is seen.
    pub fn key_by_tx_id_wtx(
        &self,
        wtx: &mut WriteTransaction,
        tx_id: &[u8; 32],
    ) -> anyhow::Result<Option<SharedImmutable<AcceptanceKey>>> {
        let mut iter = wtx.prefix(&self.0, tx_id);
        let r = iter.next().transpose()?;
        let duplicate = iter.next().is_some();
        drop(iter);
        warn_if_duplicate(tx_id, duplicate);
        Ok(r.map(|(k, _)| SharedImmutable::new(k)))
    }

    pub fn update_acceptance_wtx(
        &self,
        wtx: &mut WriteTransaction,
        k: &AcceptanceKey,
        accepting_block_hash: [u8; 32],
        acceptance_daa: u64,
    ) -> anyhow::Result<LookupOutput> {
        let old_value = wtx.fetch_update(&self.0, k.as_bytes(), |old_value| {
            let old_value = old_value?;
            let mut v = Vec::with_capacity(old_value.len());
            let h = AcceptanceValueHeader {
                accepting_block_hash,
                accepting_daa: acceptance_daa.into(),
            };
            v.extend_from_slice(h.as_bytes());
            v.extend_from_slice(old_value[size_of::<AcceptanceValueHeader>()..].as_ref());
            Some(v.into())
        })?;
        Ok(match old_value {
            None => LookupOutput::KeyDoesNotExist,
            Some(value) if value.len() > size_of::<AcceptanceValueHeader>() => {
                LookupOutput::KeysExistsWithEntries
            }
            Some(_) => LookupOutput::KeyExistsNoEntries,
        })
    }

    pub fn resolve_entries_wtx(
        &self,
        wtx: &mut WriteTransaction,
        k: &AcceptanceKey,
        key_len_fn: impl Fn(PartitionId) -> usize,
        entry_cb_fn: impl for<'a, 'b> Fn(&'a mut WriteTransaction, Entry<'b>) -> anyhow::Result<()>,
    ) -> anyhow::Result<KeyExists> {
        let Some(old_value) = self.take_entries_wtx(wtx, k)? else {
            return Ok(false);
        };
        let mut entries = &old_value[size_of::<AcceptanceValueHeader>()..];
        while !entries.is_empty() {
            let partition_id = PartitionId::try_read_from_bytes(&entries[..1])
                .map_err(|_| anyhow::anyhow!("Invalid partition id: {}", entries[0]))?;
            let key_len = key_len_fn(partition_id);
            let action = Action::try_read_from_bytes(&entries[1..2])
                .map_err(|_| anyhow::anyhow!("Invalid action: {}", entries[1]))?;
            let entry = Entry {
                partition_id,
                action,
                key: &entries[2..2 + key_len],
            };
            entry_cb_fn(wtx, entry)?;
            entries = &entries[2 + key_len..];
        }
        Ok(true)
    }

    /// Cut the record back to its header, dropping the entries that wait on the sender. Returns
    /// the value it had (None when there is no record).
    pub fn take_entries_wtx(
        &self,
        wtx: &mut WriteTransaction,
        k: &AcceptanceKey,
    ) -> anyhow::Result<Option<fjall::Slice>> {
        Ok(wtx.fetch_update(&self.0, k.as_bytes(), |old_value| {
            let old_value = old_value?;
            let new_value = &old_value.as_ref()[..size_of::<AcceptanceValueHeader>()];
            Some(new_value.into())
        })?)
    }

    /// Keys of the records that still have entries waiting on a sender (committed state).
    pub fn keys_with_entries(&self) -> impl Iterator<Item = anyhow::Result<AcceptanceKey>> + '_ {
        self.0.inner().iter().filter_map(|kv| match kv {
            Err(e) => Some(Err(e.into())),
            Ok((k, v)) if v.len() > size_of::<AcceptanceValueHeader>() => Some(
                AcceptanceKey::read_from_bytes(k.as_ref())
                    .map_err(|_| anyhow::anyhow!("db corrupted, failed to read acceptance key")),
            ),
            Ok(_) => None,
        })
    }

    /// Move the record to another receiver, keeping its value. The receiver is part of the key.
    pub fn rekey_receiver_wtx(
        &self,
        wtx: &mut WriteTransaction,
        k: &AcceptanceKey,
        receiver: AddressPayload,
    ) -> anyhow::Result<()> {
        let Some(value) = wtx.get(&self.0, k.as_bytes())? else {
            return Ok(());
        };
        wtx.remove(&self.0, k.as_bytes());
        let new_key = AcceptanceKey {
            tx_id: k.tx_id,
            receiver,
        };
        wtx.insert(&self.0, new_key.as_bytes(), value);
        Ok(())
    }

    pub fn acceptance_by_tx_id_rtx(
        &self,
        rtx: &ReadTransaction,
        tx_id: &[u8; 32],
    ) -> anyhow::Result<Option<SharedImmutable<AcceptanceValueHeaderWithSuffix>>> {
        let mut iter = rtx.prefix(&self.0, tx_id);
        let kv = iter.next().transpose()?;
        let r = match kv {
            None => Ok(None),
            Some((_k, v)) => {
                let r = SharedImmutable::<AcceptanceValueHeaderWithSuffix>::new(v);
                if r.header == AcceptanceValueHeader::new_zeroed() {
                    Ok(None)
                } else {
                    Ok(Some(r))
                }
            }
        };
        warn_if_duplicate(tx_id, iter.next().is_some());
        r
    }
}

/// A tx should have one acceptance record. A second one (another receiver, written when the tx
/// was indexed twice before the block processor reused the first record's receiver) is ignored:
/// the first record is used, as it is everywhere.
fn warn_if_duplicate(tx_id: &[u8; 32], duplicate: bool) {
    if duplicate {
        let hex: String = tx_id.iter().map(|b| format!("{b:02x}")).collect();
        tracing::warn!(
            tx_id = %hex,
            "More than one acceptance record for a tx; using the first"
        );
    }
}

type KeyExists = bool;

/// An acceptance record as it was before entries were appended to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviousAcceptance {
    pub header: AcceptanceValueHeader,
    /// Entries were still waiting on the sender.
    pub has_entries: bool,
}

impl PreviousAcceptance {
    /// The tx is accepted (a non-zero accepting block): the virtual chain processor already
    /// asked for its sender, and won't for entries appended now.
    pub fn is_accepted(&self) -> bool {
        self.header.accepting_block_hash != [0u8; 32]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupOutput {
    KeyDoesNotExist,
    KeyExistsNoEntries,
    KeysExistsWithEntries,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(partition_id: PartitionId, key: &[u8]) -> InsertionEntry {
        InsertionEntry {
            partition_id,
            action: Action::UpdateValueSender,
            partition_key: SmallVec::from_slice(key),
        }
    }

    #[test]
    fn records_report_what_they_were_and_survive_a_duplicate() {
        let dir = tempfile::tempdir().expect("temporary database directory");
        let keyspace = fjall::Config::new(dir.path())
            .open_transactional()
            .expect("transactional keyspace");
        let acceptance = TxIDToAcceptancePartition::new(&keyspace).expect("partition");
        let change = AddressPayload::from_xonly_pubkey([1; 32]);
        let recipient = AddressPayload::from_xonly_pubkey([2; 32]);
        let key = AcceptanceKey {
            tx_id: [9; 32],
            receiver: change,
        };

        // New, then appended to: the second insert sees the first, with its entry.
        let mut wtx = keyspace.write_tx().expect("wtx");
        let first = acceptance
            .insert_wtx(&mut wtx, &key, &[entry(PartitionId::HandshakeByReceiver, &[7; 4])])
            .expect("insert");
        assert_eq!(first, None);
        assert!(
            acceptance
                .key_by_tx_id_wtx(&mut wtx, &[9; 32])
                .expect("lookup")
                .is_some()
        );
        let second = acceptance.insert_wtx(&mut wtx, &key, &[]).expect("insert");
        assert_eq!(
            second,
            Some(PreviousAcceptance {
                header: AcceptanceValueHeader::default(),
                has_entries: true,
            })
        );
        assert!(!second.unwrap().is_accepted());
        wtx.commit().expect("commit").expect("no conflict");
        let waiting: Vec<_> = acceptance
            .keys_with_entries()
            .map(|k| k.expect("key").receiver)
            .collect();
        assert_eq!(waiting, vec![change]);

        // Accepted, then cut back to its header: still accepted, no entries.
        let mut wtx = keyspace.write_tx().expect("wtx");
        acceptance
            .update_acceptance_wtx(&mut wtx, &key, [5; 32], 77)
            .expect("accept");
        acceptance.take_entries_wtx(&mut wtx, &key).expect("take");
        let previous = acceptance
            .insert_wtx(&mut wtx, &key, &[])
            .expect("insert")
            .expect("existed");
        assert!(previous.is_accepted());
        assert!(!previous.has_entries);
        assert_eq!(previous.header.accepting_daa.get(), 77);
        wtx.commit().expect("commit").expect("no conflict");
        assert_eq!(acceptance.keys_with_entries().count(), 0);

        // Moved to another receiver: one record, same value.
        let mut wtx = keyspace.write_tx().expect("wtx");
        acceptance
            .rekey_receiver_wtx(&mut wtx, &key, recipient)
            .expect("rekey");
        wtx.commit().expect("commit").expect("no conflict");
        let moved = acceptance.key_by_tx_id(&[9; 32]).expect("lookup").expect("record");
        assert_eq!(moved.receiver, recipient);
        let rtx = keyspace.read_tx();
        let value = acceptance
            .acceptance_by_tx_id_rtx(&rtx, &[9; 32])
            .expect("read")
            .expect("accepted");
        assert_eq!(value.header.accepting_daa.get(), 77);
        drop(rtx);

        // A second record for the same tx (another receiver) no longer panics: the first in key
        // order is used.
        let mut wtx = keyspace.write_tx().expect("wtx");
        acceptance.insert_wtx(&mut wtx, &key, &[]).expect("insert");
        wtx.commit().expect("commit").expect("no conflict");
        let first = acceptance.key_by_tx_id(&[9; 32]).expect("lookup").expect("record");
        assert_eq!(first.receiver, change);
        let rtx = keyspace.read_tx();
        let first = acceptance
            .key_by_tx_id_rtx(&rtx, &[9; 32])
            .expect("lookup")
            .expect("record");
        assert_eq!(first.receiver, change);
        assert!(acceptance.acceptance_by_tx_id_rtx(&rtx, &[9; 32]).is_ok());
    }
}

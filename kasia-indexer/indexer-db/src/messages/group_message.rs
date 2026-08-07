use crate::{AddressPayload, SharedImmutable};
use anyhow::Result;
use fjall::{PartitionCreateOptions, ReadTransaction, WriteTransaction};
use std::fmt::Debug;
use zerocopy::big_endian::U64;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

pub const BLINDED_GROUP_ID_LEN: usize = 32;

/// blinded_group_id (32) + block_time (8) + block_hash (32) + version (1) + tx_id (32)
/// Note: the blinded group id is opaque to the indexer (it cannot recover the real
/// group id without the client-side blinding key); it is only used as an exact-match
/// lookup key so members can chronologically retrieve messages for a group they belong to.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Immutable, KnownLayout, IntoBytes, FromBytes, Unaligned,
)]
pub struct GroupMessageKeyByBlindedGroupId {
    pub blinded_group_id: [u8; BLINDED_GROUP_ID_LEN],
    pub block_time: U64,
    pub block_hash: [u8; 32],
    pub version: u8,
    pub tx_id: [u8; 32],
}

#[derive(Clone)]
pub struct GroupMessageByBlindedGroupIdPartition(fjall::TxPartition);

impl GroupMessageByBlindedGroupIdPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> Result<Self> {
        Ok(Self(keyspace.open_partition(
            "group_message_by_blinded_group_id",
            PartitionCreateOptions::default(),
        )?))
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self.0.inner().len()?)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.0.inner().is_empty()?)
    }

    pub fn approximate_len(&self) -> usize {
        self.0.approximate_len()
    }

    pub fn insert_wtx(
        &self,
        wtx: &mut WriteTransaction,
        key: &GroupMessageKeyByBlindedGroupId,
        sender: Option<AddressPayload>,
    ) -> Result<()> {
        let sender = sender.unwrap_or_default();
        wtx.update_fetch(&self.0, key.as_bytes(), |old| match old {
            None => Some(sender.as_bytes().into()),
            Some(old) => {
                let old_sender = AddressPayload::try_ref_from_bytes(old.as_bytes()).unwrap();
                if old_sender != &AddressPayload::default() {
                    Some(old.clone())
                } else {
                    Some(sender.as_bytes().into())
                }
            }
        })?;
        Ok(())
    }

    pub fn remove_wtx(&self, wtx: &mut WriteTransaction, key: &GroupMessageKeyByBlindedGroupId) {
        wtx.remove(&self.0, key.as_bytes());
    }

    pub fn iter_by_blinded_group_id_from_block_time_rtx(
        &self,
        rtx: &ReadTransaction,
        blinded_group_id: &[u8; BLINDED_GROUP_ID_LEN],
        block_time: u64,
    ) -> impl DoubleEndedIterator<
        Item = Result<(
            SharedImmutable<GroupMessageKeyByBlindedGroupId>,
            SharedImmutable<AddressPayload>,
        )>,
    > + '_ {
        const PREFIX_LEN: usize = BLINDED_GROUP_ID_LEN + 8;
        let mut range_start = [0u8; PREFIX_LEN];
        range_start[..BLINDED_GROUP_ID_LEN].copy_from_slice(blinded_group_id);
        range_start[BLINDED_GROUP_ID_LEN..].copy_from_slice(&block_time.to_be_bytes());

        let mut range_end = [0xFFu8; PREFIX_LEN];
        range_end[..BLINDED_GROUP_ID_LEN].copy_from_slice(blinded_group_id);

        rtx.range(&self.0, range_start..=range_end).map(|item| {
            let (key_bytes, value_bytes) = item?;
            Ok((
                SharedImmutable::new(key_bytes),
                SharedImmutable::new(value_bytes),
            ))
        })
    }
}

#[derive(Clone)]
pub struct TxIdToGroupMessagePartition(fjall::TxPartition);

impl TxIdToGroupMessagePartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> Result<Self> {
        Ok(Self(keyspace.open_partition(
            "tx-id-to-group-message",
            PartitionCreateOptions::default(),
        )?))
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self.0.inner().len()?)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.0.inner().is_empty()?)
    }

    pub fn approximate_len(&self) -> usize {
        self.0.approximate_len()
    }

    pub fn insert_wtx(&self, wtx: &mut WriteTransaction, tx_id: &[u8; 32], sealed_hex: &[u8]) {
        wtx.insert(&self.0, tx_id, sealed_hex);
    }

    pub fn remove_wtx(&self, wtx: &mut WriteTransaction, tx_id: &[u8; 32]) {
        wtx.remove(&self.0, tx_id);
    }

    pub fn get_rtx(
        &self,
        rtx: &ReadTransaction,
        tx_id: &[u8; 32],
    ) -> Result<Option<SharedImmutable<[u8]>>> {
        rtx.get(&self.0, tx_id)
            .map(|bts| bts.map(SharedImmutable::new))
            .map_err(anyhow::Error::from)
    }
}

/// Pins a sender-specific blinded group id to the first validated transaction pubkey that uses
/// it. Since the id is a 256-bit secret-derived value, this prevents an observer from copying an
/// id into later transactions and causing push amplification with another signing key.
#[derive(Clone)]
pub struct GroupSenderBindingPartition(fjall::TxPartition);

impl GroupSenderBindingPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> Result<Self> {
        Ok(Self(keyspace.open_partition(
            "group_sender_binding",
            PartitionCreateOptions::default(),
        )?))
    }

    /// Returns true when the binding is new or matches the existing binding.
    pub fn check_or_bind_wtx(
        &self,
        wtx: &mut WriteTransaction,
        blinded_group_id: &[u8; BLINDED_GROUP_ID_LEN],
        sender_pubkey: &[u8; 32],
    ) -> Result<bool> {
        let previous = wtx.fetch_update(&self.0, blinded_group_id, |old| {
            Some(match old {
                Some(value) => value.clone(),
                None => sender_pubkey.as_slice().into(),
            })
        })?;
        Ok(previous
            .as_ref()
            .map(|value| value.as_ref() == sender_pubkey)
            .unwrap_or(true))
    }
}

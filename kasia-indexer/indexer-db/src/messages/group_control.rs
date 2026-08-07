use crate::{AddressPayload, SharedImmutable};
use anyhow::Result;
use fjall::{PartitionCreateOptions, ReadTransaction, WriteTransaction};
use std::fmt::Debug;
use zerocopy::big_endian::U64;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

/// sender (34) + block_time (8) + block_hash (32) + version (1) + tx_id (32) + recipient (34)
/// Note: sender can be zeros (when not resolved yet)
/// Recipient is zero for the legacy `gctl:{encrypted}` format and populated for the addressed
/// `gctl:{recipient_xonly_pubkey}:{encrypted}` format.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Immutable, KnownLayout, IntoBytes, FromBytes, Unaligned,
)]
pub struct GroupControlKeyBySender {
    pub sender: AddressPayload,
    pub block_time: U64,
    pub block_hash: [u8; 32],
    pub version: u8,
    pub tx_id: [u8; 32],
    pub recipient: AddressPayload,
}

/// recipient (34) + block_time (8) + block_hash (32) + version (1) + tx_id (32)
#[repr(C)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Immutable, KnownLayout, IntoBytes, FromBytes, Unaligned,
)]
pub struct GroupControlKeyByRecipient {
    pub recipient: AddressPayload,
    pub block_time: U64,
    pub block_hash: [u8; 32],
    pub version: u8,
    pub tx_id: [u8; 32],
}

#[derive(Clone)]
pub struct GroupControlByRecipientPartition(fjall::TxPartition);

impl GroupControlByRecipientPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> Result<Self> {
        Ok(Self(keyspace.open_partition(
            "group_control_by_recipient",
            PartitionCreateOptions::default(),
        )?))
    }

    pub fn insert_wtx(
        &self,
        wtx: &mut WriteTransaction,
        key: &GroupControlKeyByRecipient,
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

    pub fn get_by_recipient_from_block_time(
        &self,
        rtx: &ReadTransaction,
        recipient: &AddressPayload,
        from_block_time: u64,
    ) -> impl DoubleEndedIterator<
        Item = Result<(
            SharedImmutable<GroupControlKeyByRecipient>,
            SharedImmutable<AddressPayload>,
        )>,
    > + '_ {
        let mut range_start = [0u8; 42];
        range_start[..34].copy_from_slice(recipient.as_bytes());
        range_start[34..].copy_from_slice(&from_block_time.to_be_bytes());

        let mut range_end = [0xFFu8; 42];
        range_end[..34].copy_from_slice(recipient.as_bytes());

        rtx.range(&self.0, range_start..=range_end).map(|item| {
            let (key, value) = item?;
            Ok((SharedImmutable::new(key), SharedImmutable::new(value)))
        })
    }
}

#[derive(Clone)]
pub struct GroupControlBySenderPartition(fjall::TxPartition);

impl GroupControlBySenderPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> Result<Self> {
        Ok(Self(keyspace.open_partition(
            "group_control_by_sender",
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

    pub fn insert_wtx(&self, wtx: &mut WriteTransaction, key: &GroupControlKeyBySender) {
        wtx.insert(&self.0, key.as_bytes(), []);
    }

    pub fn get_by_sender_from_block_time(
        &self,
        rtx: &ReadTransaction,
        sender: &AddressPayload,
        from_block_time: u64,
    ) -> impl DoubleEndedIterator<Item = Result<SharedImmutable<GroupControlKeyBySender>>> + '_
    {
        // Create range start: sender (34 bytes) + block_time (8 bytes)
        let mut range_start = [0u8; 42]; // 34 + 8
        range_start[..34].copy_from_slice(sender.as_bytes());
        range_start[34..42].copy_from_slice(&from_block_time.to_be_bytes());

        // Create range end: sender (34 bytes) + max block_time (8 bytes)
        let mut range_end = [0xFFu8; 42]; // 34 + 8
        range_end[..34].copy_from_slice(sender.as_bytes());

        rtx.range(&self.0, range_start..=range_end).map(|item| {
            item.map(|(key, _value)| SharedImmutable::new(key))
                .map_err(anyhow::Error::from)
        })
    }
}

#[derive(Clone)]
pub struct TxIdToGroupControlPartition(fjall::TxPartition);

impl TxIdToGroupControlPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> Result<Self> {
        Ok(Self(keyspace.open_partition(
            "tx-id-to-group-control",
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

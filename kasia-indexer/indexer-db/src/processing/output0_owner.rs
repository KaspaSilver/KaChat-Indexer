//! Who owns output 0 of a sealed-operation tx, when that is not the tx's resolved receiver.
//!
//! A tx whose first input spends output 0 of an indexed tx takes that output's owner as its
//! sender without asking the node. The acceptance record only holds the parent's receiver (the
//! first output that isn't the parent's sender), which is the owner of output 0 only when the
//! outputs are ordered [receiver, change]. Ordered [change, receiver], spending the change made
//! the child look as if the receiver sent it, so anyone could send as anyone they had paid dust.
//!
//! - `tx_output0_owner`: tx id -> the address output 0 paid, written only when it differs from
//!   the acceptance record's receiver. An empty address means output 0 pays no standard address,
//!   so its owner is unknown and the sender is left to the node.
//! - No record: output 0 pays the receiver, or the tx was indexed before this partition existed.

use crate::AddressPayload;
use fjall::{PartitionCreateOptions, WriteTransaction};
use zerocopy::{FromBytes, IntoBytes};

#[derive(Clone)]
pub struct TxOutput0OwnerPartition(fjall::TxPartition);

impl TxOutput0OwnerPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> anyhow::Result<Self> {
        Ok(Self(keyspace.open_partition(
            "tx_output0_owner",
            PartitionCreateOptions::default(),
        )?))
    }

    pub fn insert_wtx(&self, wtx: &mut WriteTransaction, tx_id: &[u8; 32], owner: &AddressPayload) {
        wtx.insert(&self.0, tx_id, owner.as_bytes())
    }

    /// Read through the write transaction, so a tx earlier in the same block is seen.
    pub fn get_wtx(
        &self,
        wtx: &mut WriteTransaction,
        tx_id: &[u8; 32],
    ) -> anyhow::Result<Option<AddressPayload>> {
        wtx.get(&self.0, tx_id)?
            .map(|bytes| {
                AddressPayload::read_from_bytes(bytes.as_ref())
                    .map_err(|_| anyhow::anyhow!("db corrupted, failed to read output 0 owner"))
            })
            .transpose()
    }
}

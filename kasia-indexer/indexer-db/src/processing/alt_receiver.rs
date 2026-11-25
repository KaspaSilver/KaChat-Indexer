//! The receiver a tx would have had if its sender had been known when it was indexed.
//!
//! The receiver is the first output that doesn't pay the sender. With the sender unknown at block
//! time, it is the first standard output, which for outputs ordered [change, recipient] is the
//! sender's own change: the record was filed under the sender. Sender resolution reads this to
//! move the receiver-side rows to the real recipient.
//!
//! - `tx_alt_receiver`: tx id -> (address, amount) of the first standard output that doesn't pay
//!   the receiver. Written only for a tx whose records wait on an unknown sender, and removed
//!   when the sender resolves.
//! - No record: there is no such output, or the tx was indexed before this partition existed.

use crate::AddressPayload;
use fjall::{PartitionCreateOptions, WriteTransaction};
use zerocopy::little_endian::U64;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

#[repr(C)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Immutable, IntoBytes, FromBytes, Unaligned, KnownLayout,
)]
pub struct AltReceiver {
    pub receiver: AddressPayload,
    /// What the output pays, in sompi.
    pub amount: U64,
}

#[derive(Clone)]
pub struct TxAltReceiverPartition(fjall::TxPartition);

impl TxAltReceiverPartition {
    pub fn new(keyspace: &fjall::TxKeyspace) -> anyhow::Result<Self> {
        Ok(Self(keyspace.open_partition(
            "tx_alt_receiver",
            PartitionCreateOptions::default(),
        )?))
    }

    pub fn insert_wtx(&self, wtx: &mut WriteTransaction, tx_id: &[u8; 32], alt: &AltReceiver) {
        wtx.insert(&self.0, tx_id, alt.as_bytes())
    }

    pub fn get_wtx(
        &self,
        wtx: &mut WriteTransaction,
        tx_id: &[u8; 32],
    ) -> anyhow::Result<Option<AltReceiver>> {
        wtx.get(&self.0, tx_id)?
            .map(|bytes| {
                AltReceiver::read_from_bytes(bytes.as_ref())
                    .map_err(|_| anyhow::anyhow!("db corrupted, failed to read alt receiver"))
            })
            .transpose()
    }

    pub fn remove_wtx(&self, wtx: &mut WriteTransaction, tx_id: &[u8; 32]) {
        wtx.remove(&self.0, tx_id)
    }
}

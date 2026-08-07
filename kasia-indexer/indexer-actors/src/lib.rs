pub mod block_gap_filler;
pub mod block_processor;
pub mod data_source;
pub mod fifo_set;
pub mod metrics;
pub mod periodic_processor;
pub mod push;
pub mod ticker;
pub mod virtual_chain_processor;
pub mod virtual_chain_syncer;

pub mod util;

// ---------------------------------------------------------------------------
// Personal indexing mode (KaChat fork)
//
// When one or more "personal" addresses are configured, the block processor only persists
// chat content (handshakes, 1:1 messages, payments, self-stash, group messages/controls) for
// transactions that involve one of those addresses as sender or receiver. Empty set = OFF, so
// the default public indexer stores everything. Loaded once at startup from a file the admin
// dashboard writes; changing it restarts the chat indexer process, which reloads the file.
// ---------------------------------------------------------------------------

use indexer_db::AddressPayload;
use std::sync::RwLock;

static PERSONAL_ADDRESSES: RwLock<Vec<AddressPayload>> = RwLock::new(Vec::new());

/// Replace the personal-mode address allowlist. An empty vec turns personal mode OFF.
pub fn set_personal_addresses(addrs: Vec<AddressPayload>) {
    if let Ok(mut guard) = PERSONAL_ADDRESSES.write() {
        *guard = addrs;
    }
}

/// Number of configured personal addresses (0 = personal mode off / index everything).
pub fn personal_address_count() -> usize {
    PERSONAL_ADDRESSES.read().map(|g| g.len()).unwrap_or(0)
}

/// Whether a transaction touching `sender`/`receiver` should have its chat content stored.
/// With no personal addresses configured, everything is stored (public-indexer default).
pub fn personal_allows(sender: Option<&AddressPayload>, receiver: &AddressPayload) -> bool {
    let guard = match PERSONAL_ADDRESSES.read() {
        Ok(g) => g,
        Err(_) => return true,
    };
    if guard.is_empty() {
        return true;
    }
    if guard.iter().any(|a| a == receiver) {
        return true;
    }
    matches!(sender, Some(s) if guard.iter().any(|a| a == s))
}

#[derive(Debug, Clone, Copy)]
pub struct BlockGap {
    pub from_block: [u8; 32],
    pub to_block: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
pub struct InterruptedGapSync {
    pub from_block: [u8; 32],
    pub interrupted_at: [u8; 32],
    pub to_block: [u8; 32],
}

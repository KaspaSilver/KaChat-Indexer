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
// chat content (handshakes, 1:1 messages, payments, self-stash, group control) for transactions
// that involve one of those addresses as sender or receiver. Group MESSAGES can't be matched by
// address (a group tx's receiver is the sender's own change address), so they have a parallel
// allowlist of blinded group ids (`PERSONAL_GROUP_IDS`): a group message is stored if its blinded
// id is listed OR the sender is one of my addresses. Both lists empty = OFF, so the default public
// indexer stores everything. Loaded once at startup from files the admin dashboard writes; changing
// either restarts the chat indexer process, which reloads them.
// ---------------------------------------------------------------------------

use indexer_db::AddressPayload;
use indexer_db::messages::group_message::BLINDED_GROUP_ID_LEN;
use std::collections::HashSet;
use std::sync::{LazyLock, RwLock};

static PERSONAL_ADDRESSES: RwLock<Vec<AddressPayload>> = RwLock::new(Vec::new());

/// Personal-mode GROUP allowlist: blinded group ids (per-(group,member), 32 bytes) whose group
/// messages should be stored. Parallel to `PERSONAL_ADDRESSES` — an operator who can't be matched
/// by address on group traffic (a group tx's receiver is the sender's own change address) lists
/// their blinded ids here instead. Empty = no group filtering. Loaded at startup from a file the
/// admin dashboard writes (reload = chat process restart), same as the address allowlist.
static PERSONAL_GROUP_IDS: LazyLock<RwLock<HashSet<[u8; BLINDED_GROUP_ID_LEN]>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

/// Union of every device's registered `watch_only_addresses` (Address Activity push). The push
/// registry keeps this in sync; the block processor consults it to decide whether an accepted tx
/// credits a watched address without a per-output DB lookup. Empty = the feature is fully inert
/// (no device is watching any owned address), so the block-processing gate is a no-op.
static WATCH_ONLY_ADDRESSES: LazyLock<RwLock<HashSet<AddressPayload>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

/// Replace the watch-only address set (called by the push registry on any registration change).
pub fn set_watch_only_addresses(addrs: HashSet<AddressPayload>) {
    if let Ok(mut guard) = WATCH_ONLY_ADDRESSES.write() {
        *guard = addrs;
    }
}

/// Whether no device is watching any owned address — the block processor skips all funds work then.
pub fn watch_only_is_empty() -> bool {
    WATCH_ONLY_ADDRESSES.read().map(|g| g.is_empty()).unwrap_or(true)
}

/// Whether `addr` is watched by at least one device (a slightly-stale over-approximation is fine —
/// the dispatcher does the precise per-device match, so a false positive just drops downstream).
pub fn watch_only_contains(addr: &AddressPayload) -> bool {
    WATCH_ONLY_ADDRESSES
        .read()
        .map(|g| g.contains(addr))
        .unwrap_or(false)
}

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

/// Replace the personal-mode group-id allowlist. An empty set turns group filtering OFF.
pub fn set_personal_group_ids(ids: HashSet<[u8; BLINDED_GROUP_ID_LEN]>) {
    if let Ok(mut guard) = PERSONAL_GROUP_IDS.write() {
        *guard = ids;
    }
}

/// Number of configured personal group ids (0 = no group filtering).
pub fn personal_group_id_count() -> usize {
    PERSONAL_GROUP_IDS.read().map(|g| g.len()).unwrap_or(0)
}

/// Whether any personal filtering is configured at all (address OR group list non-empty). When
/// false, the indexer is a public one and stores everything.
pub fn personal_filtering_active() -> bool {
    personal_address_count() > 0 || personal_group_id_count() > 0
}

/// Raw membership: is `addr` in the personal-address allowlist? (No empty-list short-circuit —
/// callers combine this with the group check and the global `personal_filtering_active` guard.)
pub fn is_personal_address(addr: &AddressPayload) -> bool {
    PERSONAL_ADDRESSES
        .read()
        .map(|g| g.iter().any(|a| a == addr))
        .unwrap_or(false)
}

/// Raw membership: is `blinded_group_id` in the personal-group allowlist?
pub fn is_personal_group(blinded_group_id: &[u8; BLINDED_GROUP_ID_LEN]) -> bool {
    PERSONAL_GROUP_IDS
        .read()
        .map(|g| g.contains(blinded_group_id))
        .unwrap_or(false)
}

/// Whether a group MESSAGE (sent by `sender`, addressed to blinded `blinded_group_id`) should be
/// stored under personal mode. Nothing configured => public default (store everything). Otherwise
/// store if the blinded id is mine OR the sender is one of my addresses (the latter keeps today's
/// "always keep what you send" behavior even before you've listed that group's id).
pub fn personal_group_message_allows(
    sender: Option<&AddressPayload>,
    blinded_group_id: &[u8; BLINDED_GROUP_ID_LEN],
) -> bool {
    if !personal_filtering_active() {
        return true;
    }
    is_personal_group(blinded_group_id) || matches!(sender, Some(s) if is_personal_address(s))
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

#[cfg(test)]
mod personal_group_tests {
    use super::*;

    fn addr(b: u8) -> AddressPayload {
        AddressPayload {
            inverse_version: u8::MAX, // PubKey version, matching real personal addresses
            payload: [b; 33],
        }
    }

    // One test mutates the process-global personal state in sequence (reset between phases), so it
    // stays correct regardless of test parallelism. Covers the four allowlist combinations.
    #[test]
    fn group_message_rules() {
        let my_gid = [7u8; BLINDED_GROUP_ID_LEN];
        let other_gid = [9u8; BLINDED_GROUP_ID_LEN];

        // 1) nothing configured -> public default: store everything.
        set_personal_addresses(Vec::new());
        set_personal_group_ids(HashSet::new());
        assert!(!personal_filtering_active());
        assert!(personal_group_message_allows(Some(&addr(2)), &other_gid));
        assert!(personal_group_message_allows(None, &other_gid));

        // 2) only addresses -> store iff sender is mine (today's "what you send").
        set_personal_addresses(vec![addr(1)]);
        set_personal_group_ids(HashSet::new());
        assert!(personal_filtering_active());
        assert!(personal_group_message_allows(Some(&addr(1)), &other_gid));
        assert!(!personal_group_message_allows(Some(&addr(2)), &other_gid));
        assert!(!personal_group_message_allows(None, &other_gid));

        // 3) only group ids -> store iff the blinded id is mine (1:1 path handled elsewhere).
        set_personal_addresses(Vec::new());
        set_personal_group_ids(HashSet::from([my_gid]));
        assert!(personal_filtering_active());
        assert!(personal_group_message_allows(Some(&addr(2)), &my_gid)); // my group, any sender
        assert!(!personal_group_message_allows(Some(&addr(2)), &other_gid));
        assert!(personal_group_message_allows(None, &my_gid));

        // 4) both -> blinded id mine OR sender mine.
        set_personal_addresses(vec![addr(1)]);
        set_personal_group_ids(HashSet::from([my_gid]));
        assert!(personal_group_message_allows(Some(&addr(2)), &my_gid)); // group match
        assert!(personal_group_message_allows(Some(&addr(1)), &other_gid)); // sender match
        assert!(!personal_group_message_allows(Some(&addr(2)), &other_gid)); // neither

        // Reset globals so nothing leaks into other tests / real runs.
        set_personal_addresses(Vec::new());
        set_personal_group_ids(HashSet::new());
    }
}

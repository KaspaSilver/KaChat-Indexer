//! KaChat Stats: rolling 24h / 7d windows for the six chat-family categories.
//!
//! The `/stats` totals come from `approximate_len()` of the one-row-per-tx `tx-id-to-*` partitions,
//! which carry no time, so they can only give all-time totals. This module adds the windows: a
//! background task (every ~5 min, off the request path) walks each `*_by_*` partition — whose keys
//! DO carry the accepted block time — and counts the distinct transactions newer than now-24h and
//! now-7d. `get_stats` reads the last cached counts and never scans.
//!
//! Dedup by tx_id matters: a contextual message is first stored under a zero sender and then under
//! its resolved sender, so the same tx can appear twice in `contextual_message_by_sender`. Counting
//! distinct tx_ids collapses that, and (because last24h's set is a subset of last7d's) guarantees
//! last24h <= last7d. The windows are drawn from the same stored set the totals are, so they stay
//! consistent with `total`; whatever root the totals include, the windows include too.

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fjall::{PartitionCreateOptions, TxKeyspace};
use indexer_db::TryFromBytes;
use indexer_db::messages::contextual_message::ContextualMessageBySenderKey;
use indexer_db::messages::group_control::GroupControlKeyBySender;
use indexer_db::messages::group_message::GroupMessageKeyByBlindedGroupId;
use indexer_db::messages::handshake::HandshakeKeyBySender;
use indexer_db::messages::payment::PaymentKeyBySender;
use indexer_db::messages::self_stash::SelfStashKeyByOwner;

/// category key -> (last24h, last7d)
type Windows = HashMap<&'static str, (u64, u64)>;

static CACHE: OnceLock<RwLock<Option<Windows>>> = OnceLock::new();
fn cache() -> &'static RwLock<Option<Windows>> {
    CACHE.get_or_init(|| RwLock::new(None))
}

/// (last24h, last7d) for a category, or None until the first scan completes (keep it omitted, not 0).
pub fn windows_for(category: &str) -> Option<(u64, u64)> {
    cache().read().ok()?.as_ref()?.get(category).copied()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Walk one by-* partition, counting distinct tx_ids whose block time is newer than each cutoff.
/// `decode` extracts (block_time_ms, tx_id) from a raw key.
fn scan(
    tx_keyspace: &TxKeyspace,
    partition: &str,
    d1: u64,
    d7: u64,
    decode: fn(&[u8]) -> Option<(u64, [u8; 32])>,
) -> anyhow::Result<(u64, u64)> {
    let part = tx_keyspace.open_partition(partition, PartitionCreateOptions::default())?;
    let rtx = tx_keyspace.read_tx();
    let mut s1: HashSet<[u8; 32]> = HashSet::new();
    let mut s7: HashSet<[u8; 32]> = HashSet::new();
    for kv in rtx.iter(&part) {
        let (k, _v) = kv?;
        if let Some((bt, tx)) = decode(k.as_ref()) {
            if bt > d7 {
                s7.insert(tx);
                if bt > d1 {
                    s1.insert(tx);
                }
            }
        }
    }
    Ok((s1.len() as u64, s7.len() as u64))
}

fn compute(tx_keyspace: &TxKeyspace) -> Windows {
    let now = now_ms();
    let d1 = now.saturating_sub(24 * 3600 * 1000);
    let d7 = now.saturating_sub(7 * 24 * 3600 * 1000);

    // (category, partition, key decoder). Decoders are non-capturing → fn pointers.
    let jobs: &[(&'static str, &str, fn(&[u8]) -> Option<(u64, [u8; 32])>)] = &[
        ("messages", "contextual_message_by_sender", |k| {
            ContextualMessageBySenderKey::try_read_from_bytes(k).ok().map(|x| (x.block_time.get(), x.tx_id))
        }),
        ("handshakes", "handshake_by_sender", |k| {
            HandshakeKeyBySender::try_read_from_bytes(k).ok().map(|x| (x.block_time.get(), x.tx_id))
        }),
        ("payments", "payment_by_sender", |k| {
            PaymentKeyBySender::try_read_from_bytes(k).ok().map(|x| (x.block_time.get(), x.tx_id))
        }),
        ("groupMessages", "group_message_by_blinded_group_id", |k| {
            GroupMessageKeyByBlindedGroupId::try_read_from_bytes(k).ok().map(|x| (x.block_time.get(), x.tx_id))
        }),
        ("groupUpdates", "group_control_by_sender", |k| {
            GroupControlKeyBySender::try_read_from_bytes(k).ok().map(|x| (x.block_time.get(), x.tx_id))
        }),
        ("selfStash", "self_stash_by_owner", |k| {
            SelfStashKeyByOwner::try_read_from_bytes(k).ok().map(|x| (x.block_time.get(), x.tx_id))
        }),
    ];

    let mut out: Windows = HashMap::new();
    for (cat, partition, decode) in jobs {
        match scan(tx_keyspace, partition, d1, d7, *decode) {
            Ok(w) => {
                out.insert(cat, w);
            }
            Err(e) => tracing::warn!("[stats] window scan for {cat} failed: {e}"),
        }
    }
    out
}

/// Spawn the periodic window scanner (every ~5 min). It writes the cache that `get_stats` reads;
/// the heavy key-walk runs in `spawn_blocking` so it never touches an async worker.
pub fn spawn_window_scanner(tx_keyspace: TxKeyspace) {
    tokio::spawn(async move {
        loop {
            let ks = tx_keyspace.clone();
            match tokio::task::spawn_blocking(move || compute(&ks)).await {
                Ok(windows) => {
                    if let Ok(mut guard) = cache().write() {
                        *guard = Some(windows);
                    }
                }
                Err(e) => tracing::warn!("[stats] window scan task error: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
}

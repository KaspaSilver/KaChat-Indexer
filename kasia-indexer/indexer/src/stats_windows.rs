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

use std::collections::{BTreeMap, HashMap, HashSet};
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

/// Per category, distinct transactions per hour (the last `HOURS_KEPT_MS`) and per calendar month
/// (all time), keyed by the bucket's start (ms UTC). Served at `/stats/buckets` for the webserver's
/// `/stats/activity` (EXPLORER_STATS.md §2).
#[derive(Default, Clone, serde::Serialize)]
pub struct Buckets {
    pub hours: HashMap<&'static str, BTreeMap<u64, u64>>,
    pub months: HashMap<&'static str, BTreeMap<u64, u64>>,
}

/// 14 days of hours: a week's window and the week before it.
const HOURS_KEPT_MS: u64 = 14 * 24 * 3600 * 1000;
const HOUR_MS: u64 = 3600 * 1000;

static CACHE: OnceLock<RwLock<Option<(Windows, Buckets)>>> = OnceLock::new();
fn cache() -> &'static RwLock<Option<(Windows, Buckets)>> {
    CACHE.get_or_init(|| RwLock::new(None))
}

/// (last24h, last7d) for a category, or None until the first scan completes (keep it omitted,
/// not 0).
pub fn windows_for(category: &str) -> Option<(u64, u64)> {
    cache().read().ok()?.as_ref()?.0.get(category).copied()
}

/// The hourly and monthly counts, or None until the first scan completes.
pub fn buckets() -> Option<Buckets> {
    cache().read().ok()?.as_ref().map(|(_, b)| b.clone())
}

/// The start (ms UTC) of the calendar month holding `ms`.
pub fn month_start_ms(ms: u64) -> u64 {
    // Civil-from-days (Howard Hinnant), then back to days for the 1st of that month.
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // The 1st of the month: day-of-year of the month's first day, in the same March-based year.
    let first_doy = (153 * mp + 2) / 5;
    let first_days = era * 146_097 + 365 * yoe + yoe / 4 - yoe / 100 + first_doy - 719_468;
    first_days as u64 * 86_400_000
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One category's counts from one scan.
struct Scanned {
    windows: (u64, u64),
    hours: BTreeMap<u64, u64>,
    months: BTreeMap<u64, u64>,
}

/// Walk one by-* partition, counting each transaction into the windows newer than each cutoff,
/// its hour (the last `HOURS_KEPT_MS`) and its month. `decode` extracts (block_time_ms, tx_id)
/// from a raw key.
fn scan(
    tx_keyspace: &TxKeyspace,
    partition: &str,
    now: u64,
    decode: fn(&[u8]) -> Option<(u64, [u8; 32])>,
) -> anyhow::Result<Scanned> {
    let [d1, d7] = [1, 7].map(|days| now.saturating_sub(days * 24 * HOUR_MS));
    let hours_from = now.saturating_sub(HOURS_KEPT_MS);
    let part = tx_keyspace.open_partition(partition, PartitionCreateOptions::default())?;
    let rtx = tx_keyspace.read_tx();
    // Deduped only inside the last `HOURS_KEPT_MS` (every window and hour is there): an all-time
    // set would hold every tx id ever stored. Older months count each key once; the main source
    // of repeats, a message's zero-sender key, is skipped by its decoder.
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut out = Scanned { windows: (0, 0), hours: BTreeMap::new(), months: BTreeMap::new() };
    for kv in rtx.iter(&part) {
        let (k, _v) = kv?;
        let Some((bt, tx)) = decode(k.as_ref()) else { continue };
        if bt >= hours_from && !seen.insert(tx) {
            continue;
        }
        out.windows.0 += u64::from(bt > d1);
        out.windows.1 += u64::from(bt > d7);
        if bt >= hours_from {
            *out.hours.entry(bt / HOUR_MS * HOUR_MS).or_default() += 1;
        }
        *out.months.entry(month_start_ms(bt)).or_default() += 1;
    }
    Ok(out)
}

fn compute(tx_keyspace: &TxKeyspace) -> (Windows, Buckets) {
    let now = now_ms();

    // (category, partition, key decoder). Decoders are non-capturing → fn pointers.
    let jobs: &[(&'static str, &str, fn(&[u8]) -> Option<(u64, [u8; 32])>)] = &[
        ("messages", "contextual_message_by_sender", |k| {
            // A message is first stored under a zero sender, then under its resolved one: count
            // the resolved key only.
            ContextualMessageBySenderKey::try_read_from_bytes(k)
                .ok()
                .filter(|x| x.sender != indexer_db::AddressPayload::default())
                .map(|x| (x.block_time.get(), x.tx_id))
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

    let mut windows: Windows = HashMap::new();
    let mut buckets = Buckets::default();
    for (cat, partition, decode) in jobs {
        match scan(tx_keyspace, partition, now, *decode) {
            Ok(scanned) => {
                windows.insert(cat, scanned.windows);
                buckets.hours.insert(cat, scanned.hours);
                buckets.months.insert(cat, scanned.months);
            }
            Err(e) => tracing::warn!("[stats] window scan for {cat} failed: {e}"),
        }
    }
    (windows, buckets)
}

/// Spawn the periodic window scanner (every ~5 min). It writes the cache that `get_stats` reads;
/// the heavy key-walk runs in `spawn_blocking` so it never touches an async worker.
pub fn spawn_window_scanner(tx_keyspace: TxKeyspace) {
    tokio::spawn(async move {
        loop {
            let ks = tx_keyspace.clone();
            match tokio::task::spawn_blocking(move || compute(&ks)).await {
                Ok(scanned) => {
                    if let Ok(mut guard) = cache().write() {
                        *guard = Some(scanned);
                    }
                }
                Err(e) => tracing::warn!("[stats] window scan task error: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::month_start_ms;

    #[test]
    fn month_starts_are_the_first_at_midnight_utc() {
        // 2026-11-25 15:00 UTC -> 2026-11-01; 2024-02-29 -> 2024-02-01; 2026-01-01 stays.
        assert_eq!(month_start_ms(1_795_618_800_000), 1_793_491_200_000);
        assert_eq!(month_start_ms(1_709_164_800_000), 1_706_745_600_000);
        assert_eq!(month_start_ms(1_767_225_600_000), 1_767_225_600_000);
        assert_eq!(month_start_ms(1_767_225_599_999), 1_764_547_200_000);
    }
}

//! Durable retry queue for public-chat broadcasts whose sender is not known yet
//! (kachat-audits IDX-020).
//!
//! `process_broadcast` asks the node who spent input 0 (bcast_sender.rs). When that stays
//! `Unknown` (tx not accepted yet, node restarting, wRPC reconnecting, processor just
//! started), the broadcast is parked in `kachat_bcast_pending` instead of being dropped.
//! One background task retries it with backoff, and only while the node is connected, so an
//! outage does not use up attempts. A broadcast is given up only after at least
//! `RETRY_FOR_MS` (the ingest's 1 h retention) and `MIN_ATTEMPTS` connected lookups; that
//! final drop is logged at WARN with a running count. `Forged` stays final at once.
//!
//! On startup the last hour of `kchat:1:bcast:` / `ciph_msg:1:bcast:` transactions that are
//! neither stored nor pending is swept into the queue, which covers broadcasts that an
//! earlier build dropped or that arrived while the processor was down.

use crate::database::{DbPool, Transaction};
use crate::k_protocol::{BcastOutcome, KProtocolProcessor};
use anyhow::Result;
use sqlx::Row;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Keep retrying for at least this long after a broadcast is first queued (ms).
pub const RETRY_FOR_MS: i64 = 3_600_000;
/// ...and for at least this many connected lookups, so a long outage still gets real tries.
const MIN_ATTEMPTS: i32 = 10;
/// First retry delay; doubles per attempt up to `MAX_BACKOFF_MS`.
const FIRST_BACKOFF_MS: i64 = 5_000;
const MAX_BACKOFF_MS: i64 = 300_000;
/// A retry that saves a broadcast older than this sends no push (it is history by then).
pub const PUSH_MAX_AGE_MS: i64 = 300_000;
/// Rows taken per pass.
const BATCH: i64 = 50;

const PREFIXES: [&[u8]; 2] = [b"kchat:1:bcast:", b"ciph_msg:1:bcast:"];

static FINAL_DROPS: AtomicU64 = AtomicU64::new(0);

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Delay before the next try after `attempts` failed ones.
fn backoff_ms(attempts: i32) -> i64 {
    (FIRST_BACKOFF_MS << attempts.clamp(0, 16)).min(MAX_BACKOFF_MS)
}

/// Give up only when both the time and the attempt floor are reached.
fn exhausted(first_seen: i64, attempts: i32, now: i64) -> bool {
    now - first_seen >= RETRY_FOR_MS && attempts >= MIN_ATTEMPTS
}

/// Log a broadcast that is lost for good (WARN, with the running count since start).
pub fn note_final_drop(transaction_id: &str, why: &str) {
    let n = FINAL_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
    warn!("Broadcast {transaction_id} dropped for good: {why} ({n} final broadcast drop(s) since start)");
}

/// Park a broadcast for a later sender lookup. A tx already queued keeps its schedule.
pub async fn enqueue(
    pool: &DbPool,
    transaction_id: &[u8],
    payload: &[u8],
    block_hash: &[u8],
    block_time: i64,
) -> Result<()> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO kachat_bcast_pending \
             (transaction_id, payload, block_hash, block_time, first_seen, attempts, next_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6) \
         ON CONFLICT (transaction_id) DO NOTHING",
    )
    .bind(transaction_id)
    .bind(payload)
    .bind(block_hash)
    .bind(block_time)
    .bind(now)
    .bind(now + FIRST_BACKOFF_MS)
    .execute(pool)
    .await?;
    Ok(())
}

/// Queue every broadcast of the last hour that is neither stored nor pending. Returns how many.
async fn sweep(pool: &DbPool) -> Result<u64> {
    let now = now_ms();
    let r = sqlx::query(
        "INSERT INTO kachat_bcast_pending \
             (transaction_id, payload, block_hash, block_time, first_seen, attempts, next_at) \
         SELECT t.transaction_id, t.payload, t.block_hash, t.block_time, $1, 0, $1 \
           FROM transactions t \
          WHERE t.block_time >= $2 \
            AND t.payload IS NOT NULL AND t.block_hash IS NOT NULL \
            AND (substring(t.payload FROM 1 FOR length($3)) = $3 \
                 OR substring(t.payload FROM 1 FOR length($4)) = $4) \
            AND NOT EXISTS (SELECT 1 FROM kachat_broadcasts b WHERE b.transaction_id = t.transaction_id) \
         ON CONFLICT (transaction_id) DO NOTHING",
    )
    .bind(now)
    .bind(now - RETRY_FOR_MS)
    .bind(PREFIXES[0])
    .bind(PREFIXES[1])
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// The retry loop. Runs forever; started once from main.
pub async fn run(pool: DbPool) {
    if !crate::bcast_sender::configured() {
        // Nothing can be resolved without a node; process_broadcast drops (and counts) instead.
        return;
    }
    match sweep(&pool).await {
        Ok(0) => {}
        Ok(n) => info!("Broadcast sender retry: queued {n} unstored broadcast(s) from the last hour"),
        Err(e) => error!("Broadcast sender retry: startup sweep failed: {e}"),
    }
    // Let the feature-flag / channel-list refresher (main.rs, every 15 s) load once first, so a
    // retry is judged against the configured channels rather than the defaults.
    tokio::time::sleep(Duration::from_secs(20)).await;

    let processor = KProtocolProcessor::new(pool.clone());
    loop {
        // Wait for the node instead of spending attempts while it is unreachable.
        if !crate::bcast_sender::is_connected() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let rows = match sqlx::query(
            "SELECT transaction_id, payload, block_hash, block_time, first_seen, attempts \
               FROM kachat_bcast_pending WHERE next_at <= $1 ORDER BY next_at LIMIT $2",
        )
        .bind(now_ms())
        .bind(BATCH)
        .fetch_all(&pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!("Broadcast sender retry: reading the queue failed: {e}");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        if rows.is_empty() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        for row in rows {
            let id: Vec<u8> = row.get("transaction_id");
            let payload: Vec<u8> = row.get("payload");
            let block_hash: Vec<u8> = row.get("block_hash");
            let first_seen: i64 = row.get("first_seen");
            let attempts: i32 = row.get("attempts");
            let tx = Transaction {
                transaction_id: hex::encode(&id),
                payload: Some(hex::encode(&payload)),
                block_time: Some(row.get("block_time")),
            };
            let outcome = processor.retry_broadcast(&tx, &block_hash).await;
            if let Err(e) = &outcome {
                debug!("Broadcast {} retry failed: {e}", tx.transaction_id);
            }
            if matches!(outcome, Ok(BcastOutcome::Done)) {
                let _ = sqlx::query("DELETE FROM kachat_bcast_pending WHERE transaction_id = $1")
                    .bind(&id)
                    .execute(&pool)
                    .await;
                continue;
            }
            // Lost the node mid-lookup: not a real attempt; leave the row and wait.
            if !crate::bcast_sender::is_connected() {
                break;
            }
            let attempts = attempts + 1;
            let now = now_ms();
            if exhausted(first_seen, attempts, now) {
                note_final_drop(
                    &tx.transaction_id,
                    &format!("input 0's address not resolved after {attempts} lookups over an hour"),
                );
                let _ = sqlx::query("DELETE FROM kachat_bcast_pending WHERE transaction_id = $1")
                    .bind(&id)
                    .execute(&pool)
                    .await;
            } else {
                let _ = sqlx::query(
                    "UPDATE kachat_bcast_pending SET attempts = $2, next_at = $3 WHERE transaction_id = $1",
                )
                .bind(&id)
                .bind(attempts)
                .bind(now + backoff_ms(attempts))
                .execute(&pool)
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(backoff_ms(0), 5_000);
        assert_eq!(backoff_ms(1), 10_000);
        assert_eq!(backoff_ms(5), 160_000);
        assert_eq!(backoff_ms(6), MAX_BACKOFF_MS);
        assert_eq!(backoff_ms(40), MAX_BACKOFF_MS);
    }

    #[test]
    fn gives_up_only_after_an_hour_and_enough_tries() {
        let t0 = 1_000_000;
        assert!(!exhausted(t0, 50, t0 + RETRY_FOR_MS - 1), "under an hour");
        assert!(!exhausted(t0, MIN_ATTEMPTS - 1, t0 + 10 * RETRY_FOR_MS), "long outage, few tries");
        assert!(exhausted(t0, MIN_ATTEMPTS, t0 + RETRY_FOR_MS));
    }
}

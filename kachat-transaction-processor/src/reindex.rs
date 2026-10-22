//! Recovering KaPosts the processor missed (the 2026-10-20 outage: two days with no posts,
//! replies, votes, follows, profiles or tips indexed).
//!
//! The processor only ever learns about a transaction from the ingester's insert trigger, and the
//! ingester keeps raw transactions for an hour. Two ways back in:
//!
//! - `startup_catch_up`: once at start, every KaChat transaction still in that hour is queued
//!   again, which covers a restart or a short outage without anyone doing anything.
//! - `run`: a reindex on request (`k_vars` key `kaposts_reindex_request`, written by the admin
//!   API from the panel's Indexer tab). It walks every block the node still holds, from its
//!   pruning point to the tip, stores the KaChat transactions it finds that are not indexed yet
//!   in `kachat_recovered_transactions` (its own table: the ingester prunes `transactions` by
//!   block time every hour, so an old row put there would be gone before a worker read it,
//!   kachat-audits IDX-036) and queues them to the workers like the LISTEN channel does. Workers
//!   read the row from that table when `transactions` has none. Progress is in
//!   `kaposts_reindex_status`; a status left at "running" by a restart mid-walk is marked failed
//!   at the next start (IDX-037).
//!
//! Order does not matter: profiles keep the newest by block time, and a follow or block is
//! ignored when a newer opposing action is known (k_protocol.rs, IDX-035). Pushes stay quiet
//! for anything older than a few minutes (worker.rs): a day-old like is history, not news.

use crate::bcast_pending::now_ms;
use crate::database::DbPool;
use anyhow::{anyhow, Result};
use kaspa_rpc_core::api::rpc::RpcApi;
use serde_json::{json, Value};
use sqlx::Row;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub const REQUEST_KEY: &str = "kaposts_reindex_request";
pub const STATUS_KEY: &str = "kaposts_reindex_status";
const PREFIXES: [&[u8]; 2] = [b"kchat:1:", b"ciph_msg:1:bcast:"];
/// How often the request key is looked at.
const POLL: Duration = Duration::from_secs(5);
/// A breath between pages, so a reindex never crowds the node's live callers.
const PAGE_PAUSE: Duration = Duration::from_millis(10);

async fn get_var(pool: &DbPool, key: &str) -> Option<String> {
    sqlx::query("SELECT value FROM k_vars WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get::<String, _>("value"))
}

async fn set_var(pool: &DbPool, key: &str, value: &str) {
    if let Err(e) = sqlx::query(
        "INSERT INTO k_vars (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await
    {
        warn!("reindex: could not write {key}: {e}");
    }
}

async fn del_var(pool: &DbPool, key: &str) {
    let _ = sqlx::query("DELETE FROM k_vars WHERE key = $1").bind(key).execute(pool).await;
}

/// What the panel shows while a reindex runs and after.
#[derive(Default)]
struct Status {
    state: &'static str,
    started_at: i64,
    finished_at: Option<i64>,
    blocks: u64,
    found: u64,
    queued: u64,
    oldest_ms: Option<i64>,
    newest_ms: Option<i64>,
    error: Option<String>,
}

impl Status {
    fn json(&self) -> String {
        json!({
            "state": self.state,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "blocks": self.blocks,
            "found": self.found,
            "queued": self.queued,
            "oldest_ms": self.oldest_ms,
            "newest_ms": self.newest_ms,
            "error": self.error,
        })
        .to_string()
    }
}

fn is_kachat(payload: &[u8]) -> bool {
    PREFIXES.iter().any(|p| payload.starts_with(p))
}

/// A walk that was still "running" when the processor stopped (a restart mid-walk, an
/// update) would otherwise read as running forever, and the admin refuses a new request while
/// one runs (kachat-audits IDX-037).
async fn reset_stale_status(pool: &DbPool) {
    let Some(raw) = get_var(pool, STATUS_KEY).await else { return };
    let Ok(mut v) = serde_json::from_str::<Value>(&raw) else { return };
    if v["state"].as_str() != Some("running") {
        return;
    }
    v["state"] = json!("failed");
    v["error"] = json!("interrupted: the processor restarted during the walk; run it again");
    v["finished_at"] = json!(now_ms());
    set_var(pool, STATUS_KEY, &v.to_string()).await;
    warn!("reindex: a walk was running when the processor last stopped; marked failed");
}

/// Waits for a request, runs it, records how it went; for the life of the processor.
pub async fn run(pool: DbPool, queue: mpsc::UnboundedSender<String>) {
    reset_stale_status(&pool).await;
    loop {
        tokio::time::sleep(POLL).await;
        let Some(req) = get_var(&pool, REQUEST_KEY).await else { continue };
        // Claimed before it runs: a crash half way must not start it again on every boot.
        del_var(&pool, REQUEST_KEY).await;
        let from_ms = serde_json::from_str::<Value>(&req).ok().and_then(|v| v["from_ms"].as_i64());
        let mut st = Status { state: "running", started_at: now_ms(), ..Default::default() };
        set_var(&pool, STATUS_KEY, &st.json()).await;
        info!("reindex: starting a walk of the node's blocks{}", from_ms.map(|f| format!(" from {f}")).unwrap_or_default());
        match walk(&pool, &queue, from_ms, &mut st).await {
            Ok(()) => {
                st.state = "done";
                info!("reindex: done: {} blocks, {} KaChat transactions seen, {} queued for indexing", st.blocks, st.found, st.queued);
            }
            Err(e) => {
                st.state = "failed";
                st.error = Some(format!("{e:#}"));
                warn!("reindex: failed after {} blocks: {e:#}", st.blocks);
            }
        }
        st.finished_at = Some(now_ms());
        set_var(&pool, STATUS_KEY, &st.json()).await;
    }
}

/// Already in one of the processor's tables under this transaction id: indexed, nothing to do.
async fn already_indexed(pool: &DbPool, tx_id: &[u8]) -> Result<bool> {
    let hit: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM k_contents WHERE transaction_id = $1          UNION ALL SELECT 1 FROM k_broadcasts WHERE transaction_id = $1          UNION ALL SELECT 1 FROM k_votes WHERE transaction_id = $1          UNION ALL SELECT 1 FROM k_follows WHERE transaction_id = $1          UNION ALL SELECT 1 FROM k_blocks WHERE transaction_id = $1          UNION ALL SELECT 1 FROM kachat_broadcasts WHERE transaction_id = $1          UNION ALL SELECT 1 FROM kachat_recovered_transactions WHERE transaction_id = $1          LIMIT 1",
    )
    .bind(tx_id)
    .fetch_optional(pool)
    .await?;
    Ok(hit.is_some())
}

/// Pruning point to tip, a page of blocks at a time.
async fn walk(pool: &DbPool, queue: &mpsc::UnboundedSender<String>, from_ms: Option<i64>, st: &mut Status) -> Result<()> {
    let node = crate::bcast_sender::node().ok_or_else(|| anyhow!("no node is configured (KASPA_NODE_WBORSH_URL)"))?;
    if !node.is_connected() {
        return Err(anyhow!("the node is not connected"));
    }
    let dag = node.get_block_dag_info().await.map_err(|e| anyhow!("get_block_dag_info: {e}"))?;
    let mut low = dag.pruning_point_hash;
    let mut pages: u64 = 0;
    loop {
        let page = node.get_blocks(Some(low), true, true).await.map_err(|e| anyhow!("get_blocks: {e}"))?;
        // A page is low itself plus what follows it; low alone means the tip.
        if page.blocks.len() <= 1 {
            break;
        }
        for block in &page.blocks {
            let hash = block.header.hash;
            let at = block.header.timestamp as i64;
            st.blocks += 1;
            st.oldest_ms = Some(st.oldest_ms.map_or(at, |o| o.min(at)));
            st.newest_ms = Some(st.newest_ms.map_or(at, |n| n.max(at)));
            if from_ms.is_some_and(|from| at < from) {
                continue;
            }
            for tx in &block.transactions {
                if !is_kachat(&tx.payload) {
                    continue;
                }
                let Some(v) = &tx.verbose_data else { continue };
                st.found += 1;
                let id = v.transaction_id.as_bytes().to_vec();
                // Edits, deletes, tips, polls and unfollows leave rows the check does not look
                // at (or none at all): those are queued again, which the guards make harmless.
                if already_indexed(pool, &id).await? {
                    continue;
                }
                let r = sqlx::query(
                    "INSERT INTO kachat_recovered_transactions (transaction_id, payload, block_time, block_hash, inserted_at) \
                     VALUES ($1, $2, $3, $4, $5) ON CONFLICT (transaction_id) DO NOTHING",
                )
                .bind(&id)
                .bind(tx.payload.clone())
                .bind(v.block_time as i64)
                .bind(hash.as_bytes().to_vec())
                .bind(now_ms())
                .execute(pool)
                .await?;
                if r.rows_affected() > 0 {
                    st.queued += 1;
                    if queue.send(hex::encode(&id)).is_err() {
                        return Err(anyhow!("the worker queue is closed"));
                    }
                }
            }
        }
        let Some(last) = page.block_hashes.last().copied() else { break };
        if last == low {
            break;
        }
        low = last;
        pages += 1;
        if pages % 50 == 0 {
            set_var(pool, STATUS_KEY, &st.json()).await;
        }
        tokio::time::sleep(PAGE_PAUSE).await;
    }
    Ok(())
}

/// Once at start: the KaChat transactions of the last hour, queued again. The writes they lead
/// to are idempotent, so nothing is doubled; anything already indexed is a quick no-op.
pub async fn startup_catch_up(pool: &DbPool, queue: mpsc::UnboundedSender<String>) {
    let since = now_ms() - 3_600_000;
    let rows = sqlx::query(
        "SELECT transaction_id FROM transactions \
         WHERE block_time > $1 AND payload IS NOT NULL \
           AND (substr(encode(payload, 'hex'), 1, 16) = '6b636861743a313a' \
                OR substr(encode(payload, 'hex'), 1, 34) = '636970685f6d73673a313a62636173743a') \
         ORDER BY block_time",
    )
    .bind(since)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => {
            let n = rows.len();
            for r in rows {
                let id: Vec<u8> = r.get("transaction_id");
                if queue.send(hex::encode(id)).is_err() {
                    break;
                }
            }
            info!("startup catch-up: {n} KaChat transaction(s) from the last hour queued again");
        }
        Err(e) => warn!("startup catch-up: {e}"),
    }
}

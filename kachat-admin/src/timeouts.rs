//! /api/timeouts: addresses put in a 24-hour timeout (docs/ADDRESS_TIMEOUTS.md).
//!
//! While a timeout runs, nothing the address posts publicly is stored: the transaction processor
//! and the profiles follower read `kachat_address_timeouts` with the blocklist. Its private chats
//! are left alone (the chat indexer never sees timeouts). Starting one also deletes what the
//! address posted publicly in the last 24 hours. Removing one ends it early; what was deleted
//! does not come back.

use std::time::Duration;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tracing::{info, warn};

use crate::blocklist::{parse_address, xonly_key};
use crate::{ApiError, AppState, DeleteResult, is_testnet, now_ms};

/// The transaction processor creates the same table (`ADDRESS_TIMEOUTS_TABLE`).
pub const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS kachat_address_timeouts (\
     address TEXT PRIMARY KEY, note TEXT NOT NULL DEFAULT '', started_at BIGINT NOT NULL, \
     expires_at BIGINT NOT NULL, purged BIGINT NOT NULL DEFAULT 0)";

/// How long a timeout lasts, and how far back its purge reaches.
const TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000;

/// The processes reread the list about every 15 s, so what they store in between is purged by a
/// second pass this long after the first.
const SECOND_PASS: Duration = Duration::from_secs(20);

/// How often ended timeouts are cleared.
pub const SWEEP_EVERY: Duration = Duration::from_secs(30);

/// Longest note kept, in characters.
const MAX_NOTE_CHARS: usize = 200;

/// Room channels whose messages are game stamps, counted as `games`.
const GAME_CHANNELS: [&str; 3] = ["chess-arena", "chess-bots", "kplay-arcade"];

#[derive(Serialize)]
struct TimedOut {
    address: String,
    note: String,
    started_at: i64,
    expires_at: i64,
    purged: i64,
}

#[derive(Serialize)]
pub struct TimeoutList {
    /// Running timeouts, newest first.
    timeouts: Vec<TimedOut>,
}

pub async fn get_timeouts(State(state): State<AppState>) -> Result<Json<TimeoutList>, ApiError> {
    let rows = sqlx::query(
        "SELECT address, note, started_at, expires_at, purged FROM kachat_address_timeouts \
         WHERE expires_at > $1 ORDER BY started_at DESC, address",
    )
    .bind(now_ms())
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::db)?;
    Ok(Json(TimeoutList {
        timeouts: rows
            .iter()
            .map(|r| TimedOut {
                address: r.get("address"),
                note: r.get("note"),
                started_at: r.get("started_at"),
                expires_at: r.get("expires_at"),
                purged: r.get("purged"),
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
pub struct TimeoutAdd {
    address: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Serialize, Default, Clone, Copy)]
pub struct Purged {
    /// Posts, replies, quotes, polls, votes, follows, tips, poll votes, edits and profile
    /// broadcasts, in rows.
    kaposts: i64,
    /// Room messages, reactions and edits outside KaPlay's rooms.
    public_chats: i64,
    /// Game stamps in KaPlay's rooms.
    games: i64,
    /// Profile records.
    profiles: i64,
}

impl Purged {
    fn total(&self) -> i64 {
        self.kaposts + self.public_chats + self.games + self.profiles
    }
}

#[derive(Serialize)]
pub struct AddResult {
    address: String,
    started_at: i64,
    expires_at: i64,
    purged: Purged,
    total: i64,
}

/// POST /api/timeouts/add: start a 24-hour timeout, or restart a running one (its 24 hours count
/// from now again), and delete what the address posted publicly in the last 24 hours.
pub async fn post_timeout_add(
    State(state): State<AppState>,
    Json(req): Json<TimeoutAdd>,
) -> Result<Json<AddResult>, ApiError> {
    let addr = parse_address(&req.address, is_testnet())?;
    let address = addr.to_string();
    let note: String = req.note.as_deref().unwrap_or("").trim().chars().take(MAX_NOTE_CHARS).collect();
    let started_at = now_ms();
    let expires_at = started_at + TIMEOUT_MS;
    sqlx::query(
        "INSERT INTO kachat_address_timeouts (address, note, started_at, expires_at) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (address) DO UPDATE SET note = EXCLUDED.note, started_at = EXCLUDED.started_at, \
             expires_at = EXCLUDED.expires_at",
    )
    .bind(&address)
    .bind(&note)
    .bind(started_at)
    .bind(expires_at)
    .execute(&state.pool)
    .await
    .map_err(ApiError::db)?;

    let since = started_at - TIMEOUT_MS;
    let xonly = xonly_key(&addr);
    let purged = purge(&state.pool, &address, xonly.clone(), since).await.map_err(ApiError::db)?;
    let total = purged.total();
    add_purged(&state.pool, &address, total).await.map_err(ApiError::db)?;
    info!("kachat-admin timed out {address} until {expires_at} and purged {total} rows from the last 24 h");

    // Whatever was stored before every process reread the list.
    let pool = state.pool.clone();
    let late = address.clone();
    tokio::spawn(async move {
        tokio::time::sleep(SECOND_PASS).await;
        match purge(&pool, &late, xonly, since).await {
            Ok(more) if more.total() > 0 => {
                info!("kachat-admin timeout of {late}: second pass purged {} rows", more.total());
                if let Err(e) = add_purged(&pool, &late, more.total()).await {
                    warn!("timeouts: could not count the second pass for {late}: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => warn!("timeouts: second pass for {late} failed: {e}"),
        }
    });

    Ok(Json(AddResult { address, started_at, expires_at, purged, total }))
}

async fn add_purged(pool: &PgPool, address: &str, n: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE kachat_address_timeouts SET purged = purged + $2 WHERE address = $1")
        .bind(address)
        .bind(n)
        .execute(pool)
        .await
        .map(|_| ())
}

#[derive(Deserialize)]
pub struct TimeoutRemove {
    address: String,
}

/// POST /api/timeouts/remove: end a timeout early. What was purged does not come back.
pub async fn post_timeout_remove(
    State(state): State<AppState>,
    Json(req): Json<TimeoutRemove>,
) -> Result<Json<DeleteResult>, ApiError> {
    let address = parse_address(&req.address, is_testnet())
        .map(|a| a.to_string())
        .unwrap_or_else(|_| req.address.trim().to_ascii_lowercase());
    let deleted = sqlx::query("DELETE FROM kachat_address_timeouts WHERE address = $1")
        .bind(&address)
        .execute(&state.pool)
        .await
        .map_err(ApiError::db)?
        .rows_affected();
    if deleted > 0 {
        info!("kachat-admin ended the timeout of {address}");
    }
    Ok(Json(DeleteResult { deleted }))
}

/// Clear timeouts that have ended. The processes stop honoring a timeout at `expires_at` on
/// their own; this only keeps the table short.
pub async fn sweep(pool: &PgPool) {
    match sqlx::query("DELETE FROM kachat_address_timeouts WHERE expires_at <= $1").bind(now_ms()).execute(pool).await {
        Ok(done) if done.rows_affected() > 0 => info!("kachat-admin: {} timeout(s) ended", done.rows_affected()),
        Ok(_) => {}
        Err(e) => warn!("timeouts: could not clear ended timeouts: {e}"),
    }
}

/// `col` holds the x-only key $1 in any of its three forms (`02x`, `03x`, bare `x`).
fn kx(col: &str) -> String {
    format!("{col} IN ('\\x02'::bytea || $1, '\\x03'::bytea || $1, $1)")
}

/// Delete what the address posted publicly since `since` (ms, by block time), with tombstones so
/// a reindex never stores it again. One transaction: all or nothing.
///
/// - KaPosts by its key: posts, replies and quotes (with their polls, translations, mentions and
///   hashtags), votes, follows, tips, poll votes and profile broadcasts. Blocks and deletes stay.
/// - An older post it edited in the window gets the text of its last earlier edit back, or is
///   deleted when there is none (an edit overwrites the original text).
/// - Room messages and game stamps by address.
/// - Profile records by address; the shown profile goes back to the last earlier one, if any.
async fn purge(pool: &PgPool, address: &str, xonly: Option<Vec<u8>>, since: i64) -> Result<Purged, sqlx::Error> {
    // An address with no x-only key (a script address) has no KaPosts: match nothing.
    let key = xonly.unwrap_or_default();
    let mut tx = pool.begin().await?;
    let mut purged = Purged::default();
    let now = now_ms();

    let rooms: Vec<String> = sqlx::query_scalar(&tombstoned(
        "DELETE FROM kachat_broadcasts WHERE sender_address = $2 AND block_time >= $3 RETURNING transaction_id, channel",
        "channel",
    ))
    .bind(now)
    .bind(address)
    .bind(since)
    .fetch_all(&mut *tx)
    .await?;
    purged.games = rooms.iter().filter(|c| GAME_CHANNELS.contains(&c.as_str())).count() as i64;
    purged.public_chats = rooms.len() as i64 - purged.games;

    // Posts first: their polls, translations and mentions go with them (hashtags cascade).
    let posts: Vec<Vec<u8>> = sqlx::query_scalar(&tombstoned(
        &format!(
            "DELETE FROM k_contents WHERE {} AND block_time >= $3 RETURNING transaction_id",
            kx("sender_pubkey").replace("$1", "$2")
        ),
        "transaction_id",
    ))
    .bind(now)
    .bind(&key)
    .bind(since)
    .fetch_all(&mut *tx)
    .await?;
    purged.kaposts += posts.len() as i64;
    purged.kaposts += with_posts(&mut tx, &posts).await?;

    // Edits in the window, then the older posts they rewrote.
    let edited: Vec<Vec<u8>> = sqlx::query_scalar(&tombstoned(
        &format!(
            "DELETE FROM k_edits WHERE {} AND block_time >= $3 RETURNING transaction_id, post_id",
            kx("sender_pubkey").replace("$1", "$2")
        ),
        "post_id",
    ))
    .bind(now)
    .bind(&key)
    .bind(since)
    .fetch_all(&mut *tx)
    .await?;
    purged.kaposts += edited.len() as i64;
    if !edited.is_empty() {
        // Back to the last edit left, when there is one.
        sqlx::query(
            "UPDATE k_contents c SET base64_encoded_message = e.base64_encoded_message, \
                 message_text = kachat_b64_utf8(e.base64_encoded_message), edited_at = e.block_time \
             FROM (SELECT DISTINCT ON (post_id) post_id, base64_encoded_message, block_time FROM k_edits \
                   WHERE post_id = ANY($1) ORDER BY post_id, block_time DESC, id DESC) e \
             WHERE c.transaction_id = e.post_id",
        )
        .bind(&edited)
        .execute(&mut *tx)
        .await?;
        // None left: the original text is gone, so the post goes.
        let rewritten: Vec<Vec<u8>> = sqlx::query_scalar(
            "WITH gone AS (DELETE FROM k_contents c WHERE c.transaction_id = ANY($2) \
                 AND NOT EXISTS (SELECT 1 FROM k_edits e WHERE e.post_id = c.transaction_id) \
                 RETURNING transaction_id), \
             tombstones AS (INSERT INTO kachat_removed_txs (transaction_id, removed_at) \
                 SELECT transaction_id, $1 FROM gone ON CONFLICT (transaction_id) DO NOTHING) \
             SELECT transaction_id FROM gone",
        )
        .bind(now)
        .bind(&edited)
        .fetch_all(&mut *tx)
        .await?;
        purged.kaposts += rewritten.len() as i64;
        purged.kaposts += with_posts(&mut tx, &rewritten).await?;
    }

    for (table, col) in [
        ("k_votes", "sender_pubkey"),
        ("k_follows", "sender_pubkey"),
        ("k_tips", "sender_pubkey"),
        ("k_poll_votes", "voter_pubkey"),
        ("k_broadcasts", "sender_pubkey"),
    ] {
        let gone: Vec<Vec<u8>> = sqlx::query_scalar(&tombstoned(
            &format!(
                "DELETE FROM {table} WHERE {} AND block_time >= $3 RETURNING transaction_id",
                kx(col).replace("$1", "$2")
            ),
            "transaction_id",
        ))
        .bind(now)
        .bind(&key)
        .bind(since)
        .fetch_all(&mut *tx)
        .await?;
        purged.kaposts += gone.len() as i64;
    }

    // Profiles: the records, then the profile shown, if it was one of them.
    let saves: Vec<Vec<u8>> =
        sqlx::query_scalar("DELETE FROM profile_saves WHERE address = $1 AND block_time >= $2 RETURNING tx_id")
            .bind(address)
            .bind(since)
            .fetch_all(&mut *tx)
            .await?;
    purged.profiles = saves.len() as i64;
    if !saves.is_empty() {
        sqlx::query(
            "UPDATE names_profiles p SET profile = s.profile, daa = s.daa, tx_id = s.tx_id, updated_at = s.block_time \
             FROM (SELECT DISTINCT ON (spk) spk, profile, daa, tx_id, block_time FROM profile_saves \
                   WHERE address = $1 AND profile IS NOT NULL ORDER BY spk, daa DESC) s \
             WHERE p.spk = s.spk AND p.tx_id = ANY($2)",
        )
        .bind(address)
        .bind(&saves)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM names_profiles WHERE address = $1 AND tx_id = ANY($2)")
            .bind(address)
            .bind(&saves)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(purged)
}

/// The rows keyed by deleted posts: polls, cached translations and mentions. Hashtags cascade.
async fn with_posts(tx: &mut Transaction<'_, Postgres>, posts: &[Vec<u8>]) -> Result<i64, sqlx::Error> {
    if posts.is_empty() {
        return Ok(0);
    }
    let mut n = 0;
    for sql in [
        "DELETE FROM k_polls WHERE post_id = ANY($1)",
        "DELETE FROM post_translations WHERE post_id = ANY($1)",
        "DELETE FROM k_mentions WHERE content_id = ANY($1)",
    ] {
        n += sqlx::query(sql).bind(posts).execute(&mut **tx).await?.rows_affected() as i64;
    }
    Ok(n)
}

/// `delete` (`RETURNING transaction_id, …`, parameters from $2), tombstoning every transaction
/// id it removed with `removed_at` = $1, and selecting `column` of each removed row.
fn tombstoned(delete: &str, column: &str) -> String {
    format!(
        "WITH gone AS ({delete}), \
         tombstones AS (INSERT INTO kachat_removed_txs (transaction_id, removed_at) \
           SELECT transaction_id, $1 FROM gone ON CONFLICT (transaction_id) DO NOTHING) \
         SELECT {column} FROM gone"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_are_tombstoned_and_take_the_key_from_2() {
        let sql = tombstoned(
            &format!("DELETE FROM k_votes WHERE {} AND block_time >= $3 RETURNING transaction_id", kx("sender_pubkey").replace("$1", "$2")),
            "transaction_id",
        );
        assert!(sql.contains("sender_pubkey IN ('\\x02'::bytea || $2, '\\x03'::bytea || $2, $2)"));
        assert!(sql.contains("INSERT INTO kachat_removed_txs (transaction_id, removed_at)"));
        assert!(sql.contains("SELECT transaction_id, $1 FROM gone"));
        assert!(sql.ends_with("SELECT transaction_id FROM gone"));
        assert!(!sql.contains("$4"));
    }
}

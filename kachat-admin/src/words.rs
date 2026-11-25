//! /api/word-filter: words that keep a public-chat message or KaPost from being stored
//! (docs/WORD_FILTER.md).
//!
//! The list lives in `kachat_word_filter`; the transaction processor seeds it once and rereads it
//! about every 15 s, and `feature_word_filter` (/api/settings) switches it. The filter only
//! stops new messages. `purge` finds stored ones the list catches, and removes them on request.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::info;

use crate::word_filter::{WordFilter, clean_entry, room_texts};
use crate::{ApiError, AppState, DeleteResult, now_ms, tombstoned_delete};

pub const CREATE_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS kachat_word_filter (word TEXT PRIMARY KEY, added_at BIGINT NOT NULL DEFAULT 0)";

/// KaPlay's rooms hold game data, not chat: the filter never reads them.
const GAME_CHANNELS: [&str; 3] = ["chess-arena", "chess-bots", "kplay-arcade"];

/// Most entries one add takes.
const MAX_ADD: usize = 1000;

/// Matches listed by a purge preview.
const MAX_SAMPLES: usize = 50;

#[derive(Serialize)]
pub struct WordEntry {
    word: String,
    added_at: i64,
}

#[derive(Serialize)]
pub struct WordList {
    /// `feature_word_filter`.
    enabled: bool,
    /// Alphabetical.
    words: Vec<WordEntry>,
}

async fn list(state: &AppState) -> Result<WordList, ApiError> {
    let words = sqlx::query("SELECT word, added_at FROM kachat_word_filter ORDER BY word")
        .fetch_all(&state.pool)
        .await
        .map_err(ApiError::db)?
        .iter()
        .map(|r| WordEntry { word: r.get("word"), added_at: r.get("added_at") })
        .collect();
    let enabled = crate::kv_get(&state.pool, "feature_word_filter")
        .await
        .map(|v| v.trim().to_lowercase() != "off")
        .unwrap_or(true);
    Ok(WordList { enabled, words })
}

pub async fn get_word_filter(State(state): State<AppState>) -> Result<Json<WordList>, ApiError> {
    Ok(Json(list(&state).await?))
}

#[derive(Deserialize)]
pub struct WordAdd {
    /// One or more entries, one per line or comma-separated. `*` at the end = any ending; several
    /// words = a phrase.
    words: String,
}

#[derive(Serialize)]
pub struct WordAddResult {
    /// Entries new to the list.
    added: u64,
    /// Entries that can't be one: no letters or digits, or longer than 64 characters.
    invalid: Vec<String>,
}

pub async fn post_word_filter_add(
    State(state): State<AppState>,
    Json(req): Json<WordAdd>,
) -> Result<Json<WordAddResult>, ApiError> {
    let raw: Vec<&str> = req.words.split(['\n', '\r', ',']).map(str::trim).filter(|w| !w.is_empty()).collect();
    if raw.len() > MAX_ADD {
        return Err(ApiError::bad_request(&format!("At most {MAX_ADD} entries at a time")));
    }
    let (good, invalid): (Vec<_>, Vec<_>) = raw.iter().map(|w| (*w, clean_entry(w))).partition(|(_, c)| c.is_some());
    let words: Vec<String> = good.into_iter().filter_map(|(_, c)| c).collect();
    let added = sqlx::query(
        "INSERT INTO kachat_word_filter (word, added_at) SELECT unnest($1::TEXT[]), $2 ON CONFLICT (word) DO NOTHING",
    )
    .bind(&words)
    .bind(now_ms())
    .execute(&state.pool)
    .await
    .map_err(ApiError::db)?
    .rows_affected();
    if added > 0 {
        info!("kachat-admin added {added} word filter entr(ies)");
    }
    Ok(Json(WordAddResult { added, invalid: invalid.into_iter().map(|(w, _)| w.to_string()).collect() }))
}

#[derive(Deserialize)]
pub struct WordRemove {
    word: String,
}

pub async fn post_word_filter_remove(
    State(state): State<AppState>,
    Json(req): Json<WordRemove>,
) -> Result<Json<DeleteResult>, ApiError> {
    let word = clean_entry(&req.word).unwrap_or_else(|| req.word.trim().to_lowercase());
    let deleted = sqlx::query("DELETE FROM kachat_word_filter WHERE word = $1")
        .bind(&word)
        .execute(&state.pool)
        .await
        .map_err(ApiError::db)?
        .rows_affected();
    if deleted > 0 {
        info!("kachat-admin removed word filter entry {word:?}");
    }
    Ok(Json(DeleteResult { deleted }))
}

#[derive(Deserialize)]
pub struct PurgeRequest {
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize)]
pub struct PurgeMatch {
    tx_id: String,
    /// `public_chat` or `kapost`.
    kind: &'static str,
    /// The room, for a public chat.
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<String>,
    /// The list entry it contains (the first one found).
    word: String,
    preview: String,
    timestamp: i64,
}

#[derive(Serialize)]
pub struct PurgeResult {
    dry_run: bool,
    /// Room messages (KaPlay's rooms aside) the list catches: found, or removed.
    public_chats: u64,
    /// KaPosts (posts, replies, quotes, polls, profile broadcasts) the list catches.
    kaposts: u64,
    total: u64,
    /// Up to 50 of them, newest first.
    matches: Vec<PurgeMatch>,
}

/// POST /api/word-filter/purge: the stored messages the list catches now (`dry_run`), or remove
/// them, with tombstones so they are never stored again. Works whether the filter is switched on
/// or not.
pub async fn post_word_filter_purge(
    State(state): State<AppState>,
    Json(req): Json<PurgeRequest>,
) -> Result<Json<PurgeResult>, ApiError> {
    let words: Vec<String> = sqlx::query_scalar("SELECT word FROM kachat_word_filter")
        .fetch_all(&state.pool)
        .await
        .map_err(ApiError::db)?;
    let filter = WordFilter::new(words);
    let mut matches = Vec::new();

    let rooms = sqlx::query(
        "SELECT transaction_id, channel, content, block_time FROM kachat_broadcasts \
         WHERE channel <> ALL($1) ORDER BY block_time DESC, id DESC",
    )
    .bind(&GAME_CHANNELS[..])
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::db)?;
    let mut room_txs: Vec<Vec<u8>> = Vec::new();
    for r in &rooms {
        let content: String = r.get("content");
        if let Some(word) = filter.first_match(&room_texts(&content)) {
            let tx: Vec<u8> = r.get("transaction_id");
            matches.push(PurgeMatch {
                tx_id: hex::encode(&tx),
                kind: "public_chat",
                channel: Some(r.get("channel")),
                word: word.to_string(),
                preview: content.chars().take(200).collect(),
                timestamp: r.get("block_time"),
            });
            room_txs.push(tx);
        }
    }

    // A post's current text (an edit rewrites `message_text`), a poll's options, a profile
    // broadcast's nickname and message.
    let posts = sqlx::query(
        "SELECT c.transaction_id, coalesce(c.message_text, '') AS text, coalesce(p.options, '') AS options, \
                c.block_time, 'c' AS src \
         FROM k_contents c LEFT JOIN k_polls p ON p.post_id = c.transaction_id \
         UNION ALL \
         SELECT transaction_id, coalesce(nickname_text, '') || ' ' || coalesce(kachat_b64_utf8(base64_encoded_message), ''), \
                '', block_time, 'b' FROM k_broadcasts",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::db)?;
    let mut content_txs: Vec<Vec<u8>> = Vec::new();
    let mut profile_txs: Vec<Vec<u8>> = Vec::new();
    for r in &posts {
        let text: String = r.get("text");
        let options: String = r.get("options");
        let mut texts = vec![text.clone()];
        // Poll options are stored as base64.
        texts.extend(options.split(',').filter(|o| !o.is_empty()).filter_map(decode_b64));
        if let Some(word) = filter.first_match(&texts) {
            let tx: Vec<u8> = r.get("transaction_id");
            matches.push(PurgeMatch {
                tx_id: hex::encode(&tx),
                kind: "kapost",
                channel: None,
                word: word.to_string(),
                preview: text.trim().chars().take(200).collect(),
                timestamp: r.get("block_time"),
            });
            if r.get::<String, _>("src") == "c" { content_txs.push(tx) } else { profile_txs.push(tx) }
        }
    }

    let public_chats = room_txs.len() as u64;
    let kaposts = (content_txs.len() + profile_txs.len()) as u64;
    if !req.dry_run && public_chats + kaposts > 0 {
        let now = now_ms();
        for (table, txs) in [("kachat_broadcasts", &room_txs), ("k_contents", &content_txs), ("k_broadcasts", &profile_txs)] {
            if txs.is_empty() {
                continue;
            }
            sqlx::query_scalar::<_, i64>(&tombstoned_delete(&format!(
                "DELETE FROM {table} WHERE transaction_id = ANY($2::BYTEA[]) RETURNING transaction_id"
            )))
            .bind(now)
            .bind(txs)
            .fetch_one(&state.pool)
            .await
            .map_err(ApiError::db)?;
        }
        info!("kachat-admin word filter purge removed {public_chats} room message(s) and {kaposts} KaPost(s)");
    }

    matches.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    matches.truncate(MAX_SAMPLES);
    Ok(Json(PurgeResult { dry_run: req.dry_run, public_chats, kaposts, total: public_chats + kaposts, matches }))
}

fn decode_b64(b64: &str) -> Option<String> {
    use base64::{Engine as _, engine::general_purpose};
    general_purpose::STANDARD.decode(b64.trim()).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

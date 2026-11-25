//! /api/blocklist: addresses nothing is stored from (the panel's Indexer > Manage > Block).
//!
//! The list lives in `kachat_address_blocklist`. Each process that stores data rereads it about
//! every 15 s: the transaction processor and the profiles follower from that table, the chat
//! indexer from `blocklist_file`, which is rewritten here on every change and at start. Adding an
//! address also purges what is already stored from it; removing one only stops blocking.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tracing::{error, info, warn};

use crate::{ApiError, AppState, DeleteResult, is_testnet, now_ms, with_internal_secret};

pub const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS kachat_address_blocklist (\
     address TEXT PRIMARY KEY, note TEXT NOT NULL DEFAULT '', \
     added_at BIGINT NOT NULL DEFAULT 0, purged BIGINT NOT NULL DEFAULT 0)";

/// Longest note kept, in characters.
const MAX_NOTE_CHARS: usize = 200;

/// Room channels whose messages are game stamps (chess and arcade boards), counted as `games`.
const GAME_CHANNELS: [&str; 3] = ["chess-arena", "chess-bots", "kplay-arcade"];

/// The canonical form of `input` when it is an address of this indexer's network.
fn parse_address(input: &str, testnet: bool) -> Result<kaspa_addresses::Address, ApiError> {
    let addr = kaspa_addresses::Address::try_from(input.trim().to_ascii_lowercase())
        .map_err(|_| ApiError::bad_request("Not a valid Kaspa address"))?;
    let want = if testnet { kaspa_addresses::Prefix::Testnet } else { kaspa_addresses::Prefix::Mainnet };
    if addr.prefix != want {
        return Err(ApiError::bad_request(if testnet {
            "This indexer is on testnet: use a kaspatest: address"
        } else {
            "This indexer is on mainnet: use a kaspa: address"
        }));
    }
    Ok(addr)
}

/// The x-only key KaPosts records an address's posts under: a Schnorr address's 32-byte
/// payload, or an ECDSA address's compressed key (`02/03 || x`) without its first byte.
fn xonly_key(addr: &kaspa_addresses::Address) -> Option<Vec<u8>> {
    match addr.payload.len() {
        32 => Some(addr.payload.to_vec()),
        33 => Some(addr.payload[1..].to_vec()),
        _ => None,
    }
}

/// Rewrite the chat indexer's copy of the list (one address per line). Written to a temporary
/// file and renamed, so the indexer never reads half a list.
pub async fn write_file(pool: &PgPool, path: &str) -> anyhow::Result<()> {
    // One writer at a time, so two changes can't interleave their temporary files.
    static WRITING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _writing = WRITING.lock().await;
    let rows: Vec<String> = sqlx::query_scalar("SELECT address FROM kachat_address_blocklist ORDER BY address")
        .fetch_all(pool)
        .await?;
    let mut text = rows.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    let tmp = format!("{path}.{}.tmp", std::process::id());
    tokio::fs::write(&tmp, text).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[derive(Serialize)]
struct BlockedAddress {
    address: String,
    note: String,
    added_at: i64,
    purged: i64,
}

#[derive(Serialize)]
pub struct BlocklistView {
    addresses: Vec<BlockedAddress>,
}

pub async fn get_blocklist(State(state): State<AppState>) -> Result<Json<BlocklistView>, ApiError> {
    let rows = sqlx::query(
        "SELECT address, note, added_at, purged FROM kachat_address_blocklist ORDER BY added_at DESC, address",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::db)?;
    Ok(Json(BlocklistView {
        addresses: rows
            .iter()
            .map(|r| BlockedAddress {
                address: r.get("address"),
                note: r.get("note"),
                added_at: r.get("added_at"),
                purged: r.get("purged"),
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
pub struct BlocklistAdd {
    address: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Serialize, Default)]
struct Purged {
    kaposts: i64,
    public_chats: i64,
    chats: i64,
    groups: i64,
    profiles: i64,
    games: i64,
}

impl Purged {
    fn total(&self) -> i64 {
        self.kaposts + self.public_chats + self.chats + self.groups + self.profiles + self.games
    }
}

#[derive(Serialize)]
pub struct AddResult {
    address: String,
    purged: Purged,
    total: i64,
    /// Set when part of the purge could not run (the chat indexer not answering): the address
    /// is listed and blocked all the same, and adding it again retries the purge.
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

/// POST /api/blocklist/add: list the address (or update its note), then purge what is stored
/// from it. Adding it again purges again, which also clears anything stored in the seconds
/// before every process reread the list.
pub async fn post_blocklist_add(
    State(state): State<AppState>,
    Json(req): Json<BlocklistAdd>,
) -> Result<Json<AddResult>, ApiError> {
    let addr = parse_address(&req.address, is_testnet())?;
    let address = addr.to_string();
    let note: String = req.note.as_deref().unwrap_or("").trim().chars().take(MAX_NOTE_CHARS).collect();
    sqlx::query(
        "INSERT INTO kachat_address_blocklist (address, note, added_at) VALUES ($1, $2, $3) \
         ON CONFLICT (address) DO UPDATE SET note = EXCLUDED.note",
    )
    .bind(&address)
    .bind(&note)
    .bind(now_ms())
    .execute(&state.pool)
    .await
    .map_err(ApiError::db)?;
    // The chat indexer stops storing from it before its old messages go.
    if let Err(e) = write_file(&state.pool, &state.blocklist_file).await {
        return Err(ApiError::server(&format!("could not write {}: {e}", state.blocklist_file)));
    }

    let mut purged = purge_postgres(&state.pool, &address, xonly_key(&addr)).await.map_err(ApiError::db)?;
    let warning = match purge_chats(&state, &address).await {
        Ok((chats, groups)) => {
            purged.chats = chats;
            purged.groups = groups;
            None
        }
        Err(e) => {
            warn!("blocklist: chat purge for {address} failed: {e}");
            Some(format!("chat messages not purged yet: {e}"))
        }
    };
    let total = purged.total();
    sqlx::query("UPDATE kachat_address_blocklist SET purged = purged + $2 WHERE address = $1")
        .bind(&address)
        .bind(total)
        .execute(&state.pool)
        .await
        .map_err(ApiError::db)?;
    info!("kachat-admin blocked {address} and purged {total} stored rows");
    Ok(Json(AddResult { address, purged, total, warning }))
}

/// Everything in Postgres sent from the address, in one statement (all or nothing): KaPosts by
/// its key, room messages and game stamps by address, its profile.
async fn purge_postgres(pool: &PgPool, address: &str, xonly: Option<Vec<u8>>) -> Result<Purged, sqlx::Error> {
    // `kx(col)`: the key in any of its three valid forms (`02x`, `03x`, bare `x`), as an `IN`
    // list so each table's sender index can be used.
    let kx = |col: &str| format!("{col} IN ('\\x02'::bytea || $1, '\\x03'::bytea || $1, $1)");
    let kaposts_tables = [
        ("k_contents", "sender_pubkey"),
        ("k_mentions", "sender_pubkey"),
        ("k_votes", "sender_pubkey"),
        ("k_blocks", "sender_pubkey"),
        ("k_follows", "sender_pubkey"),
        ("k_tips", "sender_pubkey"),
        ("k_edits", "sender_pubkey"),
        ("k_deletes", "sender_pubkey"),
        ("k_hashtags", "sender_pubkey"),
        ("k_poll_votes", "voter_pubkey"),
        ("k_broadcasts", "sender_pubkey"),
        // Its queued posts, so the scheduler doesn't relay them later.
        ("k_scheduled_posts", "pubkey"),
    ];
    let deletes: Vec<String> = kaposts_tables
        .iter()
        .enumerate()
        .map(|(i, (table, col))| format!("d{i} AS (DELETE FROM {table} WHERE {} RETURNING 1)", kx(col)))
        .collect();
    let counts: Vec<String> = (0..kaposts_tables.len()).map(|i| format!("(SELECT COUNT(*) FROM d{i})")).collect();
    let sql = format!(
        "WITH {}, \
         rooms AS (DELETE FROM kachat_broadcasts WHERE sender_address = $2 RETURNING channel), \
         saves AS (DELETE FROM profile_saves WHERE address = $2 RETURNING 1), \
         profiles AS (DELETE FROM names_profiles WHERE address = $2 RETURNING 1) \
         SELECT {} AS kaposts, \
                (SELECT COUNT(*) FROM rooms WHERE channel <> ALL($3)) AS public_chats, \
                (SELECT COUNT(*) FROM rooms WHERE channel = ANY($3)) AS games, \
                (SELECT COUNT(*) FROM saves) + (SELECT COUNT(*) FROM profiles) AS profiles",
        deletes.join(", "),
        counts.join(" + "),
    );
    // An address with no x-only key (a script address) has no KaPosts: match nothing.
    let row = sqlx::query(&sql)
        .bind(xonly.unwrap_or_default())
        .bind(address)
        .bind(GAME_CHANNELS.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .fetch_one(pool)
        .await?;
    Ok(Purged {
        kaposts: row.get::<i64, _>("kaposts"),
        public_chats: row.get::<i64, _>("public_chats"),
        games: row.get::<i64, _>("games"),
        profiles: row.get::<i64, _>("profiles"),
        ..Purged::default()
    })
}

#[derive(Deserialize)]
struct ChatPurgeCounts {
    #[serde(default)]
    chats: i64,
    #[serde(default)]
    groups: i64,
}

/// The chat indexer's purge of everything the address sent: (chats, groups) removed.
async fn purge_chats(state: &AppState, address: &str) -> anyhow::Result<(i64, i64)> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(600)).build()?;
    let resp = with_internal_secret(client.post(&state.chat_purge_sender_url))
        .json(&serde_json::json!({ "address": address }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body: String = resp.text().await.unwrap_or_default().trim().chars().take(200).collect();
        error!("chat indexer purge-sender answered {status}: {body}");
        anyhow::bail!("chat indexer answered {}", status.as_u16());
    }
    let counts: ChatPurgeCounts = resp.json().await?;
    Ok((counts.chats, counts.groups))
}

#[derive(Deserialize)]
pub struct BlocklistRemove {
    address: String,
}

/// POST /api/blocklist/remove: stop blocking. What was purged does not come back.
pub async fn post_blocklist_remove(
    State(state): State<AppState>,
    Json(req): Json<BlocklistRemove>,
) -> Result<Json<DeleteResult>, ApiError> {
    // An entry that no longer parses can still be removed by its stored text.
    let address = parse_address(&req.address, is_testnet())
        .map(|a| a.to_string())
        .unwrap_or_else(|_| req.address.trim().to_ascii_lowercase());
    let deleted = sqlx::query("DELETE FROM kachat_address_blocklist WHERE address = $1")
        .bind(&address)
        .execute(&state.pool)
        .await
        .map_err(ApiError::db)?
        .rows_affected();
    if let Err(e) = write_file(&state.pool, &state.blocklist_file).await {
        return Err(ApiError::server(&format!("could not write {}: {e}", state.blocklist_file)));
    }
    if deleted > 0 {
        info!("kachat-admin unblocked {address}");
    }
    Ok(Json(DeleteResult { deleted }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_checked_against_the_network() {
        let main = "kaspa:qq77ha90czy6yxxjay5jj2htwgtkftgmxvl3e2m0mj4em585900cycedvprs9";
        let Ok(addr) = parse_address(&format!("  {}  ", main.to_uppercase()), false) else {
            panic!("a mainnet address, in any case, is accepted");
        };
        assert_eq!(addr.to_string(), main);
        assert!(parse_address(main, true).is_err());
        assert!(parse_address("kaspa:nope", false).is_err());
        // A Schnorr address carries its x-only key.
        assert_eq!(xonly_key(&addr).map(|k| k.len()), Some(32));
    }
}

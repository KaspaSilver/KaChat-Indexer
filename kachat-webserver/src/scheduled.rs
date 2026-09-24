//! §5.10 Scheduled posts. The indexer holds a phone-signed transaction and broadcasts it at the
//! chosen time. It stores BYTES only — never a private key, never funds. The transaction is a
//! self-send the user already signed; this service just forwards it at `notBefore`. Submission is
//! relayed to the chat indexer's `/internal/push/submit-tx` (the only service on a node-compatible
//! wRPC version). See the KaPosts handoff §5.10.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::models::ApiError;
use crate::web_server::AppState;

/// Farthest ahead a post may be scheduled (§5.10): 30 days.
const MAX_SCHEDULE_AHEAD_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// How many due posts to submit per scheduler tick.
const TICK_BATCH: i64 = 50;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Verify a schnorr signature over Kaspa's PersonalMessageSigningHash — the exact path the
/// processor uses (no reimplemented crypto). `pubkey_hex` may be 66-hex compressed or 64-hex x-only.
fn verify_kaspa_signature(message: &str, signature_hex: &str, pubkey_hex: &str) -> bool {
    use kaspa_wallet_core::message::{verify_message, PersonalMessage};
    use secp256k1::XOnlyPublicKey;
    let Ok(sig) = hex::decode(signature_hex.trim()) else {
        return false;
    };
    if sig.len() != 64 {
        return false;
    }
    let Ok(pk) = hex::decode(pubkey_hex.trim()) else {
        return false;
    };
    let xonly = if pk.len() == 33 {
        pk[1..].to_vec()
    } else if pk.len() == 32 {
        pk
    } else {
        return false;
    };
    let Ok(pubkey) = XOnlyPublicKey::from_slice(&xonly) else {
        return false;
    };
    verify_message(&PersonalMessage(message), &sig, &pubkey).is_ok()
}

/// The inner RpcTransaction object of a stored request (accepts both `{transaction:{…}}` and a
/// bare `{…}`), so we relay `{transaction: <inner>}` regardless of how the phone nested it.
fn inner_tx(transaction: &serde_json::Value) -> serde_json::Value {
    transaction
        .get("transaction")
        .cloned()
        .unwrap_or_else(|| transaction.clone())
}

/// Decode the transaction's `payload` (hex) to its UTF-8 kchat action string, if present.
fn payload_string(transaction: &serde_json::Value) -> Option<String> {
    let payload_hex = inner_tx(transaction)
        .get("payload")
        .and_then(|v| v.as_str())?
        .to_string();
    let bytes = hex::decode(payload_hex).ok()?;
    String::from_utf8(bytes).ok()
}

/// The author pubkey embedded in a `kchat:1:<action>:<pubkey>:…` payload.
fn payload_pubkey(payload: &str) -> Option<String> {
    let body = payload.strip_prefix("kchat:1:")?;
    body.split(':').nth(1).map(|s| s.to_string())
}

/// Best-effort base64 message extracted from a kchat action, for the list preview (§5.10).
fn payload_preview(payload: &str) -> Option<String> {
    let body = payload.strip_prefix("kchat:1:")?;
    let parts: Vec<&str> = body.split(':').collect();
    let idx = match *parts.first()? {
        "post" | "quote" | "poll" => 3,
        "reply" | "edit" => 4,
        _ => return None,
    };
    parts.get(idx).map(|s| s.to_string())
}

fn internal_base() -> String {
    std::env::var("PUSH_INTERNAL_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8600/internal/push".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn internal_secret() -> Option<String> {
    std::env::var("INTERNAL_PUSH_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn bad_request(msg: &str) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: msg.to_string(),
            code: "BAD_REQUEST".to_string(),
        }),
    )
}

// ------------------------------------------------------------------ endpoints ---

#[derive(Deserialize)]
pub struct SchedulePostRequest {
    pub pubkey: String,
    #[serde(rename = "txId")]
    pub tx_id: String,
    #[serde(rename = "notBefore")]
    pub not_before: i64,
    pub signature: String,
    pub transaction: serde_json::Value,
}

#[derive(Serialize)]
pub struct ScheduleAck {
    #[serde(rename = "txId")]
    pub tx_id: String,
    #[serde(rename = "notBefore")]
    pub not_before: i64,
    pub status: String,
}

/// POST /schedule-post — validate + store a phone-signed transaction for later submission.
pub async fn handle_schedule_post(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SchedulePostRequest>,
) -> Result<Json<ScheduleAck>, (StatusCode, Json<ApiError>)> {
    let tx_id = req.tx_id.trim().to_lowercase();
    let pubkey = req.pubkey.trim().to_lowercase();
    if tx_id.is_empty() || pubkey.is_empty() {
        return Err(bad_request("missing txId or pubkey"));
    }

    // 1) The schedule request itself must be signed by pubkey (anti-spam / ownership).
    let signing_string = format!("schedule:{}:{}", tx_id, req.not_before);
    if !verify_kaspa_signature(&signing_string, &req.signature, &pubkey) {
        return Err(bad_request("invalid schedule signature"));
    }

    // 2) notBefore must be in the future and within 30 days.
    let now = now_ms();
    if req.not_before <= now || req.not_before > now + MAX_SCHEDULE_AHEAD_MS {
        return Err(bad_request("notBefore must be within (now, now+30d]"));
    }

    // 3) The transaction's payload must be a kchat:1: action by the same pubkey.
    let payload = payload_string(&req.transaction)
        .ok_or_else(|| bad_request("transaction has no decodable payload"))?;
    if !payload.starts_with("kchat:1:") {
        return Err(bad_request("transaction payload is not a kchat:1: action"));
    }
    match payload_pubkey(&payload) {
        Some(pk) if pk.eq_ignore_ascii_case(&pubkey) => {}
        _ => return Err(bad_request("transaction payload pubkey does not match")),
    }

    let tx_id_bytes = hex::decode(&tx_id).map_err(|_| bad_request("txId is not hex"))?;
    let pubkey_bytes = hex::decode(&pubkey).map_err(|_| bad_request("pubkey is not hex"))?;
    let transaction_json = req.transaction.to_string();
    let preview = payload_preview(&payload);

    // Idempotent on txId.
    let res = sqlx::query(
        r#"
        INSERT INTO k_scheduled_posts
            (tx_id, pubkey, not_before, transaction_json, post_content, status, created_at)
        VALUES ($1, $2, $3, $4, $5, 'scheduled', $6)
        ON CONFLICT (tx_id) DO NOTHING
        "#,
    )
    .bind(&tx_id_bytes)
    .bind(&pubkey_bytes)
    .bind(req.not_before)
    .bind(&transaction_json)
    .bind(&preview)
    .bind(now)
    .execute(&state.scheduled_pool)
    .await;

    if let Err(e) = res {
        tracing::warn!("schedule-post insert failed: {e}");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "storage error".to_string(),
                code: "INTERNAL_ERROR".to_string(),
            }),
        ));
    }

    Ok(Json(ScheduleAck {
        tx_id,
        not_before: req.not_before,
        status: "scheduled".to_string(),
    }))
}

#[derive(Deserialize)]
pub struct ScheduledListQuery {
    pub pubkey: Option<String>,
}

#[derive(Serialize)]
struct ScheduledItem {
    #[serde(rename = "txId")]
    tx_id: String,
    #[serde(rename = "notBefore")]
    not_before: i64,
    status: String,
    #[serde(rename = "submittedAt", skip_serializing_if = "Option::is_none")]
    submitted_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(rename = "postContent", skip_serializing_if = "Option::is_none")]
    post_content: Option<String>,
}

#[derive(Serialize)]
pub struct ScheduledListResponse {
    posts: Vec<ScheduledItem>,
}

/// GET /scheduled-posts?pubkey= — the owner's scheduled posts. Never serves the transaction bytes.
pub async fn handle_scheduled_posts(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ScheduledListQuery>,
) -> Result<Json<ScheduledListResponse>, (StatusCode, Json<ApiError>)> {
    let pubkey = q.pubkey.unwrap_or_default().trim().to_lowercase();
    if pubkey.is_empty() {
        return Err(bad_request("missing pubkey"));
    }
    let pubkey_bytes = hex::decode(&pubkey).map_err(|_| bad_request("pubkey is not hex"))?;
    let rows = sqlx::query(
        r#"
        SELECT encode(tx_id, 'hex') as tid, not_before, status, submitted_at, error, post_content
        FROM k_scheduled_posts WHERE pubkey = $1 ORDER BY not_before DESC LIMIT 500
        "#,
    )
    .bind(&pubkey_bytes)
    .fetch_all(&state.scheduled_pool)
    .await
    .map_err(|e| {
        tracing::warn!("scheduled-posts list failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "storage error".to_string(),
                code: "INTERNAL_ERROR".to_string(),
            }),
        )
    })?;

    let posts = rows
        .into_iter()
        .map(|r| ScheduledItem {
            tx_id: r.get("tid"),
            not_before: r.get("not_before"),
            status: r.get("status"),
            submitted_at: r.get("submitted_at"),
            error: r.get("error"),
            post_content: r.get("post_content"),
        })
        .collect();
    Ok(Json(ScheduledListResponse { posts }))
}

#[derive(Deserialize)]
pub struct CancelRequest {
    pub pubkey: String,
    #[serde(rename = "txId")]
    pub tx_id: String,
    pub signature: String,
}

#[derive(Serialize)]
pub struct CancelAck {
    #[serde(rename = "txId")]
    tx_id: String,
    status: String,
}

/// POST /cancel-scheduled-post — drop a still-`scheduled` entry. A submitted one can't be cancelled.
pub async fn handle_cancel_scheduled_post(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<CancelAck>, (StatusCode, Json<ApiError>)> {
    let tx_id = req.tx_id.trim().to_lowercase();
    let pubkey = req.pubkey.trim().to_lowercase();
    let signing_string = format!("cancel-schedule:{}", tx_id);
    if !verify_kaspa_signature(&signing_string, &req.signature, &pubkey) {
        return Err(bad_request("invalid cancel signature"));
    }
    let tx_id_bytes = hex::decode(&tx_id).map_err(|_| bad_request("txId is not hex"))?;
    let pubkey_bytes = hex::decode(&pubkey).map_err(|_| bad_request("pubkey is not hex"))?;
    let res = sqlx::query(
        "UPDATE k_scheduled_posts SET status='cancelled' \
         WHERE tx_id=$1 AND pubkey=$2 AND status='scheduled'",
    )
    .bind(&tx_id_bytes)
    .bind(&pubkey_bytes)
    .execute(&state.scheduled_pool)
    .await
    .map_err(|e| {
        tracing::warn!("cancel-scheduled-post failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "storage error".to_string(),
                code: "INTERNAL_ERROR".to_string(),
            }),
        )
    })?;
    if res.rows_affected() == 0 {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: "no cancellable scheduled post with that txId".to_string(),
                code: "NOT_FOUND".to_string(),
            }),
        ));
    }
    Ok(Json(CancelAck {
        tx_id,
        status: "cancelled".to_string(),
    }))
}

// ------------------------------------------------------------------ scheduler ---

/// Start the per-minute scheduler that broadcasts due scheduled posts. Idempotent submission is
/// guarded by a status transition (only `scheduled` rows are picked up, and each is flipped to
/// `submitted`/`failed` after the attempt). A transient relay/network error leaves the row
/// `scheduled` so the next tick retries; a node rejection is terminal (`failed`, no retry) per §5.10.
pub fn spawn_scheduler(state: Arc<AppState>) {
    tokio::spawn(async move {
        // Small initial delay so the API + node connection settle after startup.
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            if let Err(e) = tick(&state).await {
                tracing::warn!("[scheduled] tick error: {e}");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

async fn tick(state: &Arc<AppState>) -> anyhow::Result<()> {
    let now = now_ms();
    let due = sqlx::query(
        r#"
        SELECT encode(tx_id, 'hex') as tid, transaction_json
        FROM k_scheduled_posts
        WHERE status = 'scheduled' AND not_before <= $1
        ORDER BY not_before ASC
        LIMIT $2
        "#,
    )
    .bind(now)
    .bind(TICK_BATCH)
    .fetch_all(&state.scheduled_pool)
    .await?;

    if due.is_empty() {
        return Ok(());
    }
    let base = internal_base();
    let secret = internal_secret();
    let submit_url = format!("{base}/submit-tx");

    for row in due {
        let tid: String = row.get("tid");
        let tx_json_str: String = row.get("transaction_json");
        let transaction: serde_json::Value = match serde_json::from_str(&tx_json_str) {
            Ok(v) => v,
            Err(e) => {
                mark_failed(state, &tid, &format!("stored transaction not JSON: {e}")).await;
                continue;
            }
        };
        let body = serde_json::json!({ "transaction": inner_tx(&transaction) });
        let mut rb = state.http.post(&submit_url).json(&body);
        if let Some(s) = &secret {
            rb = rb.header("x-internal-secret", s);
        }
        match rb.send().await {
            Ok(resp) if resp.status().is_success() => {
                let _ = sqlx::query(
                    "UPDATE k_scheduled_posts SET status='submitted', submitted_at=$2, error=NULL \
                     WHERE tx_id=$1 AND status='scheduled'",
                )
                .bind(hex::decode(&tid).unwrap_or_default())
                .bind(now_ms())
                .execute(&state.scheduled_pool)
                .await;
                tracing::info!("[scheduled] submitted {tid}");
            }
            Ok(resp) => {
                // Node rejected it (e.g. inputs already spent) — terminal, no retry (§5.10).
                let msg = resp.text().await.unwrap_or_default();
                mark_failed(state, &tid, &format!("submit rejected: {msg}")).await;
            }
            Err(e) => {
                // Relay unreachable — transient; leave 'scheduled' so the next tick retries.
                tracing::warn!("[scheduled] relay unreachable for {tid}, will retry: {e}");
            }
        }
    }
    Ok(())
}

async fn mark_failed(state: &Arc<AppState>, tid_hex: &str, error: &str) {
    let truncated: String = error.chars().take(400).collect();
    let _ = sqlx::query(
        "UPDATE k_scheduled_posts SET status='failed', error=$2 WHERE tx_id=$1 AND status='scheduled'",
    )
    .bind(hex::decode(tid_hex).unwrap_or_default())
    .bind(truncated)
    .execute(&state.scheduled_pool)
    .await;
    tracing::info!("[scheduled] failed {tid_hex}: {error}");
}

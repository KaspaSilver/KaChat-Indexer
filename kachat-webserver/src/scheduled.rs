//! §5.10 Scheduled posts. The indexer holds a phone-signed transaction and broadcasts it at the
//! chosen time. It stores BYTES only — never a private key, never funds. The transaction is a
//! self-send the user already signed; this service just forwards it at `notBefore`. Submission is
//! relayed to the chat indexer's `/internal/push/submit-tx` (the only service on a node-compatible
//! wRPC version). See the KaPosts handoff §5.10.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{ConnectInfo, Query, State},
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
/// Largest transaction accepted, as JSON. A post is at most a few KB of payload; this is the
/// KaPosts budget with room for many inputs.
const MAX_TRANSACTION_JSON: usize = 100 * 1024;
/// Posts one key may have waiting (`scheduled`) at once.
const MAX_SCHEDULED_PER_PUBKEY: i64 = 50;
/// Posts waiting across everyone; past this new schedules are refused until some go out.
const MAX_SCHEDULED_TOTAL: i64 = 20_000;
/// Finished rows (`submitted`, `failed`, `cancelled`) are kept this long for the owner's list.
const KEEP_FINISHED_MS: i64 = 7 * 24 * 60 * 60 * 1000;

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

/// The transaction JSON the phones send (§5.10: the Kaspa REST `POST /transactions` shape,
/// `amount` + `{version, scriptPublicKey}`), or the RPC one (`value`, `"<version><script>"`),
/// read just far enough to compute its id.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TxJson {
    version: u16,
    inputs: Vec<TxInputJson>,
    outputs: Vec<TxOutputJson>,
    lock_time: u64,
    subnetwork_id: String,
    #[serde(default)]
    gas: u64,
    #[serde(default)]
    payload: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TxInputJson {
    previous_outpoint: TxOutpointJson,
    #[serde(default)]
    signature_script: String,
    sequence: u64,
    #[serde(default)]
    sig_op_count: u8,
    #[serde(default)]
    compute_budget: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TxOutpointJson {
    transaction_id: String,
    index: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TxOutputJson {
    #[serde(alias = "value")]
    amount: u64,
    script_public_key: ScriptJson,
    #[serde(default)]
    covenant: Option<CovenantJson>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScriptJson {
    Object {
        version: u16,
        #[serde(rename = "scriptPublicKey", alias = "script")]
        script: String,
    },
    /// Two big-endian version bytes, then the script.
    Hex(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CovenantJson {
    authorizing_input: u16,
    covenant_id: String,
}

/// The id of a stored transaction, hex, exactly as consensus computes it; `None` if it does
/// not decode as a transaction.
fn transaction_id(transaction: &serde_json::Value) -> Option<String> {
    use kaspa_consensus_core::{
        Hash,
        subnets::SubnetworkId,
        tx::{CovenantBinding, ScriptPublicKey, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput},
    };
    let tx: TxJson = serde_json::from_value(inner_tx(transaction)).ok()?;
    let mut inputs = Vec::with_capacity(tx.inputs.len());
    for i in tx.inputs {
        let outpoint = TransactionOutpoint::new(Hash::from_str(&i.previous_outpoint.transaction_id).ok()?, i.previous_outpoint.index);
        let script = hex::decode(&i.signature_script).ok()?;
        inputs.push(if tx.version >= 1 {
            TransactionInput::new_with_compute_budget(outpoint, script, i.sequence, i.compute_budget)
        } else {
            TransactionInput::new(outpoint, script, i.sequence, i.sig_op_count)
        });
    }
    let mut outputs = Vec::with_capacity(tx.outputs.len());
    for o in tx.outputs {
        let (version, script) = match o.script_public_key {
            ScriptJson::Object { version, script } => (version, hex::decode(&script).ok()?),
            ScriptJson::Hex(h) => {
                let bytes = hex::decode(&h).ok()?;
                if bytes.len() < 2 {
                    return None;
                }
                (u16::from_be_bytes([bytes[0], bytes[1]]), bytes[2..].to_vec())
            }
        };
        let covenant = match o.covenant {
            Some(c) => Some(CovenantBinding::new(c.authorizing_input, Hash::from_str(&c.covenant_id).ok()?)),
            None => None,
        };
        outputs.push(TransactionOutput::with_covenant(o.amount, ScriptPublicKey::from_vec(version, script), covenant));
    }
    let subnetwork = SubnetworkId::from_str(&tx.subnetwork_id).ok()?;
    let payload = hex::decode(&tx.payload).ok()?;
    let tx = Transaction::new(tx.version, inputs, outputs, tx.lock_time, subnetwork, tx.gas, payload);
    Some(tx.id().to_string())
}

fn too_many(msg: &str) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(ApiError {
            error: msg.to_string(),
            code: "RATE_LIMIT_EXCEEDED".to_string(),
        }),
    )
}

fn storage_error(e: sqlx::Error) -> (StatusCode, Json<ApiError>) {
    tracing::warn!("schedule-post storage error: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError {
            error: "storage error".to_string(),
            code: "INTERNAL_ERROR".to_string(),
        }),
    )
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
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<SchedulePostRequest>,
) -> Result<Json<ScheduleAck>, (StatusCode, Json<ApiError>)> {
    crate::web_server::check_rate_limit(&state, addr).await?;
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

    // 4) A real transaction of bounded size, and txId is its id (the row key and what the
    //    owner cancels by), not whatever the caller claims.
    let transaction_json = req.transaction.to_string();
    if transaction_json.len() > MAX_TRANSACTION_JSON {
        return Err(bad_request("transaction is too large"));
    }
    match transaction_id(&req.transaction) {
        Some(id) if id == tx_id => {}
        Some(_) => return Err(bad_request("txId is not the transaction's id")),
        None => return Err(bad_request("transaction does not decode")),
    }

    let tx_id_bytes = hex::decode(&tx_id).map_err(|_| bad_request("txId is not hex"))?;
    let pubkey_bytes = hex::decode(&pubkey).map_err(|_| bad_request("pubkey is not hex"))?;
    let preview = payload_preview(&payload);

    // 5) Quotas: a key's waiting posts, and everyone's.
    let row = sqlx::query(
        "SELECT COUNT(*) FILTER (WHERE pubkey = $1) AS mine, COUNT(*) AS total \
         FROM k_scheduled_posts WHERE status = 'scheduled'",
    )
    .bind(&pubkey_bytes)
    .fetch_one(&state.scheduled_pool)
    .await
    .map_err(storage_error)?;
    if row.get::<i64, _>("mine") >= MAX_SCHEDULED_PER_PUBKEY {
        return Err(too_many("too many scheduled posts for this key (50)"));
    }
    if row.get::<i64, _>("total") >= MAX_SCHEDULED_TOTAL {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError {
                error: "the scheduler is full; try again later".to_string(),
                code: "SCHEDULER_FULL".to_string(),
            }),
        ));
    }

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
        return Err(storage_error(e));
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
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Query(q): Query<ScheduledListQuery>,
) -> Result<Json<ScheduledListResponse>, (StatusCode, Json<ApiError>)> {
    crate::web_server::check_rate_limit(&state, addr).await?;
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
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<CancelAck>, (StatusCode, Json<ApiError>)> {
    crate::web_server::check_rate_limit(&state, addr).await?;
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
    prune(state, now).await;
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

/// Retention: finished rows go after `KEEP_FINISHED_MS` (by when they were submitted, else
/// when they were due), and a finished row's transaction bytes are dropped at once: only a
/// `scheduled` row is ever sent. `transaction_json` is NOT NULL, so it is emptied.
async fn prune(state: &Arc<AppState>, now: i64) {
    let deleted = sqlx::query(
        "DELETE FROM k_scheduled_posts WHERE status <> 'scheduled' AND COALESCE(submitted_at, not_before) < $1",
    )
    .bind(now - KEEP_FINISHED_MS)
    .execute(&state.scheduled_pool)
    .await;
    match deleted {
        Ok(r) if r.rows_affected() > 0 => tracing::info!("[scheduled] pruned {} finished row(s)", r.rows_affected()),
        Ok(_) => {}
        Err(e) => tracing::warn!("[scheduled] prune failed: {e}"),
    }
    if let Err(e) = sqlx::query(
        "UPDATE k_scheduled_posts SET transaction_json = '' WHERE status <> 'scheduled' AND transaction_json <> ''",
    )
    .execute(&state.scheduled_pool)
    .await
    {
        tracing::warn!("[scheduled] clearing finished transactions failed: {e}");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use kaspa_consensus_core::{
        subnets::SUBNETWORK_ID_NATIVE,
        tx::{ScriptPublicKey, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput},
    };

    fn sample() -> (Transaction, String) {
        let payload = b"kchat:1:post:02aa:aGk=".to_vec();
        let prev = kaspa_consensus_core::Hash::from_str(&"ab".repeat(32)).unwrap();
        let tx = Transaction::new(
            0,
            vec![TransactionInput::new(TransactionOutpoint::new(prev, 1), vec![0x41, 0x01], 0, 1)],
            vec![TransactionOutput::new(123_456, ScriptPublicKey::from_vec(0, vec![0x20, 0xaa, 0xac]))],
            0,
            SUBNETWORK_ID_NATIVE,
            0,
            payload,
        );
        let id = tx.id().to_string();
        (tx, id)
    }

    #[test]
    fn computes_the_id_of_the_phones_rest_shape() {
        // What KaPostsAPIClient.restJSON sends: amount, {version, scriptPublicKey}, no gas.
        let (_, id) = sample();
        let body = serde_json::json!({ "transaction": {
            "version": 0,
            "inputs": [{
                "previousOutpoint": { "transactionId": "ab".repeat(32), "index": 1 },
                "signatureScript": "4101", "sequence": 0, "sigOpCount": 1
            }],
            "outputs": [{ "amount": 123456, "scriptPublicKey": { "version": 0, "scriptPublicKey": "20aaac" } }],
            "lockTime": 0,
            "subnetworkId": "0000000000000000000000000000000000000000",
            "payload": hex::encode(b"kchat:1:post:02aa:aGk="),
        }});
        assert_eq!(transaction_id(&body).as_deref(), Some(id.as_str()));
        // Pinned: kasia-indexer's /internal/push/submit-tx test converts this same JSON to this id.
        assert_eq!(id, "639d84551894db9a2c8260d462c9dbd173e9b0490d467fb419ada153b7f160b2");
        // The signature script is not part of the id; the payload is.
        let mut other = body.clone();
        other["transaction"]["inputs"][0]["signatureScript"] = serde_json::json!("ff");
        assert_eq!(transaction_id(&other).as_deref(), Some(id.as_str()));
        other["transaction"]["payload"] = serde_json::json!("00");
        assert_ne!(transaction_id(&other).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn computes_the_id_of_the_rpc_shape() {
        let (_, id) = sample();
        let body = serde_json::json!({
            "version": 0,
            "inputs": [{
                "previousOutpoint": { "transactionId": "ab".repeat(32), "index": 1 },
                "signatureScript": "4101", "sequence": 0, "sigOpCount": 1
            }],
            "outputs": [{ "value": 123456, "scriptPublicKey": "000020aaac" }],
            "lockTime": 0, "gas": 0,
            "subnetworkId": "0000000000000000000000000000000000000000",
            "payload": hex::encode(b"kchat:1:post:02aa:aGk="),
        });
        assert_eq!(transaction_id(&body).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn junk_is_not_a_transaction() {
        assert_eq!(transaction_id(&serde_json::json!({ "payload": "6b" })), None);
        assert_eq!(transaction_id(&serde_json::json!({ "transaction": "x" })), None);
    }
}

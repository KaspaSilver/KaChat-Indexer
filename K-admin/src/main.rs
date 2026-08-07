// K-admin — ops/admin dashboard for the KaPosts (KaChat-owned) indexer.
//
// A small axum service that reads the same PostgreSQL database the processor writes and the
// webserver reads, and serves:
//   - a self-contained static dashboard (embedded index.html) at `/`
//   - GET  /api/health              pipeline freshness (node/ingest lag from `transactions`)
//   - GET  /api/stats               row counts per table, ingest rate, DB size
//   - GET  /api/moderation/recent   newest indexed content (for picking removal targets)
//   - POST /api/moderation/remove   dry-run preview or atomic remove-all-by-pubkey
//
// It intentionally does NOT talk to Docker or the Kaspa node: container status/logs stay in
// Portainer, and pipeline freshness is derived purely from the database.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose};
use clap::Parser;
use serde::{Deserialize, Serialize};
use sqlx::{Row, postgres::PgPoolOptions, PgPool};
use std::time::{SystemTime, UNIX_EPOCH};
use tower_http::cors::CorsLayer;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(name = "K-admin", about = "Ops/admin dashboard for the KaPosts indexer")]
struct Args {
    #[arg(long, default_value = "localhost")]
    db_host: String,
    #[arg(long, default_value_t = 5432)]
    db_port: u16,
    #[arg(long, default_value = "k-db")]
    db_name: String,
    #[arg(long, default_value = "username")]
    db_user: String,
    #[arg(long, default_value = "password")]
    db_password: String,
    #[arg(long, default_value_t = 4)]
    db_max_connections: u32,
    #[arg(long, default_value = "0.0.0.0:3081")]
    bind_address: String,
    /// URL of the vendored kasia (chat) indexer's metrics endpoint, proxied to the Chat tab.
    #[arg(long, default_value = "http://127.0.0.1:8600/metrics")]
    chat_metrics_url: String,
    /// Webserver health URL, probed for the per-service health panel.
    #[arg(long, default_value = "http://127.0.0.1:3080/health")]
    webserver_health_url: String,
    /// Chat indexer's contextual-message import endpoint (chat-history backfill target).
    #[arg(long, default_value = "http://127.0.0.1:8600/contextual-messages/import")]
    chat_import_url: String,
    /// Block explorer base URL used to page an address's full transaction history.
    #[arg(long, default_value = "https://api.kaspa.org")]
    explorer_url: String,
}

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    // Last observed (newest_transaction_time, sampled_at_ms) so /api/health can tell
    // "catching up" (lag high but advancing) from genuinely "stalled".
    last_tx_sample: std::sync::Arc<std::sync::Mutex<Option<(i64, i64)>>>,
    chat_metrics_url: String,
    webserver_health_url: String,
    chat_import_url: String,
    explorer_url: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let conn = format!(
        "postgresql://{}:{}@{}:{}/{}",
        args.db_user, args.db_password, args.db_host, args.db_port, args.db_name
    );

    info!("K-admin connecting to database {}:{}/{}", args.db_host, args.db_port, args.db_name);
    let pool = PgPoolOptions::new()
        .max_connections(args.db_max_connections)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&conn)
        .await?;

    let state = AppState {
        pool,
        last_tx_sample: std::sync::Arc::new(std::sync::Mutex::new(None)),
        chat_metrics_url: args.chat_metrics_url.clone(),
        webserver_health_url: args.webserver_health_url.clone(),
        chat_import_url: args.chat_import_url.clone(),
        explorer_url: args.explorer_url.clone(),
    };

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/api/health", get(get_health))
        .route("/api/stats", get(get_stats))
        .route("/api/moderation/recent", get(get_recent))
        .route("/api/moderation/remove", post(post_remove))
        .route("/api/broadcasts", get(get_broadcasts))
        .route("/api/chat-metrics", get(get_chat_metrics))
        .route("/api/services", get(get_services))
        .route("/api/chat-import", post(post_chat_import))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.bind_address).await?;
    info!("K-admin dashboard listening on http://{}", args.bind_address);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

// ---------------------------------------------------------------------------
// /api/health
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    db_ok: bool,
    now_ms: i64,
    newest_transaction_time: i64,
    newest_content_time: i64,
    node_lag_ms: i64,
    status: String,
}

async fn get_health(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    // `transactions` is simply-kaspa's table (pruned to ~1h); its freshest block_time tracks
    // how caught-up node ingestion is. k_contents freshness is informational (KaChat posts
    // are sparse, so it legitimately lags when nobody has posted recently).
    let row = sqlx::query(
        r#"
        SELECT
            COALESCE((SELECT MAX(block_time) FROM transactions), 0) AS newest_tx,
            COALESCE((SELECT MAX(block_time) FROM k_contents), 0)  AS newest_content
        "#,
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::db)?;

    let newest_tx: i64 = row.get("newest_tx");
    let newest_content: i64 = row.get("newest_content");
    let now = now_ms();
    let node_lag_ms = if newest_tx == 0 { -1 } else { now - newest_tx };

    // Did the transaction frontier advance since the previous poll? If so, a large lag means
    // initial backfill / catch-up, not a stall. Requires two samples, so the first call after
    // startup can't yet distinguish and falls back to the lag thresholds.
    let advancing = {
        let mut guard = state.last_tx_sample.lock().unwrap();
        let advancing = match *guard {
            Some((prev_tx, _)) => newest_tx > prev_tx,
            None => false,
        };
        *guard = Some((newest_tx, now));
        advancing
    };

    let status = if newest_tx == 0 {
        "starting"
    } else if node_lag_ms < 120_000 {
        "healthy"
    } else if advancing {
        "catching_up"
    } else if node_lag_ms < 600_000 {
        "lagging"
    } else {
        "stalled"
    };

    Ok(Json(HealthResponse {
        db_ok: true,
        now_ms: now,
        newest_transaction_time: newest_tx,
        newest_content_time: newest_content,
        node_lag_ms,
        status: status.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// /api/stats
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatsResponse {
    posts: i64,
    replies: i64,
    quotes: i64,
    reposts: i64,
    upvotes: i64,
    downvotes: i64,
    follows: i64,
    blocks: i64,
    mentions: i64,
    hashtags: i64,
    broadcasts: i64,
    newest_content_time: i64,
    ingest_last_5m: i64,
    ingest_last_60m: i64,
    db_size_bytes: i64,
    bcast_total: i64,
    bcast_kaspa: i64,
    bcast_kachat_bugs: i64,
}

async fn get_stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, ApiError> {
    let now = now_ms();
    let cutoff_5m = now - 5 * 60_000;
    let cutoff_60m = now - 60 * 60_000;

    let row = sqlx::query(
        r#"
        SELECT
            (SELECT COUNT(*) FROM k_contents WHERE content_type = 'post')     AS posts,
            (SELECT COUNT(*) FROM k_contents WHERE content_type = 'reply')    AS replies,
            (SELECT COUNT(*) FROM k_contents WHERE content_type = 'quote')    AS quotes,
            (SELECT COUNT(*) FROM k_contents WHERE content_type = 'repost')   AS reposts,
            (SELECT COUNT(*) FROM k_votes WHERE vote = 'upvote')             AS upvotes,
            (SELECT COUNT(*) FROM k_votes WHERE vote = 'downvote')           AS downvotes,
            (SELECT COUNT(*) FROM k_follows)                                  AS follows,
            (SELECT COUNT(*) FROM k_blocks)                                   AS blocks,
            (SELECT COUNT(*) FROM k_mentions)                                 AS mentions,
            (SELECT COUNT(*) FROM k_hashtags)                                 AS hashtags,
            (SELECT COUNT(*) FROM k_broadcasts)                               AS broadcasts,
            COALESCE((SELECT MAX(block_time) FROM k_contents), 0)             AS newest_content_time,
            (SELECT COUNT(*) FROM k_contents WHERE block_time >= $1)          AS ingest_last_5m,
            (SELECT COUNT(*) FROM k_contents WHERE block_time >= $2)          AS ingest_last_60m,
            pg_database_size(current_database())                             AS db_size_bytes,
            (SELECT COUNT(*) FROM kachat_broadcasts)                          AS bcast_total,
            (SELECT COUNT(*) FROM kachat_broadcasts WHERE channel = 'kaspa')  AS bcast_kaspa,
            (SELECT COUNT(*) FROM kachat_broadcasts WHERE channel = 'kachat-bugs') AS bcast_kachat_bugs
        "#,
    )
    .bind(cutoff_5m)
    .bind(cutoff_60m)
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::db)?;

    Ok(Json(StatsResponse {
        posts: row.get("posts"),
        replies: row.get("replies"),
        quotes: row.get("quotes"),
        reposts: row.get("reposts"),
        upvotes: row.get("upvotes"),
        downvotes: row.get("downvotes"),
        follows: row.get("follows"),
        blocks: row.get("blocks"),
        mentions: row.get("mentions"),
        hashtags: row.get("hashtags"),
        broadcasts: row.get("broadcasts"),
        newest_content_time: row.get("newest_content_time"),
        ingest_last_5m: row.get("ingest_last_5m"),
        ingest_last_60m: row.get("ingest_last_60m"),
        db_size_bytes: row.get("db_size_bytes"),
        bcast_total: row.get("bcast_total"),
        bcast_kaspa: row.get("bcast_kaspa"),
        bcast_kachat_bugs: row.get("bcast_kachat_bugs"),
    }))
}

// ---------------------------------------------------------------------------
// /api/broadcasts  (recent KaChat broadcasts, optionally filtered by channel)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BroadcastQuery {
    channel: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct BroadcastItem {
    tx_id: String,
    channel: String,
    sender_address: String,
    preview: String,
    timestamp: i64,
}

async fn get_broadcasts(
    State(state): State<AppState>,
    Query(params): Query<BroadcastQuery>,
) -> Result<Json<Vec<BroadcastItem>>, ApiError> {
    let limit = params.limit.unwrap_or(25).clamp(1, 200);
    let channel = params
        .channel
        .map(|c| c.trim().to_lowercase())
        .filter(|c| !c.is_empty());

    let sql_base = "SELECT encode(transaction_id,'hex') AS tx, channel, sender_address, \
                    content, block_time FROM kachat_broadcasts";
    let rows = if let Some(ch) = &channel {
        sqlx::query(&format!(
            "{sql_base} WHERE channel = $1 ORDER BY block_time DESC, id DESC LIMIT $2"
        ))
        .bind(ch)
        .bind(limit)
        .fetch_all(&state.pool)
        .await
    } else {
        sqlx::query(&format!(
            "{sql_base} ORDER BY block_time DESC, id DESC LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(&state.pool)
        .await
    }
    .map_err(ApiError::db)?;

    let items = rows
        .iter()
        .map(|row| {
            let content: String = row.get("content");
            // Preview: broadcasts may be reply/audio JSON envelopes; show a bounded snippet.
            let preview: String = content.chars().take(200).collect();
            BroadcastItem {
                tx_id: row.get("tx"),
                channel: row.get("channel"),
                sender_address: row.get("sender_address"),
                preview,
                timestamp: row.get("block_time"),
            }
        })
        .collect();
    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// /api/moderation/recent
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RecentQuery {
    limit: Option<i64>,
}

#[derive(Serialize)]
struct RecentItem {
    transaction_id: String,
    sender_pubkey: String,
    content_type: String,
    timestamp: i64,
    preview: String,
}

/// The KaChat exclusivity marker (U+2060), stripped from previews for readability.
const KACHAT_MARKER: &str = "\u{2060}";

fn decode_preview(base64_message: &str) -> String {
    let text = match general_purpose::STANDARD.decode(base64_message) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => return String::new(),
    };
    let stripped = text.strip_prefix(KACHAT_MARKER).unwrap_or(&text);
    let trimmed: String = stripped.chars().take(140).collect();
    trimmed
}

async fn get_recent(
    State(state): State<AppState>,
    Query(params): Query<RecentQuery>,
) -> Result<Json<Vec<RecentItem>>, ApiError> {
    let limit = params.limit.unwrap_or(25).clamp(1, 200);
    let rows = sqlx::query(
        r#"
        SELECT transaction_id, sender_pubkey, content_type, block_time, base64_encoded_message
        FROM k_contents
        ORDER BY block_time DESC, id DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::db)?;

    let items = rows
        .iter()
        .map(|row| {
            let tx: Vec<u8> = row.get("transaction_id");
            let sender: Vec<u8> = row.get("sender_pubkey");
            let msg: String = row.get("base64_encoded_message");
            RecentItem {
                transaction_id: hex::encode(&tx),
                sender_pubkey: hex::encode(&sender),
                content_type: row.get("content_type"),
                timestamp: row.get("block_time"),
                preview: decode_preview(&msg),
            }
        })
        .collect();

    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// /api/moderation/remove  (dry-run preview OR atomic remove-all-by-pubkey)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RemoveRequest {
    pubkey: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize)]
struct RemovalStats {
    dry_run: bool,
    pubkey: String,
    mentions: i64,
    contents: i64,
    votes: i64,
    broadcasts: i64,
    blocks: i64,
    follows: i64,
    total: i64,
}

async fn post_remove(
    State(state): State<AppState>,
    Json(req): Json<RemoveRequest>,
) -> Result<Json<RemovalStats>, ApiError> {
    let pubkey = req.pubkey.trim().to_lowercase();
    if (pubkey.len() != 66 && pubkey.len() != 64) || !pubkey.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(ApiError::bad_request(
            "Invalid pubkey: expected 64 or 66 hex characters",
        ));
    }
    let pubkey_bytes = hex::decode(&pubkey).map_err(|_| ApiError::bad_request("Invalid pubkey hex"))?;

    let (mentions, contents, votes, broadcasts, blocks, follows) = if req.dry_run {
        // Preview: count only.
        let row = sqlx::query(
            r#"
            SELECT
                (SELECT COUNT(*) FROM k_mentions   WHERE sender_pubkey = $1) AS mentions,
                (SELECT COUNT(*) FROM k_contents   WHERE sender_pubkey = $1) AS contents,
                (SELECT COUNT(*) FROM k_votes      WHERE sender_pubkey = $1) AS votes,
                (SELECT COUNT(*) FROM k_broadcasts WHERE sender_pubkey = $1) AS broadcasts,
                (SELECT COUNT(*) FROM k_blocks     WHERE sender_pubkey = $1) AS blocks,
                (SELECT COUNT(*) FROM k_follows    WHERE sender_pubkey = $1) AS follows
            "#,
        )
        .bind(&pubkey_bytes)
        .fetch_one(&state.pool)
        .await
        .map_err(ApiError::db)?;
        (
            row.get::<i64, _>("mentions"),
            row.get::<i64, _>("contents"),
            row.get::<i64, _>("votes"),
            row.get::<i64, _>("broadcasts"),
            row.get::<i64, _>("blocks"),
            row.get::<i64, _>("follows"),
        )
    } else {
        // Execute: atomic delete of every row authored by this pubkey (k_hashtags cascade
        // from k_contents via FK). Mirrors K-content-remover's execute_removal.
        let row = sqlx::query(
            r#"
            WITH deleted_mentions AS (
                DELETE FROM k_mentions WHERE sender_pubkey = $1 RETURNING id
            ),
            deleted_contents AS (
                DELETE FROM k_contents WHERE sender_pubkey = $1 RETURNING id
            ),
            deleted_votes AS (
                DELETE FROM k_votes WHERE sender_pubkey = $1 RETURNING id
            ),
            deleted_broadcasts AS (
                DELETE FROM k_broadcasts WHERE sender_pubkey = $1 RETURNING id
            ),
            deleted_blocks AS (
                DELETE FROM k_blocks WHERE sender_pubkey = $1 RETURNING id
            ),
            deleted_follows AS (
                DELETE FROM k_follows WHERE sender_pubkey = $1 RETURNING id
            )
            SELECT
                (SELECT COUNT(*) FROM deleted_mentions)   AS mentions,
                (SELECT COUNT(*) FROM deleted_contents)   AS contents,
                (SELECT COUNT(*) FROM deleted_votes)      AS votes,
                (SELECT COUNT(*) FROM deleted_broadcasts) AS broadcasts,
                (SELECT COUNT(*) FROM deleted_blocks)     AS blocks,
                (SELECT COUNT(*) FROM deleted_follows)    AS follows
            "#,
        )
        .bind(&pubkey_bytes)
        .fetch_one(&state.pool)
        .await
        .map_err(ApiError::db)?;
        info!("K-admin executed content removal for {}", pubkey);
        (
            row.get::<i64, _>("mentions"),
            row.get::<i64, _>("contents"),
            row.get::<i64, _>("votes"),
            row.get::<i64, _>("broadcasts"),
            row.get::<i64, _>("blocks"),
            row.get::<i64, _>("follows"),
        )
    };

    let total = mentions + contents + votes + broadcasts + blocks + follows;
    Ok(Json(RemovalStats {
        dry_run: req.dry_run,
        pubkey,
        mentions,
        contents,
        votes,
        broadcasts,
        blocks,
        follows,
        total,
    }))
}

// ---------------------------------------------------------------------------
// /api/chat-import  (backfill DM history: page a block explorer, forward to the chat indexer)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChatImportRequest {
    address: String,
}

#[derive(Serialize)]
struct ChatImportResponse {
    scanned: usize,
    forwarded: usize,
    imported: usize,
    skipped: usize,
    pages: usize,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ExplorerOutput {
    script_public_key_address: Option<String>,
}

#[derive(Deserialize)]
struct ExplorerTx {
    transaction_id: String,
    payload: Option<String>,
    block_time: Option<i64>,
    // A transaction can appear in multiple blocks, so the explorer returns an array.
    block_hash: Option<Vec<String>>,
    outputs: Option<Vec<ExplorerOutput>>,
}

#[derive(Serialize)]
struct ImportTx {
    tx_id: String,
    payload: String,
    block_time: u64,
    block_hash: String,
    address: String,
}

#[derive(Deserialize, Default)]
struct ImportResultDto {
    imported: usize,
    skipped: usize,
}

async fn post_chat_import(
    State(state): State<AppState>,
    Json(req): Json<ChatImportRequest>,
) -> Json<ChatImportResponse> {
    // "ciph_msg:" as hex — the on-chain prefix for all Kasia messaging payloads.
    const CIPH_PREFIX: &str = "636970685f6d73673a";
    let address = req.address.trim().to_string();
    let mut resp = ChatImportResponse {
        scanned: 0,
        forwarded: 0,
        imported: 0,
        skipped: 0,
        pages: 0,
        error: None,
    };
    if !address.starts_with("kaspa:") {
        resp.error = Some("address must start with kaspa:".into());
        return Json(resp);
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            resp.error = Some("http client error".into());
            return Json(resp);
        }
    };

    let mut to_import: Vec<ImportTx> = Vec::new();
    let mut offset = 0usize;
    loop {
        let url = format!(
            "{}/addresses/{}/full-transactions?limit=500&offset={}&resolve_previous_outpoints=no",
            state.explorer_url, address, offset
        );
        let txs: Vec<ExplorerTx> = match client.get(&url).send().await {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(e) => {
                    resp.error = Some(format!("explorer parse error: {e}"));
                    break;
                }
            },
            Err(e) => {
                resp.error = Some(format!("explorer request error: {e}"));
                break;
            }
        };
        let n = txs.len();
        resp.scanned += n;
        resp.pages += 1;
        for tx in txs {
            let Some(payload) = tx.payload.filter(|p| p.starts_with(CIPH_PREFIX)) else {
                continue;
            };
            let addr = tx
                .outputs
                .as_ref()
                .and_then(|o| o.first())
                .and_then(|o| o.script_public_key_address.clone());
            let bh = tx.block_hash.as_ref().and_then(|v| v.first()).cloned();
            let (Some(bt), Some(bh), Some(addr)) = (tx.block_time, bh, addr) else {
                continue;
            };
            to_import.push(ImportTx {
                tx_id: tx.transaction_id,
                payload,
                block_time: bt as u64,
                block_hash: bh,
                address: addr,
            });
        }
        if n < 500 || resp.pages >= 40 {
            break;
        }
        offset += 500;
    }

    resp.forwarded = to_import.len();
    for chunk in to_import.chunks(200) {
        match client.post(&state.chat_import_url).json(&chunk).send().await {
            Ok(r) => {
                let dto: ImportResultDto = r.json().await.unwrap_or_default();
                resp.imported += dto.imported;
                resp.skipped += dto.skipped;
            }
            Err(e) => {
                resp.error = Some(format!("chat indexer import error: {e}"));
                break;
            }
        }
    }

    Json(resp)
}

// ---------------------------------------------------------------------------
// /api/services  (per-service health for the dashboard — replaces needing Portainer)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ServiceHealth {
    name: String,
    status: String, // "healthy" | "degraded" | "down"
    detail: String,
}

fn svc(name: &str, status: &str, detail: String) -> ServiceHealth {
    ServiceHealth { name: name.to_string(), status: status.to_string(), detail }
}

async fn probe_http(name: &str, url: &str) -> ServiceHealth {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return svc(name, "down", "client error".into()),
    };
    match client.get(url).send().await {
        Ok(r) if r.status().is_success() => svc(name, "healthy", "responding".into()),
        Ok(r) => svc(name, "degraded", format!("HTTP {}", r.status().as_u16())),
        Err(_) => svc(name, "down", "unreachable".into()),
    }
}

async fn get_services(State(state): State<AppState>) -> Json<Vec<ServiceHealth>> {
    let now = now_ms();
    let mut out = Vec::new();

    // Database, ingest freshness, and processor heartbeat come from one DB round-trip.
    match sqlx::query(
        "SELECT COALESCE((SELECT MAX(block_time) FROM transactions), 0) AS newest_tx, \
         (SELECT value FROM k_vars WHERE key = 'processor_heartbeat') AS hb",
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => {
            out.push(svc("Database", "healthy", "connected".into()));

            let newest_tx: i64 = row.get("newest_tx");
            let lag = if newest_tx == 0 { -1 } else { now - newest_tx };
            let ingest = if newest_tx == 0 {
                svc("Ingest (blocks)", "degraded", "no transactions yet".into())
            } else if lag < 120_000 {
                svc("Ingest (blocks)", "healthy", format!("{}s behind tip", lag / 1000))
            } else {
                svc("Ingest (blocks)", "degraded", format!("{}m behind tip", lag / 60_000))
            };
            out.push(ingest);

            let hb: Option<String> = row.get("hb");
            let proc = match hb.and_then(|v| v.parse::<i64>().ok()) {
                Some(ts) => {
                    let age = now - ts;
                    if age < 90_000 {
                        svc("Processor", "healthy", format!("heartbeat {}s ago", age / 1000))
                    } else {
                        svc("Processor", "down", format!("stale {}s", age / 1000))
                    }
                }
                None => svc("Processor", "degraded", "no heartbeat yet".into()),
            };
            out.push(proc);
        }
        Err(_) => {
            out.push(svc("Database", "down", "unreachable".into()));
            out.push(svc("Ingest (blocks)", "down", "db unreachable".into()));
            out.push(svc("Processor", "down", "db unreachable".into()));
        }
    }

    // Webserver + chat indexer probed over HTTP.
    out.push(probe_http("Webserver (API)", &state.webserver_health_url).await);
    out.push(probe_http("Chat indexer", &state.chat_metrics_url).await);

    Json(out)
}

// ---------------------------------------------------------------------------
// /api/chat-metrics  (proxy the vendored kasia chat indexer's /metrics)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ChatMetricsResponse {
    reachable: bool,
    metrics: Option<serde_json::Value>,
}

async fn get_chat_metrics(State(state): State<AppState>) -> Json<ChatMetricsResponse> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Json(ChatMetricsResponse { reachable: false, metrics: None }),
    };
    match client.get(&state.chat_metrics_url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(v) => Json(ChatMetricsResponse { reachable: true, metrics: Some(v) }),
            Err(_) => Json(ChatMetricsResponse { reachable: true, metrics: None }),
        },
        // Not yet up / still building index / connection refused → reachable:false (normal).
        _ => Json(ChatMetricsResponse { reachable: false, metrics: None }),
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn db(e: sqlx::Error) -> Self {
        error!("database error: {}", e);
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "Database error".to_string(),
        }
    }
    fn bad_request(msg: &str) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            message: msg.to_string(),
        }
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}

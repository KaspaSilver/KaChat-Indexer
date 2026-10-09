use axum::{
    Router,
    extract::{ConnectInfo, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Json, Response},
    routing::{get, post},
};
use axum_prometheus::PrometheusMetricLayer;
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, sync::RwLock, time::Instant};
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
};
use tracing::{error as log_error, info as log_info};

use crate::api_handlers::ApiHandlers;
use crate::config::ServerConfig;
use crate::database_trait::DatabaseInterface;
use crate::models::{
    ApiError, BroadcastsResponse, ChessLeaderboardResponse, ChessPlayerRow, ChessTournamentRow,
    ChessTournamentsResponse, GetThreadResponse, PaginatedEngagementResponse,
    PaginatedNotificationsResponse, PaginatedPostsResponse, PaginatedRepliesResponse,
    PaginatedUsersResponse, PollData, PostDetailsResponse, ServerUserPost, TrendingHashtagsResponse,
};

#[derive(Debug, Clone)]
pub(crate) struct RateLimitEntry {
    count: u32,
    window_start: Instant,
}

type RateLimitMap = Arc<RwLock<HashMap<SocketAddr, RateLimitEntry>>>;

pub struct AppState {
    pub api_handlers: ApiHandlers,
    pub rate_limit_map: RateLimitMap,
    pub server_config: ServerConfig,
    pub db: Arc<dyn DatabaseInterface>,
    /// Shared outbound HTTP client (LibreTranslate calls for /translate).
    pub http: reqwest::Client,
    /// Separate per-IP limiter for /translate (requests + posts).
    pub translate_rate_limit_map: crate::translate::TranslateRateLimitMap,
    /// Chess leaderboard: replaying the whole arena is expensive, so cache the two derived views.
    pub chess_cache: Arc<RwLock<ChessCache>>,
    /// §5.10: direct pool handle for the scheduled-posts store (raw SQL).
    pub scheduled_pool: sqlx::PgPool,
    /// .kachat names registry module (testnet). Off unless KACHAT_NAMES_MANIFEST is set.
    pub names: crate::names::NamesState,
}

/// Cached result of one arena replay (see chess.rs). Recomputed when older than CHESS_CACHE_TTL.
#[derive(Default)]
pub struct ChessCache {
    computed_at: Option<Instant>,
    players: Vec<ChessPlayerRow>,
    tournaments: Vec<ChessTournamentRow>,
    games_started: u64,
}

const CHESS_CACHE_TTL: Duration = Duration::from_secs(30);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Serve the leaderboard + lobby from cache, recomputing from the DB when stale.
async fn chess_snapshot(state: &AppState) -> (Vec<ChessPlayerRow>, Vec<ChessTournamentRow>, u64) {
    {
        let cache = state.chess_cache.read().await;
        if let Some(at) = cache.computed_at {
            if at.elapsed() < CHESS_CACHE_TTL {
                return (cache.players.clone(), cache.tournaments.clone(), cache.games_started);
            }
        }
    }
    let rows = state.db.get_chess_arena_rows().await.unwrap_or_default();
    let arena: Vec<crate::chess::ArenaRow> = rows
        .into_iter()
        .map(|(tx_id, sender, block_time, content)| crate::chess::ArenaRow {
            tx_id,
            sender,
            block_time,
            content,
        })
        .collect();
    let (board, lobby, games_started) = crate::chess::compute_all(arena);
    let players: Vec<ChessPlayerRow> = board
        .into_iter()
        .map(|r| ChessPlayerRow {
            address: r.address,
            wins: r.wins,
            losses: r.losses,
            duel_wins: r.duel_wins,
            duel_losses: r.duel_losses,
            tournament_game_wins: r.tournament_game_wins,
            tournament_game_losses: r.tournament_game_losses,
            tournaments_played: r.tournaments_played,
            tournaments_won: r.tournaments_won,
            tournaments_lost: r.tournaments_lost,
            last_played_at: r.last_played_at,
        })
        .collect();
    let tournaments: Vec<ChessTournamentRow> = lobby
        .into_iter()
        .map(|s| ChessTournamentRow {
            id: s.id,
            status: s.status,
            capacity: s.capacity,
            players: s.players,
            started_at: s.started_at,
            champion: s.champion,
        })
        .collect();
    let mut cache = state.chess_cache.write().await;
    cache.computed_at = Some(Instant::now());
    cache.players = players.clone();
    cache.tournaments = tournaments.clone();
    cache.games_started = games_started;
    (players, tournaments, games_started)
}

pub struct WebServer {
    pub app_state: Arc<AppState>,
}

#[derive(Debug, Deserialize)]
struct GetPostsQuery {
    user: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetRepliesQuery {
    post: Option<String>,
    user: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>, // Changed to String to support compound cursors
    after: Option<String>,  // Changed to String to support compound cursors
}

/// Fork addition: query for GET /get-broadcasts (KaChat broadcast channel history).
#[derive(Debug, Deserialize)]
struct GetBroadcastsQuery {
    channel: Option<String>,
    limit: Option<u32>,
    before: Option<i64>,
}

/// Fork addition: query for GET /get-post-engagement (per-post actor lists).
#[derive(Debug, Deserialize)]
struct GetPostEngagementQuery {
    #[serde(rename = "postId")]
    post_id: Option<String>,
    #[serde(rename = "type")]
    engagement_type: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetPostsWatchingQuery {
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetContentsFollowingQuery {
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetUsersQuery {
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetMostActiveUsersQuery {
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    #[serde(rename = "timeWindow")]
    time_window: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchUsersQuery {
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    #[serde(rename = "searchedUserPubkey")]
    searched_user_pubkey: Option<String>,
    #[serde(rename = "searchedUserNickname")]
    searched_user_nickname: Option<String>,
}

/// Fork addition (§5.6): unified content/people search — GET /search?q=&type=posts|users.
#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    #[serde(rename = "type")]
    search_type: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetMentionsQuery {
    user: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetNotificationsQuery {
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetHashtagContentQuery {
    hashtag: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetTrendingHashtagsQuery {
    #[serde(rename = "timeWindow")]
    time_window: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ChessLeaderboardQuery {
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ChessPlayerQuery {
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChessTournamentsQuery {
    status: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct GetPostDetailsQuery {
    id: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
}

/// §5.9: GET /get-poll accepts `postId` (canonical) or `id`, and an optional `requesterPubkey`.
#[derive(Debug, Deserialize)]
struct GetPollQuery {
    #[serde(rename = "postId")]
    post_id: Option<String>,
    id: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetUserDetailsQuery {
    user: Option<String>,
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetBlockedUsersQuery {
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetFollowedUsersQuery {
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetUsersFollowingQuery {
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    #[serde(rename = "userPubkey")]
    user_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetUsersFollowersQuery {
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    #[serde(rename = "userPubkey")]
    user_pubkey: Option<String>,
    limit: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetNotificationsCountQuery {
    #[serde(rename = "requesterPubkey")]
    requester_pubkey: Option<String>,
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetUsersCountQuery {}

impl WebServer {
    pub async fn new(
        db: Arc<dyn DatabaseInterface>,
        scheduled_pool: sqlx::PgPool,
        server_config: ServerConfig,
    ) -> Self {
        let api_handlers = ApiHandlers::new(db.clone());
        let rate_limit_map = Arc::new(RwLock::new(HashMap::new()));
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        let translate_rate_limit_map = Arc::new(RwLock::new(HashMap::new()));

        let app_state = Arc::new(AppState {
            api_handlers,
            rate_limit_map,
            server_config,
            db,
            http,
            translate_rate_limit_map,
            chess_cache: Arc::new(RwLock::new(ChessCache::default())),
            scheduled_pool,
            names: crate::names::NamesState::from_env(),
        });

        // Rate-limit windows are one minute; drop entries idle for two so one-off (or rotating)
        // client addresses don't grow the maps for the life of the process.
        spawn_rate_limit_pruner(app_state.clone());

        // §5.10: start the per-minute scheduler that broadcasts due scheduled posts. A
        // names-only server has no KaPosts tables, so it has nothing to schedule.
        if !names_only() {
            crate::scheduled::spawn_scheduler(app_state.clone());
            // kachat-audits IDX-005: search decodes stored base64 with kachat_b64_utf8, which
            // returns NULL instead of failing the whole query on one bad row. The processor
            // creates it too; whichever starts first does (idempotent).
            if let Err(e) = sqlx::query(
                r#"CREATE OR REPLACE FUNCTION kachat_b64_utf8(value TEXT) RETURNS TEXT AS $$
                BEGIN
                    RETURN convert_from(decode(value, 'base64'), 'UTF8');
                EXCEPTION WHEN others THEN
                    RETURN NULL;
                END;
                $$ LANGUAGE plpgsql IMMUTABLE"#,
            )
            .execute(&app_state.scheduled_pool)
            .await
            {
                tracing::warn!("could not create kachat_b64_utf8: {e}");
            }
            // kachat-audits IDX-022: search reads the decoded columns the processor fills at
            // ingest. Add them here too (idempotent), so a webserver that starts before the
            // processor never answers /search with an error; rows the processor has not
            // backfilled yet fall back to the safe decode.
            for stmt in [
                "ALTER TABLE k_contents ADD COLUMN IF NOT EXISTS message_text TEXT",
                "ALTER TABLE k_broadcasts ADD COLUMN IF NOT EXISTS nickname_text TEXT",
            ] {
                if let Err(e) = sqlx::query(stmt).execute(&app_state.scheduled_pool).await {
                    tracing::warn!("could not add a search column ({stmt}): {e}");
                }
            }
        }

        Self { app_state }
    }

    pub fn create_router(&self) -> Router {
        let timeout_duration = Duration::from_secs(self.app_state.server_config.request_timeout);
        let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

        // .kachat names registry: status/manifest here, the read API over the
        // kachat-names-follower tables in names_api (docs/KACHAT_NAMES_APP_CONTRACT.md).
        let names = Router::new()
            .route("/names/status", get(crate::names::handle_names_status))
            .route("/names/manifest", get(crate::names::handle_names_manifest))
            .route("/names/prices", get(crate::names_api::prices))
            .route("/names/expiring", get(crate::names_api::expiring))
            .route("/names/all", get(crate::names_api::all))
            .route("/names/grace", get(crate::names_api::grace))
            .route("/names/by-owner/:address", get(crate::names_api::by_owner))
            .route("/names/gap/:key", get(crate::names_api::gap_lookup))
            .route("/names/:name", get(crate::names_api::name_lookup))
            .route("/names/:name/offers", get(crate::names_api::name_offers))
            .route("/names/:name/history", get(crate::names_api::name_history))
            .route("/offers/by-buyer/:address", get(crate::names_api::offers_by_buyer))
            .route("/market/listings", get(crate::names_api::listings))
            .route("/market/activity", get(crate::names_api::market_activity))
            .route("/names/activity", get(crate::names_api::names_activity))
            .route("/profiles/stats", get(crate::names_api::profile_stats))
            .route("/profiles/history", get(crate::names_api::profile_history))
            .route("/profiles/:address", get(crate::names_api::profile))
            .route("/identity/batch", post(crate::names_api::identity_batch))
            .route("/identity/:address", get(crate::names_api::identity));

        let router = if names_only() {
            // The .kachat Domains server (docs/KACHAT_NAMES_STANDALONE.md): the names, profiles
            // and identity API over its own database, and nothing that needs the chat or
            // KaPosts indexer.
            Router::new()
                .route("/", get(handle_names_root))
                .route("/health", get(handle_names_health))
                .merge(names)
                .route(
                    "/metrics",
                    get(move || async move { metric_handle.render() }),
                )
        } else {
            Router::new()
                .route("/", get(handle_root))
                .route("/health", get(handle_health))
                .route("/stats", get(handle_stats))
                .merge(names)
                .route(
                    "/metrics",
                    get(move || async move { metric_handle.render() }),
                )
                .route("/get-posts", get(handle_get_posts))
                .route("/get-post-details", get(handle_get_post_details))
                // Fetch one post by id, any age or author, in the feed's KPost shape.
                // Same contract as get-post-details; named for what the apps call.
                .route("/get-post", get(handle_get_post_details))
                // §5.9: single-poll refresh (options + live counts + myVote).
                .route("/get-poll", get(handle_get_poll))
                // §5.10 scheduled posts: build+sign on the phone, submit at notBefore server-side.
                .route("/schedule-post", post(crate::scheduled::handle_schedule_post))
                .route("/scheduled-posts", get(crate::scheduled::handle_scheduled_posts))
                .route(
                    "/cancel-scheduled-post",
                    post(crate::scheduled::handle_cancel_scheduled_post),
                )
                // The post plus its parent chain, walked server-side.
                .route("/get-thread", get(handle_get_thread))
                .route("/get-posts-watching", get(handle_get_posts_watching))
                .route(
                    "/get-contents-following",
                    get(handle_get_contents_following),
                )
                .route("/get-replies", get(handle_get_replies))
                .route("/get-post-engagement", get(handle_get_post_engagement))
                .route("/get-broadcasts", get(handle_get_broadcasts))
                .route("/get-mentions", get(handle_get_mentions))
                .route("/get-users", get(handle_get_users))
                .route("/get-most-active-users", get(handle_get_most_active_users))
                .route("/get-users-count", get(handle_get_users_count))
                .route("/search-users", get(handle_search_users))
                // Unified content/people search (§5.6). type=posts (default) | users.
                .route("/search", get(handle_search))
                .route("/get-user-details", get(handle_get_user_details))
                .route("/get-followed-users", get(handle_get_followed_users))
                .route("/get-users-following", get(handle_get_users_following))
                .route("/get-users-followers", get(handle_get_users_followers))
                .route("/get-blocked-users", get(handle_get_blocked_users))
                .route(
                    "/get-notifications-count",
                    get(handle_get_notifications_count),
                )
                .route("/get-notifications", get(handle_get_notifications))
                .route("/get-hashtag-content", get(handle_get_hashtag_content))
                .route("/get-trending-hashtags", get(handle_get_trending_hashtags))
                // Chess Tournaments (5.1) leaderboard (§6).
                .route("/chess/leaderboard", get(handle_chess_leaderboard))
                .route("/chess/player", get(handle_chess_player))
                .route("/chess/tournaments", get(handle_chess_tournaments))
                .route("/translate", post(crate::translate::handle_translate))
                .route(
                    "/translate/languages",
                    get(crate::translate::handle_translate_languages),
                )
        };

        router
            .layer(prometheus_layer)
            .layer(TimeoutLayer::new(timeout_duration))
            // 2MB: /translate accepts up to 50 posts × 25k chars.
            .layer(RequestBodyLimitLayer::new(2 * 1024 * 1024))
            .layer(
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(Any)
                    .allow_headers(Any),
            )
            // Resolve the real client IP from proxy headers BEFORE handlers rate-limit on it.
            .layer(middleware::from_fn_with_state(self.app_state.clone(), resolve_client_ip))
            .with_state(self.app_state.clone())
    }

    pub async fn serve(&self, bind_address: &str) -> Result<(), Box<dyn std::error::Error>> {
        let router = self.create_router();
        let listener = TcpListener::bind(bind_address).await?;

        log_info!("Web server starting on {}", bind_address);
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;

        Ok(())
    }
}

// Rate limiting middleware
pub(crate) async fn check_rate_limit(
    state: &AppState,
    client_addr: SocketAddr,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    check_rate_limit_n(state, client_addr, 1).await
}

/// Charge `n` requests at once (e.g. `/identity/batch`, one per address looked up).
pub(crate) async fn check_rate_limit_n(
    state: &AppState,
    client_addr: SocketAddr,
    n: u32,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    let now = Instant::now();
    let mut rate_limits = state.rate_limit_map.write().await;

    let entry = rate_limits.entry(client_addr).or_insert(RateLimitEntry {
        count: 0,
        window_start: now,
    });

    // Reset window if 1 minute has passed
    if now.duration_since(entry.window_start) >= Duration::from_secs(60) {
        entry.count = 0;
        entry.window_start = now;
    }

    entry.count = entry.count.saturating_add(n);

    if entry.count > state.server_config.rate_limit {
        let error = ApiError {
            error: "Rate limit exceeded. Too many requests per minute.".to_string(),
            code: "RATE_LIMIT_EXCEEDED".to_string(),
        };
        return Err((StatusCode::TOO_MANY_REQUESTS, Json(error)));
    }

    Ok(())
}

const RATE_LIMIT_IDLE: Duration = Duration::from_secs(120);

/// Every minute, drop rate-limit entries (general and /translate) whose window started over
/// two minutes ago: their count would be reset on the next request anyway.
fn spawn_rate_limit_pruner(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let now = Instant::now();
            state
                .rate_limit_map
                .write()
                .await
                .retain(|_, e| now.duration_since(e.window_start) < RATE_LIMIT_IDLE);
            crate::translate::prune_rate_limits(&state.translate_rate_limit_map, now, RATE_LIMIT_IDLE).await;
        }
    });
}

/// Extract the real client IP from proxy headers. Prefers the LAST hop of `X-Forwarded-For`:
/// the entry the immediate trusted proxy appended itself (`$proxy_add_x_forwarded_for`), which
/// the client cannot forge (earlier hops are client-supplied and spoofable, so they are never
/// used). `X-Real-IP` is consulted only when there is no `X-Forwarded-For` at all: a proxy that
/// does not set it (Kaspa Quick Start's nginx deliberately does not) passes a client's own
/// `X-Real-IP` straight through, so it must never outrank XFF (IDX-021). Only called when the
/// TCP peer is a trusted proxy; anyone else could put anything in these headers.
fn client_ip_from_headers(headers: &HeaderMap) -> Option<IpAddr> {
    let xff: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    if !xff.is_empty() {
        // Several XFF header lines are one list in order; the last hop is the proxy's own.
        return xff
            .last()
            .and_then(|s| s.split(',').next_back())
            .and_then(|s| s.trim().parse::<IpAddr>().ok());
    }
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
}

/// The client a request is charged to: the forwarded client when the TCP peer is a trusted
/// proxy (`--trusted-proxies`), else the peer itself. IPv4-mapped addresses become IPv4 and
/// IPv6 is keyed by its /64, the block one subscriber is usually given.
fn rate_limit_key(peer: IpAddr, headers: &HeaderMap, trusted: &[crate::config::IpNet]) -> IpAddr {
    let peer = peer.to_canonical();
    let ip = if trusted.iter().any(|n| n.contains(peer)) {
        client_ip_from_headers(headers).map(|ip| ip.to_canonical()).unwrap_or(peer)
    } else {
        peer
    };
    match ip {
        IpAddr::V6(v6) => IpAddr::V6(std::net::Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
        v4 => v4,
    }
}

/// Middleware: rewrite `ConnectInfo<SocketAddr>` to the real client IP so per-IP rate limiting
/// works behind nginx (otherwise every user shares the proxy's single IP). Port is normalized to
/// 0 so the rate-limit map keys purely by client IP. Runs before handlers, which then rate-limit
/// transparently via their existing `ConnectInfo` extractor — no per-handler changes needed.
async fn resolve_client_ip(State(state): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let peer_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    if let Some(peer) = peer_ip {
        let ip = rate_limit_key(peer, req.headers(), &state.server_config.trusted_proxies);
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip, 0)));
    }
    next.run(req).await
}

// API Handler Functions

/// `KACHAT_WEBSERVER_ONLY=names`: serve only the .kachat names, profiles and identity API
/// (the standalone .kachat Domains server). Anything else, or unset, is the full indexer API.
pub fn names_only() -> bool {
    std::env::var("KACHAT_WEBSERVER_ONLY").map(|v| v.trim() == "names").unwrap_or(false)
}

async fn handle_names_root() -> &'static str {
    "KaChat .kachat Domains API"
}

async fn handle_names_health(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    check_rate_limit(&app_state, addr).await?;
    let network = app_state.names.manifest.as_ref().and_then(|m| m.network.clone());
    Ok(Json(serde_json::json!({
        "status": "healthy",
        "service": "kachat-names",
        "version": env!("CARGO_PKG_VERSION"),
        "network": network,
        "names": app_state.names.is_on(),
    })))
}

async fn handle_root() -> &'static str {
    "K-indexer API Server - Posts API v1.0"
}

async fn handle_health(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Query database for network on every health check
    let network = app_state
        .db
        .get_network()
        .await
        .unwrap_or_else(|_| "unknown".to_string());

    Ok(Json(serde_json::json!({
        "status": "healthy",
        "service": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "network": network
    })))
}

async fn handle_stats(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    match app_state.db.get_stats().await {
        Ok(stats) => {
            let mut out = serde_json::json!({
                "broadcasts": stats.broadcasts_count,
                "posts": stats.posts_count,
                "replies": stats.replies_count,
                "quotes": stats.quotes_count,
                "votes": stats.votes_count,
                "follows": stats.follows_count,
                "blocks": stats.blocks_count
            });
            // Merge in the Kaspa Hub → KaChat Stats shape (updatedAt/indexedSince/categories),
            // including the chat indexer's slice. Superset response: old + new consumers both work.
            let extra = build_kachat_stats(&app_state).await;
            if let (Some(o), Some(e)) = (out.as_object_mut(), extra.as_object()) {
                for (k, v) in e {
                    o.insert(k.clone(), v.clone());
                }
            }
            Ok(Json(out))
        }
        Err(e) => {
            log_error!("Failed to get database stats: {}", e);
            let error = ApiError {
                error: "Failed to retrieve database statistics".to_string(),
                code: "INTERNAL_ERROR".to_string(),
            };
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
        }
    }
}

async fn handle_get_posts(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetPostsQuery>,
) -> Result<Json<PaginatedPostsResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;
    // Check if user parameter is provided
    let user_public_key = match params.user {
        Some(user) => user,
        None => {
            let error = ApiError {
                error: "Missing required parameter: user".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated posts for the user with voting status
    match app_state
        .api_handlers
        .get_posts_paginated(
            &user_public_key,
            &requester_pubkey,
            limit,
            params.before,
            params.after,
        )
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedPostsResponse
            match serde_json::from_str::<PaginatedPostsResponse>(&response_json) {
                Ok(posts_response) => Ok(Json(posts_response)),
                Err(err) => {
                    log_error!("Failed to parse paginated posts response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_post_details(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetPostDetailsQuery>,
) -> Result<Json<PostDetailsResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;
    // Check if id parameter is provided
    let post_id = match params.id {
        Some(id) => id,
        None => {
            let error = ApiError {
                error: "Missing required parameter: id".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get post details with voting information and blocking status
    match app_state
        .api_handlers
        .get_post_details(&post_id, &requester_pubkey)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PostDetailsResponse
            match serde_json::from_str::<PostDetailsResponse>(&response_json) {
                Ok(post_details_response) => Ok(Json(post_details_response)),
                Err(err) => {
                    log_error!("Failed to parse post details response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_POST_ID" => StatusCode::BAD_REQUEST,
                        "NOT_FOUND" => StatusCode::NOT_FOUND,
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

/// §5.9: GET /get-poll?postId=&requesterPubkey= → the poll object plus `id`, so a client can
/// refresh one poll's live numbers without reloading the feed. 404 when the id is not an indexed
/// poll. Reuses the get-post path (which already enriches the poll), then returns just the poll.
#[derive(serde::Serialize)]
struct GetPollResponse {
    id: String,
    #[serde(flatten)]
    poll: PollData,
}

async fn handle_get_poll(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetPollQuery>,
) -> Result<Json<GetPollResponse>, (StatusCode, Json<ApiError>)> {
    check_rate_limit(&app_state, addr).await?;

    let post_id = match params.post_id.or(params.id) {
        Some(id) => id,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: "Missing required parameter: postId".to_string(),
                    code: "MISSING_PARAMETER".to_string(),
                }),
            ));
        }
    };
    // requesterPubkey is optional here (a poll's numbers are public); myVote is null without it.
    let requester_pubkey = params.requester_pubkey.unwrap_or_default();

    match app_state
        .api_handlers
        .get_post_details(&post_id, &requester_pubkey)
        .await
    {
        Ok(response_json) => match serde_json::from_str::<PostDetailsResponse>(&response_json) {
            Ok(details) => match details.post.poll {
                Some(poll) => Ok(Json(GetPollResponse { id: post_id, poll })),
                None => Err((
                    StatusCode::NOT_FOUND,
                    Json(ApiError {
                        error: "Not a poll".to_string(),
                        code: "NOT_FOUND".to_string(),
                    }),
                )),
            },
            Err(_) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: "Internal server error".to_string(),
                    code: "INTERNAL_ERROR".to_string(),
                }),
            )),
        },
        Err(_) => Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: "Poll not found".to_string(),
                code: "NOT_FOUND".to_string(),
            }),
        )),
    }
}

// -------------------------------------------------------------- KaChat Stats ---
// GET /stats — transaction counts by category for Kaspa Hub → KaChat Stats. This content indexer
// reports the KaPosts/chess/public-chat categories from Postgres, and aggregates the chat indexer's
// slice (comm/handshakes/payments/groups/self-stash) from 127.0.0.1:8600/stats (same container).
// kchat:1: root only. Cached ~60s to absorb pull-to-refresh. Public, no auth (200 always).

static STATS_CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>>> =
    std::sync::OnceLock::new();
const STATS_TTL: Duration = Duration::from_secs(60);

fn stats_now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The `.kachat` Stats categories and the history ops each counts (KaChat STATS_INDEXER.md).
const KACHAT_STATS_CATEGORIES: [(&str, &[&str]); 5] = [
    ("kachatRegistrations", &["register"]),
    ("kachatRenewals", &["extend", "renew"]),
    ("kachatSales", &["sale", "offer_accepted"]),
    ("kachatOffers", &["offer"]),
    (
        "kachatActivity",
        &["list", "delist", "transfer", "release", "reclaim", "offer_decline", "offer_withdraw", "offer_refund"],
    ),
];

/// Run a `count(*) / FILTER(>d1) / FILTER(>d7)` query, returning (total, last24h, last7d).
async fn stats_count3(pool: &sqlx::PgPool, sql: &str, d1: i64, d7: i64) -> (i64, i64, i64) {
    use sqlx::Row;
    match sqlx::query(sql).bind(d1).bind(d7).fetch_one(pool).await {
        Ok(r) => (
            r.get::<i64, _>("t"),
            r.get::<i64, _>("d1"),
            r.get::<i64, _>("d7"),
        ),
        Err(e) => {
            log_error!("stats query failed: {}", e);
            (0, 0, 0)
        }
    }
}

/// Build the Kaspa Hub → KaChat Stats fields ({updatedAt, indexedSince, categories}) — merged into
/// GET /stats alongside the legacy flat counts. Cached ~60s.
async fn build_kachat_stats(app_state: &Arc<AppState>) -> serde_json::Value {
    // Serve from cache if fresh (absorbs pull-to-refresh bursts).
    let cache = STATS_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((at, value)) = guard.as_ref() {
            if at.elapsed() < STATS_TTL {
                return value.clone();
            }
        }
    }

    let now = stats_now_ms();
    let d1 = now - 86_400_000;
    let d7 = now - 7 * 86_400_000;
    let pool = &app_state.scheduled_pool;

    let cat = |name: &str, c: (i64, i64, i64)| {
        (name.to_string(), serde_json::json!({ "total": c.0, "last24h": c.1, "last7d": c.2 }))
    };
    let mut categories = serde_json::Map::new();

    let kaposts = stats_count3(pool,
        "SELECT count(*) t, count(*) FILTER (WHERE block_time > $1) d1, count(*) FILTER (WHERE block_time > $2) d7 \
         FROM k_contents WHERE content_type IN ('post','reply','quote','poll')", d1, d7).await;
    let (k, v) = cat("kaposts", kaposts); categories.insert(k, v);

    let actions = stats_count3(pool,
        "SELECT count(*) t, count(*) FILTER (WHERE bt > $1) d1, count(*) FILTER (WHERE bt > $2) d7 FROM ( \
           SELECT block_time bt FROM k_votes UNION ALL SELECT block_time FROM k_follows \
           UNION ALL SELECT block_time FROM k_edits UNION ALL SELECT block_time FROM k_deletes \
           UNION ALL SELECT block_time FROM k_poll_votes) a", d1, d7).await;
    let (k, v) = cat("kapostActions", actions); categories.insert(k, v);

    let public_chats = stats_count3(pool,
        "SELECT count(*) t, count(*) FILTER (WHERE block_time > $1) d1, count(*) FILTER (WHERE block_time > $2) d7 \
         FROM kachat_broadcasts WHERE channel <> 'chess-arena'", d1, d7).await;
    let (k, v) = cat("publicChats", public_chats); categories.insert(k, v);

    let chess_moves = stats_count3(pool,
        "SELECT count(*) t, count(*) FILTER (WHERE block_time > $1) d1, count(*) FILTER (WHERE block_time > $2) d7 \
         FROM kachat_broadcasts WHERE channel = 'chess-arena' AND content LIKE '%\"a\":\"move\"%'", d1, d7).await;
    let (k, v) = cat("chessMoves", chess_moves); categories.insert(k, v);

    // chessGames: games started per the leaderboard reducer (reuses the cached chess snapshot).
    let (_, _, games_started) = chess_snapshot(app_state).await;
    categories.insert("chessGames".to_string(), serde_json::json!({ "total": games_started }));

    // .kachat (KACHAT_NAMES_REGISTRY_V3.md §10): counted from the names follower's history, one
    // per transaction, never the price record's. Only where the names module is on.
    if app_state.names.is_on() {
        for (key, ops) in KACHAT_STATS_CATEGORIES {
            let list = ops.iter().map(|o| format!("'{o}'")).collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT count(DISTINCT tx_id) t, count(DISTINCT tx_id) FILTER (WHERE at > $1) d1, \
                 count(DISTINCT tx_id) FILTER (WHERE at > $2) d7 FROM names_history WHERE op IN ({list})"
            );
            let (k, v) = cat(key, stats_count3(pool, &sql, d1, d7).await);
            categories.insert(k, v);
        }
    }

    // indexedSince = earliest kchat:1: content we hold (honest "counting since").
    let indexed_since: Option<i64> = {
        use sqlx::Row;
        sqlx::query("SELECT min(block_time) m FROM k_contents")
            .fetch_one(pool)
            .await
            .ok()
            .and_then(|r| r.try_get::<i64, _>("m").ok())
    };

    // Aggregate the chat indexer's slice (same container, 127.0.0.1:8600). Best-effort: on any
    // failure we just return our own categories, so a split deployment still works.
    let chat_url = std::env::var("CHAT_STATS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8600/stats".to_string());
    if let Ok(resp) = app_state.http.get(&chat_url).timeout(Duration::from_secs(5)).send().await {
        if resp.status().is_success() {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                if let Some(obj) = body.get("categories").and_then(|c| c.as_object()) {
                    for (k, v) in obj {
                        categories.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        }
    }

    let mut out = serde_json::Map::new();
    out.insert("updatedAt".to_string(), serde_json::json!(now));
    if let Some(since) = indexed_since {
        out.insert("indexedSince".to_string(), serde_json::json!(since));
    }
    out.insert("categories".to_string(), serde_json::Value::Object(categories));
    let value = serde_json::Value::Object(out);

    if let Ok(mut guard) = cache.lock() {
        *guard = Some((std::time::Instant::now(), value.clone()));
    }
    value
}

async fn handle_get_thread(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetPostDetailsQuery>,
) -> Result<Json<GetThreadResponse>, (StatusCode, Json<ApiError>)> {
    check_rate_limit(&app_state, addr).await?;

    let post_id = match params.id {
        Some(id) => id,
        None => {
            let error = ApiError {
                error: "Missing required parameter: id".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    match app_state
        .api_handlers
        .get_thread(&post_id, &requester_pubkey)
        .await
    {
        Ok(response_json) => match serde_json::from_str::<GetThreadResponse>(&response_json) {
            Ok(thread_response) => Ok(Json(thread_response)),
            Err(err) => {
                log_error!("Failed to parse thread response: {}", err);
                let error = ApiError {
                    error: "Internal server error".to_string(),
                    code: "INTERNAL_ERROR".to_string(),
                };
                Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
            }
        },
        Err(error_json) => match serde_json::from_str::<ApiError>(&error_json) {
            Ok(api_error) => {
                let status_code = match api_error.code.as_str() {
                    "MISSING_PARAMETER" | "INVALID_POST_ID" | "INVALID_USER_KEY" => {
                        StatusCode::BAD_REQUEST
                    }
                    "NOT_FOUND" => StatusCode::NOT_FOUND,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                Err((status_code, Json(api_error)))
            }
            Err(_) => {
                let error = ApiError {
                    error: "Internal server error".to_string(),
                    code: "INTERNAL_ERROR".to_string(),
                };
                Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
            }
        },
    }
}

async fn handle_get_mentions(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetMentionsQuery>,
) -> Result<Json<PaginatedPostsResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;
    // Check if user parameter is provided
    let user_public_key = match params.user {
        Some(user) => user,
        None => {
            let error = ApiError {
                error: "Missing required parameter: user".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated mentions for the user with voting status
    match app_state
        .api_handlers
        .get_mentions_paginated(
            &user_public_key,
            &requester_pubkey,
            limit,
            params.before,
            params.after,
        )
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedPostsResponse
            match serde_json::from_str::<PaginatedPostsResponse>(&response_json) {
                Ok(mentions_response) => Ok(Json(mentions_response)),
                Err(err) => {
                    log_error!("Failed to parse paginated mentions response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_notifications(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetNotificationsQuery>,
) -> Result<Json<PaginatedNotificationsResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated notifications for the user
    match app_state
        .api_handlers
        .get_notifications_paginated(&requester_pubkey, limit, params.before, params.after)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedNotificationsResponse
            match serde_json::from_str::<PaginatedNotificationsResponse>(&response_json) {
                Ok(notifications_response) => Ok(Json(notifications_response)),
                Err(err) => {
                    log_error!("Failed to parse paginated notifications response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_hashtag_content(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetHashtagContentQuery>,
) -> Result<Json<PaginatedPostsResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Check if hashtag parameter is provided
    let hashtag = match params.hashtag {
        Some(tag) => tag.to_lowercase(), // Normalize to lowercase
        None => {
            let error = ApiError {
                error: "Missing required parameter: hashtag".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate hashtag length (max 30 characters, without #)
    if hashtag.is_empty() {
        let error = ApiError {
            error: "Hashtag parameter cannot be empty".to_string(),
            code: "INVALID_PARAMETER".to_string(),
        };
        return Err((StatusCode::BAD_REQUEST, Json(error)));
    }

    if hashtag.len() > 30 {
        let error = ApiError {
            error: "Hashtag parameter cannot exceed 30 characters".to_string(),
            code: "INVALID_PARAMETER".to_string(),
        };
        return Err((StatusCode::BAD_REQUEST, Json(error)));
    }

    // Limit defaults to 20 if not provided
    let limit = params.limit.unwrap_or(20);

    // Validate limit parameter
    if limit < 1 || limit > 100 {
        let error = ApiError {
            error: "Limit parameter must be between 1 and 100".to_string(),
            code: "INVALID_LIMIT".to_string(),
        };
        return Err((StatusCode::BAD_REQUEST, Json(error)));
    }

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated hashtag content
    match app_state
        .api_handlers
        .get_hashtag_content_paginated(
            &hashtag,
            &requester_pubkey,
            limit,
            params.before,
            params.after,
        )
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedPostsResponse
            match serde_json::from_str::<PaginatedPostsResponse>(&response_json) {
                Ok(posts_response) => Ok(Json(posts_response)),
                Err(err) => {
                    log_error!(
                        "Failed to parse paginated hashtag content response: {}",
                        err
                    );
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_PARAMETER" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        "NOT_FOUND" => StatusCode::NOT_FOUND,
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_users(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetUsersQuery>,
) -> Result<Json<PaginatedUsersResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;
    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated user introduction posts with block status
    match app_state
        .api_handlers
        .get_users_paginated(limit, &requester_pubkey, params.before, params.after)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedUsersResponse
            match serde_json::from_str::<PaginatedUsersResponse>(&response_json) {
                Ok(users_response) => Ok(Json(users_response)),
                Err(err) => {
                    log_error!("Failed to parse paginated users response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "DATABASE_ERROR" | "SERIALIZATION_ERROR" => {
                            StatusCode::INTERNAL_SERVER_ERROR
                        }
                        "MISSING_PARAMETER" | "INVALID_LIMIT" => StatusCode::BAD_REQUEST,
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_most_active_users(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetMostActiveUsersQuery>,
) -> Result<Json<PaginatedUsersResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Parse and validate time_window parameter (required)
    let time_window = match params.time_window {
        Some(tw) => tw,
        None => {
            let error = ApiError {
                error: "Missing required parameter: timeWindow".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    let valid_windows = ["1h", "6h", "24h", "7d", "30d"];
    if !valid_windows.contains(&time_window.as_str()) {
        let error = ApiError {
            error: format!(
                "Invalid timeWindow parameter. Must be one of: {}",
                valid_windows.join(", ")
            ),
            code: "INVALID_PARAMETER".to_string(),
        };
        return Err((StatusCode::BAD_REQUEST, Json(error)));
    }

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get most active users ranked by content count
    match app_state
        .api_handlers
        .get_most_active_users_paginated(
            limit,
            &requester_pubkey,
            &time_window,
            params.before,
            params.after,
        )
        .await
    {
        Ok(response_json) => match serde_json::from_str::<PaginatedUsersResponse>(&response_json) {
            Ok(users_response) => Ok(Json(users_response)),
            Err(err) => {
                log_error!("Failed to parse most active users response: {}", err);
                let error = ApiError {
                    error: "Internal server error".to_string(),
                    code: "INTERNAL_ERROR".to_string(),
                };
                Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
            }
        },
        Err(error_json) => match serde_json::from_str::<ApiError>(&error_json) {
            Ok(api_error) => {
                let status_code = match api_error.code.as_str() {
                    "DATABASE_ERROR" | "SERIALIZATION_ERROR" => StatusCode::INTERNAL_SERVER_ERROR,
                    "MISSING_PARAMETER" | "INVALID_LIMIT" | "INVALID_PARAMETER" => {
                        StatusCode::BAD_REQUEST
                    }
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                Err((status_code, Json(api_error)))
            }
            Err(_) => {
                let error = ApiError {
                    error: "Internal server error".to_string(),
                    code: "INTERNAL_ERROR".to_string(),
                };
                Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
            }
        },
    }
}

/// Rate-limit units one `/search` request costs (kachat-audits IDX-022).
const SEARCH_RATE_COST: u32 = 5;
/// Shortest accepted `/search` query, in characters (kachat-audits IDX-022).
const SEARCH_MIN_QUERY_CHARS: usize = 3;

/// GET /search?q=&type=posts|users (§5.6). Dispatches to post-content or user search; both share
/// the feed's pagination envelope. Returns a raw JSON value since the two payloads differ in shape.
async fn handle_search(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    // kachat-audits IDX-022: a substring search scans far more than a cached GET, so it costs
    // SEARCH_RATE_COST units of the per-IP limit.
    check_rate_limit_n(&app_state, addr, SEARCH_RATE_COST).await?;

    // Required: non-empty q.
    let query_text = params.q.unwrap_or_default();
    let query_text = query_text.trim();
    if query_text.is_empty() {
        let error = ApiError {
            error: "Missing required parameter: q".to_string(),
            code: "MISSING_PARAMETER".to_string(),
        };
        return Err((StatusCode::BAD_REQUEST, Json(error)));
    }
    // At least 3 characters: shorter patterns match nearly everything and a trigram index
    // cannot serve them (IDX-022).
    if query_text.chars().count() < SEARCH_MIN_QUERY_CHARS {
        let error = ApiError {
            error: format!("Parameter q must be at least {SEARCH_MIN_QUERY_CHARS} characters"),
            code: "QUERY_TOO_SHORT".to_string(),
        };
        return Err((StatusCode::BAD_REQUEST, Json(error)));
    }

    // Required: limit in 1..=100.
    let limit = match params.limit {
        Some(limit) if (1..=100).contains(&limit) => limit,
        Some(_) => {
            let error = ApiError {
                error: "Limit parameter must be between 1 and 100".to_string(),
                code: "INVALID_LIMIT".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Required: requesterPubkey (used for per-viewer decoration).
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    let search_type = params
        .search_type
        .unwrap_or_else(|| "posts".to_string())
        .to_lowercase();

    let result = match search_type.as_str() {
        "users" => {
            app_state
                .api_handlers
                .search_users_posted_paginated(
                    &requester_pubkey,
                    query_text,
                    limit,
                    params.before,
                    params.after,
                )
                .await
        }
        // Default and "posts" both search post/quote content.
        _ => {
            app_state
                .api_handlers
                .search_posts_paginated(
                    &requester_pubkey,
                    query_text,
                    limit,
                    params.before,
                    params.after,
                )
                .await
        }
    };

    match result {
        Ok(response_json) => match serde_json::from_str::<serde_json::Value>(&response_json) {
            Ok(value) => Ok(Json(value)),
            Err(err) => {
                log_error!("Failed to parse search response: {}", err);
                let error = ApiError {
                    error: "Internal server error".to_string(),
                    code: "INTERNAL_ERROR".to_string(),
                };
                Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
            }
        },
        Err(error_json) => {
            let (status_code, api_error) = match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status = match api_error.code.as_str() {
                        "DATABASE_ERROR" | "SERIALIZATION_ERROR" => {
                            StatusCode::INTERNAL_SERVER_ERROR
                        }
                        _ => StatusCode::BAD_REQUEST,
                    };
                    (status, api_error)
                }
                Err(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    },
                ),
            };
            Err((status_code, Json(api_error)))
        }
    }
}

async fn handle_search_users(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<SearchUsersQuery>,
) -> Result<Json<PaginatedUsersResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to search users
    match app_state
        .api_handlers
        .search_users_paginated(
            limit,
            &requester_pubkey,
            params.before,
            params.after,
            params.searched_user_pubkey,
            params.searched_user_nickname,
        )
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedUsersResponse
            match serde_json::from_str::<PaginatedUsersResponse>(&response_json) {
                Ok(users_response) => Ok(Json(users_response)),
                Err(err) => {
                    log_error!("Failed to parse search users response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "DATABASE_ERROR" | "SERIALIZATION_ERROR" => {
                            StatusCode::INTERNAL_SERVER_ERROR
                        }
                        "MISSING_PARAMETER" | "INVALID_LIMIT" | "INVALID_USER_KEY" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_posts_watching(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetPostsWatchingQuery>,
) -> Result<Json<PaginatedPostsResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;
    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated posts for watching with voting status
    match app_state
        .api_handlers
        .get_posts_watching_paginated(&requester_pubkey, limit, params.before, params.after)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedPostsResponse
            match serde_json::from_str::<PaginatedPostsResponse>(&response_json) {
                Ok(posts_response) => Ok(Json(posts_response)),
                Err(err) => {
                    log_error!("Failed to parse paginated posts response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "DATABASE_ERROR" | "SERIALIZATION_ERROR" => {
                            StatusCode::INTERNAL_SERVER_ERROR
                        }
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_contents_following(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetContentsFollowingQuery>,
) -> Result<Json<PaginatedPostsResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;
    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated content from followed users
    match app_state
        .api_handlers
        .get_content_following_paginated(&requester_pubkey, limit, params.before, params.after)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedPostsResponse
            match serde_json::from_str::<PaginatedPostsResponse>(&response_json) {
                Ok(posts_response) => Ok(Json(posts_response)),
                Err(err) => {
                    log_error!("Failed to parse paginated content response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "DATABASE_ERROR" | "SERIALIZATION_ERROR" => {
                            StatusCode::INTERNAL_SERVER_ERROR
                        }
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

/// Fork addition: GET /get-broadcasts?channel=&limit=&before= — KaChat broadcast history for a
/// tracked channel, served on the same host as KaPosts (same indexer URL). Newest-first.
/// A missing or unknown channel returns 200 with empty messages (per BROADCAST_INDEXER.md).
/// GET /chess/leaderboard?limit=100 — the replayed arena leaderboard (§6).
async fn handle_chess_leaderboard(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<ChessLeaderboardQuery>,
) -> Result<Json<ChessLeaderboardResponse>, (StatusCode, Json<ApiError>)> {
    check_rate_limit(&app_state, addr).await?;
    let (players, _, _) = chess_snapshot(&app_state).await;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000) as usize;
    Ok(Json(ChessLeaderboardResponse {
        players: players.into_iter().take(limit).collect(),
        generated_at: now_ms(),
    }))
}

/// GET /chess/player?address= — one player's leaderboard row (zeroed if they've never played).
async fn handle_chess_player(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<ChessPlayerQuery>,
) -> Result<Json<ChessPlayerRow>, (StatusCode, Json<ApiError>)> {
    check_rate_limit(&app_state, addr).await?;
    let address = match params.address {
        Some(a) if !a.trim().is_empty() => a.trim().to_string(),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: "Missing required parameter: address".to_string(),
                    code: "MISSING_PARAMETER".to_string(),
                }),
            ));
        }
    };
    let (players, _, _) = chess_snapshot(&app_state).await;
    let row = players
        .into_iter()
        .find(|r| r.address == address)
        .unwrap_or(ChessPlayerRow {
            address,
            wins: 0,
            losses: 0,
            duel_wins: 0,
            duel_losses: 0,
            tournament_game_wins: 0,
            tournament_game_losses: 0,
            tournaments_played: 0,
            tournaments_won: 0,
            tournaments_lost: 0,
            last_played_at: 0,
        });
    Ok(Json(row))
}

/// GET /chess/tournaments?status=open|live|done&limit= — the precomputed lobby list (§6, optional).
async fn handle_chess_tournaments(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<ChessTournamentsQuery>,
) -> Result<Json<ChessTournamentsResponse>, (StatusCode, Json<ApiError>)> {
    check_rate_limit(&app_state, addr).await?;
    let (_, mut tournaments, _) = chess_snapshot(&app_state).await;
    if let Some(status) = params.status.as_deref() {
        let status = status.trim().to_lowercase();
        if !status.is_empty() {
            tournaments.retain(|t| t.status == status);
        }
    }
    let limit = params.limit.unwrap_or(200).clamp(1, 1000) as usize;
    tournaments.truncate(limit);
    Ok(Json(ChessTournamentsResponse {
        tournaments,
        generated_at: now_ms(),
    }))
}

async fn handle_get_broadcasts(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetBroadcastsQuery>,
) -> Result<Json<BroadcastsResponse>, (StatusCode, Json<ApiError>)> {
    check_rate_limit(&app_state, addr).await?;

    let channel = params.channel.unwrap_or_default().trim().to_lowercase();
    if channel.is_empty() {
        return Ok(Json(BroadcastsResponse {
            messages: Vec::new(),
            has_more: false,
        }));
    }
    let limit = params.limit.unwrap_or(200).clamp(1, 500);

    match app_state
        .db
        .get_broadcasts(&channel, limit, params.before)
        .await
    {
        Ok((messages, has_more)) => Ok(Json(BroadcastsResponse { messages, has_more })),
        Err(err) => {
            log_error!("Failed to fetch broadcasts for channel {}: {}", channel, err);
            let error = ApiError {
                error: "Internal server error".to_string(),
                code: "INTERNAL_ERROR".to_string(),
            };
            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
        }
    }
}

/// Fork addition: GET /get-post-engagement?postId=&type=&requesterPubkey=&limit=&before=
/// Returns actors who upvoted/downvoted/reposted/quoted a post. `type` defaults to "all".
async fn handle_get_post_engagement(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetPostEngagementQuery>,
) -> Result<Json<PaginatedEngagementResponse>, (StatusCode, Json<ApiError>)> {
    check_rate_limit(&app_state, addr).await?;

    let post_id = match params.post_id {
        Some(post_id) => post_id,
        None => {
            let error = ApiError {
                error: "Missing required parameter: postId".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // limit defaults to 50 and is capped at 100
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => 50,
    };

    let engagement_type = params.engagement_type.unwrap_or_else(|| "all".to_string());

    match app_state
        .api_handlers
        .get_post_engagement_paginated(&post_id, &engagement_type, limit, params.before, params.after)
        .await
    {
        Ok(response_json) => match serde_json::from_str::<PaginatedEngagementResponse>(&response_json)
        {
            Ok(response) => Ok(Json(response)),
            Err(err) => {
                log_error!("Failed to parse post engagement response: {}", err);
                let error = ApiError {
                    error: "Internal server error".to_string(),
                    code: "INTERNAL_ERROR".to_string(),
                };
                Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
            }
        },
        Err(error_json) => match serde_json::from_str::<ApiError>(&error_json) {
            Ok(api_error) => {
                let status_code = match api_error.code.as_str() {
                    "MISSING_PARAMETER" | "INVALID_POST_ID" | "INVALID_PARAMETER"
                    | "INVALID_LIMIT" => StatusCode::BAD_REQUEST,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                Err((status_code, Json(api_error)))
            }
            Err(_) => {
                let error = ApiError {
                    error: "Internal server error".to_string(),
                    code: "INTERNAL_ERROR".to_string(),
                };
                Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
            }
        },
    }
}

async fn handle_get_replies(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetRepliesQuery>,
) -> Result<Json<PaginatedRepliesResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;
    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if exactly one of post or user parameter is provided
    match (params.post.as_ref(), params.user.as_ref()) {
        (Some(post_id), None) => {
            // Post replies mode: get replies to a specific post
            match app_state
                .api_handlers
                .get_replies_paginated(
                    post_id,
                    &requester_pubkey,
                    limit,
                    params.before,
                    params.after,
                )
                .await
            {
                Ok(response_json) => {
                    match serde_json::from_str::<PaginatedRepliesResponse>(&response_json) {
                        Ok(replies_response) => Ok(Json(replies_response)),
                        Err(err) => {
                            log_error!("Failed to parse paginated replies response: {}", err);
                            let error = ApiError {
                                error: "Internal server error".to_string(),
                                code: "INTERNAL_ERROR".to_string(),
                            };
                            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                        }
                    }
                }
                Err(error_json) => match serde_json::from_str::<ApiError>(&error_json) {
                    Ok(api_error) => {
                        let status_code = match api_error.code.as_str() {
                            "MISSING_PARAMETER" | "INVALID_POST_ID" | "INVALID_USER_KEY"
                            | "INVALID_LIMIT" => StatusCode::BAD_REQUEST,
                            _ => StatusCode::INTERNAL_SERVER_ERROR,
                        };
                        Err((status_code, Json(api_error)))
                    }
                    Err(_) => {
                        let error = ApiError {
                            error: "Internal server error".to_string(),
                            code: "INTERNAL_ERROR".to_string(),
                        };
                        Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                    }
                },
            }
        }
        (None, Some(user_public_key)) => {
            // User replies mode: get all replies made by a specific user
            match app_state
                .api_handlers
                .get_user_replies_paginated(
                    user_public_key,
                    &requester_pubkey,
                    limit,
                    params.before,
                    params.after,
                )
                .await
            {
                Ok(response_json) => {
                    match serde_json::from_str::<PaginatedRepliesResponse>(&response_json) {
                        Ok(replies_response) => Ok(Json(replies_response)),
                        Err(err) => {
                            log_error!("Failed to parse paginated user replies response: {}", err);
                            let error = ApiError {
                                error: "Internal server error".to_string(),
                                code: "INTERNAL_ERROR".to_string(),
                            };
                            Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                        }
                    }
                }
                Err(error_json) => match serde_json::from_str::<ApiError>(&error_json) {
                    Ok(api_error) => {
                        let status_code = match api_error.code.as_str() {
                            "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                                StatusCode::BAD_REQUEST
                            }
                            _ => StatusCode::INTERNAL_SERVER_ERROR,
                        };
                        Err((status_code, Json(api_error)))
                    }
                    Err(_) => {
                        let error = ApiError {
                            error: "Internal server error".to_string(),
                            code: "INTERNAL_ERROR".to_string(),
                        };
                        Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                    }
                },
            }
        }
        (Some(_), Some(_)) => {
            // Both parameters provided - not allowed
            let error = ApiError {
                error: "Cannot provide both 'post' and 'user' parameters. Use 'post' for post replies or 'user' for user replies.".to_string(),
                code: "INVALID_PARAMETERS".to_string(),
            };
            Err((StatusCode::BAD_REQUEST, Json(error)))
        }
        (None, None) => {
            // Neither parameter provided - not allowed
            let error = ApiError {
                error: "Missing required parameter: either 'post' or 'user' must be provided"
                    .to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            Err((StatusCode::BAD_REQUEST, Json(error)))
        }
    }
}

async fn handle_get_user_details(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetUserDetailsQuery>,
) -> Result<Json<ServerUserPost>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Check if user parameter is provided
    let user_public_key = match params.user {
        Some(user) => user,
        None => {
            let error = ApiError {
                error: "Missing required parameter: user".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get user details
    match app_state
        .api_handlers
        .get_user_details(&user_public_key, &requester_pubkey)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to ServerUserPost
            match serde_json::from_str::<ServerUserPost>(&response_json) {
                Ok(user_details_response) => Ok(Json(user_details_response)),
                Err(err) => {
                    log_error!("Failed to parse user details response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" => StatusCode::BAD_REQUEST,
                        "USER_NOT_FOUND" => StatusCode::NOT_FOUND,
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_blocked_users(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetBlockedUsersQuery>,
) -> Result<Json<PaginatedUsersResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated blocked users
    match app_state
        .api_handlers
        .get_blocked_users_paginated(&requester_pubkey, limit, params.before, params.after)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedUsersResponse
            match serde_json::from_str::<PaginatedUsersResponse>(&response_json) {
                Ok(users_response) => Ok(Json(users_response)),
                Err(err) => {
                    log_error!("Failed to parse paginated blocked users response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_followed_users(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetFollowedUsersQuery>,
) -> Result<Json<PaginatedUsersResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated followed users
    match app_state
        .api_handlers
        .get_followed_users_paginated(&requester_pubkey, limit, params.before, params.after)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedUsersResponse
            match serde_json::from_str::<PaginatedUsersResponse>(&response_json) {
                Ok(users_response) => Ok(Json(users_response)),
                Err(err) => {
                    log_error!("Failed to parse paginated followed users response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_users_following(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetUsersFollowingQuery>,
) -> Result<Json<PaginatedUsersResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if userPubkey parameter is provided
    let user_pubkey = match params.user_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: userPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated users following
    match app_state
        .api_handlers
        .get_users_following_paginated(
            &requester_pubkey,
            &user_pubkey,
            limit,
            params.before,
            params.after,
        )
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedUsersResponse
            match serde_json::from_str::<PaginatedUsersResponse>(&response_json) {
                Ok(users_response) => Ok(Json(users_response)),
                Err(err) => {
                    log_error!(
                        "Failed to parse paginated users following response: {}",
                        err
                    );
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_users_followers(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetUsersFollowersQuery>,
) -> Result<Json<PaginatedUsersResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Check if userPubkey parameter is provided
    let user_pubkey = match params.user_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: userPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Validate required limit parameter
    let limit = match params.limit {
        Some(limit) => {
            if limit < 1 || limit > 100 {
                let error = ApiError {
                    error: "Limit parameter must be between 1 and 100".to_string(),
                    code: "INVALID_LIMIT".to_string(),
                };
                return Err((StatusCode::BAD_REQUEST, Json(error)));
            }
            limit
        }
        None => {
            let error = ApiError {
                error: "Missing required parameter: limit".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get paginated users followers
    match app_state
        .api_handlers
        .get_users_followers_paginated(
            &requester_pubkey,
            &user_pubkey,
            limit,
            params.before,
            params.after,
        )
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to PaginatedUsersResponse
            match serde_json::from_str::<PaginatedUsersResponse>(&response_json) {
                Ok(users_response) => Ok(Json(users_response)),
                Err(err) => {
                    log_error!(
                        "Failed to parse paginated users followers response: {}",
                        err
                    );
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" | "INVALID_LIMIT" => {
                            StatusCode::BAD_REQUEST
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_notifications_count(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetNotificationsCountQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Check if requesterPubkey parameter is provided
    let requester_pubkey = match params.requester_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let error = ApiError {
                error: "Missing required parameter: requesterPubkey".to_string(),
                code: "MISSING_PARAMETER".to_string(),
            };
            return Err((StatusCode::BAD_REQUEST, Json(error)));
        }
    };

    // Use the API handler to get notification count
    match app_state
        .api_handlers
        .get_notification_count(&requester_pubkey, params.after)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to a generic JSON value
            match serde_json::from_str::<serde_json::Value>(&response_json) {
                Ok(response) => Ok(Json(response)),
                Err(err) => {
                    log_error!("Failed to parse notification count response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "MISSING_PARAMETER" | "INVALID_USER_KEY" => StatusCode::BAD_REQUEST,
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_users_count(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(_params): Query<GetUsersCountQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Use the API handler to get users count
    match app_state.api_handlers.get_users_count().await {
        Ok(response_json) => {
            // Parse the JSON response back to a generic JSON value
            match serde_json::from_str::<serde_json::Value>(&response_json) {
                Ok(response) => Ok(Json(response)),
                Err(err) => {
                    log_error!("Failed to parse users count response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

async fn handle_get_trending_hashtags(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<GetTrendingHashtagsQuery>,
) -> Result<Json<TrendingHashtagsResponse>, (StatusCode, Json<ApiError>)> {
    // Check rate limit first
    check_rate_limit(&app_state, addr).await?;

    // Parse and validate time_window parameter (default: "24h")
    let time_window = params.time_window.unwrap_or_else(|| "24h".to_string());

    // Validate time_window values
    let valid_windows = ["1h", "6h", "24h", "7d", "30d"];
    if !valid_windows.contains(&time_window.as_str()) {
        let error = ApiError {
            error: format!(
                "Invalid timeWindow parameter. Must be one of: {}",
                valid_windows.join(", ")
            ),
            code: "INVALID_PARAMETER".to_string(),
        };
        return Err((StatusCode::BAD_REQUEST, Json(error)));
    }

    // Limit defaults to 20 if not provided
    let limit = params.limit.unwrap_or(20);

    // Validate limit parameter
    if limit < 1 || limit > 100 {
        let error = ApiError {
            error: "Limit parameter must be between 1 and 100".to_string(),
            code: "INVALID_LIMIT".to_string(),
        };
        return Err((StatusCode::BAD_REQUEST, Json(error)));
    }

    // Use the API handler to get trending hashtags
    match app_state
        .api_handlers
        .get_trending_hashtags(&time_window, limit)
        .await
    {
        Ok(response_json) => {
            // Parse the JSON response back to TrendingHashtagsResponse
            match serde_json::from_str::<TrendingHashtagsResponse>(&response_json) {
                Ok(response) => Ok(Json(response)),
                Err(err) => {
                    log_error!("Failed to parse trending hashtags response: {}", err);
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
        Err(error_json) => {
            // Parse the error response
            match serde_json::from_str::<ApiError>(&error_json) {
                Ok(api_error) => {
                    let status_code = match api_error.code.as_str() {
                        "INVALID_PARAMETER" | "INVALID_LIMIT" => StatusCode::BAD_REQUEST,
                        _ => StatusCode::INTERNAL_SERVER_ERROR,
                    };
                    Err((status_code, Json(api_error)))
                }
                Err(_) => {
                    let error = ApiError {
                        error: "Internal server error".to_string(),
                        code: "INTERNAL_ERROR".to_string(),
                    };
                    Err((StatusCode::INTERNAL_SERVER_ERROR, Json(error)))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(real_ip: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-real-ip", real_ip.parse().unwrap());
        h
    }

    #[test]
    fn forwarded_ip_only_from_a_trusted_proxy() {
        let trusted = crate::config::parse_trusted_proxies(crate::config::DEFAULT_TRUSTED_PROXIES).unwrap();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // nginx on the docker network: the header names the client.
        assert_eq!(rate_limit_key(ip("172.18.0.3"), &headers("8.8.8.8"), &trusted), ip("8.8.8.8"));
        // A direct client cannot pick its own identity.
        assert_eq!(rate_limit_key(ip("9.9.9.9"), &headers("8.8.8.8"), &trusted), ip("9.9.9.9"));
        assert_eq!(rate_limit_key(ip("9.9.9.9"), &headers("8.8.8.8"), &[]), ip("9.9.9.9"));
        // IPv6 is keyed by its /64; IPv4-mapped addresses as IPv4.
        assert_eq!(rate_limit_key(ip("2001:db8:1:2:aaaa::1"), &HeaderMap::new(), &trusted), ip("2001:db8:1:2::"));
        assert_eq!(rate_limit_key(ip("::ffff:9.9.9.9"), &HeaderMap::new(), &trusted), ip("9.9.9.9"));
        assert_eq!(rate_limit_key(ip("127.0.0.1"), &headers("2001:db8::5"), &trusted), ip("2001:db8::"));
    }

    fn with_xff(mut h: HeaderMap, xff: &str) -> HeaderMap {
        h.append("x-forwarded-for", xff.parse().unwrap());
        h
    }

    #[test]
    fn last_forwarded_hop_beats_a_spoofed_real_ip() {
        // IDX-021: KQS's nginx does not set X-Real-IP, so a client's own header reaches us; the
        // hop nginx appended to X-Forwarded-For is the truth.
        let trusted = crate::config::parse_trusted_proxies(crate::config::DEFAULT_TRUSTED_PROXIES).unwrap();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let spoofed = with_xff(headers("1.2.3.4"), "5.6.7.8, 8.8.8.8");
        assert_eq!(rate_limit_key(ip("172.18.0.3"), &spoofed, &trusted), ip("8.8.8.8"));
        // Several XFF lines: the last line's last hop.
        let two = with_xff(with_xff(HeaderMap::new(), "5.6.7.8"), "9.9.9.9");
        assert_eq!(rate_limit_key(ip("172.18.0.3"), &two, &trusted), ip("9.9.9.9"));
        // An unparsable last hop does not fall back to X-Real-IP (or a spoofable earlier hop).
        let junk = with_xff(headers("1.2.3.4"), "5.6.7.8, nope");
        assert_eq!(rate_limit_key(ip("172.18.0.3"), &junk, &trusted), ip("172.18.0.3"));
        // Untrusted peer: headers ignored, the peer itself.
        assert_eq!(rate_limit_key(ip("9.9.9.9"), &spoofed, &trusted), ip("9.9.9.9"));
    }
}

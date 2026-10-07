//! `.kachat` read API (KACHAT_NAMES_INDEXER.md Part D, exactly as the apps decode it —
//! docs/KACHAT_NAMES_APP_CONTRACT.md §2), served from the tables `kachat-names-follower`
//! keeps (`names_state`, `names_utxos`, `names_history`, `names_profiles`).
//!
//! Every lookup answers 503 `syncing` until the follower reports `synced` (caught up and a
//! clean self-test), its heartbeat is fresh, and it follows the same registry as the loaded
//! manifest — the apps switch to this indexer on `/names/status`, so a half-built registry
//! must never be served. Rows the self-test found missing on the node (`refuted`) are
//! withheld. Owners/buyers are addresses, amounts are sompi strings, times are unix ms.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use kachat_names::{NameStatus, name_status};
use kaspa_addresses::{Address, Prefix, Version};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgRow};
use std::sync::Arc;

use crate::names::{name_key_hex, normalize_name};
use crate::web_server::AppState;

/// The follower writes its status every poll; older than this it is treated as down.
const STALE_MS: i64 = 120_000;
const MAX_BATCH: usize = 200;

/// The follower's row in `names_state`, if the tables exist and belong to `registry`.
pub struct FollowerStatus {
    pub indexed_daa: i64,
    pub synced: bool,
    pub grace_ms: i64,
    pub network: Option<String>,
    /// Registry v3: the price covenant the follower tracks.
    pub price_covenant_id: Option<String>,
    /// Why it cannot progress (`start_block_pruned`), and the block it cannot start from.
    pub fatal_reason: Option<String>,
    pub start_block: Option<Vec<u8>>,
    /// The DAA at which the registry was rebuilt from the REST API (0 = never).
    pub bootstrapped_at: i64,
}

pub async fn follower_status(pool: &PgPool, registry: &str) -> Option<FollowerStatus> {
    let row = sqlx::query(
        "SELECT registry_covenant_id, network, indexed_daa, synced, grace_ms, updated_at, price_covenant_id, fatal_reason, start_block, bootstrapped_at FROM names_state WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
    .ok()??;
    let stored: String = row.get("registry_covenant_id");
    if !stored.eq_ignore_ascii_case(registry) {
        return None;
    }
    let fresh = now_ms() - row.get::<i64, _>("updated_at") <= STALE_MS;
    Some(FollowerStatus {
        indexed_daa: row.get("indexed_daa"),
        synced: row.get::<bool, _>("synced") && fresh,
        grace_ms: row.get("grace_ms"),
        network: row.get("network"),
        price_covenant_id: row.get("price_covenant_id"),
        fatal_reason: row.get("fatal_reason"),
        start_block: row.get("start_block"),
        bootstrapped_at: row.try_get::<i64, _>("bootstrapped_at").unwrap_or(0),
    })
}

/// A v3 manifest's price covenant must be the one the follower tracks; v2 has none.
pub fn same_price_covenant(manifest: Option<&str>, follower: Option<&str>) -> bool {
    match manifest {
        None => true,
        Some(m) => follower.is_some_and(|f| f.eq_ignore_ascii_case(m)),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn err(code: StatusCode, error: &str, message: &str) -> Response {
    (code, Json(json!({ "error": error, "message": message }))).into_response()
}

/// Everything a handler needs once the registry is servable.
struct Ctx {
    pool: PgPool,
    prefix: Prefix,
    grace_ms: i64,
    indexed_daa: i64,
    now: i64,
}

async fn ctx(state: &AppState) -> Result<Ctx, Response> {
    let Some(registry) = state.names.manifest.as_ref().and_then(|m| m.registry_covenant_id.clone()) else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "names module is off (no manifest)"));
    };
    // Registry v3: the follower must also follow the manifest's price covenant.
    let price_id = state.names.manifest.as_ref().and_then(|m| m.price_covenant_id.clone());
    let pool = state.scheduled_pool.clone();
    match follower_status(&pool, &registry).await {
        Some(s) if s.synced && same_price_covenant(price_id.as_deref(), s.price_covenant_id.as_deref()) => Ok(Ctx {
            pool,
            prefix: if s.network.as_deref() == Some("mainnet") { Prefix::Mainnet } else { Prefix::Testnet },
            grace_ms: s.grace_ms,
            indexed_daa: s.indexed_daa,
            now: now_ms(),
        }),
        _ => Err(err(StatusCode::SERVICE_UNAVAILABLE, "syncing", "the registry follower is not synced yet")),
    }
}

fn internal(e: sqlx::Error) -> Response {
    tracing::error!("[names] api: {e}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "database error")
}

// ------------------------------------------------------------------ encoding ----

fn owner_address(prefix: Prefix, xonly: &[u8]) -> Option<String> {
    (xonly.len() == 32).then(|| Address::new(prefix, Version::PubKey, xonly).to_string())
}

/// The x-only key behind a schnorr P2PK address (`kaspatest:q…`); `None` for anything else.
fn address_key(address: &str) -> Option<Vec<u8>> {
    let a = Address::try_from(address.trim()).ok()?;
    (a.version == Version::PubKey && a.payload.len() == 32).then(|| a.payload.to_vec())
}

fn normalize_address(address: &str) -> Option<String> {
    Address::try_from(address.trim()).ok().map(|a| a.to_string())
}

fn status_str(s: NameStatus) -> &'static str {
    match s {
        NameStatus::Active => "active",
        NameStatus::Grace => "grace",
        NameStatus::Lapsed => "lapsed",
    }
}

fn outpoint(r: &PgRow) -> Value {
    json!({ "txId": hex::encode(r.get::<Vec<u8>, _>("txid")), "index": r.get::<i32, _>("idx") })
}

/// Name rows plus their registration/update times from history.
const NAME_SELECT: &str = r#"
    SELECT u.*,
      (SELECT h.at FROM names_history h WHERE h.key = u.key AND h.op = 'register' ORDER BY h.id DESC LIMIT 1) AS reg_at,
      (SELECT h.tx_id FROM names_history h WHERE h.key = u.key AND h.op = 'register' ORDER BY h.id DESC LIMIT 1) AS reg_tx,
      (SELECT max(h.at) FROM names_history h WHERE h.key = u.key) AS upd_at
    FROM names_utxos u
    WHERE u.kind = 'name' AND NOT u.refuted"#;

fn row_status(c: &Ctx, r: &PgRow) -> NameStatus {
    name_status(r.get::<Option<i64>, _>("expires_at").unwrap_or(0), c.grace_ms, c.now)
}

fn name_json(c: &Ctx, r: &PgRow) -> Value {
    let owner: Vec<u8> = r.get::<Option<Vec<u8>>, _>("owner").unwrap_or_default();
    let mut v = json!({
        "name": r.get::<Option<String>, _>("name").unwrap_or_default(),
        "key": hex::encode(r.get::<Option<Vec<u8>>, _>("key").unwrap_or_default()),
        "registered": true,
        "status": status_str(row_status(c, r)),
        "owner": owner_address(c.prefix, &owner),
        "ownerKey": hex::encode(&owner),
        "price": r.get::<Option<i64>, _>("price").unwrap_or(0).to_string(),
        // Registry v2: the app needs the paid period to spend (extend/renew) the name.
        "periodStart": r.get::<Option<i64>, _>("period_start").unwrap_or(0),
        "expiresAt": r.get::<Option<i64>, _>("expires_at").unwrap_or(0),
        "outpoint": outpoint(r),
    });
    let o = v.as_object_mut().unwrap();
    if let Some(at) = r.get::<Option<i64>, _>("reg_at") {
        o.insert("registeredAt".into(), json!(at));
    }
    if let Some(tx) = r.get::<Option<Vec<u8>>, _>("reg_tx") {
        o.insert("registeredTxId".into(), json!(hex::encode(tx)));
    }
    if let Some(at) = r.get::<Option<i64>, _>("upd_at") {
        o.insert("updatedAt".into(), json!(at));
    }
    v
}

fn gap_json(r: &PgRow) -> Value {
    json!({
        "lo": hex::encode(r.get::<Option<Vec<u8>>, _>("lo").unwrap_or_default()),
        "hi": hex::encode(r.get::<Option<Vec<u8>>, _>("hi").unwrap_or_default()),
        "outpoint": outpoint(r),
    })
}

fn offer_json(c: &Ctx, r: &PgRow, name: Option<String>) -> Value {
    let refund_after = r.get::<Option<i64>, _>("refund_after").unwrap_or(0);
    let buyer: Vec<u8> = r.get::<Option<Vec<u8>>, _>("buyer").unwrap_or_default();
    let mut v = json!({
        "outpoint": outpoint(r),
        "buyer": owner_address(c.prefix, &buyer),
        "amount": r.get::<i64, _>("value").to_string(),
        "refundAfter": refund_after,
        "createdAt": r.get::<i64, _>("created_at"),
        "refundable": c.indexed_daa >= refund_after,
    });
    // Registry v3: the owner the offer was made to. The app drops offers without one.
    if let Some(seller) = r.get::<Option<Vec<u8>>, _>("seller").and_then(|k| owner_address(c.prefix, &k)) {
        v.as_object_mut().unwrap().insert("seller".into(), json!(seller));
    }
    if let Some(n) = name {
        v.as_object_mut().unwrap().insert("name".into(), json!(n));
    }
    v
}

fn event_json(c: &Ctx, r: &PgRow) -> Value {
    let addr = |col: &str| r.get::<Option<Vec<u8>>, _>(col).and_then(|k| owner_address(c.prefix, &k));
    let mut v = json!({
        "txId": hex::encode(r.get::<Vec<u8>, _>("tx_id")),
        "op": r.get::<String, _>("op"),
        "at": r.get::<i64, _>("at"),
        "daa": r.get::<i64, _>("daa"),
    });
    let o = v.as_object_mut().unwrap();
    if let Some(n) = r.get::<Option<String>, _>("name") {
        o.insert("name".into(), json!(n));
    }
    if let Some(a) = addr("from_key") {
        o.insert("from".into(), json!(a));
    }
    if let Some(a) = addr("to_key") {
        o.insert("to".into(), json!(a));
    }
    if let Some(p) = r.get::<Option<i64>, _>("price") {
        o.insert("price".into(), json!(p.to_string()));
    }
    if let Some(y) = r.get::<Option<i64>, _>("years") {
        o.insert("years".into(), json!(y));
    }
    v
}

/// Opaque cursors are plain offsets / row ids as strings.
fn cursor(q: &Option<String>) -> i64 {
    q.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0).max(0)
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    cursor: Option<String>,
    length: Option<i64>,
    sort: Option<String>,
    #[serde(rename = "includeInactive")]
    include_inactive: Option<bool>,
}

fn page_len(q: &PageQuery) -> i64 {
    q.length.unwrap_or(50).clamp(1, 200)
}

// ------------------------------------------------------------------- handlers ----

/// `GET /names/{name}` — the name object, or `registered:false` with the gap to register in.
pub async fn name_lookup(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(name) = normalize_name(&name) else {
        return err(StatusCode::BAD_REQUEST, "invalid_name", "a name is 1-32 of a-z, 0-9 and inner hyphens");
    };
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let key = blake3::hash(name.as_bytes()).as_bytes().to_vec();
    match sqlx::query(&format!("{NAME_SELECT} AND u.key = $1 LIMIT 1")).bind(&key).fetch_optional(&c.pool).await {
        Ok(Some(r)) => Json(name_json(&c, &r)).into_response(),
        Ok(None) => match gap_for(&c, &key).await {
            Ok(Some(g)) => Json(json!({ "name": name, "key": name_key_hex(&name), "registered": false, "gap": g }))
                .into_response(),
            Ok(None) => err(StatusCode::NOT_FOUND, "not_found", "no live gap covers this key"),
            Err(e) => internal(e),
        },
        Err(e) => internal(e),
    }
}

async fn gap_for(c: &Ctx, key: &[u8]) -> Result<Option<Value>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT * FROM names_utxos WHERE kind = 'gap' AND NOT refuted AND lo < $1 AND hi > $1 LIMIT 1",
    )
    .bind(key)
    .fetch_optional(&c.pool)
    .await?;
    Ok(row.as_ref().map(gap_json))
}

/// `GET /names/gap/{keyHex}` — the one live gap with `lo < key < hi`.
pub async fn gap_lookup(State(state): State<Arc<AppState>>, Path(key_hex): Path<String>) -> Response {
    let key = match hex::decode(key_hex.trim()) {
        Ok(k) if k.len() == 32 => k,
        _ => return err(StatusCode::BAD_REQUEST, "invalid_key", "the key is 64 hex characters"),
    };
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    match gap_for(&c, &key).await {
        Ok(Some(g)) => Json(g).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found", "no live gap covers this key"),
        Err(e) => internal(e),
    }
}

/// `GET /names/by-owner/{address}?includeInactive=` — oldest first; active only by default.
pub async fn by_owner(
    State(state): State<Arc<AppState>>,
    Path(address): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let Some(key) = address_key(&address) else {
        return err(StatusCode::BAD_REQUEST, "invalid_address", "a schnorr (q…) address is required");
    };
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let rows = match sqlx::query(&format!("{NAME_SELECT} AND u.owner = $1 ORDER BY u.created_daa ASC"))
        .bind(&key)
        .fetch_all(&c.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let inactive = q.include_inactive.unwrap_or(false);
    let names: Vec<Value> = rows
        .iter()
        .filter(|r| inactive || row_status(&c, r) == NameStatus::Active)
        .map(|r| name_json(&c, r))
        .collect();
    Json(json!({ "names": names })).into_response()
}

/// `GET /market/listings?sort=recent|price_asc|price_desc&length=&cursor=` — listed, active.
pub async fn listings(State(state): State<Arc<AppState>>, Query(q): Query<PageQuery>) -> Response {
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let order = match q.sort.as_deref() {
        Some("price_asc") => "u.price ASC, u.created_daa DESC",
        Some("price_desc") => "u.price DESC, u.created_daa DESC",
        _ => "u.created_daa DESC",
    };
    let (len, off) = (page_len(&q), cursor(&q.cursor));
    let sql = format!("{NAME_SELECT} AND u.price > 0 AND u.expires_at > $1 ORDER BY {order} LIMIT $2 OFFSET $3");
    match sqlx::query(&sql).bind(c.now).bind(len + 1).bind(off).fetch_all(&c.pool).await {
        Ok(rows) => {
            let more = rows.len() as i64 > len;
            let listings: Vec<Value> = rows.iter().take(len as usize).map(|r| name_json(&c, r)).collect();
            Json(json!({ "listings": listings, "next": more.then(|| (off + len).to_string()) })).into_response()
        }
        Err(e) => internal(e),
    }
}

/// `GET /names/expiring?cursor=` — lapsed names still unspent (reclaimable), oldest first.
pub async fn expiring(State(state): State<Arc<AppState>>, Query(q): Query<PageQuery>) -> Response {
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let (len, off) = (page_len(&q), cursor(&q.cursor));
    let sql = format!("{NAME_SELECT} AND u.expires_at + $1 <= $2 ORDER BY u.expires_at ASC LIMIT $3 OFFSET $4");
    match sqlx::query(&sql).bind(c.grace_ms).bind(c.now).bind(len + 1).bind(off).fetch_all(&c.pool).await {
        Ok(rows) => {
            let more = rows.len() as i64 > len;
            let names: Vec<Value> = rows.iter().take(len as usize).map(|r| name_json(&c, r)).collect();
            Json(json!({ "names": names, "next": more.then(|| (off + len).to_string()) })).into_response()
        }
        Err(e) => internal(e),
    }
}

/// `GET /names/{name}/offers` — open offers on a name.
pub async fn name_offers(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(name) = normalize_name(&name) else {
        return err(StatusCode::BAD_REQUEST, "invalid_name", "a name is 1-32 of a-z, 0-9 and inner hyphens");
    };
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let key = blake3::hash(name.as_bytes()).as_bytes().to_vec();
    match sqlx::query(
        "SELECT * FROM names_utxos WHERE kind = 'offer' AND NOT refuted AND key = $1 ORDER BY created_daa DESC",
    )
    .bind(&key)
    .fetch_all(&c.pool)
    .await
    {
        Ok(rows) => {
            let offers: Vec<Value> = rows.iter().map(|r| offer_json(&c, r, Some(name.clone()))).collect();
            Json(json!({ "offers": offers })).into_response()
        }
        Err(e) => internal(e),
    }
}

/// `GET /offers/by-buyer/{address}` — a buyer's open offers, each with its `name`.
pub async fn offers_by_buyer(State(state): State<Arc<AppState>>, Path(address): Path<String>) -> Response {
    let Some(buyer) = address_key(&address) else {
        return err(StatusCode::BAD_REQUEST, "invalid_address", "a schnorr (q…) address is required");
    };
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    // The offer only carries the name's key; the name string comes from the live name row
    // or, if it isn't live, from the newest history row for that key.
    let sql = r#"
        SELECT o.*, COALESCE(
            (SELECT n.name FROM names_utxos n WHERE n.kind = 'name' AND n.key = o.key LIMIT 1),
            (SELECT h.name FROM names_history h WHERE h.key = o.key AND h.name IS NOT NULL ORDER BY h.id DESC LIMIT 1)
        ) AS offer_name
        FROM names_utxos o
        WHERE o.kind = 'offer' AND NOT o.refuted AND o.buyer = $1
        ORDER BY o.created_daa DESC"#;
    match sqlx::query(sql).bind(&buyer).fetch_all(&c.pool).await {
        Ok(rows) => {
            let offers: Vec<Value> =
                rows.iter().map(|r| offer_json(&c, r, r.get::<Option<String>, _>("offer_name"))).collect();
            Json(json!({ "offers": offers })).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn events_page(c: &Ctx, filter: &str, bind_key: Option<Vec<u8>>, q: &PageQuery) -> Response {
    let len = page_len(q);
    let before = cursor(&q.cursor);
    let before = if before > 0 { before } else { i64::MAX };
    let sql = format!("SELECT * FROM names_history WHERE id < $1 AND {filter} ORDER BY id DESC LIMIT $2");
    let mut query = sqlx::query(&sql).bind(before).bind(len + 1);
    if let Some(k) = bind_key {
        query = query.bind(k);
    }
    match query.fetch_all(&c.pool).await {
        Ok(rows) => {
            let more = rows.len() as i64 > len;
            let page = &rows[..rows.len().min(len as usize)];
            let next = if more { page.last().map(|r| r.get::<i64, _>("id").to_string()) } else { None };
            let events: Vec<Value> = page.iter().map(|r| event_json(c, r)).collect();
            Json(json!({ "events": events, "next": next })).into_response()
        }
        Err(e) => internal(e),
    }
}

/// `GET /names/{name}/history?cursor=` — newest first.
pub async fn name_history(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let Some(name) = normalize_name(&name) else {
        return err(StatusCode::BAD_REQUEST, "invalid_name", "a name is 1-32 of a-z, 0-9 and inner hyphens");
    };
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let key = blake3::hash(name.as_bytes()).as_bytes().to_vec();
    events_page(&c, "key = $3 AND op <> 'offer'", Some(key), &q).await
}

/// `GET /market/activity?cursor=` — recent sales, listings and offers across the registry.
pub async fn market_activity(State(state): State<Arc<AppState>>, Query(q): Query<PageQuery>) -> Response {
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    events_page(&c, "op IN ('sale', 'list', 'offer', 'offer_accepted', 'offer_decline')", None, &q).await
}

/// `GET /names/prices` (registry v3): the current prices and every live shard, in shard
/// order. Prices and values are sompi decimal strings; `prices`/`authority` at the top are
/// the current ones (every shard agrees after a change). 404 on a v2 registry.
pub async fn prices(State(state): State<Arc<AppState>>) -> Response {
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    if state.names.manifest.as_ref().and_then(|m| m.price_covenant_id.as_ref()).is_none() {
        // Registry v4: fixed prices, baked into the contracts and given in the manifest
        // (docs/KACHAT_NAMES_REGISTRY_V4.md §5): sompi per period, tiers 1..5+.
        if let Some(prices) = state.names.raw.as_ref().and_then(|r| r.get("params")).and_then(|p| p.get("prices")) {
            let table = |which: &str| -> Option<Vec<String>> {
                let t = prices.get(which)?;
                ["len1", "len2", "len3", "len4", "len5plus"].iter().map(|k| t.get(*k)?.as_i64().map(|v| v.to_string())).collect()
            };
            if let (Some(register), Some(renew)) = (table("register"), table("renew")) {
                return Json(json!({ "register": register, "renew": renew })).into_response();
            }
        }
        return err(StatusCode::NOT_FOUND, "no_price_record", "this registry has no price record (v2)");
    }
    let rows = match sqlx::query("SELECT * FROM names_utxos WHERE kind = 'shard' AND NOT refuted ORDER BY shard ASC")
        .fetch_all(&c.pool)
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let shards: Vec<Value> = rows
        .iter()
        .map(|r| {
            let prices: Vec<String> =
                r.get::<Option<String>, _>("prices").unwrap_or_default().split(',').filter(|p| !p.is_empty()).map(String::from).collect();
            json!({
                "shard": r.get::<Option<i64>, _>("shard").unwrap_or(0),
                "outpoint": outpoint(r),
                "authority": hex::encode(r.get::<Option<Vec<u8>>, _>("authority").unwrap_or_default()),
                "prices": prices,
                "value": r.get::<i64, _>("value").to_string(),
            })
        })
        .collect();
    let first = shards.first();
    Json(json!({
        "prices": first.map(|s| s["prices"].clone()).unwrap_or_else(|| json!([])),
        "authority": first.map(|s| s["authority"].clone()).unwrap_or(Value::Null),
        "shards": shards,
    }))
    .into_response()
}

/// `GET /names/activity?cursor=` — every registry event, newest first, same shape and paging as
/// `/market/activity`, except the price record's (`prices`, `price_authority`)
/// (KACHAT_NAMES_REGISTRY_V3.md §9).
pub async fn names_activity(State(state): State<Arc<AppState>>, Query(q): Query<PageQuery>) -> Response {
    let c = match ctx(&state).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    events_page(&c, "op NOT IN ('prices', 'price_authority')", None, &q).await
}

/// A stored record as served: re-checked against the per-field allowlist, so a link that
/// is not allowed in its field is dropped and the rest kept (`v: 1` preserved).
fn clean_profile(raw: &str) -> Value {
    match kachat_names::parse_profile(raw) {
        Some(p) => {
            let mut v = serde_json::to_value(p).unwrap_or_else(|_| json!({}));
            v["v"] = json!(1);
            v
        }
        None => Value::Null,
    }
}

/// One profile: the stored record (validated by the follower) or null.
async fn profile_for(pool: &PgPool, address: &str) -> Result<Option<(Value, i64, Vec<u8>)>, sqlx::Error> {
    let row = sqlx::query("SELECT profile, updated_at, tx_id FROM names_profiles WHERE address = $1")
        .bind(address)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| (clean_profile(&r.get::<String, _>("profile")), r.get("updated_at"), r.get("tx_id"))))
}

// ------------------------------------------------------------------ profiles ----
//
// Address profiles are kept by the profiles follower (`kachat-names-follower --profiles`,
// docs/KACHAT_PROFILES.md) on every network, with or without a names registry.

/// The profiles follower's row in `profiles_state`, if it is running (heartbeat fresh).
pub struct ProfilesStatus {
    pub network: String,
    pub indexed_daa: i64,
    pub synced: bool,
}

pub async fn profiles_status(pool: &PgPool) -> Option<ProfilesStatus> {
    let row = sqlx::query("SELECT network, indexed_daa, synced, updated_at FROM profiles_state WHERE id = 1")
        .fetch_optional(pool)
        .await
        .ok()??;
    (now_ms() - row.get::<i64, _>("updated_at") <= STALE_MS).then(|| ProfilesStatus {
        network: row.get("network"),
        indexed_daa: row.get("indexed_daa"),
        synced: row.get("synced"),
    })
}

/// Profile reads need the follower on and caught up: a profile missing because the scan
/// has not reached it yet must not be served as "no profile".
async fn profiles_pool(state: &AppState) -> Result<PgPool, Response> {
    let pool = state.scheduled_pool.clone();
    match profiles_status(&pool).await {
        Some(s) if s.synced => Ok(pool),
        Some(_) => Err(err(StatusCode::SERVICE_UNAVAILABLE, "syncing", "the profiles follower is catching up")),
        None => Err(err(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "the profiles follower is not running")),
    }
}

/// `GET /profiles/{address}`
pub async fn profile(State(state): State<Arc<AppState>>, Path(address): Path<String>) -> Response {
    let Some(address) = normalize_address(&address) else {
        return err(StatusCode::BAD_REQUEST, "invalid_address", "not a Kaspa address");
    };
    let pool = match profiles_pool(&state).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    match profile_for(&pool, &address).await {
        Ok(Some((p, at, tx))) => {
            Json(json!({ "address": address, "profile": p, "updatedAt": at, "txId": hex::encode(tx) })).into_response()
        }
        Ok(None) => Json(json!({ "address": address, "profile": Value::Null })).into_response(),
        Err(e) => internal(e),
    }
}

/// Part C identity: active names (oldest first), the profile, and the label —
/// `primaryName` while owned and active, else the oldest active name, else null.
async fn identity_for(c: &Ctx, address: &str) -> Result<Value, sqlx::Error> {
    let mut names: Vec<String> = Vec::new();
    if let Some(key) = address_key(address) {
        let rows = sqlx::query(
            "SELECT name, expires_at FROM names_utxos WHERE kind = 'name' AND NOT refuted AND owner = $1 ORDER BY created_daa ASC",
        )
        .bind(&key)
        .fetch_all(&c.pool)
        .await?;
        names = rows
            .iter()
            .filter(|r| name_status(r.get::<Option<i64>, _>("expires_at").unwrap_or(0), c.grace_ms, c.now) == NameStatus::Active)
            .filter_map(|r| r.get::<Option<String>, _>("name"))
            .collect();
    }
    let profile = profile_for(&c.pool, address).await?.map(|(p, _, _)| p).unwrap_or(Value::Null);
    let primary = profile.get("primaryName").and_then(Value::as_str).map(|s| s.trim_end_matches(".kachat").to_string());
    let label = match primary {
        Some(p) if names.contains(&p) => Some(p),
        _ => names.first().cloned(),
    };
    Ok(json!({ "address": address, "label": label, "names": names, "profile": profile }))
}

/// Where identities come from: the registry (names + label + profile) when a names
/// manifest is loaded, else profiles only (`label: null`, `names: []`) — mainnet today.
enum IdentitySource {
    Registry(Ctx),
    ProfilesOnly(PgPool),
}

async fn identity_source(state: &AppState) -> Result<IdentitySource, Response> {
    if state.names.is_on() {
        return ctx(state).await.map(IdentitySource::Registry);
    }
    profiles_pool(state).await.map(IdentitySource::ProfilesOnly)
}

async fn identity_from(src: &IdentitySource, address: &str) -> Result<Value, sqlx::Error> {
    match src {
        IdentitySource::Registry(c) => identity_for(c, address).await,
        IdentitySource::ProfilesOnly(pool) => {
            let profile = profile_for(pool, address).await?.map(|(p, _, _)| p).unwrap_or(Value::Null);
            Ok(json!({ "address": address, "label": Value::Null, "names": [], "profile": profile }))
        }
    }
}

/// `GET /identity/{address}`
pub async fn identity(State(state): State<Arc<AppState>>, Path(address): Path<String>) -> Response {
    let Some(address) = normalize_address(&address) else {
        return err(StatusCode::BAD_REQUEST, "invalid_address", "not a Kaspa address");
    };
    let src = match identity_source(&state).await {
        Ok(s) => s,
        Err(e) => return e,
    };
    match identity_from(&src, &address).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct BatchBody {
    addresses: Vec<String>,
}

/// `POST /identity/batch {"addresses": [...]}` (≤ 200) → `{"identities": {address: identity}}`.
pub async fn identity_batch(State(state): State<Arc<AppState>>, Json(body): Json<BatchBody>) -> Response {
    if body.addresses.len() > MAX_BATCH {
        return err(StatusCode::BAD_REQUEST, "too_many", "at most 200 addresses");
    }
    let src = match identity_source(&state).await {
        Ok(s) => s,
        Err(e) => return e,
    };
    let mut out: HashMap<String, Value> = HashMap::new();
    for raw in &body.addresses {
        let Some(address) = normalize_address(raw) else { continue };
        match identity_from(&src, &address).await {
            Ok(v) => {
                out.insert(raw.clone(), v);
            }
            Err(e) => return internal(e),
        }
    }
    Json(json!({ "identities": out })).into_response()
}

/// `GET /profiles/stats` — the Kaspa Quick Start panel's KaChat → Profiles tab
/// (docs/KACHAT_PROFILES.md §4). Served while the follower runs, synced or not (`synced`
/// says which); 503 when it is off. Links only: pictures and bios are never fetched.
pub async fn profile_stats(State(state): State<Arc<AppState>>) -> Response {
    let pool = state.scheduled_pool.clone();
    let Some(status) = profiles_status(&pool).await else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "the profiles follower is not running");
    };
    match compute_stats(&pool, &status).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

async fn compute_stats(pool: &PgPool, status: &ProfilesStatus) -> Result<Value, sqlx::Error> {
    const DAY: i64 = 86_400_000;
    let now = now_ms();
    let rows = sqlx::query("SELECT address, profile, tx_id, updated_at, created_at FROM names_profiles ORDER BY updated_at DESC")
        .fetch_all(pool)
        .await?;
    let saves7d: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM profile_saves WHERE block_time >= $1")
        .bind(now - 7 * DAY)
        .fetch_one(pool)
        .await?;
    let records: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM profile_saves").fetch_one(pool).await?;

    let mut c = ProfileCounts::default();
    let mut recent = Vec::new();
    for r in &rows {
        let Some(p) = kachat_names::parse_profile(&r.get::<String, _>("profile")) else { continue };
        c.add(&p, r.get::<Option<i64>, _>("created_at").unwrap_or(0), now);
        if recent.len() < 25 {
            recent.push(json!({
                "address": r.get::<String, _>("address"),
                "updatedAt": r.get::<i64, _>("updated_at"),
                "txId": hex::encode(r.get::<Vec<u8>, _>("tx_id")),
                "avatar": p.avatar,
                "banner": p.banner,
                "bio": p.bio,
                "linktree": p.linktree,
            }));
        }
    }
    Ok(json!({
        "network": status.network,
        "synced": status.synced,
        "indexedDaa": status.indexed_daa,
        "total": c.total,
        "new24h": c.new24h,
        "new7d": c.new7d,
        "new30d": c.new30d,
        "saves7d": saves7d,
        "records": records,
        "withAvatar": c.with_avatar,
        "withBanner": c.with_banner,
        "withBio": c.with_bio,
        "withLinktree": c.with_linktree,
        "withPrimaryName": c.with_primary_name,
        "platforms": { "avatar": c.avatar, "banner": c.banner, "bio": c.bio },
        "recent": recent,
    }))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    /// Only this address's saves.
    address: Option<String>,
}

/// `GET /profiles/history?limit=&offset=&address=` — every accepted profile record, all
/// time, newest first: `{total, items: [{address, txId, savedAt, current, avatar, banner,
/// bio, linktree, primaryName}]}`. `current` = still the address's live record. For the
/// panel's numbered pager, like KaPosts' `moderation/recent`. Links only.
pub async fn profile_history(State(state): State<Arc<AppState>>, Query(q): Query<HistoryQuery>) -> Response {
    let pool = state.scheduled_pool.clone();
    if profiles_status(&pool).await.is_none() {
        return err(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "the profiles follower is not running");
    }
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);
    let address = match q.address.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => match normalize_address(a) {
            Some(a) => Some(a),
            None => return err(StatusCode::BAD_REQUEST, "invalid_address", "not a Kaspa address"),
        },
        None => None,
    };
    let total: i64 = match sqlx::query_scalar("SELECT COUNT(*) FROM profile_saves WHERE ($1::TEXT IS NULL OR address = $1)")
        .bind(&address)
        .fetch_one(&pool)
        .await
    {
        Ok(n) => n,
        Err(e) => return internal(e),
    };
    let rows = match sqlx::query(
        r#"SELECT s.address, s.tx_id, s.block_time, s.profile, (p.tx_id IS NOT NULL) AS current
           FROM profile_saves s LEFT JOIN names_profiles p ON p.spk = s.spk AND p.tx_id = s.tx_id
           WHERE ($1::TEXT IS NULL OR s.address = $1)
           ORDER BY s.block_time DESC, s.daa DESC, s.tx_id DESC
           LIMIT $2 OFFSET $3"#,
    )
    .bind(&address)
    .bind(limit)
    .bind(offset)
    .fetch_all(&pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let p = r.get::<Option<String>, _>("profile").and_then(|j| kachat_names::parse_profile(&j)).unwrap_or_default();
            json!({
                "address": r.get::<String, _>("address"),
                "txId": hex::encode(r.get::<Vec<u8>, _>("tx_id")),
                "savedAt": r.get::<i64, _>("block_time"),
                "current": r.get::<bool, _>("current"),
                "avatar": p.avatar,
                "banner": p.banner,
                "bio": p.bio,
                "linktree": p.linktree,
                "primaryName": p.primary_name,
            })
        })
        .collect();
    Json(json!({ "total": total, "items": items })).into_response()
}

/// Per-profile tallies for `/profiles/stats` (kept apart from the query so it is testable).
#[derive(Default)]
struct ProfileCounts {
    total: i64,
    new24h: i64,
    new7d: i64,
    new30d: i64,
    with_avatar: i64,
    with_banner: i64,
    with_bio: i64,
    with_linktree: i64,
    with_primary_name: i64,
    avatar: std::collections::BTreeMap<&'static str, i64>,
    banner: std::collections::BTreeMap<&'static str, i64>,
    bio: std::collections::BTreeMap<&'static str, i64>,
}

impl ProfileCounts {
    fn add(&mut self, p: &kachat_names::Profile, created_at: i64, now: i64) {
        const DAY: i64 = 86_400_000;
        self.total += 1;
        let age = now - created_at;
        self.new24h += i64::from(age <= DAY);
        self.new7d += i64::from(age <= 7 * DAY);
        self.new30d += i64::from(age <= 30 * DAY);
        let tally = |field: &Option<String>, count: &mut i64, map: &mut std::collections::BTreeMap<&'static str, i64>| {
            if let Some(url) = field {
                *count += 1;
                if let Some(platform) = kachat_names::platform_of(url) {
                    *map.entry(platform).or_default() += 1;
                }
            }
        };
        tally(&p.avatar, &mut self.with_avatar, &mut self.avatar);
        tally(&p.banner, &mut self.with_banner, &mut self.banner);
        tally(&p.bio, &mut self.with_bio, &mut self.bio);
        self.with_linktree += i64::from(p.linktree.is_some());
        self.with_primary_name += i64::from(p.primary_name.is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // kachat-domains' `kachat-names-vectors` p2pk vector (KaChatTests/KachatNamesVectors.json).
    const XONLY: &str = "6dece92abd087978562b0e47943d859bd444672f89bc68fc8bfa03a3d0b27ee8";
    const ADDRESS: &str = "kaspatest:qpk7e6f2h5y8j7zk9v8y09paskdag3r897ymc68u30aq8g7skflwsaxs0p99v";

    #[test]
    fn owner_key_and_address_round_trip_the_builder_vector() {
        let key = hex::decode(XONLY).unwrap();
        assert_eq!(owner_address(Prefix::Testnet, &key).as_deref(), Some(ADDRESS));
        assert_eq!(address_key(ADDRESS), Some(key));
        assert_eq!(normalize_address(&format!("  {ADDRESS} ")).as_deref(), Some(ADDRESS));
    }

    #[test]
    fn only_schnorr_addresses_name_an_owner() {
        assert_eq!(address_key("not-an-address"), None);
        assert_eq!(owner_address(Prefix::Testnet, &[0u8; 31]), None);
    }

    #[test]
    fn cursors_are_non_negative_offsets() {
        assert_eq!(cursor(&None), 0);
        assert_eq!(cursor(&Some("40".into())), 40);
        assert_eq!(cursor(&Some("-5".into())), 0);
        assert_eq!(cursor(&Some("junk".into())), 0);
    }

    #[test]
    fn stats_count_fields_platforms_and_windows() {
        const DAY: i64 = 86_400_000;
        let now = 100 * DAY;
        let mut c = ProfileCounts::default();
        let a = kachat_names::parse_profile(r#"{"v":1,"avatar":"https://x.com/a","bio":"https://github.com/a","linktree":"https://linktr.ee/a"}"#).unwrap();
        let b = kachat_names::parse_profile(r#"{"v":1,"avatar":"https://x.com/b","banner":"https://www.youtube.com/@b"}"#).unwrap();
        c.add(&a, now - DAY / 2, now);
        c.add(&b, now - 10 * DAY, now);
        assert_eq!((c.total, c.new24h, c.new7d, c.new30d), (2, 1, 1, 2));
        assert_eq!((c.with_avatar, c.with_banner, c.with_bio, c.with_linktree, c.with_primary_name), (2, 1, 1, 1, 0));
        assert_eq!(c.avatar.get("x"), Some(&2));
        assert_eq!(c.banner.get("youtube"), Some(&1));
        assert_eq!(c.bio.get("github"), Some(&1));
    }

    #[test]
    fn v3_needs_the_follower_on_the_same_price_covenant() {
        assert!(same_price_covenant(None, None), "v2: no price covenant");
        assert!(same_price_covenant(Some("AB"), Some("ab")));
        assert!(!same_price_covenant(Some("ab"), None), "a v2 follower cannot serve a v3 manifest");
        assert!(!same_price_covenant(Some("ab"), Some("cd")));
    }

    #[test]
    fn served_profiles_drop_disallowed_links() {
        let v = clean_profile(r#"{"v":1,"avatar":"https://x.com/a","banner":"https://github.com/a"}"#);
        assert_eq!(v["avatar"], "https://x.com/a");
        assert!(v.get("banner").is_none(), "github is not a banner platform");
        assert_eq!(v["v"], 1);
        assert_eq!(clean_profile(r#"{"avatar":"https://x.com/a"}"#), Value::Null);
    }
}

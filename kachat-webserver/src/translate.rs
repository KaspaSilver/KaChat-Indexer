//! Post-translation endpoint (`POST /translate`, `GET /translate/languages`).
//!
//! A thin layer in front of a self-hosted LibreTranslate that (a) caches translations of immutable
//! KaPosts by `(txid, target)` forever and (b) only ever caches the server's OWN verified copy of a
//! post — never text supplied in the request (cache-poisoning rule). Privacy-critical: this endpoint
//! takes no identity (no requesterPubkey / auth), and must not log request bodies or post text.

use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

use crate::models::ApiError;
use crate::web_server::AppState;

/// KaChat message marker (U+2060 WORD JOINER) prefixing every post body.
const KACHAT_MARKER: char = '\u{2060}';
/// Client-side post length cap (POST_CHARACTER_LIMIT).
const MAX_TEXT_CHARS: usize = 25_000;
const MAX_POSTS: usize = 50;
/// Per-IP translate limits (generous — a reader scrolling a multilingual feed sends batches).
const TRANSLATE_MAX_REQUESTS_PER_MIN: u32 = 60;
const TRANSLATE_MAX_POSTS_PER_MIN: u32 = 600;

// ---- request / response contract (see the handoff doc §2) ----

#[derive(Debug, Deserialize)]
pub struct TranslateRequest {
    pub target: Option<String>,
    pub posts: Option<Vec<TranslatePostIn>>,
}

#[derive(Debug, Deserialize)]
pub struct TranslatePostIn {
    pub id: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TranslateResponse {
    pub translations: Vec<TranslationOut>,
}

#[derive(Debug, Default, Serialize)]
pub struct TranslationOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub untranslated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LanguagesResponse {
    pub source: Vec<String>,
    pub target: Vec<String>,
}

// ---- per-IP rate limiting (requests + posts) ----

#[derive(Debug, Clone)]
pub struct TranslateRateEntry {
    requests: u32,
    posts: u32,
    window_start: Instant,
}

pub type TranslateRateLimitMap = Arc<RwLock<HashMap<SocketAddr, TranslateRateEntry>>>;

async fn check_translate_rate_limit(
    app: &AppState,
    addr: SocketAddr,
    num_posts: u32,
) -> Result<(), (StatusCode, Json<ApiError>)> {
    let now = Instant::now();
    let mut map = app.translate_rate_limit_map.write().await;
    let entry = map.entry(addr).or_insert(TranslateRateEntry {
        requests: 0,
        posts: 0,
        window_start: now,
    });
    if now.duration_since(entry.window_start) >= Duration::from_secs(60) {
        entry.requests = 0;
        entry.posts = 0;
        entry.window_start = now;
    }
    entry.requests += 1;
    entry.posts += num_posts;
    if entry.requests > TRANSLATE_MAX_REQUESTS_PER_MIN || entry.posts > TRANSLATE_MAX_POSTS_PER_MIN {
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "Translate rate limit exceeded.",
            "RATE_LIMITED",
        ));
    }
    Ok(())
}

// ---- helpers ----

fn err(status: StatusCode, msg: &str, code: &str) -> (StatusCode, Json<ApiError>) {
    (
        status,
        Json(ApiError {
            error: msg.to_string(),
            code: code.to_string(),
        }),
    )
}

fn entry_err(id: Option<String>, msg: &str, code: &str) -> TranslationOut {
    TranslationOut {
        id,
        error: Some(msg.to_string()),
        code: Some(code.to_string()),
        ..Default::default()
    }
}

/// Normalize a BCP-47 tag to its bare primary language subtag, lowercased (`pt-BR` -> `pt`).
fn primary_subtag(tag: &str) -> String {
    tag.trim()
        .to_lowercase()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_string()
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Decode a stored `base64_encoded_message` into the published plain text (strip the KaChat marker).
/// Returns None if the bytes aren't valid base64/UTF-8 or the marker is missing.
fn decode_post_text(base64_message: &str) -> Option<String> {
    use base64ct::{Base64, Encoding};
    let bytes = Base64::decode_vec(base64_message.trim()).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    text.strip_prefix(KACHAT_MARKER).map(|s| s.to_string())
}

/// Strip URLs and @mentions for language DETECTION only (they skew identification). The full text is
/// still what gets translated/returned.
fn strip_for_detection(text: &str) -> String {
    text.split_whitespace()
        .filter(|t| {
            !t.starts_with("http://") && !t.starts_with("https://") && !t.starts_with('@')
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- LibreTranslate client ----

enum LtError {
    Unsupported,
    Failed,
}

/// Detect the language of `q` via LibreTranslate `/detect`. Returns the top language subtag, or None
/// on any failure / empty input (caller falls back to source="auto").
async fn lt_detect(app: &AppState, q: &str) -> Option<String> {
    if q.trim().is_empty() {
        return None;
    }
    let url = format!("{}/detect", app.server_config.libretranslate_url);
    let resp = app
        .http
        .post(&url)
        .json(&json!({ "q": q }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let arr: serde_json::Value = resp.json().await.ok()?;
    arr.get(0)?
        .get("language")?
        .as_str()
        .map(|s| primary_subtag(s))
}

/// Translate `q` from `source` (may be "auto") to `target` via LibreTranslate `/translate`.
/// Returns (translated_text, detected_source_subtag_if_auto).
async fn lt_translate(
    app: &AppState,
    q: &str,
    source: &str,
    target: &str,
) -> Result<(String, Option<String>), LtError> {
    let url = format!("{}/translate", app.server_config.libretranslate_url);
    let resp = app
        .http
        .post(&url)
        .json(&json!({ "q": q, "source": source, "target": target, "format": "text" }))
        .send()
        .await
        .map_err(|_| LtError::Failed)?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|_| LtError::Failed)?;
    if !status.is_success() {
        // LibreTranslate reports an unknown/unsupported pair in the "error" message.
        let msg = body.get("error").and_then(|e| e.as_str()).unwrap_or("");
        if msg.to_lowercase().contains("not support") || msg.to_lowercase().contains("language") {
            return Err(LtError::Unsupported);
        }
        return Err(LtError::Failed);
    }
    let translated = body
        .get("translatedText")
        .and_then(|t| t.as_str())
        .ok_or(LtError::Failed)?
        .to_string();
    let detected = body
        .get("detectedLanguage")
        .and_then(|d| d.get("language"))
        .and_then(|l| l.as_str())
        .map(primary_subtag);
    Ok((translated, detected))
}

// ---- handlers ----

pub(crate) async fn handle_translate(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app): State<Arc<AppState>>,
    Json(req): Json<TranslateRequest>,
) -> Result<Json<TranslateResponse>, (StatusCode, Json<ApiError>)> {
    // target is required (whole-request error).
    let target = req
        .target
        .as_deref()
        .map(primary_subtag)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "Missing required parameter: target",
                "MISSING_PARAMETER",
            )
        })?;

    let posts = req.posts.unwrap_or_default();
    if posts.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Missing required parameter: posts",
            "MISSING_PARAMETER",
        ));
    }
    if posts.len() > MAX_POSTS {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Too many posts (max 50 per request).",
            "TOO_MANY_POSTS",
        ));
    }

    check_translate_rate_limit(&app, addr, posts.len() as u32).await?;

    let mut translations = Vec::with_capacity(posts.len());
    for p in posts {
        translations.push(translate_one(&app, &target, p).await);
    }
    Ok(Json(TranslateResponse { translations }))
}

/// Translate a single entry. Never fails the batch — returns a TranslationOut with an error on
/// per-entry problems.
async fn translate_one(app: &AppState, target: &str, p: TranslatePostIn) -> TranslationOut {
    let id_out = p.id.clone();

    // Resolve the text to translate + whether we may cache under a txid.
    // cache_id = Some(normalized hex) only when we hold the server's OWN verified copy.
    let (text, cache_id): (String, Option<String>) = match &p.id {
        Some(raw_id) => {
            let idn = raw_id.trim().to_lowercase();
            if !is_hex64(&idn) {
                return entry_err(
                    id_out,
                    "Invalid post id. Must be 64 hex characters.",
                    "INVALID_POST_ID",
                );
            }
            // Cache hit? (only possible when a verified copy was cached before)
            if let Ok(Some((source, cached_text))) =
                app.db.get_cached_translation(&idn, target).await
            {
                return TranslationOut {
                    id: Some(idn),
                    source: Some(source),
                    target: Some(target.to_string()),
                    text: Some(cached_text),
                    cached: Some(true),
                    ..Default::default()
                };
            }
            // Prefer the server's own verified copy; only cache that.
            match app.db.get_post_base64_message(&idn).await {
                Ok(Some(b64)) => match decode_post_text(&b64) {
                    Some(t) => (t, Some(idn)),
                    // Post is held but undecodable — fall back to supplied text, do not cache.
                    None => match p.text {
                        Some(t) => (t, None),
                        None => {
                            return entry_err(
                                Some(idn),
                                "Post text unavailable.",
                                "TRANSLATION_FAILED",
                            );
                        }
                    },
                },
                // Not held by this indexer: translate supplied text, never cache under the txid.
                _ => match p.text {
                    Some(t) => (t, None),
                    None => {
                        return entry_err(
                            Some(idn),
                            "Missing required parameter: text (post not indexed here).",
                            "MISSING_PARAMETER",
                        );
                    }
                },
            }
        }
        None => match p.text {
            Some(t) => (t, None),
            None => {
                return entry_err(id_out, "Missing required parameter: text.", "MISSING_PARAMETER");
            }
        },
    };

    if text.chars().count() > MAX_TEXT_CHARS {
        return entry_err(id_out, "Text exceeds 25000 characters.", "TEXT_TOO_LONG");
    }

    // Detect on stripped text; translate the full text.
    let detected = lt_detect(app, &strip_for_detection(&text)).await;

    // source == target: echo the input rather than fail (detection is a guess).
    if detected.as_deref() == Some(target) {
        return TranslationOut {
            id: id_out,
            source: detected,
            target: Some(target.to_string()),
            text: Some(text),
            untranslated: Some(true),
            cached: Some(false),
            ..Default::default()
        };
    }

    let source_in = detected.clone().unwrap_or_else(|| "auto".to_string());
    match lt_translate(app, &text, &source_in, target).await {
        Ok((translated, auto_detected)) => {
            let source = detected
                .or(auto_detected)
                .unwrap_or_else(|| "auto".to_string());
            // Auto-detection may reveal source == target after the fact — return original untranslated.
            if source == target {
                return TranslationOut {
                    id: id_out,
                    source: Some(source),
                    target: Some(target.to_string()),
                    text: Some(text),
                    untranslated: Some(true),
                    cached: Some(false),
                    ..Default::default()
                };
            }
            // Cache only the server's own verified copy.
            if let Some(idn) = &cache_id {
                let _ = app
                    .db
                    .insert_translation(idn, target, &source, &translated, now_ms())
                    .await;
            }
            TranslationOut {
                id: id_out,
                source: Some(source),
                target: Some(target.to_string()),
                text: Some(translated),
                cached: Some(false),
                ..Default::default()
            }
        }
        Err(LtError::Unsupported) => {
            entry_err(id_out, "Unsupported language pair.", "UNSUPPORTED_PAIR")
        }
        Err(LtError::Failed) => entry_err(id_out, "Translation failed.", "TRANSLATION_FAILED"),
    }
}

pub(crate) async fn handle_translate_languages(
    State(app): State<Arc<AppState>>,
) -> Result<Json<LanguagesResponse>, (StatusCode, Json<ApiError>)> {
    let url = format!("{}/languages", app.server_config.libretranslate_url);
    let resp = app.http.get(&url).send().await.map_err(|_| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Language list unavailable.",
            "TRANSLATION_FAILED",
        )
    })?;
    let arr: serde_json::Value = resp.json().await.map_err(|_| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Language list unavailable.",
            "TRANSLATION_FAILED",
        )
    })?;
    // LibreTranslate /languages: [{ "code": "en", "name": "English", "targets": ["es", ...] }, ...]
    // Normalize codes to bare BCP-47 primary subtags (e.g. "zh-Hans" -> "zh") so they match the
    // bare subtags the clients send; LibreTranslate accepts the bare form on translate/detect.
    let mut source: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut target: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(list) = arr.as_array() {
        for item in list {
            if let Some(code) = item.get("code").and_then(|c| c.as_str()) {
                source.insert(primary_subtag(code));
            }
            if let Some(ts) = item.get("targets").and_then(|t| t.as_array()) {
                for t in ts {
                    if let Some(code) = t.as_str() {
                        target.insert(primary_subtag(code));
                    }
                }
            }
        }
    }
    let source: Vec<String> = source.into_iter().collect();
    // Fall back to sources as targets if the engine didn't list per-source targets.
    let target: Vec<String> = if target.is_empty() {
        source.clone()
    } else {
        target.into_iter().collect()
    };
    Ok(Json(LanguagesResponse { source, target }))
}

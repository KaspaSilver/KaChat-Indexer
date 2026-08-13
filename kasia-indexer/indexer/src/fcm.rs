//! Firebase Cloud Messaging (FCM) HTTP v1 client — the Android counterpart to the APNs client
//! in `push.rs`. Authenticates with a Firebase *service account* (OAuth2 JWT-bearer flow) and
//! sends **data-only** messages so the Android `KaChatFirebaseMessagingService` always runs and
//! builds the notification itself (mirrors iOS's `mutable-content` + Notification Service
//! Extension model). Empty/missing config simply disables FCM delivery — registration still works.

use crate::config::IndexerConfig;
use jsonwebtoken::{EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const GOOGLE_TOKEN_URI_FALLBACK: &str = "https://oauth2.googleapis.com/token";
/// Access tokens are valid for 3600s; refresh a little early.
const TOKEN_REFRESH_SKEW_SECS: u64 = 300;

/// Delivery failure classes, mapped onto the same handling the APNs path uses.
#[derive(Debug)]
pub enum FcmError {
    /// The registration token is no longer valid (uninstalled / rotated) — drop it.
    Unregistered,
    /// The token is malformed / not for this project — drop after repeated failures.
    InvalidToken,
    /// OAuth2 / credential problem — keep the token, retry later.
    Auth(String),
    /// Transport error.
    Request(reqwest::Error),
    /// Any other non-success from FCM.
    Rejected { status: u16, reason: Option<String> },
}

impl std::fmt::Display for FcmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FcmError::Unregistered => write!(f, "unregistered token"),
            FcmError::InvalidToken => write!(f, "invalid token"),
            FcmError::Auth(err) => write!(f, "auth error: {err}"),
            FcmError::Request(err) => write!(f, "request error: {err}"),
            FcmError::Rejected { status, reason } => {
                write!(f, "rejected: status={status} reason={reason:?}")
            }
        }
    }
}

/// The subset of a Firebase service-account JSON we need.
#[derive(Debug, Clone, Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
    #[serde(default)]
    token_uri: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

#[derive(Serialize)]
struct OAuthClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_expires_in() -> u64 {
    3600
}

struct CachedToken {
    token: String,
    /// Unix seconds at which this token should be considered expired (already skewed).
    expires_at: u64,
}

pub struct FcmClient {
    client: reqwest::Client,
    project_id: String,
    client_email: String,
    token_uri: String,
    signing_key: EncodingKey,
    send_endpoint: String,
    token_cache: Mutex<Option<CachedToken>>,
}

impl FcmClient {
    /// Build from indexer config. `Ok(None)` when FCM is not configured (no project id / no
    /// service account); `Err` only when config is present but unusable, so the caller can warn.
    pub fn from_config(config: &IndexerConfig) -> anyhow::Result<Option<Self>> {
        let Some(raw_json) = load_service_account_json(config)? else {
            return Ok(None);
        };
        let account: ServiceAccount = serde_json::from_str(&raw_json)
            .map_err(|err| anyhow::anyhow!("FCM service account JSON is invalid: {err}"))?;

        // Prefer the explicit config project id, fall back to the one in the service account.
        let project_id = config
            .fcm_project_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .or_else(|| account.project_id.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("FCM_PROJECT_ID missing (and no project_id in service account)")
            })?;

        let signing_key = EncodingKey::from_rsa_pem(account.private_key.as_bytes())
            .map_err(|err| anyhow::anyhow!("FCM service-account private_key is invalid: {err}"))?;

        let token_uri = account
            .token_uri
            .filter(|uri| !uri.trim().is_empty())
            .unwrap_or_else(|| GOOGLE_TOKEN_URI_FALLBACK.to_string());

        let send_endpoint =
            format!("https://fcm.googleapis.com/v1/projects/{project_id}/messages:send");

        Ok(Some(Self {
            client: reqwest::Client::builder().build()?,
            project_id,
            client_email: account.client_email,
            token_uri,
            signing_key,
            send_endpoint,
            token_cache: Mutex::new(None),
        }))
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// OAuth2 JWT-bearer flow; caches the access token until shortly before expiry.
    async fn access_token(&self) -> Result<String, FcmError> {
        let now = unix_time_secs();
        {
            let cache = self.token_cache.lock().await;
            if let Some(cached) = cache.as_ref()
                && now < cached.expires_at
            {
                return Ok(cached.token.clone());
            }
        }

        let header = Header {
            alg: jsonwebtoken::Algorithm::RS256,
            ..Default::default()
        };
        let claims = OAuthClaims {
            iss: &self.client_email,
            scope: FCM_SCOPE,
            aud: &self.token_uri,
            iat: now,
            exp: now + 3600,
        };
        let assertion = jsonwebtoken::encode(&header, &claims, &self.signing_key)
            .map_err(|err| FcmError::Auth(format!("failed to sign OAuth JWT: {err}")))?;

        let resp = self
            .client
            .post(&self.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion),
            ])
            .send()
            .await
            .map_err(|err| FcmError::Auth(format!("token endpoint request failed: {err}")))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(FcmError::Auth(format!(
                "token endpoint returned {status}: {body}"
            )));
        }

        let token: OAuthTokenResponse = resp
            .json()
            .await
            .map_err(|err| FcmError::Auth(format!("token response parse failed: {err}")))?;

        let expires_at = now + token.expires_in.saturating_sub(TOKEN_REFRESH_SKEW_SECS);
        let mut cache = self.token_cache.lock().await;
        *cache = Some(CachedToken {
            token: token.access_token.clone(),
            expires_at,
        });
        Ok(token.access_token)
    }

    /// Send a data-only message to a single registration token. `collapse_key` de-dupes retries
    /// on the device (analogous to `apns-collapse-id`).
    pub async fn send_data(
        &self,
        token: &str,
        data: &BTreeMap<String, String>,
        collapse_key: Option<&str>,
    ) -> Result<(), FcmError> {
        let access_token = self.access_token().await?;

        let android = AndroidConfig {
            priority: "high",
            // FCM caps collapse_key length; tx ids are well under it.
            collapse_key: collapse_key.map(|c| c.to_string()),
        };
        let message = FcmSendRequest {
            message: FcmMessage {
                token,
                data,
                android,
            },
        };

        let resp = self
            .client
            .post(&self.send_endpoint)
            .bearer_auth(&access_token)
            .json(&message)
            .send()
            .await
            .map_err(FcmError::Request)?;

        if resp.status().is_success() {
            return Ok(());
        }

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
        // FCM v1 surfaces the machine-readable code in error.status (e.g. "NOT_FOUND",
        // "UNREGISTERED", "INVALID_ARGUMENT") and/or error.details[].errorCode.
        let status_str = parsed
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(|e| e.get("status"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        let detail_code = parsed
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(|e| e.get("details"))
            .and_then(|d| d.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find_map(|d| d.get("errorCode").and_then(|c| c.as_str()))
            })
            .map(|s| s.to_string());

        let code = detail_code.or_else(|| status_str.clone());
        match code.as_deref() {
            // Token gone: app uninstalled or token rotated.
            Some("UNREGISTERED") | Some("NOT_FOUND") => Err(FcmError::Unregistered),
            // Malformed token / wrong sender.
            Some("INVALID_ARGUMENT") | Some("SENDER_ID_MISMATCH") => Err(FcmError::InvalidToken),
            // 401/403 → credential/permission issue.
            _ if status == 401 || status == 403 => Err(FcmError::Auth(format!(
                "status={status} reason={status_str:?}"
            ))),
            _ => Err(FcmError::Rejected {
                status,
                reason: status_str,
            }),
        }
    }
}

#[derive(Serialize)]
struct FcmSendRequest<'a> {
    message: FcmMessage<'a>,
}

#[derive(Serialize)]
struct FcmMessage<'a> {
    token: &'a str,
    data: &'a BTreeMap<String, String>,
    android: AndroidConfig,
}

#[derive(Serialize)]
struct AndroidConfig {
    priority: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    collapse_key: Option<String>,
}

fn load_service_account_json(config: &IndexerConfig) -> anyhow::Result<Option<String>> {
    if let Some(inline) = config
        .fcm_service_account_json
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Ok(Some(inline.to_string()));
    }
    if let Some(path) = config.fcm_service_account_path.as_ref() {
        let contents = std::fs::read_to_string(path).map_err(|err| {
            anyhow::anyhow!(
                "failed to read FCM service account at {}: {err}",
                path.display()
            )
        })?;
        return Ok(Some(contents));
    }
    Ok(None)
}

fn unix_time_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

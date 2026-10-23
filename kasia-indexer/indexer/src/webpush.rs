//! Web Push for the installed KaChat web app (KaChat-Desktop docs/WEB_PUSH_SERVER.md).
//!
//! A device with `platform == "web"` registers its browser PushSubscription: the endpoint URL
//! is the device token, and `web_push` carries the subscriber's P-256 key and auth secret.
//! Delivery POSTs an RFC 8291 `aes128gcm`-encrypted record to the endpoint, authenticated to
//! the push service (Apple, Google, Mozilla) with an RFC 8292 VAPID ES256 JWT. The payload is
//! the FCM data map with `"v": "1"` added and `enc_payload` removed: the service worker cannot
//! decrypt message content (the keys stay in the app's vault), and the push provider cannot
//! read the payload at all (it is encrypted to the browser).
//!
//! Configuration (absent key = Web Push off; registrations are kept, nothing is sent):
//!   KACHAT_WEBPUSH_VAPID_PRIVATE_KEY   base64url (no padding) of the raw 32-byte P-256 scalar,
//!                                      or a path to a PEM file (SEC1 or PKCS#8)
//!   KACHAT_WEBPUSH_VAPID_SUBJECT       mailto:/https: contact URL (RFC 8292 `sub`)
//!   KACHAT_WEBPUSH_ENABLED             optional; a clear "off" disables even with a key set
//!
//! The public key is always derived from the private key, so the pair cannot disagree; the
//! client reads it from `GET /v1/push/web-config` and re-subscribes by itself if it changes.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
use anyhow::{Context, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// RFC 8291 §4: one record of 4096 bytes carries at most 3993 bytes of plaintext (minus the
/// delimiter). Anything larger is refused rather than letting a push service drop it.
pub const MAX_PLAINTEXT_BYTES: usize = 3992;
/// The subscription endpoint is a URL; 2048 is generous for every known push service.
pub const MAX_WEB_ENDPOINT_LEN: usize = 2048;

/// A browser PushSubscription, stored on the device registration (outside the auth preimage).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebPushSubscription {
    /// The push service URL; always equal to the registration's `device_token`.
    pub endpoint: String,
    /// base64url (no padding) of the subscriber's uncompressed P-256 public key (65 bytes, 0x04…).
    pub p256dh: String,
    /// base64url (no padding) of the 16-byte auth secret.
    pub auth: String,
}

/// Endpoint hosts the service will POST to. The endpoint is client-supplied, so without this
/// the service could be used to POST to arbitrary URLs (SSRF); every mainstream browser's push
/// service is covered.
pub fn endpoint_host_allowed(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.split('@').next_back().unwrap_or(""); // no userinfo tricks
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    host == "fcm.googleapis.com"
        || host == "updates.push.services.mozilla.com"
        || host.ends_with(".push.apple.com")
        || host.ends_with(".googleapis.com")
        || host.ends_with(".notify.windows.com")
}

/// The VAPID key pair plus subject, loaded once from the environment.
pub struct WebPushConfig {
    signing_key: SigningKey,
    /// base64url (no padding) of the 65-byte uncompressed public point, as `web-config` serves it.
    pub public_key_b64: String,
    subject: String,
}

impl WebPushConfig {
    /// None = not configured (no key, or switched off). An invalid key is an error worth a log,
    /// but never stops the indexer: it behaves as "off".
    pub fn from_env() -> Option<WebPushConfig> {
        let enabled = std::env::var("KACHAT_WEBPUSH_ENABLED")
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
            .unwrap_or(true);
        if !enabled {
            return None;
        }
        let key = std::env::var("KACHAT_WEBPUSH_VAPID_PRIVATE_KEY").ok()?;
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        let subject = std::env::var("KACHAT_WEBPUSH_VAPID_SUBJECT")
            .unwrap_or_default()
            .trim()
            .to_string();
        if subject.is_empty() {
            warn!("[Push][Web] KACHAT_WEBPUSH_VAPID_SUBJECT is not set; Web Push disabled");
            return None;
        }
        if !(subject.starts_with("mailto:") || subject.starts_with("https:")) {
            warn!("[Push][Web] VAPID subject must be a mailto: or https: URL; Web Push disabled");
            return None;
        }
        match Self::load_secret(key) {
            Ok(secret) => {
                let public = secret.public_key().to_encoded_point(false);
                Some(WebPushConfig {
                    signing_key: SigningKey::from(&secret),
                    public_key_b64: URL_SAFE_NO_PAD.encode(public.as_bytes()),
                    subject,
                })
            }
            Err(err) => {
                warn!("[Push][Web] VAPID private key unusable ({err:#}); Web Push disabled");
                None
            }
        }
    }

    /// base64url of the raw 32-byte scalar, or a path to a PEM file (SEC1 or PKCS#8).
    fn load_secret(value: &str) -> anyhow::Result<SecretKey> {
        if let Ok(raw) = URL_SAFE_NO_PAD.decode(value) {
            if raw.len() == 32 {
                return SecretKey::from_slice(&raw).context("not a valid P-256 scalar");
            }
        }
        let pem = std::fs::read_to_string(value)
            .with_context(|| "neither 32 raw base64url bytes nor a readable PEM file")?;
        use p256::pkcs8::DecodePrivateKey;
        if let Ok(key) = SecretKey::from_pkcs8_pem(&pem) {
            return Ok(key);
        }
        p256::SecretKey::from_sec1_pem(&pem).context("PEM is neither PKCS#8 nor SEC1 EC key")
    }

    /// RFC 8292 `Authorization: vapid t=<JWT>, k=<pub>`. The JWT is per push-service origin,
    /// valid 12 h, cached and renewed at half-life.
    fn vapid_jwt(&self, origin: &str, now: u64) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({
            "aud": origin,
            "exp": now + 12 * 3600,
            "sub": self.subject,
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        let signing_input = format!("{header}.{claims}");
        // ES256 over the signing input; the JWT wants the raw r||s (64 bytes), not ASN.1.
        let signature: Signature = self.signing_key.sign(signing_input.as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(signature.to_bytes());
        format!("{signing_input}.{sig}")
    }
}

/// How urgent a push is and how long the push service should keep it for an offline browser,
/// by the payload's `type` (the spec's table).
pub fn ttl_and_urgency(event_type: &str) -> (u32, &'static str) {
    match event_type {
        "broadcast" => (3600, "normal"),
        "kaposts" => (21600, "normal"),
        "name_event" => (86400, "normal"),
        _ => (86400, "high"), // contextual, payment, handshake, group_*, address_activity
    }
}

/// The `Topic` header (collapse on the push service), only where a replay must replace the
/// earlier one: name events, keyed by their dedup string. ≤ 32 chars of base64url.
pub fn topic_for(event_type: &str, dedup: Option<&str>) -> Option<String> {
    if event_type != "name_event" {
        return None;
    }
    let dedup = dedup?;
    let digest = Sha256::digest(dedup.as_bytes());
    let mut topic = URL_SAFE_NO_PAD.encode(digest);
    topic.truncate(32);
    Some(topic)
}

/// What one send came to; mapped onto the dispatcher's `DeliveryOutcome` by the caller.
pub enum WebPushSendResult {
    Delivered,
    /// 404 / 410: the subscription is gone for good; drop the registration.
    Unregistered,
    /// 401 / 403: our VAPID key or JWT is wrong; the token is fine.
    Auth(String),
    Failed(String),
}

/// The VAPID public key when Web Push is configured, read once. Used by `/v1/push/web-config`;
/// the dispatcher builds its own `WebPushConfig` (same env, same answer).
pub fn configured_public_key() -> Option<&'static str> {
    static KEY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    KEY.get_or_init(|| WebPushConfig::from_env().map(|config| config.public_key_b64))
        .as_deref()
}

pub struct WebPushClient {
    config: WebPushConfig,
    http: reqwest::Client,
    /// origin -> (jwt, issued_at); renewed once older than 6 h (half of the 12 h validity).
    jwt_cache: Mutex<HashMap<String, (String, u64)>>,
}

impl WebPushClient {
    pub fn new(config: WebPushConfig) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .context("building the Web Push HTTP client")?,
            jwt_cache: Mutex::new(HashMap::new()),
        })
    }

    fn authorization_for(&self, origin: &str) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut cache = self.jwt_cache.lock().unwrap_or_else(|p| p.into_inner());
        let jwt = match cache.get(origin) {
            Some((jwt, issued)) if now.saturating_sub(*issued) < 6 * 3600 => jwt.clone(),
            _ => {
                let jwt = self.config.vapid_jwt(origin, now);
                cache.insert(origin.to_string(), (jwt.clone(), now));
                jwt
            }
        };
        format!("vapid t={jwt}, k={}", self.config.public_key_b64)
    }

    /// Encrypt `plaintext` to the subscription and POST it. `topic` collapses on the service.
    pub async fn send(
        &self,
        subscription: &WebPushSubscription,
        plaintext: &[u8],
        ttl: u32,
        urgency: &str,
        topic: Option<&str>,
    ) -> WebPushSendResult {
        let endpoint = subscription.endpoint.trim();
        let origin = match origin_of(endpoint) {
            Some(origin) => origin,
            None => return WebPushSendResult::Failed("endpoint is not a valid https URL".into()),
        };
        if plaintext.len() > MAX_PLAINTEXT_BYTES {
            return WebPushSendResult::Failed(format!(
                "payload of {} bytes exceeds the one-record limit; not sent",
                plaintext.len()
            ));
        }
        let body = match encrypt_aes128gcm(&subscription.p256dh, &subscription.auth, plaintext) {
            Ok(body) => body,
            Err(err) => return WebPushSendResult::Failed(format!("encrypting: {err:#}")),
        };

        let mut request = self
            .http
            .post(endpoint)
            .header("Authorization", self.authorization_for(&origin))
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", ttl.to_string())
            .header("Urgency", urgency);
        if let Some(topic) = topic {
            request = request.header("Topic", topic);
        }
        let response = match request.body(body).send().await {
            Ok(response) => response,
            Err(err) => return WebPushSendResult::Failed(format!("request: {err}")),
        };
        let status = response.status().as_u16();
        match status {
            200..=202 => WebPushSendResult::Delivered,
            404 | 410 => WebPushSendResult::Unregistered,
            401 | 403 => {
                let body = response.text().await.unwrap_or_default();
                WebPushSendResult::Auth(format!("{status}: {}", body.chars().take(200).collect::<String>()))
            }
            _ => {
                let body = response.text().await.unwrap_or_default();
                debug!("[Push][Web] {status} from {origin}: {}", body.chars().take(200).collect::<String>());
                WebPushSendResult::Failed(format!("{status} from the push service"))
            }
        }
    }
}

fn origin_of(endpoint: &str) -> Option<String> {
    let rest = endpoint.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    Some(format!("https://{host}"))
}

/// RFC 8291 message encryption with RFC 8188's `aes128gcm` framing: one record, the whole
/// message. Returns the HTTP body: header (salt, rs, keyid = our ephemeral public key) plus the
/// ciphertext.
fn encrypt_aes128gcm(p256dh_b64: &str, auth_b64: &str, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let ua_public_bytes = URL_SAFE_NO_PAD
        .decode(p256dh_b64.trim())
        .map_err(|_| anyhow!("p256dh is not base64url"))?;
    let auth_secret = URL_SAFE_NO_PAD
        .decode(auth_b64.trim())
        .map_err(|_| anyhow!("auth is not base64url"))?;
    if auth_secret.len() != 16 {
        bail!("auth secret is {} bytes, want 16", auth_secret.len());
    }
    let ua_public = PublicKey::from_sec1_bytes(&ua_public_bytes)
        .map_err(|_| anyhow!("p256dh is not a P-256 point"))?;

    let salt: [u8; 16] = rand::random();
    let ephemeral = SecretKey::random(&mut rand::rngs::OsRng);
    encrypt_with(&ua_public, &ua_public_bytes, &auth_secret, &salt, &ephemeral, plaintext)
}

/// The deterministic core, split out so the RFC 8291 §5 example can drive it in a test.
fn encrypt_with(
    ua_public: &PublicKey,
    ua_public_bytes: &[u8],
    auth_secret: &[u8],
    salt: &[u8; 16],
    as_secret: &SecretKey,
    plaintext: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let as_public_bytes = as_secret.public_key().to_encoded_point(false).as_bytes().to_vec();
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_public.as_affine());

    // IKM = HKDF(salt = auth, ecdh_secret, "WebPush: info" || 0x00 || ua_public || as_public, 32)
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public_bytes);
    key_info.extend_from_slice(&as_public_bytes);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|_| anyhow!("hkdf ikm"))?;

    // CEK and nonce from the record salt (RFC 8188).
    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|_| anyhow!("hkdf cek"))?;
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|_| anyhow!("hkdf nonce"))?;

    // One record: plaintext || 0x02 (the last-record delimiter), AES-128-GCM.
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    let ciphertext = Aes128Gcm::new_from_slice(&cek)
        .map_err(|_| anyhow!("cek length"))?
        .encrypt(Nonce::from_slice(&nonce), record.as_slice())
        .map_err(|_| anyhow!("aes-gcm encrypt"))?;

    // aes128gcm header: salt(16) || record size(4, BE) || keyid len(1) || keyid (our public key).
    let mut body = Vec::with_capacity(16 + 4 + 1 + as_public_bytes.len() + ciphertext.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(as_public_bytes.len() as u8);
    body.extend_from_slice(&as_public_bytes);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8291 §5: the worked example, end to end. Inputs and the expected body are the RFC's.
    #[test]
    fn rfc8291_example_round_trips() {
        let plaintext = b"When I grow up, I want to be a watermelon";
        let ua_public_b64 = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
        let auth_b64 = "BTBZMqHH6r4Tts7J_aSIgg";
        let as_secret_b64 = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
        let salt_b64 = "DGv6ra1nlYgDCS1FRnbzlw";
        let expected_b64 = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

        let ua_public_bytes = URL_SAFE_NO_PAD.decode(ua_public_b64).unwrap();
        let ua_public = PublicKey::from_sec1_bytes(&ua_public_bytes).unwrap();
        let auth = URL_SAFE_NO_PAD.decode(auth_b64).unwrap();
        let as_secret = SecretKey::from_slice(&URL_SAFE_NO_PAD.decode(as_secret_b64).unwrap()).unwrap();
        let salt: [u8; 16] = URL_SAFE_NO_PAD.decode(salt_b64).unwrap().try_into().unwrap();

        let body = encrypt_with(&ua_public, &ua_public_bytes, &auth, &salt, &as_secret, plaintext).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.encode(&body), expected_b64);
    }

    #[test]
    fn endpoint_hosts() {
        assert!(endpoint_host_allowed("https://web.push.apple.com/QGuQ"));
        assert!(endpoint_host_allowed("https://fcm.googleapis.com/fcm/send/abc"));
        assert!(endpoint_host_allowed("https://updates.push.services.mozilla.com/wpush/v2/g"));
        assert!(endpoint_host_allowed("https://db5p.notify.windows.com/w/?token=x"));
        assert!(!endpoint_host_allowed("https://evil.example.com/fcm.googleapis.com"));
        assert!(!endpoint_host_allowed("https://fcm.googleapis.com.evil.example/x"));
        assert!(!endpoint_host_allowed("http://fcm.googleapis.com/x"));
        assert!(!endpoint_host_allowed("https://user@evil.example/x"));
    }

    #[test]
    fn topics_only_for_name_events_and_fit() {
        assert_eq!(topic_for("payment", Some("x")), None);
        assert_eq!(topic_for("name_event", None), None);
        let topic = topic_for("name_event", Some("offer:alice:ab12")).unwrap();
        assert_eq!(topic.len(), 32);
        assert!(topic.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }
}

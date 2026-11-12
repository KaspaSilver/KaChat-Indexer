// KaChat fork: fire-and-forget notifications to the push service (kasia-indexer) when the
// processor ingests a push-worthy broadcast or KaPosts action. The push service owns device-token
// lookup + APNs delivery; we just hand it display-ready text. All calls are best-effort and never
// block or fail the indexing pipeline.

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once at startup from the processor's --network: push subtitles show the actor's
/// address with this network's prefix (kaspa: / kaspatest:). Routing never uses it --
/// KaPosts pushes are matched by pubkey.
static TESTNET: AtomicBool = AtomicBool::new(false);

pub fn set_network(network: &str) {
    TESTNET.store(network.starts_with("testnet"), Ordering::Relaxed);
}
use serde_json::json;

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default()
});

/// Base URL of the push service's internal endpoints (same box). Override with PUSH_INTERNAL_URL.
fn base_url() -> String {
    std::env::var("PUSH_INTERNAL_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8600/internal/push".to_string())
}

fn secret() -> Option<String> {
    std::env::var("INTERNAL_PUSH_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Push disabled unless a push URL is configured to be reachable (always defaulted) — but skip
/// entirely if PUSH_INTERNAL_URL is explicitly set to empty.
fn enabled() -> bool {
    !std::env::var("PUSH_INTERNAL_URL").is_ok_and(|v| v.trim().is_empty())
}

tokio::task_local! {
    /// True while a worker processes a transaction older than a few minutes (a reindex, a
    /// catch-up after downtime): its pushes would be stale news, so none are sent.
    pub static QUIET: bool;
}

fn quiet() -> bool {
    QUIET.try_with(|q| *q).unwrap_or(false)
}

fn post(path: &'static str, body: serde_json::Value) {
    post_inner(path, body, None);
}

/// Like `post`, but the subtitle is the actor's .kachat name when they hold one (looked up in the
/// spawned task, so indexing never waits), else their short address.
fn post_with_actor(path: &'static str, body: serde_json::Value, actor_pubkey: &str) {
    post_inner(path, body, Some(actor_pubkey.trim().to_string()));
}

fn post_inner(path: &'static str, mut body: serde_json::Value, actor_pubkey: Option<String>) {
    if !enabled() || quiet() {
        return;
    }
    let url = format!("{}/{}", base_url().trim_end_matches('/'), path);
    let secret = secret();
    tokio::spawn(async move {
        if let Some(actor) = actor_pubkey {
            body["subtitle"] = serde_json::Value::String(actor_display(&actor).await);
        }
        // What the log line names: the target (kaposts) or author (new-post) by key suffix,
        // and the transaction.
        let who = body["target_pubkey"].as_str().or(body["author_pubkey"].as_str()).unwrap_or("");
        let who = who.get(who.len().saturating_sub(8)..).unwrap_or(who).to_string();
        let tx = body["tx_id"].as_str().unwrap_or("").to_string();
        let mut req = CLIENT.post(&url).json(&body);
        if let Some(secret) = secret {
            req = req.header("x-internal-secret", secret);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                if path.starts_with("kaposts") {
                    tracing::info!("push notify {path} sent: target ...{who} tx {tx}");
                } else {
                    tracing::debug!("push notify {path} sent: tx {tx}");
                }
            }
            // A 401 (secret mismatch) or 5xx used to pass silently as "sent".
            Ok(resp) => tracing::warn!(
                "push notify to {} failed: HTTP {} (target ...{who} tx {tx})",
                url,
                resp.status().as_u16()
            ),
            Err(e) => tracing::warn!("push notify to {} failed: {} (target ...{who} tx {tx})", url, e),
        }
    });
}

/// Shorten a kaspa address for a notification subtitle: `kaspa:qq12…wxyz`.
fn shorten_kaspa(addr: &str) -> String {
    match addr.split_once(':') {
        Some((prefix, body)) if body.len() > 12 => {
            format!("{prefix}:{}…{}", &body[..4], &body[body.len() - 4..])
        }
        _ => addr.to_string(),
    }
}

/// Derive the kaspa address (this network's prefix) from a compressed (66-hex) or x-only
/// (64-hex) pubkey.
fn address_from_pubkey_hex(pubkey_hex: &str) -> Option<String> {
    let pk = pubkey_hex.trim();
    let xonly_hex = if pk.len() >= 64 { &pk[pk.len() - 64..] } else { pk };
    let bytes = hex::decode(xonly_hex).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    Some(
        kaspa_addresses::Address::new(
            if TESTNET.load(Ordering::Relaxed) {
                kaspa_addresses::Prefix::Testnet
            } else {
                kaspa_addresses::Prefix::Mainnet
            },
            kaspa_addresses::Version::PubKey,
            &bytes,
        )
        .to_string(),
    )
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Decode a base64 KaChat message to its text body (marker-stripped, ~140 chars). Returns an
/// empty string for a plain repost (body is exactly the marker). None if it can't be decoded.
pub fn kachat_snippet(base64_message: &str) -> Option<String> {
    use base64::{Engine, engine::general_purpose};
    let bytes = general_purpose::STANDARD.decode(base64_message.trim()).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let body = text.strip_prefix('\u{2060}').unwrap_or(&text);
    Some(truncate_chars(body.trim(), 140))
}

/// Build a broadcast notification body: reply envelope → inner text; an image file → "Photo";
/// any other file/audio → "Voice message"; else the text verbatim (~150 chars).
pub fn broadcast_preview(content: &str) -> String {
    let trimmed = content.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "reply" => {
                // A reply envelope (MessageReplyContent) carries the reply's own text in `text`;
                // unwrap it BEFORE truncating, or a 150-char cut of the raw JSON drops the text
                // entirely and the phone shows only "Replied to a message". `content` is a legacy
                // fallback for any older envelope shape.
                if let Some(inner) = v
                    .get("text")
                    .or_else(|| v.get("content"))
                    .and_then(|c| c.as_str())
                {
                    if inner.contains("data:audio") {
                        return "Voice message".to_string();
                    }
                    return truncate_chars(inner, 150);
                }
            }
            "file" | "audio" => {
                let image = v.get("mimeType").and_then(|m| m.as_str()).is_some_and(|m| m.starts_with("image/"))
                    || v.get("content").and_then(|c| c.as_str()).is_some_and(|c| c.starts_with("data:image/"));
                return if image { "Photo" } else { "Voice message" }.to_string();
            }
            _ => {}
        }
    }
    if trimmed.contains("data:audio") {
        return "Voice message".to_string();
    }
    truncate_chars(trimmed, 150)
}

/// True if `content` is a reaction envelope (`{"type":"reaction",...}`). Reactions are invisible
/// protocol traffic and must never generate a push (NOTIFICATION_EXTENSIONS_TODO.md §3). Broadcasts
/// are plaintext, so we can inspect and suppress here; encrypted 1:1 reactions are suppressed
/// client-side instead.
pub fn is_reaction_content(content: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(content.trim())
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|t| t == "reaction"))
        .unwrap_or(false)
}

/// True if `content` is an edit envelope (`{"type":"edit","targetTxId":…,"text":…}`). An edit
/// rewrites an earlier message in place (MESSAGING.md "Message Edits") — there is nothing new to
/// announce, so it must never generate a push. Broadcasts are plaintext, so we suppress here.
pub fn is_edit_content(content: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(content.trim())
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|t| t == "edit"))
        .unwrap_or(false)
}

/// The note or photo a Public Chats media part belongs to: its kind (`voice` or `photo`), its
/// lowercased id and its `count`, or None for anything that isn't a well-formed part (a plain
/// voice message or photo pushes as before). KaChat's long voice notes and large photos go out as
/// up to 12 parts, one message each, tagged `voicePart` on `audio/*` and `mediaPart` on `image/*`
/// (docs/ROOM_VOICE_PART_PUSHES.md, KaChat iPhone MESSAGING.md). A tag under the other kind's key
/// is no tag.
pub fn media_part_key(content: &str) -> Option<(&'static str, String, u32)> {
    let v = serde_json::from_str::<serde_json::Value>(content.trim()).ok()?;
    if v.get("type")?.as_str()? != "file" {
        return None;
    }
    let mime = v.get("mimeType")?.as_str()?;
    let (kind, tag) = if mime.starts_with("audio/") {
        ("voice", "voicePart")
    } else if mime.starts_with("image/") {
        ("photo", "mediaPart")
    } else {
        return None;
    };
    let part = v.get(tag)?.as_object()?;
    let id = part.get("id")?.as_str()?;
    if !(16..=64).contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let count = part.get("count")?.as_u64()?;
    let index = part.get("index")?.as_u64()?;
    ((2..=12).contains(&count) && (1..=count).contains(&index))
        .then(|| (kind, id.to_ascii_lowercase(), count as u32))
}

/// How long a note's (or photo's) first push holds back its other parts. A whole note is out within a minute
/// or two; a retried part rarely lands later than this.
const MEDIA_NOTE_PUSHED_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// A part's note: (channel, sender, kind, id, count).
type MediaNoteKey = (String, String, &'static str, String, u32);

/// Notes and photos that have already rung. In memory only: after a restart a note mid-send
/// rings once more, which is fine.
#[derive(Default)]
struct MediaNotesPushed {
    at: std::collections::HashMap<MediaNoteKey, std::time::Instant>,
}

impl MediaNotesPushed {
    /// True for the first part of a note to arrive (it rings); false for the rest.
    fn first(&mut self, key: MediaNoteKey, now: std::time::Instant) -> bool {
        self.at.retain(|_, pushed| now.duration_since(*pushed) < MEDIA_NOTE_PUSHED_TTL);
        match self.at.entry(key) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(now);
                true
            }
        }
    }
}

static MEDIA_NOTES_PUSHED: Lazy<std::sync::Mutex<MediaNotesPushed>> = Lazy::new(Default::default);

/// Whether a room message should ring as far as media parts go: anything that isn't a part does,
/// and a part does only when it is the first of its note or photo seen here. Call it last, just
/// before pushing, so a part that wasn't going to push anyway doesn't use up its one push.
pub fn rings_as_media_part(channel: &str, sender_address: &str, content: &str) -> bool {
    let Some((kind, id, count)) = media_part_key(content) else {
        return true;
    };
    let key = (channel.to_string(), sender_address.to_string(), kind, id, count);
    MEDIA_NOTES_PUSHED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .first(key, std::time::Instant::now())
}

/// Notify the push service of a new broadcast in a tracked channel.
pub fn notify_broadcast(channel: &str, sender_address: &str, body: String, tx_id: &str) {
    post(
        "broadcast",
        json!({
            "channel": channel,
            "sender_address": sender_address,
            "subtitle": shorten_kaspa(sender_address),
            "body": body,
            "tx_id": tx_id,
        }),
    );
}

/// KaPosts push subtitle for an actor: their address (this network's prefix) shortened, else the
/// pubkey shortened.
fn actor_subtitle(actor_pubkey: &str) -> String {
    address_from_pubkey_hex(actor_pubkey)
        .map(|a| shorten_kaspa(&a))
        .unwrap_or_else(|| {
            let pk = actor_pubkey.trim();
            if pk.len() > 12 {
                format!("{}…{}", &pk[..6], &pk[pk.len() - 4..])
            } else {
                pk.to_string()
            }
        })
}

/// This stack's webserver (same container), whose `/identity/{address}` gives an address's
/// .kachat label. Override with KACHAT_IDENTITY_URL (the base, without `/identity`); empty = off.
fn identity_base_url() -> Option<String> {
    match std::env::var("KACHAT_IDENTITY_URL") {
        Ok(v) if v.trim().is_empty() => None,
        Ok(v) => Some(v.trim().trim_end_matches('/').to_string()),
        Err(_) => Some(format!(
            "http://127.0.0.1:{}",
            std::env::var("WEBSERVER_PORT")
                .ok()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "3080".to_string())
        )),
    }
}

/// The `label` of an `/identity/{address}` answer as a display name (`alice.kachat`), if any.
fn label_display(identity: &serde_json::Value) -> Option<String> {
    let label = identity.get("label")?.as_str()?.trim();
    let label = label.strip_suffix(".kachat").unwrap_or(label);
    (!label.is_empty()).then(|| format!("{label}.kachat"))
}

/// KaPosts push subtitle for an actor: their .kachat name when they hold one, else
/// `actor_subtitle`. The lookup is short (2 s) and any failure falls back to the address.
async fn actor_display(actor_pubkey: &str) -> String {
    let fallback = actor_subtitle(actor_pubkey);
    let (Some(base), Some(address)) = (identity_base_url(), address_from_pubkey_hex(actor_pubkey))
    else {
        return fallback;
    };
    let lookup = async {
        let resp = CLIENT
            .get(format!("{base}/identity/{address}"))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        label_display(&resp.json::<serde_json::Value>().await.ok()?)
    };
    lookup.await.unwrap_or(fallback)
}

/// Body of the new-post push (PUSH_EXTENSIONS.md §3a): `posted: <snippet>` (marker-stripped,
/// ~140 chars, like a reply's), or `posted` when the text cannot be decoded.
fn new_post_body(base64_message: &str) -> String {
    match kachat_snippet(base64_message) {
        Some(snippet) if !snippet.is_empty() => format!("posted: {snippet}"),
        _ => "posted".to_string(),
    }
}

/// Notify the push service of a new TOP-LEVEL post or poll by `author_pubkey` (PUSH_EXTENSIONS.md
/// §3a). The push service delivers it to every device whose `kaposts_watch_authors` holds the
/// author (x-only), except the author's own devices. `tx_id` is the post itself (= `postId`).
pub fn notify_kaposts_new_post(author_pubkey: &str, base64_message: &str, tx_id: &str) {
    post_with_actor(
        "kaposts-new-post",
        json!({
            "author_pubkey": author_pubkey.trim().to_lowercase(),
            "subtitle": actor_subtitle(author_pubkey),
            "body": new_post_body(base64_message),
            "post_id": tx_id,
            "tx_id": tx_id,
        }),
        author_pubkey,
    );
}

/// Notify the push service of a KaPosts action targeting `target_pubkey`'s content.
/// `actor_pubkey` is the actor (skipped from delivery); `post_id` is the target content txid.
/// `action` is the coarse machine-readable kind (`like`/`dislike`/`comment`/`repost`/`follow`) so
/// the push service can honor per-type KaPosts notification toggles (kaposts_notify). `kind` is the
/// FINE kind (`vote_up`/`vote_down`/`reply`/`quote`/`repost`/`follow`/`mention`) forwarded to the
/// client for precise rendering.
pub fn notify_kaposts(
    target_pubkey: &str,
    actor_pubkey: &str,
    action: &str,
    kind: &str,
    body: String,
    post_id: Option<String>,
    tx_id: &str,
) {
    let subtitle = actor_subtitle(actor_pubkey);
    post_with_actor(
        "kaposts",
        json!({
            "target_pubkey": target_pubkey.trim().to_lowercase(),
            "actor_pubkey": actor_pubkey.trim().to_lowercase(),
            "action": action,
            "kaposts_kind": kind,
            "subtitle": subtitle,
            "body": body,
            "post_id": post_id,
            "tx_id": tx_id,
        }),
        actor_pubkey,
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_well_formed_media_part_has_a_note_key() {
        let part = |tag: &str, mime: &str| {
            format!(
                r#"{{"type":"file","name":"part-2of12","size":30807,"mimeType":"{mime}",{tag}"content":"data:{mime};base64,AAAA"}}"#
            )
        };
        let tagged = |key: &str, mime: &str, id: &str, index: &str, count: &str| {
            part(&format!(r#""{key}":{{"id":"{id}","index":{index},"count":{count}}},"#), mime)
        };
        let voice = |id: &str, index: &str, count: &str| tagged("voicePart", "audio/webm", id, index, count);
        let id = "3F9A0B1C2D3E4F5061728394A5B6C7D8";
        let lower = id.to_ascii_lowercase();
        assert_eq!(super::media_part_key(&voice(id, "2", "12")), Some(("voice", lower.clone(), 12)));
        assert!(super::media_part_key(&voice(id, "1", "2")).is_some());
        assert_eq!(
            super::media_part_key(&tagged("mediaPart", "image/jpeg", id, "2", "6")),
            Some(("photo", lower, 6))
        );
        for (id, index, count) in [
            (id, "0", "12"),
            (id, "13", "12"),
            (id, "1", "1"),
            (id, "1", "13"),
            ("0123456789", "1", "2"),
            ("zz9a0b1c2d3e4f5061728394a5b6c7d8", "1", "2"),
            (id, "true", "2"),
            (id, r#""1""#, "2"),
            (id, "1.5", "2"),
        ] {
            assert_eq!(super::media_part_key(&voice(id, index, count)), None, "{id} {index}/{count}");
        }
        // A tag under the other kind's key is no tag.
        assert_eq!(super::media_part_key(&tagged("voicePart", "image/jpeg", id, "1", "2")), None);
        assert_eq!(super::media_part_key(&tagged("mediaPart", "audio/webm", id, "1", "2")), None);
        assert_eq!(super::media_part_key(&tagged("voicePart", "video/mp4", id, "1", "2")), None);
        assert_eq!(super::media_part_key(&part("", "audio/webm")), None);
        assert_eq!(super::media_part_key("gm"), None);
    }

    #[test]
    fn a_voice_note_or_photo_rings_once_per_sender_and_room() {
        let mut pushed = super::MediaNotesPushed::default();
        let key = |channel: &str, sender: &str, kind: &'static str| {
            (channel.to_string(), sender.to_string(), kind, "3f9a0b1c2d3e4f50".to_string(), 3)
        };
        let start = std::time::Instant::now();
        assert!(pushed.first(key("kaspa", "kaspa:alice", "voice"), start));
        assert!(!pushed.first(key("kaspa", "kaspa:alice", "voice"), start));
        // The same id from someone else, in another room, or for a photo is another note.
        assert!(pushed.first(key("kaspa", "kaspa:bob", "voice"), start));
        assert!(pushed.first(key("general", "kaspa:alice", "voice"), start));
        assert!(pushed.first(key("kaspa", "kaspa:alice", "photo"), start));
        assert!(!pushed.first(key("kaspa", "kaspa:alice", "photo"), start));
        // Once the entry expires the key rings again.
        let later = start + super::MEDIA_NOTE_PUSHED_TTL;
        assert!(pushed.first(key("kaspa", "kaspa:alice", "voice"), later));
    }

    #[test]
    fn a_photo_in_a_room_is_announced_as_a_photo() {
        let file = |mime: &str, data: &str| {
            format!(r#"{{"type":"file","name":"f","size":1,"mimeType":"{mime}","content":"data:{data};base64,AAAA"}}"#)
        };
        assert_eq!(super::broadcast_preview(&file("image/jpeg", "image/jpeg")), "Photo");
        assert_eq!(super::broadcast_preview(&file("application/octet-stream", "image/png")), "Photo");
        assert_eq!(super::broadcast_preview(&file("audio/webm", "audio/webm")), "Voice message");
        assert_eq!(super::broadcast_preview(&file("application/pdf", "application/pdf")), "Voice message");
    }

    use super::*;

    #[test]
    fn subtitle_address_follows_the_network() {
        let pk = "6dece92abd087978562b0e47943d859bd444672f89bc68fc8bfa03a3d0b27ee8";
        set_network("testnet-10");
        assert!(address_from_pubkey_hex(pk).unwrap().starts_with("kaspatest:"));
        set_network("mainnet");
        assert!(address_from_pubkey_hex(pk).unwrap().starts_with("kaspa:"));
    }

    #[test]
    fn identity_label_becomes_a_kachat_name() {
        assert_eq!(label_display(&json!({"label": "alice"})).as_deref(), Some("alice.kachat"));
        assert_eq!(label_display(&json!({"label": "alice.kachat"})).as_deref(), Some("alice.kachat"));
        assert_eq!(label_display(&json!({"label": null})), None);
        assert_eq!(label_display(&json!({"label": "  "})), None);
        assert_eq!(label_display(&json!({})), None);
    }

    #[test]
    fn new_post_body_is_posted_plus_snippet() {
        use base64::{Engine, engine::general_purpose};
        let b64 = general_purpose::STANDARD.encode("\u{2060}gm kaspa".as_bytes());
        assert_eq!(new_post_body(&b64), "posted: gm kaspa");
        assert_eq!(new_post_body("%%%"), "posted");
    }

    #[test]
    fn reply_preview_reads_inner_text_not_raw_json() {
        // A reply envelope's txId + sender alone are ~130 chars, so truncating the raw JSON at 150
        // would drop the text. Must unwrap `text` first (BROADCAST_INDEXER.md §5).
        let env = r#"{"type":"reply","replyToId":"a3f9c1e28b7d4655aa10cc9021fe4477deadbeef00112233","replyToSender":"kaspa:qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq","replyToPreview":"hi there","text":"actually the answer is 42"}"#;
        assert_eq!(broadcast_preview(env), "actually the answer is 42");
    }

    #[test]
    fn file_and_audio_previews_are_voice_message() {
        assert_eq!(broadcast_preview(r#"{"type":"file","content":"data:audio/m4a;base64,AAA"}"#), "Voice message");
        assert_eq!(broadcast_preview(r#"{"type":"audio","content":"x"}"#), "Voice message");
    }

    #[test]
    fn plain_text_is_verbatim_and_truncated() {
        assert_eq!(broadcast_preview("gm kaspa"), "gm kaspa");
        let long: String = "x".repeat(300);
        assert_eq!(broadcast_preview(&long).chars().count(), 150);
    }

    #[test]
    fn reactions_and_edits_are_suppressed() {
        let reaction = r#"{"type":"reaction","targetTxId":"abc","emoji":"❤️","action":"add"}"#;
        let edit = r#"{"type":"edit","targetTxId":"abc","text":"fixed typo"}"#;
        assert!(is_reaction_content(reaction));
        assert!(is_edit_content(edit));
        // Cross-checks: a reaction is not an edit and vice versa; plain text is neither.
        assert!(!is_edit_content(reaction));
        assert!(!is_reaction_content(edit));
        assert!(!is_reaction_content("gm") && !is_edit_content("gm"));
    }
}

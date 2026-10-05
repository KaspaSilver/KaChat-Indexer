//! Address profiles (KACHAT_NAMES_INDEXER.md §C, as revised 2026-10-02 / c180259).
//!
//! A profile record is `kchat:1:profile:<json>` on a self-transfer. The current format is a
//! **source link per piece** — `avatar`, `banner`, `bio` each say *where* that piece comes
//! from (they may be three different accounts, and are never the picture/text themselves) —
//! plus a `linktr.ee` link and a `primaryName`. Full replacement, newest-wins, ≤ 2 KB. The
//! allowed platforms differ per field. The indexer stores/serves the links as strings and
//! **never fetches, stores or proxies pictures or bios** — each device resolves those itself,
//! so each platform's own moderation applies. No free text, no display name.

use serde::{Deserialize, Serialize};

/// Maximum profile-record size (§C).
pub const MAX_PROFILE_BYTES: usize = 2048;

// Per-platform normalized `https://<host>/` prefixes the app writes.
const X: &str = "https://x.com/";
const YOUTUBE: &str = "https://www.youtube.com/";
const DISCORD: &str = "https://discord.gg/";
const TELEGRAM: &str = "https://t.me/";
const TWITCH: &str = "https://www.twitch.tv/";
const KICK: &str = "https://kick.com/";
const GITHUB: &str = "https://github.com/";
const FACEBOOK: &str = "https://www.facebook.com/";
const INSTAGRAM: &str = "https://www.instagram.com/";
const TIKTOK: &str = "https://www.tiktok.com/";
const LINKEDIN: &str = "https://www.linkedin.com/";

// Which platforms each field accepts (§C table).
const AVATAR_HOSTS: &[&str] = &[X, YOUTUBE, DISCORD, TELEGRAM, TWITCH, KICK, GITHUB, FACEBOOK, INSTAGRAM, TIKTOK, LINKEDIN];
const BANNER_HOSTS: &[&str] = &[X, YOUTUBE, DISCORD];
const BIO_HOSTS: &[&str] = &[X, YOUTUBE, DISCORD, TELEGRAM, TWITCH, KICK, GITHUB];

const LINKTREE_PREFIX: &str = "https://linktr.ee/";

/// A validated, normalized profile. Fields that failed validation are dropped (`None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linktree: Option<String>,
    #[serde(rename = "primaryName", skip_serializing_if = "Option::is_none")]
    pub primary_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawProfile {
    #[serde(default)]
    v: Option<u32>,
    #[serde(default)]
    avatar: Option<String>,
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    bio: Option<String>,
    #[serde(default)]
    linktree: Option<String>,
    #[serde(rename = "primaryName", default)]
    primary_name: Option<String>,
}

/// Whether `url` is a normalized profile link on one of `hosts` (with a handle after it).
fn allowed(url: &str, hosts: &[&str]) -> bool {
    hosts.iter().any(|h| url.starts_with(h) && url.len() > h.len())
}

/// Parse + validate a profile record's JSON. `None` if over 2 KB or not a `v:1` object; a
/// link not allowed in its field is dropped (set to `None`) rather than rejecting the record.
pub fn parse_profile(json: &str) -> Option<Profile> {
    if json.len() > MAX_PROFILE_BYTES {
        return None;
    }
    let raw: RawProfile = serde_json::from_str(json).ok()?;
    if raw.v != Some(1) {
        return None;
    }
    Some(Profile {
        avatar: raw.avatar.filter(|s| allowed(s, AVATAR_HOSTS)),
        banner: raw.banner.filter(|s| allowed(s, BANNER_HOSTS)),
        bio: raw.bio.filter(|s| allowed(s, BIO_HOSTS)),
        linktree: raw.linktree.filter(|s| s.starts_with(LINKTREE_PREFIX) && s.len() > LINKTREE_PREFIX.len()),
        primary_name: raw.primary_name.filter(|n| crate::is_valid_name(n.as_bytes())),
    })
}

/// The platform a stored profile link points at (`x`, `youtube`, …), for stats. `None` for
/// anything not on the allowlist.
pub fn platform_of(url: &str) -> Option<&'static str> {
    [
        (X, "x"),
        (YOUTUBE, "youtube"),
        (DISCORD, "discord"),
        (TELEGRAM, "telegram"),
        (TWITCH, "twitch"),
        (KICK, "kick"),
        (GITHUB, "github"),
        (FACEBOOK, "facebook"),
        (INSTAGRAM, "instagram"),
        (TIKTOK, "tiktok"),
        (LINKEDIN, "linkedin"),
    ]
    .into_iter()
    .find(|(h, _)| url.starts_with(h))
    .map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_source_link_per_piece() {
        let p = parse_profile(
            r#"{"v":1,"avatar":"https://www.instagram.com/alice/","banner":"https://www.youtube.com/@alice","bio":"https://t.me/alice","linktree":"https://linktr.ee/alice","primaryName":"alice"}"#,
        )
        .unwrap();
        assert_eq!(p.avatar.as_deref(), Some("https://www.instagram.com/alice/"));
        assert_eq!(p.banner.as_deref(), Some("https://www.youtube.com/@alice"));
        assert_eq!(p.bio.as_deref(), Some("https://t.me/alice"));
        assert_eq!(p.linktree.as_deref(), Some("https://linktr.ee/alice"));
        assert_eq!(p.primary_name.as_deref(), Some("alice"));
    }

    #[test]
    fn enforces_per_field_allowlists() {
        // Instagram/TikTok/Facebook/LinkedIn are avatar-only; not allowed as banner or bio.
        let p = parse_profile(
            r#"{"v":1,"avatar":"https://www.tiktok.com/@a","banner":"https://www.instagram.com/a/","bio":"https://www.facebook.com/a"}"#,
        )
        .unwrap();
        assert_eq!(p.avatar.as_deref(), Some("https://www.tiktok.com/@a")); // ok for avatar
        assert_eq!(p.banner, None); // instagram not allowed as banner
        assert_eq!(p.bio, None); // facebook not allowed as bio
        // Telegram/Twitch/Kick/GitHub: bio+avatar yes, banner no.
        let p2 = parse_profile(r#"{"v":1,"banner":"https://github.com/a","bio":"https://github.com/a","avatar":"https://github.com/a"}"#).unwrap();
        assert_eq!(p2.banner, None);
        assert_eq!(p2.bio.as_deref(), Some("https://github.com/a"));
        assert_eq!(p2.avatar.as_deref(), Some("https://github.com/a"));
    }

    #[test]
    fn drops_old_and_bad_fields() {
        // Old single "social"/"links" + free text are ignored; bad linktree + name dropped.
        let p = parse_profile(
            r#"{"v":1,"social":"https://x.com/a","bio":"not a url","linktree":"https://evil/a","primaryName":"-bad-"}"#,
        )
        .unwrap();
        assert_eq!(p, Profile::default());
    }

    #[test]
    fn requires_v1_and_size_limit() {
        assert!(parse_profile(r#"{"avatar":"https://x.com/a"}"#).is_none());
        assert!(parse_profile(r#"{"v":2,"avatar":"https://x.com/a"}"#).is_none());
        let big = format!("{{\"v\":1,\"avatar\":\"https://x.com/{}\"}}", "a".repeat(2100));
        assert!(parse_profile(&big).is_none());
    }

    #[test]
    fn platform_from_host() {
        assert_eq!(platform_of("https://x.com/a"), Some("x"));
        assert_eq!(platform_of("https://www.youtube.com/@a"), Some("youtube"));
        assert_eq!(platform_of("https://linktr.ee/a"), None);
    }

    #[test]
    fn banner_only_three_platforms() {
        for ok in ["https://x.com/a", "https://www.youtube.com/@a", "https://discord.gg/abc"] {
            assert!(allowed(ok, BANNER_HOSTS), "{ok} allowed as banner");
        }
        for no in ["https://t.me/a", "https://github.com/a", "https://www.twitch.tv/a"] {
            assert!(!allowed(no, BANNER_HOSTS), "{no} not a banner");
        }
    }
}

//! Address profiles (KACHAT_NAMES_INDEXER.md §C, as revised 2026-10-02).
//!
//! A profile record is `kchat:1:profile:<json>` on a self-transfer. The 2026-10-02 format
//! is exactly three fields — one allowlisted social link, a `linktr.ee` link, and a
//! `primaryName` — a full replacement, newest-wins, ≤ 2 KB. Older fields (`avatar`,
//! `banner`, `bio`, `links`) are dropped. The indexer stores and serves the two links as
//! strings and **never fetches pictures or bios** — each device resolves those from the
//! social profile, so the platform's own moderation applies.

use serde::{Deserialize, Serialize};

/// Maximum profile-record size (§C).
pub const MAX_PROFILE_BYTES: usize = 2048;

/// Allowed `social` hosts, each as the normalized `https://<host>/` prefix the app writes.
const SOCIAL_HOST_PREFIXES: &[&str] = &[
    "https://x.com/",
    "https://www.youtube.com/",
    "https://www.facebook.com/",
    "https://www.instagram.com/",
    "https://www.tiktok.com/",
    "https://www.twitch.tv/",
    "https://kick.com/",
    "https://github.com/",
    "https://t.me/",
    "https://www.linkedin.com/",
    "https://discord.gg/",
];

const LINKTREE_PREFIX: &str = "https://linktr.ee/";

/// A validated, normalized profile. Fields that failed validation are dropped (`None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub social: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linktree: Option<String>,
    #[serde(rename = "primaryName", skip_serializing_if = "Option::is_none")]
    pub primary_name: Option<String>,
}

/// The raw record as it may arrive (unknown fields ignored by serde default).
#[derive(Debug, Deserialize)]
struct RawProfile {
    #[serde(default)]
    v: Option<u32>,
    #[serde(default)]
    social: Option<String>,
    #[serde(default)]
    linktree: Option<String>,
    #[serde(rename = "primaryName", default)]
    primary_name: Option<String>,
}

fn valid_social(url: &str) -> bool {
    SOCIAL_HOST_PREFIXES.iter().any(|p| url.starts_with(p) && url.len() > p.len())
}

/// Parse + validate a profile record's JSON. Returns `None` if it is over 2 KB, is not a
/// `v:1` object, or carries no usable field after validation. Invalid individual fields are
/// dropped rather than rejecting the whole record.
pub fn parse_profile(json: &str) -> Option<Profile> {
    if json.len() > MAX_PROFILE_BYTES {
        return None;
    }
    let raw: RawProfile = serde_json::from_str(json).ok()?;
    if raw.v != Some(1) {
        return None;
    }
    let social = raw.social.filter(|s| valid_social(s));
    let linktree = raw
        .linktree
        .filter(|s| s.starts_with(LINKTREE_PREFIX) && s.len() > LINKTREE_PREFIX.len());
    let primary_name = raw
        .primary_name
        .filter(|n| crate::is_valid_name(n.as_bytes()));

    let profile = Profile { social, linktree, primary_name };
    // A record with nothing usable is still a valid "cleared" profile — keep it, since it is
    // a full replacement (it legitimately clears a previous one).
    Some(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_new_three_field_format() {
        let p = parse_profile(r#"{"v":1,"social":"https://x.com/alice","linktree":"https://linktr.ee/alice","primaryName":"alice"}"#).unwrap();
        assert_eq!(p.social.as_deref(), Some("https://x.com/alice"));
        assert_eq!(p.linktree.as_deref(), Some("https://linktr.ee/alice"));
        assert_eq!(p.primary_name.as_deref(), Some("alice"));
    }

    #[test]
    fn drops_old_fields_and_bad_links() {
        // avatar/banner/bio/links are ignored; a non-allowlisted social + bad linktree drop.
        let p = parse_profile(
            r#"{"v":1,"avatar":"https://evil/x.png","bio":"hi","social":"https://evil.example/a","linktree":"https://notlinktree/a","primaryName":"-bad-"}"#,
        )
        .unwrap();
        assert_eq!(p, Profile::default()); // everything dropped
    }

    #[test]
    fn requires_v1_and_size_limit() {
        assert!(parse_profile(r#"{"social":"https://x.com/a"}"#).is_none()); // no v
        assert!(parse_profile(r#"{"v":2,"social":"https://x.com/a"}"#).is_none());
        let big = format!("{{\"v\":1,\"social\":\"https://x.com/{}\"}}", "a".repeat(2100));
        assert!(parse_profile(&big).is_none());
    }

    #[test]
    fn validates_each_social_platform() {
        for ok in [
            "https://x.com/a",
            "https://www.youtube.com/@a",
            "https://www.instagram.com/a/",
            "https://t.me/a",
            "https://discord.gg/abc",
            "https://github.com/a",
        ] {
            assert!(valid_social(ok), "{ok} should be allowed");
        }
        for bad in ["http://x.com/a", "https://x.com/", "https://twitter.com/a", "ftp://x.com/a"] {
            assert!(!valid_social(bad), "{bad} should be rejected");
        }
    }
}

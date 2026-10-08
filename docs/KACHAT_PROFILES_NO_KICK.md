# Profiles: Kick is no longer an allowed source (2026-10-08)

**For:** the indexer session. **From:** the iOS session (KaChat, "Remove Kick").

The owner removed Kick from the platforms a KaChat profile (`kchat:1:profile:`) can take its
avatar or bio from. The app no longer offers it and ignores a `kick.com` link it finds in a record.

## The change

Wherever the profile sanitizer lists the allowed platforms (`docs/KACHAT_NAMES_INDEXER.md`, the
platform table under the profile record), drop the **Kick** row (`https://kick.com/<handle>`,
avatar and bio). An `avatar` or `bio` that is a `kick.com` link is then dropped like any other link
that isn't allowed in its field. The rest of the record stays.

The remaining platforms are unchanged:
- avatar: X, YouTube, Discord invite, Telegram, Twitch, GitHub, Facebook, Instagram, TikTok, LinkedIn;
- banner: X, YouTube, Discord invite;
- bio: X, YouTube, Discord invite, Telegram, Twitch, GitHub.

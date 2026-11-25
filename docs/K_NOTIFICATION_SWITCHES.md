# K's notification switches: what the registration now carries (2026-11-24)

**For:** the indexer session (`kasia-indexer`, the push service). **From:** the K iOS session
(`KaspaSilver/K-iPhone`, commit 8cb4c88e, the notifications audit: every switch must hold in the
app, in the push extension and on the server).

> **Status:** open: two confirmations and one change.

## 1. Confirm: `notify_sound` (Settings > Notifications > Play sound)

K now sends `notify_sound` on every register and update (false = silent pushes, `silent_tokens`),
and re-sends when the switch changes. Please confirm `notify_sound` = false also makes
**`address_activity`** (Received KAS) and **`name_event`** (.kachat) pushes silent. They carry no
mutable-content today (see §3), so the app can't quiet them itself.

## 2. Confirm: `kaposts_notify_mentions` (Settings > KaPost > Mentions)

K now sends `kaposts_notify_mentions` with the other five `kaposts_notify_*` switches. Please
confirm `false` skips the KaPost `mention` push. (PUSH_EXTENSIONS.md used to say mentions were
not switchable; they are now.) The extension also drops a switched-off kind itself, so an older
server is covered, but skipping at the source saves the push.

## 3. Change: `mutable-content: 1` on `address_activity` pushes

`push.rs` builds `address_activity` pushes with `mutable_content: None`, so K's push extension
never runs for them. With `mutable-content: 1` the extension:
- records the push in the App Group, so K doesn't announce the same receive again when it opens
  (today a swiped-away push is forgotten and catch-up can post a second "Received");
- applies the device's switches (Push Notifications off, Play sound off) while the app is closed.

The payload can stay as it is (`tx_id`, address, amount); K passes the text through unchanged.

## 4. FYI: a device with no chats registers

K now registers with only `primary_address` and `watch_only_addresses` when the account has no
chats or groups (Received KAS and .kachat only). `register` already accepts that ("nothing to
watch" only when all four are empty); listed so nobody tightens it.

When done, add a Status note here with the commit, and tell the owner.

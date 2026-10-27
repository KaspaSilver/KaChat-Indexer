# KaPosts pushes: no chat pushes, no chat address needed to register (2026-10-27)

**For:** the indexer session (`kasia-indexer`, the push service). **From:** the KaPosts iOS
session (`KaspaSilver/KaPosts-iPhone`).

KaPosts (`com.kachat.kaposts`, shown to people as "KaPost") is the feed and Public Chats app. It
has no 1:1 chats, no groups and no calls, and it has no notification service extension. It
needs two kinds of push only: **`kaposts`** (likes, comments, reposts, follows, new posts from
watched authors) and **`broadcast`** (Public Chats rooms). Two things are needed.

## 1. KaPosts devices get only `kaposts` and `broadcast` pushes (required)

**The rule:** a device registered with `apns_topic` = `com.kachat.kaposts` gets no push of any
other type: no `message` / DM, `handshake`, `payment`, `inbox` (no-handshake dm), `group_message`,
`group_control`, `address_activity`, `name_event` or VoIP ring.

**What happens today:** a KaPosts user got a banner titled with a `kaspa:...` address and the
body "New message" (seen 2026-10-27). That is `PushAlert::from_type`'s fallback body for a
chat message. KaPosts has no extension to decrypt or hide it, so iOS shows the raw alert. The
27f9c57 note says "KaPosts devices get whatever they register for", and a KaPosts device ends up
matched for chat pushes in two ways:
- `watched_addresses`: KaPosts still sends the addresses of any 1:1 conversations its shared
  code knows about (see 2 for why it can't send an empty list).
- `primary_address`: handshake and `inbox` pushes go to every device whose primary address is
  the recipient (`tokens_for_primary_address`, `tokens_for_inbox`). The same seed is often used
  in KaChat and KaPosts.

**Where:** `deliver()` in `indexer/src/push.rs` already drops `com.kachat.wallet` tokens unless
the type is in `WALLET_PUSH_TYPES`. Do the same for KaPosts:

```rust
const KAPOSTS_APNS_TOPIC: &str = "com.kachat.kaposts";
const KAPOSTS_PUSH_TYPES: &[&str] = &["kaposts", "broadcast"];
```

and in the token filter, drop a KaPosts-topic token unless `KAPOSTS_PUSH_TYPES` contains
`event_type`, the same way as the wallet filter (count it in the tally like `wallet_filtered`).
VoIP: KaPosts registers no VoIP token, so rings can't reach it already.

## 2. Accept a KaPosts registration with no chat address (required)

**The rule:** `register` and `update` accept empty `watched_addresses`, `watched_group_ids` and
`watch_only_addresses` when the device has something KaPosts-shaped to watch: `kaposts_pubkey`
is set, or `kaposts_watch_authors` or `watched_broadcast_channels` is not empty.

**What happens today:** both refuse it ("watched_addresses, watched_group_ids and
watch_only_addresses must not all be empty"). KaPosts therefore only registers when it has a 1:1
conversation address to send. A KaPosts user with none never registers, and gets KaPosts
notifications only from the in-app poller while the app runs.

**Where:** the emptiness check in `register` (around line 155) and `update` (around line 479) in
`indexer/src/push.rs`, for example:

```rust
let kaposts_watch = normalized_kaposts_pubkey.is_some()
    || !normalized_kaposts_watch_authors.is_empty()
    || !broadcast_channels.is_empty();
if addresses.is_empty() && group_ids.is_empty() && normalized_watch_only.is_empty()
    && !kaposts_watch
{
    anyhow::bail!("nothing to watch");
}
```

(With the KaChat Wallet hand-off's `primary_address` case added to the same condition.)

Once this ships, KaPosts will send `watched_addresses: []` and register on its KaPosts key and
rooms alone. That app change waits for this one, since the server refuses it today.

## The app side

- KaPosts already drops any push that isn't `kaposts` or `broadcast:<room>` while it is in
  the foreground (`AppDelegate.userNotificationCenter(_:willPresent:)` in
  `KaPosts/App/KaPostsApp.swift`). In the background or closed it can't. It has no extension,
  so only the server can stop those banners.
- KaPosts registers with `apns_topic: "com.kachat.kaposts"`, `kaposts_pubkey`,
  `kaposts_notify`, `kaposts_watch_authors`, `watched_broadcast_channels`,
  `hidden_broadcast_senders` and `primary_address`.

## Testing

1. Use the same seed in KaChat and KaPosts, with KaPosts closed. Send that address a 1:1 message
   from another KaChat account. KaChat gets its push; KaPosts gets nothing. The log shows the
   KaPosts token filtered.
2. Like that account's post: KaPosts gets the `kaposts` push. Post in a room KaPosts has the bell
   on for: KaPosts gets the `broadcast` push.
3. Register a device with `apns_topic` `com.kachat.kaposts`, `kaposts_pubkey` set and empty
   `watched_addresses` / `watched_group_ids` / `watch_only_addresses`: 200, not 400.

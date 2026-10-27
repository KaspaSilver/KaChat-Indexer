# KaPosts pushes: only KaPosts and Public Chats pushes (2026-10-27)

**For:** the indexer session (`kasia-indexer`, the push service). **From:** the KaPosts iOS
session (`KaspaSilver/KaPosts-iPhone`).

KaPosts (`com.kachat.kaposts`, shown to people as "KaPost") is the feed and Public Chats app. It
has no 1:1 chats, no groups and no calls, and it has no notification service extension. It
needs two kinds of push only: **`kaposts`** (likes, comments, reposts, follows, new posts from
watched authors) and **`broadcast`** (Public Chats rooms). One server change is needed (section 1).

## 1. KaPosts devices get only `kaposts` and `broadcast` pushes (required)

**The rule:** a device registered with `apns_topic` = `com.kachat.kaposts` gets no push of any
other type: no `message` / DM, `handshake`, `payment`, `inbox` (no-handshake dm), `group_message`,
`group_control`, `address_activity`, `name_event` or VoIP ring.

**What happens today:** a KaPosts user got a banner titled with a `kaspa:...` address and the
body "New message" (seen 2026-10-27). That is `PushAlert::from_type`'s fallback body for a
chat message. KaPosts has no extension to decrypt or hide it, so iOS shows the raw alert. The
27f9c57 note says "KaPosts devices get whatever they register for", and a KaPosts device ends up
matched for chat pushes in two ways:
- `watched_addresses`: KaPosts sent the addresses of any 1:1 conversations its shared code
  knew about. It sends none from now on (see 2).
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

## 2. Registering without a chat address (done by 97692c5)

KaPosts used to send the addresses of any 1:1 conversations its shared code knew about. It did
this only because `register` / `update` refused an empty `watched_addresses`. 97692c5 accepts a
registration whose only watch is `primary_address`, and KaPosts always sends one. So KaPosts now
sends `watched_addresses: []` and registers on its main address, `kaposts_pubkey` and rooms. This
needs 97692c5 deployed; until then, its registration gets a 400.

The primary address still matches handshake and `inbox` pushes, so section 1 is still needed.

## The app side

- KaPosts already drops any push that isn't `kaposts` or `broadcast:<room>` while it is in
  the foreground (`AppDelegate.userNotificationCenter(_:willPresent:)` in
  `KaPosts/App/KaPostsApp.swift`). In the background or closed it can't. It has no extension,
  so only the server can stop those banners.
- KaPosts registers with `apns_topic: "com.kachat.kaposts"`, `watched_addresses: []`, `kaposts_pubkey`,
  `kaposts_notify`, `kaposts_watch_authors`, `watched_broadcast_channels`,
  `hidden_broadcast_senders` and `primary_address`.

## Testing

1. Use the same seed in KaChat and KaPosts, with KaPosts closed. Send that address a 1:1 message
   from another KaChat account. KaChat gets its push; KaPosts gets nothing. The log shows the
   KaPosts token filtered.
2. Like that account's post: KaPosts gets the `kaposts` push. Post in a room KaPosts has the bell
   on for: KaPosts gets the `broadcast` push.
3. Make sure 97692c5 is deployed. The KaPosts register log line shows `watched=0` and
   `primary=true`, and the response is 200.

# App push kinds: KaPay and KaPost on Android, KaChat chats only (2026-10-29)

**For:** the indexer session (`kasia-indexer`, the push service). **From:** the KaPay Android
session (`KaspaSilver/KaPay-Android`, formerly KaChat-Wallet-Android), also on behalf of KaPost
Android (`KaspaSilver/KaPosts-Android`) and KaChat Android (`KaspaSilver/KaChat-Android`).

Each app may get only its own kinds of push, on iPhone and Android alike:

| App | iPhone `apns_topic` | Android `apns_topic` | Pushes it may get |
|---|---|---|---|
| KaPay (the wallet) | `com.kachat.wallet` | `kapay.kachat.app` (new) | `address_activity`, `name_event` |
| KaPost | `com.kachat.kaposts` | `kapost.kachat.app` (new) | `kaposts`, `broadcast` |
| KaChat | none sent (APNs default topic `com.kachat.app`) | none sent, the same as iPhone | `contextual`, `handshake`, `payment`, `group_message`, `group_control` (new rule) |

The Android apps are published under their own package names, which differ from the iPhone
bundle ids. KaPay and KaPost Android send `apns_topic` = their package (debug builds send the
same, without `.debug`). KaPlay (chess, `kaplay.kachat.app`) has no push on either platform and
never registers.

Firebase is unchanged: `com.kachat.app(.debug)`, `kapay.kachat.app(.debug)` and
`kapost.kachat.app(.debug)` are all Android apps in the project the service already sends with
(`kachat-fc22a`), so their FCM tokens work with the existing service account. No new key or
credential.

§1 and §2 change the same function and must ship together. Until §1 ships, KaPay and KaPost
Android devices store `apns_topic = None`, and under §2 they would get KaChat's chat pushes.

> **Status (2026-10-29): done in the indexer alone.** `KNOWN_APP_TOPICS` (`push.rs`) always
> accepts the five app topics, whatever `APNS_ALLOWED_TOPICS` lists, so Kaspa Quick Start needs no
> change. `kapay.kachat.app` is filtered with `com.kachat.wallet` and `kapost.kachat.app` with
> `com.kachat.kaposts` (`WALLET_APNS_TOPICS` / `KAPOSTS_APNS_TOPICS` in `topic_takes_push`).
> Covered by `a_wallet_registers_watch_only_and_keeps_an_allowed_topic`.

## 1. Accept the two Android topics (required)

**What happens today:** `normalize_apns_topic` (`indexer/src/push.rs`) keeps a topic only if it
is in `APNS_ALLOWED_TOPICS` (set by Kaspa Quick Start to `com.kachat.app, com.kachat.kaposts,
com.kachat.wallet`). `kapay.kachat.app` and `kapost.kachat.app` are not there, so they are
dropped to `None`, and `topic_takes_push(None, ..)` lets every push kind through. A KaPay Android
phone would then be treated as a KaChat device: handshake, inbox and payment pushes matched on
its `primary_address` would reach an app that has no chats and shows them as bare banners. The
same goes for KaPost Android, which is the bug `KAPOSTS_PUSH_KINDS.md` fixed for iOS.

**The rule:** a device registered with `apns_topic` = `kapay.kachat.app` is filtered exactly like
`com.kachat.wallet` (only `address_activity` and `name_event`); `kapost.kachat.app` exactly like
`com.kachat.kaposts` (only `kaposts` and `broadcast`).

**Suggested fix** (any approach that meets the rule is fine):

- Let the two Android topics through `normalize_apns_topic`. Either:
  - add them to `APNS_ALLOWED_TOPICS` in Kaspa Quick Start's indexer env (that's the Kaspa Quick
    Start repo, so it needs its own change and a redeploy, and the server keeps dropping the
    topics until both have shipped), and to the doc comment on `allowed_apns_topics`; or
  - accept them in code whatever the env says. They are Android package names, never used as
    an APNs topic (FCM delivery doesn't read the topic), so the env, which gates real APNs
    bundle ids, doesn't need to list them. This needs no Quick Start change.
- In `topic_takes_push`, map both Android topics onto the existing kinds, e.g. turn
  `WALLET_APNS_TOPIC` / `KAPOSTS_APNS_TOPIC` into small lists
  (`["com.kachat.wallet", "kapay.kachat.app"]`, `["com.kachat.kaposts", "kapost.kachat.app"]`)
  and match on membership. The filter in `deliver` runs before the APNs/FCM split, so this
  covers Android with no other change.
- Update the tally in `deliver` the same way (`Some(KAPOSTS_APNS_TOPIC) => tally.kaposts_filtered`).
  Otherwise a filtered KaPost Android push is logged as `wallet=` instead of `kaposts=`.
- Tests: extend `a_wallet_registers_watch_only_and_keeps_an_allowed_topic` with an `android`
  registration for `kapay.kachat.app`. The topic is kept, a `handshake` push for its primary
  address skips it and `address_activity` reaches it. Do the same for `kapost.kachat.app` with
  `kaposts` against `message`, and add both Android topics to the `topic_takes_push` table test.
  If the allow-list stays env-based, put the Android topics in that test's existing
  `set_var("APNS_ALLOWED_TOPICS", ..)` rather than a new test with its own `set_var`, because
  `allowed_apns_topics` is read once (`OnceLock`) and a second test's list would race the
  first.

## 2. KaChat gets only chat pushes (required)

KaChat is now 1:1 and group chats only. On iPhone, KaChat-iPhone `e2f6732b` removed Public Chats,
KaPosts and Chess Online, and `d54ea87c` removed the wallet features and the .kachat market. On
Android the same removals are `d6ff6f17` and `21e9d74d`. Its pushes are now limited to:

- 1:1 chats: `contextual` (messages, including inbox messages and self-stash, which goes out as
  `contextual`) and `handshake` (a new conversation);
- group chats: `group_message` and `group_control`;
- KAS received on the chat address: `payment`, the payment sent inside a chat (the sealed
  payment; the app shows "<who> - Sent you 1.5 KAS");
- call rings, which go through `/v1/push/ring` and PushKit, outside this filter, and are
  unchanged.

**What happens today:** `topic_takes_push` has no KaChat entry: a device with `apns_topic`
`None` or `com.kachat.app` takes every kind. Both apps register the removed features empty
(`watched_broadcast_channels: []`, `kaposts_pubkey: null`, no KaPosts watch authors, no
`watch_only_addresses`), so `broadcast`, `kaposts` and `address_activity` mostly no longer match
a KaChat device. But:

- `name_event` matches on `primary_address` (`tokens_for_primary_address_str`), so every KaChat
  device whose address owns a .kachat name still gets each sold, offer, expiry and reminder
  push. The apps can't show them any more: iPhone files them silently, and Android drops them
  (`KaChatFirebaseMessagingService`, `"name_event"`). Each one still wakes the phone and uses
  up the device's rate limit.
- A KaChat phone that last registered on an older build still holds its public chat channels
  and KaPosts pubkey until it next re-registers. Until then it keeps getting `broadcast` and
  `kaposts` pushes the app drops.

**The rule:** a KaChat device (`apns_topic` `None` or `com.kachat.app`) takes only
`contextual`, `handshake`, `payment`, `group_message` and `group_control`. `name_event`,
`address_activity`, `kaposts` and `broadcast` never reach it.

**Suggested fix:**

- `const KACHAT_PUSH_TYPES: &[&str] = &["contextual", "handshake", "payment", "group_message",
  "group_control"];` and in `topic_takes_push`
  `None | Some("com.kachat.app") => KACHAT_PUSH_TYPES.contains(&event_type)`. After §1, `None`
  only ever means a KaChat device, because every other app sends a topic. Leave any other
  unknown topic at `true`; `normalize_apns_topic` already turns those into `None`.
- Check the `type` strings `deliver` sees against the FCM `type` field: the chat events write
  `payload.message_type`, which is `contextual` / `payment` / `handshake` / `group_message` /
  `group_control` (the `kind_label` `self_stash` is only a log label). The existing table test
  lists `message` and `inbox`. If either really occurs as a `type`, add it to the list.
  Otherwise fix the test.
- Tally: add `kachat_filtered` next to `wallet_filtered` / `kaposts_filtered`, so the log line
  shows KaChat devices being skipped.
- Tests: in `wallet_and_kaposts_devices_take_only_their_own_pushes`, assert that `None` and
  `Some("com.kachat.app")` take each chat kind and refuse `name_event`, `address_activity`,
  `kaposts` and `broadcast`. Add one registry-level test: a KaChat registration (no topic)
  whose `primary_address` owns a name gets no `name_event` push, while a KaPay registration on
  the same address does.

**Not part of this:** a plain KAS transfer from another wallet (no chat payment attached) is an
`address_activity` event. That reaches only devices with `watch_only_addresses`, which is KaPay.
KaChat registers none, on iPhone or Android, so it gets no push for those, the same as iPhone
today.

## How to test

1. Install a KaPay Android build made after 2026-10-29 (it has `google-services.json`; b5 or
   later) and open it with Address Activity on.
2. The register log line (`54bef70`) shows `platform=android apns_topic=kapay.kachat.app` and the
   watch-only count, not `apns_topic=None`.
3. Send KAS to the wallet's address from another wallet with KaPay closed: one "Received KAS"
   push arrives. A KaChat chat message to the same address produces no push on that phone.
4. KaPost Android: likes and replies push; a DM to its address does not.
5. KaChat (iPhone and Android): a 1:1 message, a new conversation, a group message and a
   payment sent in a chat each push. A .kachat name event for the account's own name doesn't
   push, and neither does a KaPosts like. The `deliver` log shows them as `kachat=` filtered.

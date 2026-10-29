# Android app topics: KaPay and KaPost on Android (2026-10-29)

**For:** the indexer session (`kasia-indexer`, the push service). **From:** the KaPay Android
session (`KaspaSilver/KaPay-Android`, formerly KaChat-Wallet-Android), also on behalf of KaPost
Android (`KaspaSilver/KaPosts-Android`).

The Android apps are published under their own package names, which differ from the iPhone
bundle ids, and each registers with `apns_topic` = its Android package (debug builds send the
same, without `.debug`):

| App | iPhone `apns_topic` (already handled) | Android `apns_topic` (new) | Pushes it may get |
|---|---|---|---|
| KaPay (the wallet) | `com.kachat.wallet` | `kapay.kachat.app` | `address_activity`, `name_event` |
| KaPost | `com.kachat.kaposts` | `kapost.kachat.app` | `kaposts`, `broadcast` |
| KaChat | `com.kachat.app` | (none sent, as today) | everything |

KaChat Android (`com.kachat.app`) needs no change. It already works and keeps getting every
kind of push. KaPlay (chess, `kaplay.kachat.app`) has no push on either platform and never
registers.

Firebase is unchanged: the Android apps `kapay.kachat.app(.debug)` and `kapost.kachat.app(.debug)`
are registered in the same project the service already sends with (`kachat-fc22a`), so their FCM
tokens work with the existing service account. No new key or credential.

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

## How to test

1. Install a KaPay Android build made after 2026-10-29 (it has `google-services.json`; b5 or
   later) and open it with Address Activity on.
2. The register log line (`54bef70`) shows `platform=android apns_topic=kapay.kachat.app` and the
   watch-only count, not `apns_topic=None`.
3. Send KAS to the wallet's address from another wallet with KaPay closed: one "Received KAS"
   push arrives. A KaChat chat message to the same address produces no push on that phone.
4. KaPost Android: likes and replies push; a DM to its address does not.

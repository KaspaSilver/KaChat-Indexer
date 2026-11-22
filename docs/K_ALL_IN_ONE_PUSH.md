# K gets every push: chats, groups, KaPost, rooms, Received KAS and .kachat (2026-11-22)

**For:** the indexer session (`kasia-indexer`, the push service). **From:** the K iOS session
(`KaspaSilver/KaChat-iPhone`, now `K-iPhone`).

## Why

The KaChat iPhone app is now **K**, the all-in-one app (owner, 2026-11-20): chats, groups,
Wallet, KaPost, Public Chats rooms, KaPlay, `.kachat` names, Portfolio and Cold Storage in one
app. KaPost, KaPay and KaPlay are being retired. Its bundle id, and so its APNs topic, is still
`com.kachat.app`, and it still sends no `apns_topic`.

`topic_takes_push` (`indexer/src/push.rs`) treats a registration with no topic, or with
`com.kachat.app`, as the old chats-only KaChat and gives it only `KACHAT_PUSH_TYPES`
(`contextual`, `handshake`, `payment`, `group_message`, `group_control`). So K never gets the
`kaposts`, `broadcast`, `address_activity` or `name_event` pushes it now registers for:

- K already sends `kaposts_pubkey`, the five `kaposts_notify_*` switches,
  `kaposts_watch_authors`, `kaposts_blocked_pubkeys`, `watched_broadcast_channels` and
  `hidden_broadcast_senders` on every register and update (since 2026-11-21).
- The owner found it on a TestFlight build: nobody's like or reply on their KaPost ever reached
  K, open or closed.

K can't switch topic (the topic has to be the bundle id). Old KaChat iPhone builds and KaChat
Android also register with no topic and must keep getting only what they get today, so the rule
can't key on the topic alone.

## 1. A registration field that marks K (required)

K will send one new top-level field on `POST /v1/push/register` and on the update call:

```json
"app": "k"
```

- Store it on the registration, like `apns_topic`. A register or update without it keeps the
  stored value (same rule as the topic). An unknown value is stored as none.
- It is not part of the auth preimage, so adding it can't break older clients' signatures, same
  as the KaPosts and room fields.
- In `topic_takes_push` (or wherever `deliver()` filters tokens), a registration with `app == "k"`
  takes **every** kind: `contextual`, `handshake`, `payment`, `group_message`,
  `group_control`, `kaposts`, `broadcast`, `address_activity` and `name_event`.
- A registration without it is unchanged: none/`com.kachat.app` = the KaChat list, the KaPost
  and KaPay topics = their lists.

## 2. Received KAS for K (required)

K's Wallet tab is KaPay's wallet, so K wants the `address_activity` pushes KaPay gets. K will
send its addresses in `watch_only_addresses`, as KaPay does (`KACHAT_WALLET_PUSH.md`):

- its spending addresses;
- its visible Cold Storage addresses on accounts with Receive Notifications on;
- an empty list when Settings > Notifications > Address Activity is off, still with
  `primary_address` set (the `name_event` pushes keep coming; §2 of `KACHAT_WALLET_PUSH.md`
  already allows that).

The chatting address is in `watched_addresses`, as today. Please make sure:

- **One push per transaction per device.** A KAS payment sent inside a chat (`kchat:1:pay`, a
  `payment` push) must not also produce an `address_activity` push for the same tx on the same
  device, and neither may a chat message's change output. If a tx would give K both a chat-type
  push and an `address_activity` push, send only the chat-type one.
- **Self-sends never ping.** The rule from `KACHAT_WALLET_PUSH.md` §1 (no "Received" push when any
  input spends an output that paid one of that device's watched addresses) applies to K too.
  With K, every send from the chatting address, a spending address or a KaPost post is a
  self-send from the same seed, so it matters more here: check that it also covers K's
  `watched_addresses` (the chatting address), not only `watch_only_addresses`.

## 3. `.kachat` name events for K (required)

`name_event` pushes go by `primary_address`, which K already sends. Once §1 is in, K gets them.
K's notification extension will show them (it has delivered them silently since 2026-10-24,
when the market left KaChat; it shows them again from the build that sends `"app": "k"`).

## 4. Same phone, several apps

One wallet can be signed in to K and to KaPost or KaPay on the same phone until those are
retired. Each app has its own token and registration, so each gets its own push. That's
expected; nothing to dedupe across apps.

## What K does on its side

- Sends `"app": "k"` and `watch_only_addresses` on register and update.
- Its notification extension shows `name_event` and `address_activity` pushes.
- While K is open it also banners new messages, room messages and KaPost activity itself (from
  2026-11-22). It shows one banner per message: a foreground push whose `tx_id` (or, for a room,
  `apns-collapse-id`) it already bannered is only listed. So keep `tx_id` in every chat, group
  and `address_activity` payload and the txid as the room push's collapse id, as today. For
  `kaposts` pushes keep `postId`, `actor_pubkey` and `kaposts_kind`.

> **K's side is written (2026-11-22), not yet in a TestFlight build:** `"app": "k"` and
> `watch_only_addresses` on register and update (`PushNotificationManager`, part of the change
> check, so existing installs update once); the extension shows `name_event` while signed in; a
> tapped name push opens K's .kachat section, a tapped `address_activity` push the address's
> history. Today's server ignores the unknown `app` field and still filters K to chat kinds, so
> sending it early is safe. K commit: `428a797b` (KaspaSilver/K-iPhone, 2026-11-22).

## Testing

1. K signed in (TestFlight, production APNs), app closed. From another seed:
   - like and reply to one of K's KaPost posts: two pushes;
   - post in a room K has its bell on for: one push;
   - send KAS to one of K's spending addresses: one "Funds received" push;
   - make an offer on a `.kachat` name K owns: one name push.
2. Same, with K open: one banner each, never two.
3. Send a KAS payment in a 1:1 chat to K: one `payment` push, no `address_activity` push.
4. Post on KaPost from K, send a chat message, send KAS from a spending address: no "Received"
   push for any of them.
5. An old KaChat build or KaChat Android with the same wallet (no `"app"` field): still only chat
   and group pushes, nothing new.
6. Settings > Notifications > Address Activity off in K: no `address_activity` pushes; name
   events still arrive.

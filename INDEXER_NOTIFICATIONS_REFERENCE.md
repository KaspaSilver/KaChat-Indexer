# Indexer Push Notifications — Client Integration Reference

Audience: an AI (or human) building **client-side** push handling for KaChat (iOS NSE + Android
`KaChatFirebaseMessagingService`). This describes exactly what the indexer sends today, so the
client can parse every payload without reading the server source.

Server crate: `kasia-indexer` (Rust). Push code: `indexer/src/push.rs`, `indexer/src/fcm.rs`,
`indexer/src/api/v1/push.rs`, `indexer-actors/src/{push,block_processor,lib}.rs`.
K-side (broadcast/KaPosts) injectors: `kachat-transaction-processor/src/push_notify.rs`.

Related design/spec doc: [`NOTIFICATION_EXTENSIONS_TODO.md`](NOTIFICATION_EXTENSIONS_TODO.md).

---

## 1. Transport model (read this first)

Every device registers **once** with a `platform` (`ios` | `macos` | `android`). The dispatcher
routes each notification to the right transport by that platform:

| platform          | transport | payload style |
|-------------------|-----------|---------------|
| `ios` / `macos`   | **APNs**  | rich JSON (`aps` + top-level fields). Encrypted body in `payload`. |
| `android`         | **FCM** (HTTP v1) | **`data` map of string→string** (see per-type keys below), optionally plus a `notification` block. |
| unknown / legacy  | APNs      | (default fallback) |

**FCM `notification` block — the critical distinction:**

- **Data-only** (no `notification` block): used for **encrypted chat content** (DM / group /
  handshake / payment / group control). The app's FCM handler MUST run, decrypt `enc_payload`
  locally, and build the notification itself (mirrors iOS's NSE). Trade-off: a fully force-closed
  app only wakes for data-only messages if the OS lets it (battery "Unrestricted"); otherwise the
  push is silently dropped by Android.
- **Notification + data** (`notification` block present): used for **public / display-ready
  content** (broadcast, KaPosts, address_activity). The OS shows `notification.title`/`.body`
  even when the app is dead; the `data` map is still delivered for richer in-app handling.

All FCM `data` values are **strings** (FCM requirement) — numbers are stringified; parse client-side.

APNs payloads are already handled by the shipped iOS NSE; the new `address_activity` type (§6) is
the only one iOS doesn't branch on yet.

---

## 2. Notification catalog (what fires, and its `type`)

| `type` (FCM `data.type` / APNs `message_type`) | Trigger | FCM notification block? | Source |
|---|---|---|---|
| `contextual` | 1:1 DM (also self-stash notes) | no (data-only) | chat block stream |
| `handshake`  | someone started a conversation with you | no | chat block stream |
| `payment`    | Kasia payment to you | no | chat block stream |
| `group_message` | new message in a group you're in | no | chat block stream |
| `group_control` | group membership/update addressed to you ("added to a group") | no | chat block stream |
| `broadcast`  | a post in a broadcast channel you follow | **yes** | K-processor → internal HTTP |
| `kaposts`    | someone liked/reposted/commented/etc. on your KaPost | **yes** | K-processor → internal HTTP |
| `address_activity` | **funds received on an address you own** | **yes** | block processor (new) |

---

## 3. Encrypted chat types — `contextual` / `handshake` / `payment` / `group_message` / `group_control`

Data-only FCM. The handler decrypts `enc_payload` (when present) and renders the real sender/text;
`title`/`body` are only fallbacks for when decryption isn't possible.

**FCM `data` keys:**

| key | always? | meaning |
|---|---|---|
| `type` | yes | one of the five above (note: server-internal self-stash maps to `contextual`) |
| `sender` | yes | sender kaspa address (bech32) |
| `tx_id` | yes | hex transaction id (use as dedupe/collapse key) |
| `timestamp` | yes | block/accept time (ms, stringified) |
| `daa_score` | yes | stringified integer |
| `title` | yes | fallback title = sender address |
| `body` | yes | fallback body: `New message` / `Started a conversation` / `Payment received` / `New group message` / `Group update` |
| `amount` | payment only | sompi (stringified integer) |
| `blinded_group_id` | group_message only | hex; the per-(group,member) blinded id the message was addressed to |
| `enc_payload` | when it fits | base64 sealed message body to decrypt locally. **Omitted when the sealed body > 3500 bytes** (`MAX_PUSH_PAYLOAD_BYTES`) — e.g. on-chain media — so the app must fall back to generic text in that case. |

**Decryption notes (from prior client work):**
- DM / contextual decrypt is **stateless**: `KasiaCipher` (ECDH secp256k1 → HKDF-SHA256 →
  ChaCha20-Poly1305) with the ephemeral key embedded in the sealed payload + the wallet key.
  Android reference: `Base64.decode(enc_payload)` → `KasiaCipher.EncryptedMessage.fromBytes` →
  `MessageProtocol.decrypt`.
- **Group** decrypt is **stateful** (needs the group seed the app already holds); the server cannot
  supply it, so `group_message` often renders as generic text unless the app has the seed.
- Reactions are suppressed server-side (no push), so the handler needn't special-case them here.

**APNs shape (already handled by iOS NSE)** — top-level `tx_id`, `sender`, `message_type`,
`amount?`, `payload?` (the base64 body), `timestamp`, `daa_score`, `blinded_group_id?`, plus
`aps: { alert:{title,body}, mutable-content:1, content-available:1 }`.

---

## 4. `broadcast`

Display-ready; FCM carries a `notification` block. **FCM `data` keys:**
`type=broadcast`, `channel`, `title` (`#<channel>`), `subtitle`, `body`,
`thread_id` (`broadcast:<channel>`), `tx_id`.

---

## 5. `kaposts`

Display-ready; FCM `notification` block present. **FCM `data` keys:**
`type=kaposts`, `title` (`KaPosts`), `subtitle`, `body`, `thread_id` (`kaposts`), `tx_id`,
`post_id` (optional — the target content tx id; absent for follows).

Per-action filtering is done **server-side** using the device's `kaposts_notify` toggles (§7), so
the client does not need to filter these by action kind. Action→toggle mapping (for reference):
reply→comment, quote→repost, upvote→like, downvote→dislike, follow→follow.

---

## 6. `address_activity` — NEW, client handler not yet written

"Funds received on an address you own." Fires when an accepted tx credits any address the device
registered in `watch_only_addresses` (§7). Display-ready; FCM `notification` block present, so it
shows even with no client handler — but you'll want a real handler for grouping/formatting.

**One push per credited address** (a tx crediting two of your addresses = two pushes).

**FCM `data` keys:**

| key | meaning |
|---|---|
| `type` | `address_activity` |
| `address` | the credited address (bech32) |
| `amount_sompi` | amount credited to that address, in sompi (stringified integer) |
| `amount_kas` | same amount pre-formatted as KAS (e.g. `1.25`, trailing zeros trimmed) |
| `title` | `Funds received` |
| `subtitle` | the credited address |
| `body` | `Received <amount_kas> KAS` |
| `thread_id` | `funds:<address>` |
| `tx_id` | hex tx id |
| `ts` | timestamp (stringified integer) |

**APNs shape:** `aps.alert = { title, subtitle, body }`, `sound: default`,
`thread-id: funds:<address>` (uses the same `ExtensionPayload` struct as broadcast/KaPosts; no
`post_id`). **iOS NSE note:** the NSE currently blanks payloads whose sender equals the wallet
address — `address_activity` has **no `sender` field**, so it must be routed around that path.

**Server-side behavior the client can rely on (so the client rule can match):**
- **Self-send is already filtered server-side**: no push is sent to a device when the tx sender is
  one of that same device's `watch_only_addresses` (suppresses change from your own payments). This
  is best-effort (first-input sender resolution; may miss some multi-input txs), so the client may
  still apply its own local self-send suppression as defense-in-depth.
- **Rate limited** to 30 pushes/hour per device (drop-oldest window).
- **Deviation to be aware of:** fires on **block sighting**, not strict acceptance. A tx later
  reorged out could produce a rare false "received". Deduped server-side per `(tx_id, address)`.

---

## 7. Registration API (what the client sends)

Endpoints (base host: `https://kachat.duckdns.org`, proxied to the chat indexer):
- `POST /v1/push/register` — full upsert of the device's registration.
- `POST /v1/push/update` — same fields; updates an existing registration.
- `POST /v1/push/unregister` — remove a device.
- `GET  /v1/push/challenge` — nonce for the auth signature.

**Auth:** BIP-340 Schnorr signature over a canonical `\n`-joined preimage (formats: LegacyV1 /
TransitionalGroups / V2 — the server auto-selects by which fields are present). Newer optional
fields are deliberately **kept out of the LegacyV1 preimage** so adding them never breaks old
clients' signatures. `kaposts_notify` and `watch_only_addresses` are both outside the preimage.

**Request body fields** (JSON):

| field | type | purpose |
|---|---|---|
| `device_token` | string | APNs hex token OR FCM registration token (server is format-aware) |
| `platform` | string | `ios` \| `macos` \| `android` |
| `watched_addresses` | [string] | **counterparties** whose activity toward you should push (contacts / your receive addr for handshakes). NOT your own-wallet watch list. |
| `watched_group_ids` | [string] | hex blinded group ids you belong to |
| `capabilities` | [string] | feature capability strings |
| `primary_address` | string? | your authenticated primary address (self-send filter, group-control addressing) |
| `aliases` | [string] | incoming-conversation aliases to narrow DM matching to your own conversations (prevents over-notification) |
| `watched_broadcast_channels` | [string] | broadcast channels you follow |
| `hidden_broadcast_senders` | map<channel,[address]> | per-channel muted senders |
| `kaposts_pubkey` | string? | your KaPosts identity pubkey (to receive KaPosts action pushes) |
| `kaposts_notify` | object? | per-action toggles: `{ likes, dislikes, reposts, comments, follows }` (all bool, default all `true` / absent = all on) |
| `watch_only_addresses` | [string] | **NEW** — your own receive addresses to be notified about incoming KAS (Address Activity). Absent/empty = feature off for this device. Canonical bech32; server dedupes, caps at 200, drops invalid entries. |
| `auth` | object? | Schnorr challenge/signature block |

**Important semantic distinction:** `watched_addresses` = *other people's* addresses whose
activity toward you matters. `watch_only_addresses` = *your own* addresses. Do not conflate them —
they drive different pushes (chat vs. address_activity) and are matched by different code paths.

---

## 8. Dedupe & grouping cheat-sheet

- **Dedupe key:** `tx_id` is globally unique. Chat pushes carry no APNs collapse id (server dedups
  in a 1-hour window). `address_activity` dedups per `(tx_id, address)`.
- **Grouping / thread:** `thread_id` is provided for broadcast (`broadcast:<channel>`), kaposts
  (`kaposts`), and address_activity (`funds:<address>`). Chat types use `blinded_group_id` /
  conversation context client-side.
- **Amounts:** always in **sompi** on the wire (`amount`, `amount_sompi`). 1 KAS = 100,000,000
  sompi. `address_activity` also ships a human `amount_kas`.

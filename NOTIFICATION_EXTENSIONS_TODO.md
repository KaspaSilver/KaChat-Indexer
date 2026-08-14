# Notification Extensions — Server Work Handoff

Client-side work in KaChat 4.0 (iOS first, Android/desktop ports following) added two
notification features the server cannot currently support. This file specifies exactly what the
indexer needs so the apps' features work with the app closed. Written against the current source
of this repo (verified findings reference real code sites below).

---

## 1. Address Activity push — "funds received on an address I own"

### What the client shipped
iOS notifies "Received X KAS" when ANY of the user's own addresses (spending addresses, cold
storage watch-only addresses) receives Kaspa. No chat is created. It works via the app's own
UTXO subscription **only while the app is running**; with the app closed, nothing fires. Gated
by Settings > Notifications > Wallet > "Address Activity" (default on).

### Why the current server cannot do this (verified)
- `K-webserver`/indexer ingestion drops any transaction whose payload does not parse as a
  `ciph_msg:`-prefixed sealed operation (`block_processor.rs:378`). A bare KAS transfer exits
  before any address matching — there is **no UTXO/balance watcher anywhere in the indexer**.
- `PushEventKind` (`indexer-actors/src/push.rs`) is exclusively Kasia-protocol events.
- Even Kasia payment events are filtered by the receiver-must-match-`primary_address` check
  (`indexer/src/push.rs` ~1865-1874). `watched_addresses` semantically means *counterparties*
  (contacts whose activity toward the user should push), NOT addresses the user owns. Do not
  overload it.

### Required server changes
1. **Registration field**: add optional `watch_only_addresses: [String]` to
   `/v1/push/register` and `/v1/push/update`. Backward compatible: absent = empty. Keep it OUT
   of the LegacyV1 auth preimage (same pattern used when `watched_group_ids` was added — newer
   optional fields must not break old clients' signatures). Validate: bech32, correct network
   prefix, cap the list (suggest 200 per device), dedupe.
2. **Ingestion source**: watch accepted transactions whose outputs credit any registered
   `watch_only_addresses`. Two viable designs:
   - subscribe to kaspad `UtxosChanged` for the union of all registered watch-only addresses
     (resubscribe on registration changes), or
   - during block processing, before the `ciph_msg` payload gate, match tx outputs against an
     in-memory index of registered watch-only addresses (payload-less txs must reach this
     check — that is the whole point).
   Fire on **acceptance**, not first block sighting (mirrors how message pushes behave).
3. **Self-send filtering (server side)**: do NOT push when any tx *input* address belongs to
   the same device's owned set (`watch_only_addresses` ∪ `primary_address`). This suppresses
   change from the user's own payments, consolidations, and compounds. The clients implement
   the same rule locally; the server must too or closed-app users get change spam.
4. **New event kind + payload**: e.g. `PushEventKind::FundsReceived`. Data payload keys
   (data-only on FCM, alert+payload on APNs, matching existing conventions):
   - `type`: `"address_activity"`
   - `tx_id`: accepting tx id
   - `address`: the credited address (bech32) — note the existing `PushPayload` has `amount`
     but no field for *which address* was credited; add it.
   - `amount_sompi`: integer, SUMMED across all of this device's credited outputs in the tx
     (one push per tx per device, never one per output)
   - `ts`: acceptance timestamp ms
   - APNs `thread-id` / FCM collapse grouping: `"address-activity"`
   - Include a server-rendered `title`/`body` ("Received 1.25 KAS") so clients without a
     dedicated handler still show something sensible.
5. **Rate limiting** (recommended): cap address-activity pushes per device (e.g. 30/hour,
   drop-oldest) — an address posted publicly could otherwise be used to ping-spam a device.

### Server status — IMPLEMENTED (2026-08)
Shipped in the indexer. Notes / deviations from the spec above:
- **Registration field** `watch_only_addresses` added to `/v1/push/register` + `/v1/push/update`
  (DTOs + `DeviceRegistration` value), kept out of the auth preimage. Normalized to canonical
  bech32, deduped, sorted (stable fast-path compare), capped at 200
  (`MAX_WATCH_ONLY_ADDRESSES`); invalid entries are skipped, not rejected. Stored on the value
  only — matching is done by scanning registrations (no reverse partition / DB migration).
- **Ingestion**: chose the in-process block-processing match (design 2), not `UtxosChanged`.
  `block_processor.rs::emit_funds_received` runs *before* the `ciph_msg` gate, matches outputs
  against the `WATCH_ONLY_ADDRESSES` global set (union of all devices' watch-only addresses,
  rebuilt by the push registry on every registration change and at startup), and emits a
  `FundsPushEvent` over a dedicated channel. Fully inert when the global set is empty.
- **DEVIATION — fires on block sighting, not acceptance.** The emit sits on the block-processor
  path (same as the message pushes), so a tx later reorged out of the selected chain could yield
  a rare false "received". The dispatcher dedups per `(tx, address)`. Revisit if false positives
  show up in practice.
- **Self-send filtering**: done per-device in `PushRegistry::watch_only_tokens` — a device is
  dropped when the resolved tx sender is also in that device's watch-only set. Sender is a
  best-effort first-input resolution via the acceptance partition (catches the common
  change-from-own-payment case; may miss multi-input txs).
- **Payload**: one push per credited address (NOT summed per-tx as the spec suggested — simpler,
  and reads clearly as "received X on addr"). `type: "address_activity"`, `address`,
  `amount_sompi`, `amount_kas`, server-rendered `title`/`body` ("Received 1.25 KAS"),
  `thread_id: "funds:<address>"`, `tx_id`, `ts`. FCM carries a notification block (non-sensitive,
  so it shows when the app is dead), like broadcast/KaPosts. No APNs collapse id (sent_cache
  dedups instead).
- **Rate limiting**: per-device sliding window, 30/hour (`WATCH_ONLY_RATE_*`) in the dispatcher.

### Client status (for coordination, no server action)
The iOS NSE (`KaChatNotificationService`) and Android `KaChatFirebaseMessagingService` do NOT
yet have a branch for `type: "address_activity"` — that client work is queued to land when this
server capability exists. Until then, ship the server-rendered title/body so early payloads
still display. iOS NSE today blanks payloads whose sender equals the wallet address; the new
type must bypass that path (it has no `sender`).

---

## 2. KaPosts pushes — per-action-type filtering

### What the client shipped
Settings > Notifications > KaPosts: five independent toggles, each default ON — Likes,
Reposts, Follows, Dislikes, Comments. The local polling path filters by action kind. Remote
KaPosts pushes cannot be filtered because the payload carries no machine-readable action kind
(server-rendered alert + `thread-id: "kaposts"` + optional `postId` only).

### Action-kind mapping (must match the clients)
- `contentType == "vote"` and `voteType == "downvote"` → **dislike**
- `contentType == "vote"` otherwise → **like**
- `contentType == "reply"` → **comment**
- `contentType == "quote"` → **repost** (quotes are K's repost mechanism, including
  quotes-with-text)
- `contentType == "follow"` → **follow**
- Unknown kinds: treat as always-notify.

### Required server change — pick ONE (option A recommended)
**Option A — server-side preference filtering (recommended)**: add an optional registration
field `kaposts_notify` (object of five booleans: `likes`, `reposts`, `follows`, `dislikes`,
`comments`; absent = all true; keep out of the LegacyV1 preimage like other new optional
fields). At send time, map the event to an action kind per the table above and skip disabled
kinds. No client changes needed beyond sending the field on registration; saves device wakeups
entirely.

**Option B — payload action key + client filtering**: add `action` (one of `like`, `dislike`,
`repost`, `follow`, `comment`) to the KaPosts push payload and set APNs `mutable-content: 1`
so the NSE can drop disabled kinds. More client work, wakes the device only to discard —
inferior; implement only if registration changes are undesirable.

---

## 3. Cross-checks (already in the contract docs, verify implemented)
- **Reaction envelopes must not generate pushes** — broadcast and 1:1 reaction messages
  (`{"type":"reaction",...}` content) are invisible protocol traffic. The KaChat repo's
  `PUSH_EXTENSIONS.md` / `BROADCAST_INDEXER.md` were updated during the 4.0 broadcast work to
  require suppression server-side where content is inspectable (broadcasts are plaintext).
  Verify the deployed dispatcher enforces this for broadcast events.
- Encrypted 1:1 pool envelopes (`addr_pool` etc.) are indistinguishable ciphertext to the
  server — no server change possible or needed; the clients' NSE suppresses their display.

---

*Generated from the KaChat 4.0 client work; findings verified against this repo's source at
the time of writing. Coordinate payload key names with the client team before shipping — the
values above are what the iOS client expects to consume.*

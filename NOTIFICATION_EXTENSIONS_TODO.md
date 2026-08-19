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
- `kachat-webserver`/indexer ingestion drops any transaction whose payload does not parse as a
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

## 4. Per-device APNs environment routing — **URGENT, currently breaks all TestFlight push**

### Symptom
iOS TestFlight builds receive **zero** push notifications. Because remote push is the only
background delivery path, no messages arrive at all until the user opens the app. Development
builds installed straight from Xcode work fine on the same phone, same wallet, same indexer.

### Root cause (verified in this repo)
APNs device tokens are **environment-scoped**. A build signed with `aps-environment =
development` (Xcode install) gets a token that is only valid at `api.sandbox.push.apple.com`.
TestFlight and App Store builds are signed `aps-environment = production` and get a token only
valid at `api.push.apple.com`. Posting a token to the wrong host fails **silently per device** —
Apple returns HTTP 400 `BadDeviceToken`, the send is simply dropped.

This indexer picks the endpoint **once, globally, at startup**:

- `indexer/src/config.rs:19-20` — `apns_environment: ApnsEnvironment`, `#[serde(default = "default_apns_environment")]`
- `indexer/src/config.rs:53-55` — `default_apns_environment()` returns `ApnsEnvironment::Sandbox`
- `indexer/src/push.rs:2287-2290` — `ApnsClient::new` resolves `config.apns_environment` into a
  single `endpoint` string stored on the client
- `indexer/src/push.rs:2341` — every send does `format!("{}/3/device/{}", self.endpoint, token)`

So with the default (or an explicit `sandbox`) config, **every production token is posted to the
sandbox host and dropped**. One global setting cannot serve a fleet that contains both dev and
TestFlight/App Store installs, which is always the case once a developer is also a tester.

### IMMEDIATE UNBLOCK (do this first, no code change)
Set `apns_environment = production` in the deployed config. TestFlight and App Store builds are
production; that single flip restores push for every real user. The cost is that the operator's
own Xcode dev builds stop receiving push until the per-device work below lands. **Do this before
anything else** — it is a one-line config change and it fixes the reported outage.

### The real fix — route per device

**Client side: already shipped.** `KaChat/Services/PushNotificationManager.swift` now sends its
own environment on both registration and update. Exact contract:

| Field | Type | Values | Notes |
|---|---|---|---|
| `apns_environment` | `String` | `"development"` \| `"production"` | Sent on `POST /v1/push/register` **and** `PUT /v1/push/update`. Absent = old client. |

The client determines this at runtime by reading `aps-environment` out of the embedded
provisioning profile (`embedded.mobileprovision`), falling back to build config (`DEBUG` →
`development`, else `production`) when no profile is present. It is also part of the client's
registration fingerprint and is persisted, so a device that crosses environments (TestFlight
build installed over an Xcode build) forces a **full re-registration** instead of reusing the
stale record — the server will therefore always see a fresh `register` with the correct value
after an environment change, not just an `update`.

**Server changes required:**

1. **Accept the field.** Add to both request DTOs in `indexer/src/api/v1/push.rs` (the
   `PushRegisterRequest` around line 90-115 and `PushUpdateRequest` around line 118-145), using
   exactly the pattern already used for `watched_broadcast_channels` / `kaposts_pubkey`:
   ```rust
   #[serde(default)]
   #[serde(rename = "apns_environment")]
   pub apns_environment: Option<String>,
   ```
   **Keep it OUT of the LegacyV1 auth preimage** — same rule as every other post-hoc optional
   field. Old clients that never send it must keep validating.

2. **Normalize + validate.** Accept only `"development"` and `"production"` (lowercase, trimmed).
   Anything else → treat as `None` (fall back, see 4) rather than rejecting the registration; a
   bad value must never cost a device its push. Only meaningful for `platform = ios | macos`;
   ignore it for `android`/FCM tokens.

3. **Store it per device.** Add `#[serde(default)] pub apns_environment: Option<String>` to the
   stored registration struct in `indexer/src/push.rs` (~line 1540-1577, alongside
   `watched_broadcast_channels`) and thread it through `PushRegistry::register` (~line 92-245)
   and `PushRegistry::update` (~line 370-520), including the "nothing changed" comparison at
   ~line 200-210 / ~line 478-486 so a pure environment change still rewrites the record.

4. **Route on the DEVICE's value at send time.** `ApnsClient` currently bakes one `endpoint` in
   at construction (`push.rs:2287-2290`). Change to either:
   - build **two** `ApnsClient`s (sandbox + production) sharing the same team id / key id / .p8 /
     topic — the JWT and topic are identical across environments, only the host differs — and
     pick per token in the dispatcher's APNs branch (`push.rs:1815-1825`, where `apns_tokens` is
     iterated), or
   - keep one client and pass the endpoint (or an `ApnsEnvironment`) into `send_collapsible`
     (`push.rs:2331-2341`).

   The dispatcher currently splits tokens by platform only; extend that split so APNs tokens are
   grouped by stored environment and each group goes to its host. Both groups can still be sent
   concurrently under `APNS_SEND_CONCURRENCY`.

5. **Fallback for pre-existing devices.** Devices registered before this field existed have
   `apns_environment = None`. Those MUST fall back to the global `config.apns_environment` —
   which is why the config key stays. Do not drop or reject them.

6. **Recommended: self-heal on `BadDeviceToken`.** APNs' 400 `BadDeviceToken` for an otherwise
   well-formed token is the exact signature of an environment mismatch. On that specific reason,
   retry the same token once against the *other* host; if it succeeds, persist the corrected
   environment on the registration. This makes stale records repair themselves without waiting
   for the device to re-register.

7. **Logging.** The existing `[Push] deliver split: apns=… fcm=…` line (`push.rs:1805-1811`)
   should also report the sandbox/production split. Silent `BadDeviceToken` drops are what made
   this outage invisible for so long — log the reason string per failed token at `warn`.

### Acceptance check
With a TestFlight build and an Xcode build of the same app installed on two devices, both
registered against the same indexer, a single incoming message must notify **both**. Server logs
should show one send to `api.push.apple.com` and one to `api.sandbox.push.apple.com`.

---

*Generated from the KaChat 4.0 client work; findings verified against this repo's source at
the time of writing. Coordinate payload key names with the client team before shipping — the
values above are what the iOS client expects to consume.*

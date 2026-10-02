# `.kachat` names - what the iOS app relies on (2026-10-02)

The read API in `KACHAT_NAMES_INDEXER.md` Part D, as the shipped iOS app actually calls and
decodes it (`KaChat/Services/KachatNames/KachatNamesRegistry.swift` and
`KachatNamesRegistryState.swift`, `enum IndexerAPI`). Match this exactly and the app switches
from its own chain walker to the indexer with no app change.

It also covers three things that block "testnet fully working":
1. Publishing the testnet indexer (section 3).
2. Where the test vectors are (section 4).
3. The order of work (section 5).

---

## 1. When the app uses the indexer

- **Base URL.** The app's indexer base is the **chat indexer URL** setting (`AppSettings.indexerURL`).
  - On mainnet that is the one public hostname whose root goes to the content API (3080) and
    whose chat paths go to 8600 (Kaspa-Quick-Start `manager/lib/apps.js`, `publish.routes`).
  - `/names/*`, `/identity/*`, `/profiles/*`, `/market/*` and `/offers/*` therefore must be
    served by **kachat-webserver (3080)** under that same hostname. The default root route
    already does that.
- **The switch.** On every refresh the app calls `GET {base}/names/status`. It uses the indexer
  when the answer decodes **and** `registryCovenantId` equals the bundled manifest's id
  (registry v2 `82f4315c…0f89` on testnet-10; v1 `9444187f…7a51` is retired). Anything else keeps it on its own chain walker. So:
  - **Withhold `registryCovenantId` until synced.** Already done in `95645dd`. While it is
    withheld the app keeps walking the chain itself, which is correct.
  - **Never serve a stale or refuted row once you report it.** The app trusts the indexer for
    lookups, gaps and listings from that moment on.
- **Every transaction is still checked against a node.** Before any action the app re-reads the
  UTXOs it spends (covenant id included) from a node. A wrong indexer answer fails safely: the
  transaction isn't built. It does, however, block the user, so self-test before reporting.

## 2. Exact shapes the app decodes

General rules:
- Every field marked optional (`?`) may be omitted.
- Amounts are **strings of sompi**. Times are unix **ms** numbers.
- Hex is lowercase, without `0x`.
- Names have **no `.kachat` suffix**.
- `outpoint` is `{"txId": "<64 hex>", "index": <uint32>}`.

### `GET /names/status`
```json
{ "network": "testnet-10", "registryCovenantId": "82f4…0f89", "genesisTxId": "e203…a426",
  "indexedDaa": 585800000, "synced": true }
```
Every field is optional to the decoder, but `registryCovenantId` is the switch.

### Name object

Used by `/names/{name}` and as each element of `names` and `listings`:
```json
{ "name": "alice", "key": "<64 hex>", "registered": true, "status": "active",
  "owner": "kaspatest:q…", "ownerKey": "<64 hex x-only>", "price": "0",
  "periodStart": 1790000000000, "expiresAt": 1822000000000,
  "outpoint": {"txId": "…", "index": 2},
  "registeredAt": 1790000000000, "registeredTxId": "…", "updatedAt": 1790000000000 }
```
- The app needs `expiresAt`, `outpoint`, `periodStart` (registry v2: without it the app can't
  spend the name and shows "the names indexer didn't send this name's paid period"), and
  `ownerKey` **or** a decodable `owner` address.
- **Free name:** `{"name": "alice", "key": "…", "registered": false, "gap": <gap object>}`. The
  app registers by spending exactly that gap.
- **Invalid name:** `400 {"error": "invalid_name"}`.

### Gap object (`GET /names/gap/{keyHex}`)
```json
{ "lo": "<64 hex>", "hi": "<64 hex>", "outpoint": {"txId": "…", "index": 0} }
```
- `keyHex` is any 32-byte key. Answer the **one live gap with `lo < key < hi`**.
- **Release and reclaim depend on this.** The app finds the two gaps around a name by asking for
  `name.key - 1` and `name.key + 1`, then requires `below.hi == name.key` and
  `above.lo == name.key`. Make sure a key that sits exactly one step from a seam returns the
  neighbour gap, not a 404.

### Lists
| Endpoint | Shape |
|---|---|
| `GET /names/by-owner/{address}?includeInactive=false\|true` | `{"names": [<name object>]}` |
| `GET /market/listings?sort=recent` | `{"listings": [<name object>], "next": "<cursor>"\|null}`. Listed (`price` > 0) and `active` names only |
| `GET /names/expiring` | `{"names": [<name object>]}`. `lapsed` names (past `expiresAt + graceMs`) still unspent, oldest first |

### Offers
| Endpoint | Shape |
|---|---|
| `GET /names/{name}/offers` | `{"offers": [<offer>]}` |
| `GET /offers/by-buyer/{address}` | `{"offers": [<offer>]}` |

Offer object:
```json
{ "outpoint": {"txId": "…", "index": 0}, "buyer": "kaspatest:q…", "amount": "900000000",
  "refundAfter": 585900000, "createdAt": 1790000000000, "refundable": false, "name": "alice" }
```
- `buyer` is an **address**: the app derives the x-only key from it.
- `refundAfter` is a **DAA score** (number).
- `name` is required in `/offers/by-buyer`; it's optional when the path already names it.

### History and activity
| Endpoint | Shape |
|---|---|
| `GET /names/{name}/history` | `{"events": [<event>], "next": null}` |
| `GET /market/activity` | `{"events": [<event>], "next": null}` |

Event object:
```json
{ "txId": "…", "op": "register", "name": "alice", "at": 1790000000000,
  "from": "kaspatest:q…", "to": "kaspatest:q…", "price": "3500000000", "years": 1 }
```
`op` is one of `register`, `transfer`, `list`, `delist`, `sale`, `renew`, `release`, `reclaim`,
`offer_accepted`. The app shows any string, but these are the ones it labels.

### Identity and profiles
| Endpoint | Shape |
|---|---|
| `GET /identity/{address}` | `{"address": "kaspatest:q…", "label": "alice"\|null, "names": ["alice"], "profile": <profile>\|null}` |
| `GET /profiles/{address}` | `{"address": "…", "profile": <profile>\|null, "updatedAt": <ms>, "txId": "…"}` |

The profile object is the record as stored, in the current format:
`{"v": 1, "avatar": "<link>", "banner": "<link>", "bio": "<link>", "linktree": "https://linktr.ee/<name>", "primaryName": "alice"}`.
The app sanitizes it again on read.

### Errors
`{"error": "<code>", "message": "…"}` with:
- 400: bad input;
- 404: an unknown address or outpoint;
- 503: syncing, or no manifest.

The app treats any non-200 as "not available" and falls back where it can.

## 3. Publishing the testnet indexer: missing today

The control panel publishes **only the mainnet** indexer (`kachat-app`) under a public hostname,
with the 3080/8600 route split in `apps.js`. The testnet container (`kachat-app-testnet`) has no
public hostname, so a phone can't reach it, even once the follower is live.

Needed:
- A separate public hostname for `kachat-app-testnet`, for example `tn10.<your domain>`, with
  TLS.
- The **same** route split as mainnet: root → 3080, plus the chat paths
  (`/handshakes`, `/contextual-messages`, `/payments`, `/self-stash`, `/group-messages`,
  `/group-control`, `/v1/push`, `/metrics`) → 8600.
- The **same** public blocks: `/internal/push`, `/self-stash-gc-orphans`,
  `/contextual-messages/import`.
- **Send the app owner that URL.** The app's testnet profile will default its chat indexer,
  KaPosts, public chats and push to it. Once `/names/status` reports the registry id, `.kachat`
  switches over automatically.

## 4. Test vectors: already generated

`docs/HANDOFF.md` says the vectors couldn't be generated here, because `silverc` isn't
available. They exist already:

- **Where:** `KaChat/KaChatTests/KachatNamesVectors.json` in **vsmirn0v/KaChat** (your read-only
  clone).
- **What's in it:**
  - 28 transactions: the full e2e plan plus edge cases, including `cancel commit`;
  - codec vectors;
  - all built by `kachat-domains`' own `kachat-names-vectors`, on a synthetic genesis.
- **How to use it:** point `KACHAT_NAMES_VECTORS` at that file and `matches_generated_vectors`
  can run.
- **Caveat:** the offer template in the vectors is baked for the synthetic registry id, not the
  live one, so compare offer transactions by structure, not by the live offer template hash.

## 5. Order of work to "testnet fully working"

1. **wRPC `ChainSource`** into `Follower`, against the testnet node (`kaspad-testnet`, borsh
   17210), starting at `genesis.scanFrom`.
2. **Postgres persistence** (gaps, names, offers, profiles, history, checkpoint), and
   `Templates` from the manifest.
3. **The read API in section 2:**
   1. `/names/{name}` + `/names/gap/{key}` + `/names/by-owner`, so the app can register and
      exit through the indexer;
   2. `/identity` + `/profiles`;
   3. offers, listings, activity, expiring, history.
4. **Self-test**, then report `registryCovenantId` + `synced: true`.
5. **Publish the testnet hostname** (section 3) and send the URL.
6. **Part E push.** The app's notification text for name events comes later and doesn't block
   any of the above.

After step 5, the app owner registers, lists, buys, makes offers and edits a profile on testnet
in the app. Every step should appear in the indexer and agree with the `kachat-domains` CLI's
`status`.

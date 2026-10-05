# Address profiles on both networks: a profiles follower (handoff, 2026-10-05)

**For:** the indexer AI.
**From:** the iOS and panel session.
**Status:** the app and the panel are both done (KaChat `main`, Kaspa-Quick-Start `main`). This
document covers what the indexer needs so they work end to end.

**Indexer: implemented (2026-10-05).** `kachat-names-follower --profiles` runs as the
supervisord program `profiles` (`docker/kachat/app/run-profiles.sh`) on both networks, always
on, and is the only writer of `names_profiles` (the names follower no longer touches it).
State is in `profiles_state`; saves in `profile_saves`; `names_profiles.created_at` added.
Start block: `KACHAT_PROFILES_SCAN_FROM`, else the manifest's `scanFrom`, else the node's
pruning point. A block older than the pruning point falls back to it. `/profiles/*` and
`/identity/*` work without a manifest (profile-only identity). `GET /profiles/stats` is
served while the follower runs. Logs are tagged `[profiles]` with a heartbeat.

## Why

A KaChat profile isn't tied to a `.kachat` name. The avatar, banner, bio and Linktree are
stamped to the **address itself**, by a `kchat:1:profile:<json>` **self-send** from that address.
No registry contract is involved, and the only name-related field (`primaryName`) is optional.

From now on, the iOS app's **Edit KaChat Profile** screen saves profiles on **mainnet and
testnet**. On mainnet the primary name is greyed out ("Coming soon"), and the profile saves
without it.

Today profiles are indexed only *as a side job of the names follower*. That follower runs only
when `KACHAT_NAMES_MANIFEST` points at a genesis manifest (`docker/kachat/app/run-names.sh`), so:

| Indexer | Manifest | Profiles indexed? |
|---|---|---|
| `https://kachat.duckdns.org` (mainnet) | none, `/identity/*` → 503 | **no** |
| `https://tnkachat.duckdns.org:7443` (testnet) | `/names/status` shows `on: false`, `manifestPath: null` as of 2026-10-05 | **no**, not while the manifest is unloaded |

## What to build

### 1. A profiles follower that doesn't need a manifest

Run it on **both networks, always on** with the indexer: either its own process, or the names
follower in a profile-only mode when there's no manifest. When a manifest *is* loaded, the names
follower may keep doing profiles itself, as long as only one writer owns `names_profiles`.

Apply exactly the current profile rules (`ingest.rs` `apply_profile`, `profile.rs`
`parse_profile`):
- the payload is `kchat:1:profile:` followed by a JSON object with `v: 1`, at most 2 KB;
- fields are validated against the per-field allowlist; a field that's not allowed is dropped and
  the rest of the record kept;
- it must be a **real self-send** (03b924f): the first output's script is spent by at least one
  input whose spent script is known;
- newest wins, by `(accepting_daa, txid)`;
- reorgs roll back.

`primaryName` is stored as given. The label only comes from names, and stays `null` where there's
no registry.

**Start block.** Use a setting such as `KACHAT_PROFILES_SCAN_FROM=<block hash>` for each
network:

| Network | Start from |
|---|---|
| mainnet | a block from **before 2026-10-05**, when the first build that writes mainnet profiles was made. Nothing earlier can exist |
| testnet | the testnet-10 manifest's `genesis.scanFrom`, or earlier: profiles have existed there since the names work began |

After the first run, the stored checkpoint wins.

### 2. Storage

Keep the same `names_profiles` table, keyed by address (`kaspa:` or `kaspatest:`), and add:
- **`created_at`**: the block time of the address's *first* accepted profile record, kept across
  later saves (for "new profiles" counts);
- **a save log**, or at least a counter per day, for "saves in the last 7 days" and "records
  indexed". This can be a small `profile_saves(address, tx_id, block_time)` table.

### 3. Read API (same shapes as today, now on mainnet too)

| Endpoint | Answer |
|---|---|
| `GET /profiles/{address}` | `{"address", "profile": <profile>\|null, "updatedAt", "txId"}`. **200 with `profile: null`** when the address has none |
| `GET /identity/{address}` | `{"address", "label": <name>\|null, "names": [...], "profile": <profile>\|null}`. On mainnet `label: null`, `names: []` |
| `POST /identity/batch` | same, per address |

`/names/*`, `/market/*` and `/offers/*` stay as they are (off without a registry).
`GET /names/status` keeps `registryCovenantId: null` where there's no registry: the app keys its
registry reads on that.

### 4. New: `GET /profiles/stats` (for the Kaspa Quick Start panel)

The panel's new **KaChat → Profiles** tab, on both networks, reads exactly this:

```json
{
  "network": "mainnet",
  "synced": true,
  "indexedDaa": 123456789,
  "total": 412,
  "new24h": 9,
  "new7d": 51,
  "new30d": 180,
  "saves7d": 77,
  "records": 690,
  "withAvatar": 400,
  "withBanner": 210,
  "withBio": 305,
  "withLinktree": 96,
  "withPrimaryName": 0,
  "platforms": {
    "avatar": { "x": 250, "github": 60, "youtube": 40 },
    "banner": { "x": 180, "youtube": 30 },
    "bio":    { "x": 240, "github": 65 }
  },
  "recent": [
    {
      "address": "kaspa:q…",
      "updatedAt": 1759680000000,
      "txId": "…",
      "avatar": "https://x.com/alice",
      "banner": null,
      "bio": "https://x.com/alice",
      "linktree": "https://linktr.ee/alice"
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `total` | addresses with a current profile |
| `new*` | addresses whose **first** profile (`created_at`) falls in the window |
| `saves7d` | accepted profile records in the last 7 days, including updates |
| `records` | all accepted records ever |
| `with*` | current profiles that have that field set |
| `platforms` | per field, current profiles by platform. Keys are lowercase platform names: `x`, `youtube`, `discord`, `telegram`, `twitch`, `kick`, `github`, `facebook`, `instagram`, `tiktok`, `linkedin`, taken from the link's host |
| `recent` | the 25 most recently saved profiles, newest first. Links only: never fetch pictures or bios (§C) |
| `synced` | false while the follower is catching up |

Times are unix ms. Answer **503** while the profiles follower is off. The panel then shows "not
following", and a 404 tells the user to update the indexer.

### 5. Panel log

Tag the profile follower's lines `[profiles]`, with a progress heartbeat, so they show on the
KaChat log (KACHAT_NAMES_PANEL_LOGS.md style).

## How the app reads it (already shipped in KaChat)

- **When the network has no registry** (`KachatNamesService.isLaunched == false`, i.e. mainnet),
  `KachatNamesRegistry.identity(address:)` returns a **profile-only identity**:
  - the profile is this device's own saved record when the address is the wallet's own;
  - otherwise it's `GET /profiles/{address}`;
  - the label is `nil` and names are `[]`.
- **On testnet** the app still uses `/identity/{address}` through the names source.
- **A 503** from `/profiles` pauses *all* profile lookups for 10 minutes, so an indexer without
  the follower costs one request per 10 minutes, not one per contact.
- **Any other failure** pauses that address for 5 minutes.
- **Avatars, banners and bios** are resolved on the device from the stored social links. The
  indexer never fetches them.

## Done when

1. The app owner saves a profile on mainnet (Profile → Edit KaChat Profile → Save Profile).
2. Within a minute, `GET https://kachat.duckdns.org/profiles/<their kaspa: address>` returns it.
3. Their avatar shows on a **second** device that has them as a contact.
4. The panel's KaChat → Profiles tab shows the profile under "Recently saved" on mainnet, and
   the same works on testnet.
5. A bad profile, or a profile payload paid *to* someone (not a self-send), is ignored.

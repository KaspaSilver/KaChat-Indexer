# `.kachat` names - registry v4 (2026-10-07)

> **Not live yet.** The v4 testnet-10 genesis has not been sent. Until it is, keep following v3.
> The registry id below is from the **dry-run** test vectors and will change at the real genesis.
> The real manifest will be `manifests/kachat-names-testnet-10.json` in kachat-domains (branch
> `v4`), and v3's will be archived next to it. When it lands: re-point
> `KACHAT_NAMES_MANIFEST_TESTNET` at it; the new registry id resets the follower tables.

v4 is **v3 minus the price record**, with fixed prices baked into the contracts. Contracts are
immutable, so this is a **new registry**; v3's test names are left behind, as v1's and v2's were.
v4 is the design meant for mainnet.

The source of truth is KaspaSilver/kachat-domains, branch `v4`:
- `docs/REGISTRY_V4.md` (the spec and the owner's decisions);
- `contracts/*.sil`;
- `tools/kachat-names-cli`.

The iOS app is ported on KaChat branch `kachat-names-v4` (`0ed15e9`, `c8f1086`). Its chain
walker, `KaChat/Services/KachatNames/KachatNamesRegistryState.swift`, is the reference for what
this follower must produce.

## 1. What changes from v3

1. **No price record.** `KachatPrice`, the price covenant, the price genesis, the K shards and the
   authority key are gone. Nothing to seed, follow or serve for prices any more.
2. **Fixed prices, two tables.** The gap bakes a **register** table and a **renew** table; the name
   bakes the renew table (sompi per period, names of 1, 2, 3, 4, 5+ bytes). They are in the
   manifest's `params.prices.register` and `params.prices.renew`.
   - register pays `register[tier] + renew[tier] × (years − 1)`;
   - extend and renew pay `renew[tier] × years`;
   - `tier = min(max(len, 1), 5) − 1`.
3. **New windows.** Testnet-10: `periodMs` 600000, **`graceMs` 1800000** (30 minutes),
   `renewWindowMs` 600000. Mainnet: a year, **90 days** of grace, a **30-day** renewal window.
   Status (active / grace / lapsed) is still computed from `expiresAt` and the manifest's
   `graceMs`, so this is only a number in the manifest.
4. **One genesis.** Only the registry genesis exists; the manifest has no `priceCovenantId` and
   no `priceGenesis`.

Unchanged from v3: the gap (66 B), name (126 B) and offer (108 B) states, seller-bound offers and
`decline`, `periodMs`, the payload markers (minus the `kchat:1:prices:` one), every event op.

## 2. Templates and dispatch tags

silverscript v1.0.0. The prefix is 1 byte (`6b`) and the state starts at offset 1 in every
template.

| Template | Size | State / suffix | Template hash (testnet-10) |
|---|---|---|---|
| KachatGap | 4057 B | 66 / 3990 | `85cf57f8d300331c2acc5191794065d60fafdd29cac90e3b82e3e1ba1c3876f0` (bakes only params: fixed before the genesis) |
| KachatName | 3090 B | 126 / 2963 | `394204b612f345787412156521c0aabbd36bba30311f008302964d4c4ece685a` (bakes only params) |
| KachatOffer | 1114 B | 108 / 1005 | depends on the registry id (dry run: `6f043410…f52a`) |

Three tags changed, because three entries lost their trailing `priceIdx` argument. Load the tags
from the manifest per contract, as for v3.

| Contract | Entry | Tag (v4) | v3 |
|---|---|---|---|
| KachatGap | `register(name, ownerKey, salt, now, years, namePrefix, nameSuffix)` | **`8667af5e`** | `0e5c563d` |
| | `merge` / `absorbed` | `63d25bc2` / `dab76355` | same |
| KachatName | `extend(years)` | **`2ce7cceb`** | `ed8d84c6` |
| | `renew(years)` | **`b706ac38`** | `6daae613` |
| | `transfer` / `list` / `buy` / `release` / `reclaim` | `794dca54` / `674a8ea4` / `76a02eb9` / `388ad0b4` / `f56af4df` | same |
| KachatOffer | `accept` / `decline` / `withdraw` / `refund` | `2e18ed39` / `aead5037` / `80344ff1` / `777f5b11` | same |

Argument positions hold: register's `now` is arg 3 and `years` arg 4; extend's and renew's
`years` is arg 0. Only the trailing price index is gone.

## 3. The manifest (v4)

- `registryVersion: 4` - refuse anything else for a v4 follower.
- `registryCovenantId` and `genesis` (as in v3's registry genesis).
- `params`: `bond`, `gapValue`, `tCommit`, `maxYears`, `periodMs`, `graceMs`, `renewWindowMs`,
  `offerMaxFee`, `genesisGap`, and `prices: { register: {len1..len5plus}, renew: {len1..len5plus} }`.
- `artifacts`: KachatGap, KachatName, KachatOffer only.

Verify as the app does (`KachatNamesManifest.swift` `verify`): every template hash matches its
prefix ‖ suffix; the gap suffix contains the name template hash; the offer suffix contains the
registry id and the name template hash; `registryCovenantId == covenant_id(genesis.outpoint,
[(0, genesis gap)])`. The app also pins the gap and name hashes above and refuses a manifest
whose price tables differ from the ones those templates bake.

## 4. What the follower tracks

Seed the genesis gap at `genesis.txid:0`; there is nothing else to seed. Follow the registry
covenant only.

| Transaction | Inputs | Outputs |
|---|---|---|
| register | 0 gap `register(…)`, 1 commit, 2.. funding | 0 gap `(lo,key)`, 1 gap `(key,hi)`, 2 name, change |
| extend | 0 name `extend(years)`, 1.. funding | 0 name, change |
| renew | 0 name `renew(years)`, 1.. funding | 0 name, change |

Everything else (transfer, list, buy, offers, release, reclaim) is exactly as in v3.

## 5. API changes

- **`GET /names/status`**: drop `priceCovenantId`. The app matches the indexer on
  `registryCovenantId` alone.
- **`GET /names/prices`**: the app no longer calls it. Either remove it, or serve the manifest's
  two tables, for example
  `{"register": ["4000000000", …], "renew": ["1000000000", …]}` (decimal strings, sompi per
  period, tiers 1..5+). Nothing reads shards any more.
- **History / activity**: no `prices` / `price_authority` events can occur.
- `GET /stats` (`docs/KACHAT_NAMES_REGISTRY_V3.md` §10): the price fields there, if any were
  added, come from the manifest tables now.

## 6. What to do

1. **`kachat-names` crate:** let the manifest say `registryVersion: 4`, with no price covenant,
   price genesis or `KachatPrice` artifact. With a v4 manifest: don't seed or track shards, and
   don't expect a shard input or output in register / extend / renew. The per-contract tag
   loading from v3 picks up the new tags by itself.
2. **Store:** the shards table and `price_covenant_id` stay empty for v4 (or go, once v3 is no
   longer followed anywhere).
3. **Webserver:** the `/names/status` and `/names/prices` changes above.
4. **Pushes** (`KACHAT_NAMES_REGISTRY_V3.md` §8): the reminder schedule must keep reading
   `periodMs`, `graceMs` and `renewWindowMs` from the manifest. On testnet, grace is now three
   periods, not one.
5. **Test vectors:** 35 transactions at `KaChat/KaChatTests/KachatNamesVectors.json` on KaChat
   branch `kachat-names-v4`. The first 21 steps are the end-to-end run after the genesis:
   3 registrations, extend, renew, transfer, list, buy, 4 offers (accept, decline, refund,
   withdraw), release and reclaim. The other 14 are edge cases on their own records. No step
   carries a `shard` record any more. Point `vector_replay.rs` at them. The app's walker passes all
   of `scripts/test_kachat_names_registry.swift` on this file.
6. **Switch over:** follow the v4 manifest once the owner sends the genesis, the same day (the
   start block must still be inside the node's pruning window, see
   `docs/KACHAT_NAMES_PRUNED_START.md` §4). Stop following v3.

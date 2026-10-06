# `.kachat` names - registry v3 (2026-10-05)

> **Not live yet.** The v3 testnet-10 genesis has not been sent. Until it is, keep following v2.
> The registry and price covenant ids below come from the **dry-run** test vectors and will
> change at the real genesis. The real manifest will be `manifests/kachat-names-testnet-10.json`
> in kachat-domains (branch `v3`), and v2's will be archived in `manifests/v2/`. When it lands:
> re-point `KACHAT_NAMES_MANIFEST_TESTNET` at it and drop the v2 follower state.

> **Indexer: implemented (2026-10-06), dormant until the genesis.** One follower serves v2
> and v3; everything version-specific comes from the manifest (`registryVersion: 3`, the
> `KachatPrice` artifact, both covenant ids, `periodMs`, per-contract `dispatchTags`). It
> replays all 38 v3 vector transactions and the frozen 32 v2 ones
> (`kachat-names/testdata/vectors-v2.json`) exactly. v3 outputs are matched on their covenant
> binding (authorizing input + covenant id), so a look-alike output is never tracked. A v3
> manifest is verified at start (the genesis gap and all K shards must hash to their deployed
> scripts) and scanning starts at `priceGenesis.scanFrom`. Switching over = point
> `KACHAT_NAMES_MANIFEST_TESTNET` at the v3 manifest: a new registry id resets the tables.

> **2026-10-06:** the testnet follower is stuck on a start block the node has pruned. The plan for
> the v3 launch and the follower fixes it needs are in `docs/KACHAT_NAMES_PRUNED_START.md`.

v3 adds four things. Contracts are immutable, so this is a **new registry**; v2's test names are
left behind, as v1's were.

1. **Adjustable prices.** The prices are no longer template constants. They live in a **price
   record**: K = 8 identical **shards** under their own **price covenant**. Register, extend and
   renew read one shard in the same transaction. An authority key can change every price at
   once, up or down. There is **one table** for registering and renewing.
2. **`periodMs`** replaces the hard-coded year: a year on mainnet, **10 minutes** on testnet-10.
3. **Offers are bound to a seller**: the owner the offer was made to. Only that seller can accept
   it, and only while the name is still theirs. A change of owner ends every earlier offer.
4. **`decline`**: the seller sends an offer back to the buyer at any time. The app declines
   every other open offer when the owner transfers, releases, or accepts one.

The source of truth is KaspaSilver/kachat-domains, branch `v3`:
- `docs/REGISTRY_V3.md` (the spec);
- `contracts/*.sil`;
- `tools/kachat-names-cli`.

The iOS app is already ported, in KaChat `49c0baa`. Its chain walker,
`KaChat/Services/KachatNames/KachatNamesRegistryState.swift`, is the reference for what this
follower must produce.

## 1. States

The gap (66 B) and name (126 B) states are **unchanged** from v2. Two states change.

**Offer: 108 B** (v2: 75 B). It gains `seller`:

```
0x20 key[32] 0x20 buyer[32] 0x20 seller[32] 0x08 refundAfter[8]
```

**Price shard: 87 B** (new):

```
0x08 shard[8] 0x20 authority[32] 0x08 p1[8] 0x08 p2[8] 0x08 p3[8] 0x08 p4[8] 0x08 p5[8]
```

- `shard` runs 0..K-1 and is fixed for the shard's life.
- `authority` is the x-only key allowed to change the prices.
- `p1..p5` are sompi **per period** for names of 1, 2, 3, 4 and 5+ bytes. Every number is `num8`,
  as in the name state.

## 2. Templates

All built with silverscript v1.0.0. The prefix is 1 byte (`6b`) and the state starts at offset 1
in every template.

| Template | Size | State / suffix | Template hash |
|---|---|---|---|
| KachatPrice | 1691 B | 87 / 1603 | `d225c3a302b91866a8a7cb09d513b3375715794adf4f1e05eec872b32cb781d3` (fixed; it bakes nothing from the genesis) |
| KachatGap | 4498 B | 66 / 4431 | depends on the price covenant id (dry run: `004c4644…51cf`) |
| KachatName | 4058 B | 126 / 3931 | depends on the price covenant id (dry run: `3bd4442d…026b`) |
| KachatOffer | 1114 B | 108 / 1005 | depends on the registry id (dry run: `e9e9cb4d…46bf`) |

Load prefixes, suffixes and **dispatch tags** from the manifest. **Every v3 tag that matters
changed** (table below), and the tags only mean something per contract. So match a spend's tag
against the tags of the contract it spends: a tracked gap, name, offer or shard. A single global
tag → entry map is not enough. `transition::entry_for_tag` hard-codes the v2 tags today.

| Contract | Entry | Tag (v3) |
|---|---|---|
| KachatPrice | `use()` | `418a030f` |
| | `update(newAuthority, n1..n5, sig)` | `330d4c0b` |
| | `follow()` | `e823f7f1` |
| KachatGap | `register(name, ownerKey, salt, now, years, namePrefix, nameSuffix, priceIdx)` | `0e5c563d` |
| | `merge` / `absorbed` | `63d25bc2` / `dab76355` |
| KachatName | `extend(years, priceIdx)` | `ed8d84c6` |
| | `renew(years, priceIdx)` | `6daae613` |
| | `transfer` / `list` / `buy` | `794dca54` / `674a8ea4` / `76a02eb9` (as in v2) |
| | `release` / `reclaim` | `388ad0b4` / `f56af4df` |
| KachatOffer | `accept(nameIdx, sellerSig)` | `2e18ed39` |
| | **`decline(sellerSig)`** (new) | `aead5037` |
| | `withdraw` / `refund` | `80344ff1` / `777f5b11` |

New arguments are only ever appended, so existing argument positions hold:
- register: `now` is still arg 3 and `years` arg 4; `priceIdx` is appended as arg 7;
- extend / renew: `years` is still arg 0; `priceIdx` is arg 1.

## 3. The manifest (v3)

- `registryVersion: 3` - refuse anything else for a v3 follower.
- `priceCovenantId` and `registryCovenantId`.
- `priceGenesis`: `{ txid, outpoint, authority, authorizedOutputs[K], priceCovenantId, scanFrom }`.
  Each `authorizedOutputs[i]` is shard `i` at output `i`, with `state {shard, authority, prices[5]}`,
  `scriptPublicKey` and `value`.
- `genesis`: the registry genesis, as in v2. It is sent **after** the price genesis, because the
  gap and name templates bake the price covenant id.
- `params`:
  - `periodMs`, `graceMs`, `renewWindowMs`;
  - `prices` (the genesis prices, `len1`..`len5plus`);
  - `priceShards` (8) and `priceValue` (1 KAS, the exact value of every shard);
  - `bond`, `gapValue`, `tCommit`, `maxYears`, `offerMaxFee`.

On testnet-10, `periodMs`, `graceMs` and `renewWindowMs` are all `600000` (10 minutes), and
`maxYears` is 2, so a name is paid 20 minutes ahead at most.

Verify the manifest as the app does (`KachatNamesManifest.swift` `verify`):
- every template hash matches its prefix ‖ suffix;
- the gap and name suffixes contain `priceCovenantId` and the price template hash;
- the price genesis outputs are shards 0..K-1 of the price template, worth `priceValue`;
- `priceCovenantId == covenant_id(priceGenesis.outpoint, [(i, shard_i)])`;
- `registryCovenantId == covenant_id(genesis.outpoint, [(0, genesis gap)])`.

## 4. What the follower tracks

Seed **both** geneses:
- the K shards at `priceGenesis.txid:0..K-1`;
- the genesis gap at `genesis.txid:0`.

Follow the price covenant's UTXOs as well as the registry's. A shard output counts only if it
carries **`priceCovenantId`** and its script is the shard state you predicted. Check the
covenant id: a look-alike P2SH under another id is not a shard.

| Transaction | Inputs | Outputs | What to record |
|---|---|---|---|
| register | 0 gap `register(…, priceIdx=2)`, 1 commit, **2 shard `use`**, 3.. funding | 0 gap `(lo,key)`, 1 gap `(key,hi)`, 2 name, **3 shard continuation** (same state), change | as v2; `expiresAt = now + years·periodMs`; the shard moves to output 3 unchanged |
| extend | 0 name `extend(years, 1)`, **1 shard `use`**, 2.. funding | 0 name, **1 shard** (same state), change | `expiresAt += years·periodMs`, periodStart kept |
| renew | 0 name `renew(years, 1)`, **1 shard `use`**, 2.. funding | 0 name, **1 shard**, change | `periodStart = old expiresAt`, `expiresAt = old expiresAt + years·periodMs` |
| **price change** | 0 shard 0 `update(newAuthority, n1..n5, sig)`, 1..K-1 shards `follow`, K.. funding | 0..K-1: shard `j` = `(j, newAuthority, n1..n5)`, each at `priceValue`; change | every shard's new state; event `prices` (or `price_authority` when only the key changed) |
| offer accept | 0 name `transfer(buyer, ownerSig)`, 1 offer `accept(0, sellerSig)` | 0 name to the buyer, 1 payout to the seller | as v2 (`offer_accepted` on the name) |
| **offer decline** | 0 offer `decline(sellerSig)`, **alone** | 0 to the buyer | offer gone; event **`offer_decline`** |
| offer withdraw / refund | as v2 | as v2 | offer gone; events `offer_withdraw` / `offer_refund` (the app shows all three) |

- **Shard continuations.** In `use()` the shard's output is authorized by the shard's own
  input. In a change, each shard's continuation is authorized by that shard's input. The app
  checks `authorizingInput` for every prediction (registry and price alike); do the same.
- **The name's `price` field** is still the listing price. The fee paid is
  `shard.p[tier(len)] × years`, where `tier(len) = min(max(len,1),5) − 1`. Recording it on
  `register` / `extend` / `renew` events is optional.
- **Status** (active / grace / lapsed) is computed from `expiresAt` and the manifest's `graceMs`
  (10 minutes on testnet).
- **Replace `YEAR_MS`** (`transition.rs`) with the manifest's `periodMs`.
  `kachat-names-follower/src/pushes.rs` also hard-codes a year.

## 5. Payload markers

| Marker | Carried by | Notes |
|---|---|---|
| `kchat:1:offer:<keyHex>:<buyerHex>:<sellerHex>:<refundAfterDaa>` | offer creation | **4 parts now** (v2: 3). Build the 108-B state and trust it only when `P2SH(offer state)` matches an output, as in v2 §B4 |
| `kchat:1:prices:<p1>:<p2>:<p3>:<p4>:<p5>:<authorityHex>` | price change | informational only: the shard outputs are the truth |
| `kchat:1:name:<op>:<name>` | register, extend, renew, transfer, list, buy, accept, release, reclaim | unchanged |

Decline, withdraw and refund carry no payload.

## 6. API changes (`docs/KACHAT_NAMES_APP_CONTRACT.md` §2)

The app uses the indexer **only when `/names/status` matches both ids** of its manifest.
Otherwise it walks the chain itself. So an indexer that hasn't been updated is ignored, not
misread.

- **`GET /names/status`**: add **`"priceCovenantId"`**, reported only once synced, exactly like
  `registryCovenantId`.
- **Offer objects** (`/names/{name}/offers`, `/offers/by-buyer/{address}`): add
  **`"seller": "<address>"`**. The app **drops any offer without a seller**, because a v3 offer
  can't be accepted or declined without it. The app works out "declined" itself
  (`seller != the name's current owner`) and greys those offers out.
- **History / activity `op`**: add **`offer_decline`**. Keep `prices` / `price_authority` out of
  `/market/activity` (the app's walker leaves them out too); a separate feed is fine.
- **New `GET /names/prices`:**

  ```json
  {
    "prices": ["4000000000", "2000000000", "1000000000", "250000000", "35000000"],
    "authority": "<x-only hex>",
    "shards": [
      { "shard": 0, "outpoint": { "txId": "<hex>", "index": 3 },
        "authority": "<x-only hex>", "prices": ["4000000000", "…5 strings"], "value": "100000000" }
    ]
  }
  ```

  - Prices and values are **decimal strings** in sompi, as elsewhere.
  - `prices` and `authority` at the top are the current ones (every shard agrees after a change).
  - `shards` lists every live shard, in shard order. The app drops a shard entry without exactly
    5 prices.
  - The app picks a shard at random and re-reads its UTXO from a node before spending it.
    Several apps can then spend different shards at the same moment, and a stale entry only costs
    a retry.

## 7. What to do

1. **`kachat-names` crate:**
   - an `OfferState` of 108 B with `seller`;
   - a new `PriceState` (87 B);
   - a new `Tracked::Shard`;
   - entries `PriceUse` / `PriceUpdate` / `PriceFollow` and `OfferDecline`;
   - per-contract tags loaded from the manifest;
   - `periodMs` from the manifest;
   - the 4-part offer marker;
   - seed both geneses;
   - check `priceCovenantId` on shard outputs.
2. **Store:**
   - a shards table (outpoint, shard, authority, p1..p5, value);
   - `names_state` keeps `price_covenant_id` next to `registry_covenant_id`;
   - offers get `seller`;
   - the reorg journal covers shards like any tracked UTXO.
3. **Webserver:** `/names/status` `priceCovenantId`, offer `seller`, `/names/prices`, the
   `offer_decline` op.
4. **Test vectors:** 38 transactions, regenerated for v3, at
   `KaChat/KaChatTests/KachatNamesVectors.json` (vsmirn0v/KaChat). The first 23 steps are the
   end-to-end run after both geneses:
   - 3 registrations;
   - **2 price changes**;
   - extend and renew (each reading a shard);
   - transfer, list, buy;
   - 4 offers: accept, **decline**, refund, withdraw;
   - release and reclaim.

   The other 15 are edge cases on their own records: a price change on 8 seeded shards, a decline
   alone, renew and extend with a shard, and others. Every step's `records` carries its
   `shard` / `shards`, and every offer record its `seller`. Point `vector_replay.rs` at them. The
   app's walker passes all 1,000 checks of `scripts/test_kachat_names_registry.swift` on this
   file, so it is a good reference for the expected states and events.
5. **Switch over:** follow the v3 manifest once the owner sends the two geneses (price first,
   then registry). Stop following v2.

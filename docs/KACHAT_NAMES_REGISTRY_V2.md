# `.kachat` names - registry v2 (2026-10-02)

> **Live on testnet-10:**
> - registry `82f4315c8f7b3e0e76fc2f77466fe7651d2c1fac4e0b5810d4da878a9cfa0f89`
> - genesis tx `e20325f70db06192b619e6ef161b45b4a24b3cde58b5d2b0188532395175a426`, accepted at DAA
>   586,328,979
> - manifest: `manifests/kachat-names-testnet-10.json` in kachat-domains; v1 is archived in
>   `manifests/v1/`
>
> Re-point `KACHAT_NAMES_MANIFEST_TESTNET` at the new manifest and drop the v1 follower state.

The contracts changed to cap how far ahead a name can be paid: **2 years at most**, with no
stacking of renewals. Contracts are immutable, so this is a **new registry**. The live
testnet-10 registry `9444187f…7a51` (v1) is retired. It held only
its genesis gap, so no names are stranded.

The source of truth is KaspaSilver/kachat-domains (README "Registry v2", `contracts/*.sil`,
`tools/kachat-names-cli`). The app is being updated to match. This file lists what the
`kachat-names` crate and the API must change.

## 1. Name state: 126 bytes, new field `periodStart`

```
0x20 key[32] 0x20 name[32] 0x20 owner[32] 0x08 price[8] 0x08 periodStart[8] 0x08 expiresAt[8]
```

- **`periodStart`** (unix ms, `num8`) is the start of the current paid period. It occupies bytes
  `109..117`, and **`expiresAt` moves to `118..126`**.
- The gap state (66 B) and the offer state (75 B) are unchanged.

## 2. Templates

Silverscript v1.0.0; the same on testnet and mainnet.

| Template | Size | Prefix / state / suffix | Template hash |
|---|---|---|---|
| KachatName | 3108 B | 1 / 126 / 2981 | `e8ded947687947b565e10cbf6e6fec60e5c90cf992c7bce2298e6dce8db29d16` |
| KachatGap | 4014 B | 1 / 66 / 3947 | `182c463cf59f6d175f75339e4efc75d2065e8e7bb8dcc515e4769d3ff805dd46` |
| KachatOffer | 948 B | 1 / 75 / 872 | depends on the registry id |

The new manifest (`manifests/kachat-names-testnet-10.json` in kachat-domains) carries the
prefix/suffix bytes, the new param `renewWindowMs` = `864000000` (10 days), and the new registry
id. Load templates from the manifest as before. The v1 manifest is archived.

## 3. Transitions (replaces the name rows of §B3)

`YEAR` = 31,536,000,000 ms.

| Spend | Tag | New state |
|---|---|---|
| gap `register(name, ownerKey, salt, now, years, …)` | `8667af5e` | name `(key, pad(name), ownerKey, price 0, periodStart = now, expiresAt = now + years·YEAR)` |
| name `transfer(newOwner, sig)` | `794dca54` | owner = newOwner, price 0; periodStart and expiresAt unchanged |
| name `list(price, sig)` | `674a8ea4` | price; the rest unchanged |
| name `buy(newOwner)` | `76a02eb9` | owner = newOwner, price 0; periodStart and expiresAt unchanged |
| **name `extend(years)`** | **`2ce7cceb`** (new) | `expiresAt += years·YEAR`; **periodStart unchanged**. The contract allows it only while `expiresAt + years·YEAR <= periodStart + 2·YEAR` |
| **name `renew(years)`** | `b706ac38` (same tag, **new meaning**) | **`periodStart = old expiresAt`**, `expiresAt = old expiresAt + years·YEAR`. Allowed from `expiresAt − renewWindowMs` on, by a timestamp time lock |
| offer `accept` + name transfer/buy | `9d4043b4` | the name goes to the buyer; periodStart and expiresAt unchanged |
| exits, gap entries, offer withdraw/refund | unchanged | unchanged |

- **Payload markers:** `kchat:1:name:extend:<name>` is new. The others are unchanged.
- **Verify, don't trust:** check every derived state against the output's P2SH (§B3).
- **Status:** active / grace / lapsed is still computed from `expiresAt` and `graceMs`.

## 4. API additions (`docs/KACHAT_NAMES_APP_CONTRACT.md` §2)

- **Name object:** add **`"periodStart": <ms>`** next to `expiresAt`. The app uses it to offer
  "Extend" (while `periodStart + 2y − expiresAt >= 1 year`) and to show when renewal opens
  (`expiresAt − renewWindowMs`).
- **History event `op`:** add **`extend`**. `renew` keeps its name.
- **`/names/status`** reports the **v2** registry id once synced.

## 5. What to do

1. In `kachat-names`:
   - the name state codec goes to 126 B, with periodStart;
   - add the `extend` entry tag;
   - the `renew` transition sets periodStart;
   - the register, transfer, buy and offer-accept transitions carry periodStart.
2. **Re-validate.** The v1 live-genesis test (`reproduces_live_testnet_genesis_gap_output`)
   proves the gap codec, and the gap state didn't change. Re-run it against the **v2** genesis
   output once it exists: the gap template hash changed, so the P2SH changes.
3. **Test vectors:** 32 transactions, regenerated for v2, at
   `KaChat/KaChatTests/KachatNamesVectors.json` (vsmirn0v/KaChat). They include extend, renew in
   the window, renew right at the window opening, renew in grace, and extend as a gift. Each name
   record carries `periodStart`.
4. **Retire v1.** Follow the v2 manifest once the app owner sends the new genesis; don't follow
   the v1 registry.

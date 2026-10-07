# `.kachat` testnet-10: registry v4 redeployed on a day clock (2026-10-07)

> **LIVE on testnet-10 since 2026-10-07 ~20:18 UTC. Switch today**, while the scan-from block is
> still inside the node's pruning window (`docs/KACHAT_NAMES_PRUNED_START.md`).
> - genesis tx `5ffdd006230bcba0ee52c3ce7b69fba2b9a4d489b57fee93e2b103eb1622a777`, accepted at
>   DAA ~590718327; scan from block `f08cac7e201efd9a068b5454c5edcdf186b1f9edde18161c0a6ee7f959722161`
>   (the manifest's `genesis.scanFrom`);
> - registry covenant id `e6b7244831004e1db928458bce570347317b50ff124c010d342d73a6c2017f0d`;
> - template hashes: gap `9f057f40…8bf5`, name `c263a8c2…b56b`, offer `5a7e22af…2a7a`;
> - manifest: kachat-domains `manifests/kachat-names-testnet-10.json` (`main`, `d53d7bf`); the
>   10-minute deployment's manifest is archived in `manifests/v4-10min/`.

**For:** the indexer session. **From:** the iOS session (KaChat `fab03f1`).

## What changed

The **contracts and the format are the same registry v4**: same tags, entries, state layouts and
prices. Only the testnet params changed, and the gap and name bake them, so the template hashes
are new, and that means a new genesis and a new registry id. The owner wants names to be held
long enough to rehearse migrating them to a new version.

| testnet-10 | 10-minute v4 (retired) | **day-clock v4 (live)** |
|---|---|---|
| `periodMs` | 600,000 (10 min) | **86,400,000 (24 h)** |
| `graceMs` | 1,800,000 (30 min) | **21,600,000 (6 h)** |
| `renewWindowMs` | 600,000 (10 min) | **7,200,000 (2 h)** |
| registry | `bff18554…0e2f` | **`e6b72448…7f0d`** |

Mainnet params are unchanged. The old registry's names don't carry over.

The follower reads `periodMs`, `graceMs` and `renewWindowMs` from the manifest, so no logic change
should be needed. Push reminders, `/names/grace`, `/names/expiring` and the expiring-soon logic
all follow those values.

## To do

1. Re-point `KACHAT_NAMES_MANIFEST_TESTNET` at the new manifest and restart the follower. The new
   registry id resets the tables.
2. **Kaspa-Quick-Start** bundles the manifest at `manager/lib/names/kachat-names-testnet-10.json`
   (`BUNDLED_NAMES_MANIFEST` in `manager/server.js`). Replace it with the kachat-domains one
   (byte-identical copy, as in KQS `0b5d46c`).
3. `kachat-names-follower/src/main.rs` (~line 817): the test that reads the live manifest asserts
   the old registry id `bff18554…`. Change it to `e6b72448…7f0d`. The `bootstrap.rs` fixture that
   uses the old id is just a parser sample and can stay.
4. The 35 v4 vectors were regenerated on the day clock (KaChat `KaChatTests/KachatNamesVectors.json`,
   or `kachat-names-vectors` in kachat-domains). If the follower's replay test pins the 10-minute
   set, either keep it as the frozen `v4-10min` set or add the new one.

Until `/names/status` reports `e6b72448…`, the app ignores the indexer and walks the chain itself
(`chooseSource` compares the registry id). This also covers the `/identity` grace-label change in
`docs/KACHAT_NAMES_GRACE_RESOLVES.md`: do both together if convenient.

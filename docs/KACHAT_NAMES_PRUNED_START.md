# `.kachat` follower - a start block the node has pruned (2026-10-06)

## What happened on testnet-10

After the registry v3 update (`2971bd6`), the testnet names follower stopped making progress:

```
WARN kachat_names_follower: [names] fetch failed (getBlock: RPC Server (remote error) ->
  cannot find header c340e72ddb7fe364ffb7248e866366f3170bbd2a4f2c5baa0efd72df7f719f30);
  next batch capped to Some(50) blue score
```

That line repeats every 10 seconds. `/names/status` shows `indexedDaa: 0, synced: false` for good.

1. The follower had **no stored checkpoint**, so it started from the manifest's `scanFrom`: the
   v2 genesis block of 2026-10-02.
2. A pruned node keeps roughly the last day of blocks, so the node no longer has that block.
   `getVirtualChainFromBlockV2` fails, and the `Window` logic halves the batch size and retries
   forever.
3. Nothing reports the condition. The status page reads exactly like "just started".

Two problems, both worth fixing before mainnet:
- **(a)** a checkpoint was lost;
- **(b)** a follower that can't reach its start block never recovers, and never says why.

## 1. Never lose the checkpoint (find out why it happened)

- The tables are reset only when `store::stored_registry() != manifest.registry`. So either the
  Postgres data was emptied, or the stored id and the manifest's differ (case, a stray
  whitespace, a different manifest file).
  - Compare case-insensitively.
  - Log both values when a reset happens: `"[names] fresh registry tables: stored X, manifest Y"`.
- Check that a KQS **Update** keeps the names database volume.
  - **Owner (2026-10-06):** "a normal update keeps it and resumes" is the requirement.
  - If an update can recreate the volume, that is the bug to fix in Kaspa-Quick-Start.

## 2. Say so when the start block is gone

When `next_batch(from, ...)` fails with `cannot find header <from>`, compare `from` with the
node's pruning point (`getBlockDagInfo`: `pruning_point_hash`, plus the DAA scores).

- If `from` is older than the pruning point, it will never come back, so don't retry in a loop.
  - Record it in `names_state` (for example a `fatal_reason TEXT` column).
  - Log it once a minute as an **ERROR**, not a WARN:
    `"[names] start block <hash> is below the node's pruning point; this node can't replay the registry from it (see docs/KACHAT_NAMES_PRUNED_START.md)"`.
- Serve it in `/names/status`, for example `"error": "start_block_pruned", "startBlock": "<hash>"`,
  so the KQS panel's .kachat card can show it.
- `registryCovenantId` / `priceCovenantId` stay `null`, as for any unsynced follower. The app then
  walks the chain itself, so it is unaffected.

## 3. Recover without the old blocks: a REST bootstrap

> **Implemented (2026-10-07):** `kachat-names-follower/src/bootstrap.rs`. When §2's condition
> holds (and `KACHAT_NAMES_REST_BOOTSTRAP` is not `off`), the follower walks the registry from the
> manifest seeds through `getUtxosByAddresses` + `GET {rest}/addresses/{p2sh}/full-transactions`
> (`KACHAT_NAMES_REST_URL`, default api.kaspa.org / api-tn10.kaspa.org), applies each spend with the
> engine once every registry/price-covenant input of it is tracked, checkpoints at the node's sink
> and continues over the node; transactions it already applied are skipped there. `/names/status`
> reports `bootstrappedAt` (DAA). `--bootstrap-probe` runs the walk without a database: on the
> retired v3 testnet registry it replays all 14 transactions and the self-test finds no mismatch.

The app already rebuilds the whole registry without any node history. Its chain walker,
`KaChat/Services/KachatNames/KachatNamesRegistryState.swift` `walk(...)`, does this:

1. **Seed** from the manifest: the genesis gap, and for v3 the K price shards at the price genesis
   outputs. The manifest already holds their outpoints, scripts and states.
2. **Find what was spent.** For every tracked outpoint, ask the node whether it is still unspent:
   `getUtxosByAddresses` on its P2SH address (the follower already needs `--utxoindex` for the
   self-test).
3. **Find the spender.** For each spent one, find the spending transaction in the REST API's
   history for that address:
   `GET {rest}/addresses/{p2sh}/full-transactions?resolve_previous_outpoints=no`, then match on the
   input outpoint.
4. **Apply** it with the same engine (`Registry::apply`): it verifies every derived state against
   its output's script and covenant binding, as now. Track the new outputs.
5. **Repeat** until no tracked outpoint is spent.
6. **Switch to the node.** Store a checkpoint at a block the node has (the current sink, or a
   block a few hundred DAA back) and continue with `getVirtualChainFromBlockV2` as today. Overlap
   is safe: `apply` is idempotent per txid.

The walker's integration test runs this exact loop against a simulated chain:
`KaChat/scripts/test_kachat_names_registry.swift` `runWalk`. Its `--live` mode walks the real TN10
registry through `api-tn10.kaspa.org`.

Rules:
- **Use it only when needed:** when §2's condition holds, or with an explicit
  `--bootstrap-rest <base>` flag. Normal syncing stays on the node.
- **Network:** the REST base comes from the network (`api-tn10.kaspa.org` /
  `api.kaspa.org`), with an env override.
- **Offers are the one gap.** Offer outputs carry no covenant id, and their creating transaction
  touches no registry UTXO, so a REST walk can't discover an offer created before the switch
  point. (The marker is in the payload, but REST can't search payloads.)
  - Offers live 7 days at most in the app.
  - Accept that offers older than the bootstrap are missing until they are accepted, declined,
    withdrawn or refunded; each of those spends is found by the walk once its name or offer is
    tracked.
  - Write this limit into the status, for example `"bootstrappedAt": <daa>`.
- **History:** every applied transaction gives the same events as today, so
  `/names/{name}/history` is complete for registry transitions.
- **Archival node:** if a KQS node runs with `--archival`, prefer the node and skip the REST
  walk.

## 4. For the testnet v3 launch (now)

The owner chose to time the launch with the indexer instead of waiting for §3:

1. Send the price genesis and then the registry genesis. Each gets a dry run first and the
   owner's "send it".
2. **The same day:** put the v3 manifest (`manifests/kachat-names-testnet-10.json` in
   kachat-domains) on the testnet indexer and restart the follower. Its `scanFrom` is then the
   fresh price genesis block, well inside the node's window.
3. Check that `/names/status` moves (`indexedDaa` rises, then `synced: true` with both covenant
   ids), and that `/names/prices` lists 8 shards.
4. From then on the checkpoint must survive every update (§1).

v2 on testnet is retired by the v3 genesis, so its pruned history doesn't need recovering. §2 and
§3 matter for mainnet, and for any testnet indexer set up or reset more than a day after a
genesis.

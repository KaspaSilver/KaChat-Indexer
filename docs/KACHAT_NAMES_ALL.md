# `GET /names/all`: every registered name, for the completeness proof (2026-10-07)

**For:** the indexer session. **From:** the iOS session (kachat-domains `verify --live`).

kachat-domains now has `kachat-names verify --live --indexer <url>`. It proves the indexer's list
of names is **exactly** the registry on chain:
- every gap between the sorted keys must exist on chain, which rules out hidden or invented names;
- every name must exist with its exact owner, price and dates.

It's run by Kaspa Quick Start operators against their own indexer (kachat-domains `docs/KQS.md`
section 2.1). It needs one endpoint that lists every name.

## The endpoint

`GET /names/all?cursor=` returns **every** registered name UTXO, whatever its status: active, in
grace and lapsed alike (a lapsed name is still on chain until someone reclaims it). That's
`NAME_SELECT` with no status filter, **ordered by key** so pages are stable:

```rust
let sql = format!("{NAME_SELECT} ORDER BY u.key ASC LIMIT $1 OFFSET $2");
// .bind(len + 1).bind(off)
```

The response has the same shape and paging as `/names/expiring`:
`{"names": [<name_json>...], "next": <cursor or null>}`.

The verifier reads, from each `name_json`:
- `name` and `key` (it checks `key == blake3(name)`);
- `ownerKey`;
- `price` (a string or a number);
- `periodStart` and `expiresAt`.

It also reads `/names/status` and requires `registryCovenantId` to match its manifest.

Route it next to `/names/expiring` in `web_server.rs`, **before** `/names/:name`, so that "all"
isn't taken as a name.

## Notes

- Offers are not part of the proof.
- If a block lands between pages, a page may skip or repeat a name. The proof then fails and is
  retried. That's acceptable at testnet volumes. If it isn't, serve the list from one snapshot
  (one query) and return `indexedDaa` with it.

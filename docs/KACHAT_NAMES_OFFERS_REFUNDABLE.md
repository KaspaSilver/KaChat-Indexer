# `.kachat`: `GET /offers/refundable` and keyset paging for `/names/all` (2026-10-16)

**For:** the indexer session. **From:** the kachat-domains session. Audit findings DOM-036 and
DOM-037 (kachat-audits).

## Why

kachat-domains has a refund keeper, `kachat-names refund-expired --indexer <url>`. KQS is asked to
run it on a schedule (kachat-domains `docs/KQS.md` section 6). On chain an offer stays acceptable
by its seller until someone spends it, so the keeper refunds every offer whose `refundAfter` has
passed back to its buyer. That is the protection behind `docs/MAINNET.md` C1 and C6.

Today the keeper can only find offers by name: `/names/all`, then `/names/<name>/offers` for each
name. That has three problems:

- **DOM-037: it misses offers whose name is no longer registered.** If the owner released the
  name, or someone reclaimed it, `/names/all` doesn't list it. The offer is still open, and if the
  same seller registers the name again, it can be accepted again (C6). These are exactly the
  offers the keeper exists for.
- **DOM-036: one request per name** runs into the rate limit (100/min by default).
- **DOM-036: the offset cap.** `/names/all` pages by offset, and `MAX_OFFSET = 10_000`. Past
  10,050 names, every `/names/all` reader fails: the keeper, and also `kachat-names verify --live
  --indexer`, which proves the indexer against the chain.

## 1. New: `GET /offers/refundable?cursor=&length=`

Every open offer whose `refundAfter` has passed, in one keyset-paged listing.

- **Rows:** `names_utxos WHERE kind = 'offer' AND NOT refuted AND refund_after < indexed_daa`
  (refund needs `tx.daa >= refundAfter`, and its lock time is only final once the DAA is past it).
- **Order and cursor:** keyset on `(refund_after, txid, idx)`. `next` is an opaque cursor, or
  `null` on the last page. No offset, and no cap on how far it pages.
- **`length`:** default 50, max 200 (the keeper asks for 200).
- **Rate limit:** count one request per page, as `/names/all` does.

**Response:**

```json
{
  "offers": [
    {
      "outpoint": { "txId": "<hex>", "index": 0 },
      "key": "<hex blake3(name), the offer state's key>",
      "name": "<name, when known (live row or history), else omitted>",
      "buyer": "<kaspa: / kaspatest: address>",
      "seller": "<address>",
      "amount": "<sompi, string>",
      "refundAfter": 123456789
    }
  ],
  "next": "<cursor>" ,
  "indexedDaa": 123456999
}
```

- **`key` is required.** The keeper rebuilds the offer's state from `key`, buyer, seller and
  `refundAfter`, and checks the UTXO at that address against its own node. So nothing in the
  answer is trusted.
- **`name` is optional.** If present, it must hash to `key`.
- **The keeper uses it as soon as it exists.** It tries this endpoint first, and falls back to the
  per-name walk on a 404. It already handles the shape above (kachat-domains
  `tools/kachat-names-cli/src/keeper.rs`, `collect_offers`).

## 2. `/names/all`: keyset paging

Replace `ORDER BY u.key ASC LIMIT $1 OFFSET $2` and the `offset_cursor` / `MAX_OFFSET` path with
keyset paging: `WHERE u.key > $cursor ORDER BY u.key ASC LIMIT $1`, with `next` = the last row's
key (hex), and no cap.

`verify --live` and the keeper only follow `next` as an opaque string. They send
`length=200` and `cursor=<next>`, so the cursor format can change without a client change. Until
then, the CLI reports the 10,000-row `cursor_too_far` as "the registry has outgrown offset paging".

## When

Not urgent today: mainnet has about 42 names and no offers. But the keeper's protection depends on
it, and mainnet opened to the public on 2026-10-16. When it's done, a note in `docs/KQS.md`
section 6 (kachat-domains) can drop the "registered names only" caveat.

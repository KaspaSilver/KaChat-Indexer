# Names in grace keep resolving: `/identity` label rule (2026-10-07)

**For:** the indexer session. **From:** the iOS session (KaChat `f7c371a`, spec `d6720a4`).

## The decision

An expired name **still resolves to its owner and still labels them while it's in its grace
period**. It stops only when it lapses (`expires_at + grace_ms <= now`) and is back on the market.
The owner decided this so a user in grace doesn't become hard to find.

| Status | Resolves / labels? |
|---|---|
| Active (`now < expires_at`) | yes |
| Grace (`expires_at <= now < expires_at + grace_ms`) | **yes (was no)** |
| Lapsed (`now >= expires_at + grace_ms`) | no |

The app already resolves forward lookups this way (it takes `/names/{name}` and computes the status
itself). The one thing it can't do itself is the **label** that `/identity/{address}` and
`/identity/batch` return. That label is computed here.

## The change

`kachat-webserver/src/names_api.rs`, `identity_for`: keep names that are not lapsed, instead of
only the active ones:

```rust
.filter(|r| name_status(r.get::<Option<i64>, _>("expires_at").unwrap_or(0), c.grace_ms, c.now) != NameStatus::Lapsed)
```

Update its doc comment as well: "held names (active or in grace), oldest first … `primaryName`
while held, else the oldest held name". `names` in the response uses the same set, which matches
the app's walker (`heldNames`).

## Not affected

- `/names/by-owner`, `/names/expiring`, `/names/grace`, listings and offers keep their filters.
- Listing, transfer and offers still need an active name (the app enforces this).
- Contracts: no change.

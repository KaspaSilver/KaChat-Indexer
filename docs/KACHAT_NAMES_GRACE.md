# `GET /names/grace`: names in their grace period (2026-10-07)

**For:** the indexer session. **From:** the iOS session (KaChat `cb3c27d`).

The app's .kachat marketplace has a new **Expired** tab. It lists names that have expired and
are still in their grace period, with a live countdown to the moment each is released (becomes
claimable). It's for people waiting to grab a name they want.

## What's missing

`GET /names/expiring` returns **lapsed** names (`expires_at + grace <= now`): those are the
app's **Available** tab. Nothing serves names **in grace** (`expires_at <= now < expires_at + grace`).

Until this endpoint exists, the app builds the list from its own chain walk. That only works
while it isn't using the indexer.

## The endpoint

`GET /names/grace?cursor=` - registered names with `expires_at <= now < expires_at + grace_ms`,
**soonest release first** (`ORDER BY expires_at ASC`). Paged exactly like `/names/expiring`.
The response is `{"names": [<name_json>...], "next": <cursor or null>}`, the same name objects
as `/names/expiring` and `/names/by-owner`.

Next to the existing `expiring` handler in `kachat-webserver/src/names_api.rs`:

```rust
let sql = format!("{NAME_SELECT} AND u.expires_at <= $2 AND u.expires_at + $1 > $2 \
                   ORDER BY u.expires_at ASC LIMIT $3 OFFSET $4");
// .bind(c.grace_ms).bind(c.now).bind(len + 1).bind(off)
```

Route: `.route("/names/grace", get(crate::names_api::grace))` in `web_server.rs`, next to
`/names/expiring`.

## Notes

- The countdown target is `expiresAt + graceMs` from the manifest. The app computes it itself,
  so nothing else is needed in the response.
- **Testnet-10 (registry v4):** grace is 30 minutes, so names pass through this list quickly.
- **Mainnet:** grace is 90 days.
- This is the same set of names the follower's push schedule already treats as "in grace"
  (the `grace` reminder).

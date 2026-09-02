# KaPosts indexer (KaChat-owned fork)

This is a fork of [K-indexer](https://github.com/thesheepcat/K-indexer) adapted to be the
**KaChat-owned KaPosts indexer**. It keeps full API compatibility with the K read API the
KaChat iOS app already speaks, and adds the KaChat-specific behaviour described in the app's
`KAPOSTS_INDEXER.md`.

## What this fork changes vs. upstream K-indexer

1. **Server-side exclusivity (two-way).** The transaction processor indexes a post/reply/quote
   only if its decoded message begins with the KaChat marker **U+2060** (WORD JOINER).
   K-website content never enters this database.
   → `kachat-transaction-processor/src/k_protocol.rs` (`validate_kachat_message`, gates in
   `save_k_post/reply/quote_to_database`).
   **Text-only content policy:** the same gate also rejects anything that isn't bounded plain
   text — non-UTF-8, over `MAX_KACHAT_MESSAGE_CHARS` (4096), embedded media / data URIs
   (`data:image/…`, `;base64,`, …), or control characters. So even a modified client cannot
   get media stored/served by this indexer; raw binary was already rejected by the UTF-8 +
   marker check.
   Votes are additionally gated on their **target being indexed KaChat content**, and follows
   on **at least one party being a KaChat identity** (having authored KaChat content), so
   K-network engagement never enters the DB either — KaPosts is a fresh, self-contained
   network with no relation to the K social graph.
2. **Removal counter-actions.** Two new on-chain actions, verified against the actor's pubkey,
   that delete rows (all social counts are computed live, so counts drop automatically):
   - `unvote` — withdraws the sender's prior upvote/downvote on a post.
   - `unquote` — withdraws the sender's prior quote/repost of a content id.
   → `k_protocol.rs` (parser + `save_k_vote_to_database` unvote branch +
   `process_k_unquote_in_database`).
3. **Per-post actor lists.** New read endpoint `GET /get-post-engagement` returning who
   upvoted/downvoted/reposted/quoted **any** post (upstream only exposed this for your own
   posts, via notifications).
   → `kachat-webserver/src/{web_server,api_handlers,database_trait,database_postgres_impl,models}.rs`.
4. **kachat-admin dashboard.** A new workspace crate serving an ops/admin GUI (pipeline health,
   table stats, content moderation, broadcasts).
   → `kachat-admin/`.
5. **KaChat broadcast indexing.** The same stack also indexes KaChat **broadcast** channel
   messages (a different protocol — `ciph_msg:1:bcast:<channel>:<content>`, no signatures,
   sender = the self-send address). Served from the **same webserver/host** as KaPosts, so
   the app's *Broadcast Indexer URL* is the same URL as the *KaPost Indexer URL*.
   - Fourteen curated channels are indexed — **`#kaspa`**, **`#kachat-bugs`**, and 12 language
     rooms (`kaspa-indonesia`, `kaspa-czech`, `kaspa-german`, `kaspa-espanol`, `kaspa-francais`,
     `kaspa-portugues`, `kaspa-slovak`, `kaspa-chinese`, `kaspa-japanese`, `kaspa-korean`,
     `kaspa-hebrew`, `kaspa-romania`); everything else on the bcast protocol is dropped (`BROADCAST_CHANNELS` in
     `k_protocol.rs`). The accent-free spellings are deliberate so names survive normalization.
   - Content is stored **verbatim** (text or reply/audio JSON envelopes) with a size cap
     (`MAX_BROADCAST_CONTENT_CHARS`); deduped by transaction id.
   - **Retention: 30 days** (matches the app's "All messages persist for 30 days" UI). Unlike
     KaPosts content (kept forever), broadcasts are pruned by a background task in the processor
     (`--broadcast-retention-days`, default 30); it deletes rows older than the window every hour.
   - Endpoint: `GET /get-broadcasts?channel=<name>&limit=<n>[&before=<blockTimeMs>]` →
     `{ messages: [{ txId, channel, senderAddress, content, blockTime }], hasMore }`,
     newest-first (matches the app's `BroadcastIndexerClient`).
   - Requires `simply-kaspa` to populate `addresses_transactions` (the `--disable` list drops
     `addresses_transactions_table` and `transactions_outputs` for KaPosts; the KAPOSTS
     compose re-enables them, bounded by the 1h staging retention).
   → `kachat-transaction-processor/src/{database.rs (trigger + kachat_broadcasts table),
   k_protocol.rs (process_broadcast)}`, `kachat-webserver` get-broadcasts, `kachat-admin` Broadcasts tab.

New on-chain payload/signing shapes (colon-joined, `k:1:` prefix; signature is Kaspa
personal-message schnorr over the signing string):

```
unvote  →  k:1:vote:<pubkey>:<sig>:<post_id>:unvote:<author_pubkey>     signs "<post_id>:unvote:<author_pubkey>"
unquote →  k:1:unquote:<pubkey>:<sig>:<content_id>                       signs "<content_id>"
```

`/get-post-engagement?postId=<txid>&type=<upvote|downvote|repost|quote|all>&requesterPubkey=&limit=&before=`
→ `{ "engagement": [{ "actorPubkey", "actionTxId", "timestamp", "kind" }], "pagination": {...} }`
(`actionTxId` is the action's txid, for explorer deep-links.)

## Prerequisites (already satisfied on this box)

- Docker + Docker Compose.
- A **mainnet** rusty-kaspa node with `--utxoindex` and BORSH wRPC on `0.0.0.0:17110`
  (the KaChat mining node already provides this).

## Run

```bash
cd K-indexer/docker/KAPOSTS
# review .env first — set a real DB_PASSWORD
docker compose up -d --build
```

Services (all `network_mode: host`; the bundled Portainer is dropped — one already runs on
:9000):

| Container | Purpose | Port |
|---|---|---|
| `k-indexer-db-kaposts` | PostgreSQL 17 | `${DB_PORT}` = 5442 |
| `simply-kaspa-indexer-kaposts` | feeds txs from the node | `${SKI_PORT}` = 8500 |
| `k-transaction-processor-kaposts` | parses/stores `k:1:` (marker-filtered) | — |
| `k-webserver-kaposts` | REST read API for the app | `${WEBSERVER_PORT}` = 3080 |
| `k-admin-kaposts` | ops/admin dashboard | `127.0.0.1:${ADMIN_PORT}` = 3081 (loopback only) |

Then:
- Admin dashboard → SSH tunnel: `ssh -L 3081:localhost:3081 <box>`, then `http://localhost:3081`
- Point the app at this indexer → Settings → Connection Settings → **KaPost Indexer** =
  `http://<box>:3080` (or the public `https://kaposts.duckdns.org` once proxied)

**Running it as the public default** (nginx-proxy-manager + TLS + DuckDNS): see
[`docker/KAPOSTS/DEPLOY.md`](docker/KAPOSTS/DEPLOY.md).

## Verify

```bash
# webserver up
curl -s http://localhost:3080/health

# a global feed page (needs a real 66-hex requesterPubkey)
curl -s "http://localhost:3080/get-posts-watching?requesterPubkey=<PUBKEY>&limit=10" | jq .

# per-post engagement (new)
curl -s "http://localhost:3080/get-post-engagement?postId=<TXID>&type=all&requesterPubkey=<PUBKEY>" | jq .

# admin stats / health
curl -s http://localhost:3081/api/stats  | jq .
curl -s http://localhost:3081/api/health | jq .
```

Backfill sanity (from the app's `KAPOSTS_INDEXER.md §6`): once caught up, quote tx
`f28587d7ac7ba1f8545e3b4f18dfc24f03160fa596feccbfb3da964272ca054b`
(quoting `cb60eea63d13ac668704670a0e843b0733be2a2123f4b2a864cc8605fe7ebdb9`) should be indexed,
and non-marker K-website posts should be absent.

## Notes

- **No DB migration** was required: the marker filter and removals use the existing tables,
  and the engagement endpoint is read-only.
- Moderation removal in the dashboard mirrors upstream `kachat-content-remover` (atomic
  delete-all-by-pubkey); k_hashtags cascade via FK.
- Pipeline health in the dashboard is derived from the `transactions` table freshness;
  per-container up/down status stays in Portainer.

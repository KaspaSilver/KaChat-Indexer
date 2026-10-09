# .kachat Domains server (standalone)

The `.kachat` names registry no longer needs the KaChat Indexer. Anyone can self-host exactly
what the KaChat apps read for names: a Kaspa node, a Postgres database, and this one container.

## What runs

Image: `docker/kachat-names/Dockerfile` (build context = repo root). Under supervisord:

| Process | Binary | Does |
|---|---|---|
| `names` | `kachat-names-follower` | Follows the registry covenants named by the manifest (`KACHAT_NAMES_MANIFEST`). Idles when no manifest is set. |
| `profiles` | `kachat-names-follower --profiles` | Address profiles (`kchat:1:profile:`), feeding `/profiles/*` and the profile half of `/identity/*`. |
| `webserver` | `kachat-webserver` with `KACHAT_WEBSERVER_ONLY=names` | Serves only the names API below, plus `/health` and `/metrics`. No KaPosts, chat, chess, translate or scheduled posts. |

There's no block ingester, chat indexer, KaPosts processor or admin. The image builds only
`kachat-webserver`, `kachat-names` and `kachat-names-follower`.

## API (same paths and shapes as the full indexer)

- `/names/status`, `/names/manifest`, `/names/prices`, `/names/expiring`, `/names/all`,
  `/names/grace`, `/names/activity`;
- `/names/by-owner/:address`, `/names/gap/:key`, `/names/:name`, `/names/:name/offers`,
  `/names/:name/history`;
- `/offers/by-buyer/:address`, `/market/listings`, `/market/activity`;
- `/profiles/stats`, `/profiles/history`, `/profiles/:address`;
- `POST /identity/batch`, `/identity/:address`.

A reverse proxy can put these paths on the apps' indexer domain, so the apps need no change.

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `KACHAT_NAMES_MANIFEST` | (empty) | Path of the verified manifest (`.kachat Domains` publishes it). Empty = names follower idle. |
| `NETWORK` | `mainnet` | `mainnet` or `testnet-10` (the profiles follower's address prefix). |
| `KASPA_NODE_WBORSH_URL` | `ws://kaspad:17110` | The node's wRPC Borsh endpoint. |
| `DB_HOST` `DB_PORT` `DB_NAME` `DB_USER` `DB_PASSWORD` | localhost / 5432 | Its own Postgres. The followers create their tables. |
| `WEBSERVER_PORT` | `3080` | API port. |
| `PUSH_INTERNAL_URL` + `INTERNAL_PUSH_SECRET` | (empty) | The push service's internal route, for name pushes. Empty = no pushes (a self-hoster has no push service). |
| `KACHAT_NAMES_REST_BOOTSTRAP` | `auto` | Rebuild from the Kaspa REST API when the node has pruned the start block. |
| `KACHAT_PROFILES_SCAN_FROM` | (empty) | First-run profiles start block; empty = the manifest's `scanFrom`, else the pruning point. |

## The full indexer

Unchanged. Its webserver still serves the same names routes when it has a manifest, so a stack
that has not moved still works. Kaspa Quick Start now hands the manifest only to this server and
routes the names paths to it.

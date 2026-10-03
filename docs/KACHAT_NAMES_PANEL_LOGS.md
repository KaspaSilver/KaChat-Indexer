# `.kachat` names: its own log, in the panel (2026-10-03)

**The ask.** The control panel (Kaspa-Quick-Start) gets a log view for the `.kachat` names
module:
- a log card in the **.kachat** tab;
- its own tile in **All logs**, next to the other containers.

The work is in two repos:
- **this repo**: a small logging contract, in §2;
- **Kaspa-Quick-Start**: the views, in §3.

---

## 1. Why it isn't there today

The names follower doesn't have a container of its own. It runs as the supervisord program
`[program:names]` inside the testnet indexer container `kaspa-node-kachat-testnet`, next to:
- the chat indexer;
- the KaPosts ingester and processor;
- the webserver;
- admin.

Every program writes to the container's stdout (`stdout_logfile=/dev/stdout`), so the panel only
sees one interleaved stream (the "kachat indexer (testnet)" tile). Supervisord adds no
per-program prefix there.

The fix is to filter that stream on a tag:
- Most follower lines already start with `[names]`. §2 makes that a guarantee.
- The panel shows the matching lines as their own log. §3 covers this.

The tag approach needs no new container, file or volume.

## 2. Indexer side: the `[names]` contract

Every log line about the names module starts its message with **`[names]`**, from every process
that logs one: the follower, plus the webserver's names module and its API.

Lines that don't follow the contract today:

| Where | Line | Change to |
|---|---|---|
| `kachat-webserver/src/names.rs:75` | `KACHAT_NAMES_MANIFEST did not parse; names module OFF` | `[names] manifest did not parse; module off` |
| `kachat-webserver/src/names.rs:77` | `names module ON (manifest loaded)` | `[names] module on (manifest loaded)` |
| `kachat-webserver/src/names.rs:86` | `... is not valid JSON; names module OFF` | `[names] manifest is not valid JSON; module off` |
| `kachat-webserver/src/names.rs:91` | `... unreadable; names module OFF` | `[names] manifest unreadable; module off` |
| `kachat-webserver/src/names_api.rs:96` | `names api: {e}` | `[names] api: {e}` |
| `kachat-names-follower/src/main.rs` (probe mode) | `[probe] ...` | `[names] [probe] ...` |
| `kachat-names-follower` `main` returning `Err` | Rust prints `Error: ...` untagged, then supervisord restarts it | catch it and `error!("[names] fatal: {e:#}")` before exiting non-zero, so a crash loop is visible in the names log |

`run-names.sh` already echoes `[names] no manifest configured; follower idle`. Keep that line:
it's what the panel shows while the module is off.

**Add a progress heartbeat.** About once a minute, and whenever `synced` flips, log:

```
[names] at DAA 586371110 (tip 587366389, 995279 behind, synced=false)
```

Use the same numbers as `/names/status`. Without it the log is silent during a long catch-up
or on a quiet registry, and reads as dead.

**Colour.** `tracing_subscriber`'s fmt layer writes ANSI colour codes by default. Either:
- set `.with_ansi(false)` in the follower; or
- leave it, because the panel strips `\x1b\[[0-9;]*m` before matching and showing lines (§3).

Doing both is fine.

Don't change log levels or volume for this. The tag is the only contract.

## 3. Panel side (Kaspa-Quick-Start)

### 3.1 A filtered log source

Today a log source is one container (`containerFor` in `manager/server.js`, `STACK_CONTAINERS`
in `manager/lib/dockerctl.js`). Allow a source to be **a container plus a line filter**:

```js
// dockerctl.js
export const NAMES_LOG_SOURCE = {
    key: 'kachat-names',
    label: '.kachat names',
    name: 'kaspa-node-kachat-testnet',
    match: '[names]',   // keep lines whose ANSI-stripped text contains this
};
```

- **`GET /api/logs?container=kachat-names`.** Read a deep tail of the container and keep the last
  N matching lines. Use about 5000 lines, the existing cap: names lines are a small share of
  that container's output, so a plain `--tail 300` would come back nearly empty.
- **`GET /api/logs/stream?container=kachat-names`.** Same follow as today, filtered line by line,
  with the ANSI codes stripped.
- If `kaspa-node-kachat-testnet` doesn't exist, the source doesn't exist: the same rule the
  listing applies to containers.

### 3.2 All logs: its own tile

`/api/logs/stream-all` keeps its followers in a `Map` **keyed by container name**, and tags lines
with `key`. A second tile on the same container (full `kachat-testnet` plus filtered
`kachat-names`) would collide in that map.

Key the followers by the **source `key`**, so one container can feed two tiles. Then:
- add `NAMES_LOG_SOURCE` to the list `stream-all` and `/api/logs/containers` scan, placed right
  after `kachat-testnet`;
- the filtered follower drops non-matching lines before `send('line', …)`;
- the existence check, the restart re-attach (`startedAt`) and the `containers` signature all
  work per source, exactly as today.

The full "kachat indexer (testnet)" tile stays as it is. The names lines still appear there
too.

### 3.3 The .kachat tab: a log card

Add a card to the .kachat tab like the Kaspad tab's `sub-kaspadlog` card:
- `<pre class="logview" data-logview="names">`;
- the same zoom − / + buttons and auto-scroll checkbox;
- the same front-trimming so it can't grow without limit.

Open the `EventSource` on `/api/logs/stream?container=kachat-names` when the tab is shown, and
close it when the tab is left. The kaspad log's open/close logic does exactly this; copy it.

What it should show:
- **Module off:** the `[names] no manifest configured; follower idle` line, and nothing else.
- **No testnet indexer:** a short note in the card, rather than an empty box.
- **Running:** registry and resume lines, the heartbeat, one line per registry event
  (`[names] register alice tx …`), reorgs, push sends, and fetch warnings.

## 4. Done when

1. With the module on, the .kachat tab's log card follows the names lines live. Nothing from
   chat, KaPosts or the webserver's other routes appears in it.
2. All logs shows a ".kachat names" tile next to "kachat indexer (testnet)", with the same
   filtered lines. The full testnet tile is unchanged.
3. Restarting Indexer-testnet re-attaches both tiles, as for every container today.
4. With the module off, the card shows the idle line. A follower that crashes on start shows
   `[names] fatal: …` on every restart.
5. During catch-up the heartbeat shows the DAA climbing toward the tip about once a minute.
6. A machine with no testnet stack shows neither the tile nor an error.

## 5. Notes

- Mainnet later: the same tag works when the module runs in the mainnet container too. Give that
  source its own `key`, e.g. `kachat-names-mainnet`.
- `/names/status` stays the thing the app and the panel's status card read. The log is for
  people.

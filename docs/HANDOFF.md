# HANDOFF — KaChat indexer + `.kachat` names + testnet stack

Current state and next steps, so a fresh session (any device, from a clone) can continue
without the chat history. Last updated after commit `bacb014` on `KaspaSilver/KaChat-Indexer`.

## Repos

| Repo | Path (dev box) | What it is |
|---|---|---|
| **KaChat-Indexer** | `/home/vahome/kachat-indexer` | the indexer workspace (this repo) |
| **Kaspa-Quick-Start** | `/home/vahome/Kaspa-Quick-Start` | the KQS control panel / stack (node, mining, indexer, proxy, testnet) |
| vsmirn0v/KaChat | `/home/vahome/KaChat` | iOS app (reference, read-only). Holds the spec docs. |
| KaspaSilver/kachat-domains | `/home/vahome/kachat-domains` | `.kachat` covenant contracts, artifacts, **live manifest**, CLI + test vectors |

Everything described below is **committed and pushed**. Clone the two KaspaSilver repos to continue.

### KaChat-Indexer workspace layout
- `kachat-transaction-processor` — payload parser (KaPosts/public-chats → Postgres), driven by `pg_notify` from the upstream `simply-kaspa-indexer`.
- `kachat-webserver` — content REST API (:3080). Has the `.kachat` names module **foundation** (`src/names.rs`).
- `kachat-admin` — admin API (:3081): `/api/moderation/recent` (paginated), deletes.
- `kachat-names` — **pure `.kachat` follower engine** (codec + transitions + applier + undo + loop). NEW this effort.
- `kasia-indexer/` — the chat/push indexer (fjall, :8600); separate inner workspace. Holds the no-handshake §5 work.

## Build / test
```bash
cd /home/vahome/kachat-indexer
cargo test -p kachat-names        # 29 tests, all green — the follower engine
cargo build -p kachat-webserver   # includes the names module foundation
# chat/push indexer:
cd kasia-indexer && cargo test -p protocol -p indexer-db -p indexer-actors
```

---

## Part 1 — No-handshake messaging (KaChat 5.2) — DONE, deployed

`NO_HANDSHAKE_MESSAGING.md` §5 (indexer side). Fully implemented, tested, pushed, and
**verified live** on the running indexer (`/contextual-messages/by-inbox` returns 200, etc.).
iOS + Desktop + Android all carry it; the inbox-tag algorithm matches byte-for-byte
(`SHA-256("kachat-inbox:v1:"+lowercased-address)[..16]`). Nothing left here.

Key commits: `beca5ad` (§5.1-5.3,5.5 + personal mode), `e79f3c4` (§5.4 push), `68fe646` (test).

---

## Part 2 — Testnet runs beside mainnet (Kaspa-Quick-Start) — DONE (deployable)

A full **parallel testnet-10 stack** alongside mainnet, opt-in. In `Kaspa-Quick-Start`:
- `docker-compose.yml`: `kaspad-testnet`, `bridge-testnet`, `kachat-db-testnet`, `kachat-app-testnet` behind `testnet*` profiles; own volumes/hostnames; indexer points at `kaspad-testnet` borsh `:17210`; `NETWORK=testnet-10`; mounts `conf/names` + `KACHAT_NAMES_MANIFEST_TESTNET`.
- `manager/`: testnet node args (`writeTestnetArgsFile` → `--testnet --netsuffix=10`), lifecycle units `node-testnet`/`kachat-testnet`/`mining-testnet`, testnet bridge config.
- Panel UI: a **Mainnet/Testnet** switch under Overview; the Testnet view re-points the shared Kaspad/Mining/Indexer switches + health dots at the `*-testnet` units.
- A **`.kachat`** nav tab (testnet-only) — configures the names manifest, shows `/names/status`.

**Deploy:** panel → "Update the control panel" (+ "Update" the indexer). Then Testnet view →
install/start Kaspad-testnet, then Indexer-testnet. Needs a TN10 node (own or public, gRPC 16210).

---

## Part 3 — `.kachat` names registry — follower engine DONE, live wiring REMAINS

Spec: `docs/KACHAT_NAMES_INDEXER.md` + `docs/KACHAT_NAMES_UPDATE_2026-10-02.md` (the update
**wins** on any conflict). The `.kachat` registry is a set of Kaspa **covenant** contracts
(KachatGap / KachatName / KachatOffer) the indexer follows; served via `/names/*`.

### Live facts (testnet-10)
- Registry id: `9444187f09a3e77450e125d448b21eb79b3c54b692a5b3f3e8af38343b9a7a51`
- Genesis tx: `cba68dd1b07f374410270f1e609a3e71deaf42d3bd3b5849ac9b0e9cc687f45f`, DAA 585,767,203
- Manifest: `kachat-domains/manifests/kachat-names-testnet-10.json` (has per-contract
  prefix/suffix/stateSpan/dispatchTags/templateHash, params, `registryCovenantId`,
  `genesis.txid`, `genesis.scanFrom`). Artifacts in `kachat-domains/artifacts/testnet10/`.

### DONE — the pure engine (`kachat-names` crate, 29 tests)
- **Codec** (`src/lib.rs`): `name_key`=blake3, `p2sh_script`=BLAKE2b-256 `aa 20 <hash> 87`
  (matches rusty-kaspa `pay_to_script_hash_script`), `num8` (8-byte LE sign-magnitude),
  minimal-push/script-number parsing, `decode_sig_script`→{args,tag,redeem}, Gap/Name/Offer
  state decode, commitments, name validation.
  **Validated byte-for-byte against the LIVE genesis output** (`reproduces_live_testnet_genesis_gap_output`).
- **Transitions** (`src/transition.rs`, §B3): `entry_for_tag` (12 pinned tags),
  register/transfer/list/buy/renew/offer_accept/merge, `name_status` (active/grace/lapsed).
- **Applier** (`src/ingest.rs`, §B): `Registry::apply(templates, tx)` — finds spends of tracked
  UTXOs, decodes the entry, computes the new state, **P2SH-verifies against a real output before
  recording** (never tracks the unverified). Offer markers (§B4) + profiles (§C). Emits `Event`s.
- **Undo journal** (`src/ingest.rs`, §4.1/B6): `undo_block` reverses a removed block's txs
  exactly; `prune_journal` drops past finality.
- **Follower loop** (`src/follower.rs`, §3): `ChainSource` trait + `Follower::step` (seed
  genesis/scanFrom → undo reorg → apply accepted → advance checkpoint → prune). Tested with a
  fake source (register then reorg).
- **Profiles** (`src/profile.rs`, §C as of `c180259`): `parse_profile` — `avatar`/`banner`/`bio`
  are **source links** with **per-field allowlists** (banner: X/YouTube/Discord only; bio adds
  Telegram/Twitch/Kick/GitHub; avatar all 11), + `linktr.ee` + `primaryName`. No free text.

### Webserver foundation (`kachat-webserver/src/names.rs`) — DONE
- `GET /names/status`, `/names/manifest`, `/names/:name` (validates → 503 while syncing).
- Loads the manifest from `KACHAT_NAMES_MANIFEST` (reads the live `genesis.txid` shape).
- Panel config: `GET/PUT /api/names/config`, `GET /api/names/status` (manager proxy).

### REMAINING — the live-node integration (validate against a running TN10 node)
In the handoff's priority order (§5 of the update doc):

1. **Real `ChainSource`** — a wRPC (or gRPC) client to a Toccata-capable node
   (rusty-kaspa 2.0.1/2.1.0, `--utxoindex`) calling
   `getVirtualChainFromBlockV2(start, includeAcceptedTransactions=true)` from
   `manifest.genesis.scanFrom`, mapping each accepted tx (inputs: previous_outpoint +
   signature_script; outputs: scriptPublicKey + amount + covenant binding; payload;
   accepting block hash + DAA) into `kachat_names::ingest::Tx`. Response also lists removed
   chain blocks → `VccBatch.removed_blocks`. Slots straight into `Follower`.
   - A registry tx = spends a tracked UTXO (its continuation/output carries `registryCovenantId`),
     OR a `kchat:1:offer:` payload matching an output (B4), OR spends a tracked offer. Profiles
     come from `kchat:1:profile:` payloads on a self-send.
2. **Postgres persistence** — swap the in-memory `Registry`/checkpoint for tables (gaps, names,
   offers, profiles, history) + a checkpoint row. Build `ingest::Templates` from the manifest's
   artifacts (prefixHex/suffixHex/stateSpan).
3. **Part D read API** (webserver), priority: `/names/{name}`, `/names/by-owner/{address}`,
   `/names/gap/{keyHex}` → `/identity/{address}` + `POST /identity/batch` + `/profiles/{address}`
   → `/names/{name}/offers`, `/offers/by-buyer`, `/market/listings`, `/market/activity`,
   `/names/expiring`, `/names/{name}/history`. Amounts are **sompi strings**; names without `.kachat`.
4. **Self-test** (§4.2): periodically `getUtxosByAddresses` to confirm each served row's outpoint
   is still unspent; withhold a row the chain refutes. Only report `synced:true` once caught up.
5. **Part A**: a testnet-10 deployment of the whole indexer (Part 2 stack); send the app owner its
   base URL. The app switches from its own walker to the indexer once
   `GET {indexer}/names/status` returns 200 with the manifest's `registryCovenantId`.
6. **Part E push**: name_offer / name_sold / name_offer_accepted / name_expiring / name_grace
   (reuse the existing push registrations, routed by `primaryAddress`).

### Test vectors
`kachat-domains` has a `kachat-names-vectors` CLI (28 txs + codec vectors). Generating it needs
the `silverc` compiler (the offer artifact bakes the registry id), which isn't in this env — so
the codec was instead validated against the **live genesis output**. If you get `silverc`
(github.com/kaspanet/silverscript @ 3ed9733) and generate a vectors file, point
`KACHAT_NAMES_VECTORS` at it and `matches_generated_vectors` will assert against it.

### Guardrails
- **dotk-indexer (github.com/supertypo/dotk-indexer) is AGPL** — use its *ideas* (undo journal,
  self-test, checkpoint) in your own words only; **do not copy/port its code**.
- Deploy only via the KQS panel "Update" (push to `main`; the user clicks Update). Don't rebuild
  the container image directly.
- A reclaim bot stays **off** and needs the app owner's explicit go-ahead.
- Never handle private keys in plaintext or execute transfers.

## Open tasks (status)
- DONE: no-handshake §5; testnet parallel stack; `.kachat` tab; names codec/transitions/applier/
  undo/loop; profiles (per-field).
- TODO (task #28/#29 follow-on): wRPC `ChainSource` + Postgres persistence + Part D API +
  self-test + Part A testnet deploy + Part E push — all validated against a live TN10 node.

## Next concrete step
Bring up the testnet stack + a TN10 node, then write the wRPC `ChainSource` + Postgres
persistence and watch real `.kachat` registrations flow into `/names/*`, validated live.

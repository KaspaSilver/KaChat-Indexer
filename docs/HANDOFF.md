# HANDOFF — KaChat indexer + `.kachat` names + testnet stack

Current state and next steps, so a fresh session (any device, from a clone) can continue
without the chat history. Last updated 2026-10-02 after `a2438df` on `KaspaSilver/KaChat-Indexer`.

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

### DONE — live follower + read API (2026-10-02, `c742233`, `a2438df`)
- **`kachat-names-follower`** (new bin): wRPC Borsh `getVirtualChainFromBlockV2` (High
  verbosity, `--min-confirmations` hold-back) from the checkpoint or `genesis.scanFrom` →
  the tested `Follower` → Postgres (`names_state`, `names_utxos`, `names_history`,
  `names_profiles`), one transaction per batch. Self-test via `getUtxosByAddresses`: a row
  missing twice is `refuted` and withheld. `synced` = within ~1 min of virtual DAA **and** a
  clean self-test. `--node-url resolver` (public node), `--probe` (in memory, no DB).
- **Read API** (`kachat-webserver/src/names_api.rs`): every Part D endpoint in the exact
  app shapes (`docs/KACHAT_NAMES_APP_CONTRACT.md` §2). All 503 until the follower is synced,
  fresh, and on the manifest's registry; `/names/status` reports `registryCovenantId` only then.
- **Image**: builds the follower; supervisord `names` program (`run-names.sh`) idles unless
  `KACHAT_NAMES_MANIFEST` names a file. The webserver now depends on `kachat-names`.
- **Panel** (Kaspa-Quick-Start `96fa4b0`): target kind `kachat-testnet` publishes the testnet
  indexer on its own name with the mainnet route split / private 404s / CORS (Testnet view).
- **Tests**: 31 engine tests incl. all 28 iOS-vector transactions replayed exactly
  (`vector_replay.rs`, vectors at `KaChat/KaChatTests/KachatNamesVectors.json`); follower and
  API helper tests against the builder's p2pk vector and the live manifest.

### REMAINING
1. **Postgres end-to-end** (follower writes → API reads) — not yet run against a real DB.
2. **Deploy on testnet**: Update the indexer + panel, start Kaspad-testnet + Indexer-testnet,
   set the manifest in the `.kachat` tab, publish the testnet indexer (Proxy & domains,
   Testnet view), send the URL to the app owner.
3. **Part E push** (name_offer / name_sold / name_offer_accepted / name_expiring / name_grace).
4. **Android call ring**: `/v1/push/ring` is APNs-VoIP only; needs an Android FCM handler first.

### Test vectors
The generated vectors ship in the iOS repo (`KaChat/KaChatTests/KachatNamesVectors.json`) and
are read from there by default (or `KACHAT_NAMES_VECTORS`); `matches_generated_vectors` and
`vector_replay` both run against them.

### Guardrails
- **dotk-indexer (github.com/supertypo/dotk-indexer) is AGPL** — use its *ideas* (undo journal,
  self-test, checkpoint) in your own words only; **do not copy/port its code**.
- Deploy only via the KQS panel "Update" (push to `main`; the user clicks Update). Don't rebuild
  the container image directly.
- A reclaim bot stays **off** and needs the app owner's explicit go-ahead.
- Never handle private keys in plaintext or execute transfers.

## Open tasks (status)
- DONE: no-handshake §5; testnet parallel stack; `.kachat` tab; names engine (codec/transitions/
  applier/undo/loop/profiles), vector replay, live follower, Postgres store, Part D read API,
  self-test, testnet publish target.
- TODO: Postgres end-to-end run; testnet deploy + URL to the app owner; Part E push; Android
  call ring (needs the app's FCM handler first).

## Next concrete step
Run the follower + webserver against a real Postgres (and the testnet node), then deploy on
the KQS testnet stack and publish it; the app switches to the indexer on its own once
`/names/status` reports `synced` with the registry id.

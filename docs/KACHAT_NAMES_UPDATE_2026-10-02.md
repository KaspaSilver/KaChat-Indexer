# `.kachat` names - update for the indexer (2026-10-02)

This builds on `docs/KACHAT_NAMES_INDEXER.md`, the full handoff. That copy is the current
version, synced from the app repo; when the two disagree, the copy here wins over anything older.

It covers:
1. What changed since 2026-10-01.
2. A review of what is already built (`kachat-names` crate + `kachat-webserver/src/names.rs`).
3. The follower: the one design decision that must be made correctly.
4. Ideas worth taking from the open-sourced dotk-indexer, **without its code**.
5. The order of work to finish testnet.

---

## 1. What changed since 2026-10-01

### 1.1 Profiles: one social link, plus Linktree (replaces Part C's old format)

The profile record (`kchat:1:profile:<json>`, a self-transfer) now has exactly three fields:

```json
{ "v": 1, "social": "https://x.com/name", "linktree": "https://linktr.ee/name", "primaryName": "alice" }
```

- **`social`** is a profile link on one of these platforms, stored normalized by the app:

  | Platform | Normalized form |
  |---|---|
  | X | `https://x.com/<handle>` |
  | YouTube | `https://www.youtube.com/@<handle>` or `/channel\|c\|user/<id>` |
  | Facebook | `https://www.facebook.com/<handle>` |
  | Instagram | `https://www.instagram.com/<handle>/` |
  | TikTok | `https://www.tiktok.com/@<handle>` |
  | Twitch | `https://www.twitch.tv/<handle>` |
  | Kick | `https://kick.com/<handle>` |
  | GitHub | `https://github.com/<handle>` |
  | Telegram | `https://t.me/<handle>` |
  | LinkedIn | `https://www.linkedin.com/in\|company/<id>` |
  | Discord server invite | `https://discord.gg/<code>` |

  Drop anything else.
- **`linktree`** must be `https://linktr.ee/<name>`. Drop anything else.
- **Old fields are not part of the format.** `avatar`, `banner`, `bio` and `links` are dropped,
  even if an older client writes them.
- **Never fetch pictures or bios.** The app looks up the avatar, banner and bio from the social
  profile on each device and caches them there. That way the platform's own moderation applies
  to all three. The indexer stores and serves the two links as strings, nothing more.
- Everything else in Part C is unchanged:
  - only a self-transfer from the address counts;
  - the newest record wins, as a full replacement;
  - 2 KB maximum;
  - `primaryName` is honored only while the address owns that name and it is active.

### 1.2 The testnet-10 registry is live (unchanged since your last commits)

- Registry `9444187f09a3e77450e125d448b21eb79b3c54b692a5b3f3e8af38343b9a7a51`.
- Genesis tx `cba68dd1b07f374410270f1e609a3e71deaf42d3bd3b5849ac9b0e9cc687f45f`, accepted at
  DAA 585,767,203.
- The manifest is `manifests/kachat-names-testnet-10.json` in KaspaSilver/kachat-domains.
- Its `genesis.scanFrom` block is a safe place to start following.
- The test vectors now hold **28** transactions. The new one is `cancel commit`: an owner
  spending an unused commit back to themselves. It is not a registry transaction, carries no
  payload and has no covenant binding, so the follower should ignore it.

### 1.3 The app already works on testnet without the indexer

- **Chain walker:** the iOS app follows the registry itself, through `api-tn10.kaspa.org`
  `/addresses/{p2sh}/full-transactions`, verified against a node. It can register, renew, list,
  buy, transfer, release and reclaim on its own.
- **What it needs the indexer for:**
  - offers that **other people** made on your names (offer outputs are plain P2SH, only
    discoverable through the payload marker of Part B4);
  - other people's profiles (records are payloads, and nodes prune those after about 30 hours);
  - name history beyond what the phone saw;
  - the name push events (Part E).
- **When the app switches to the indexer:** once `GET {indexer}/names/status` answers 200 with
  the manifest's `registryCovenantId`, the app uses the indexer instead of its walker. Until then,
  answering 503 from the lookup endpoints is exactly right.

---

## 2. Review of what is built

Done and matching the handoff:
- **`kachat-names` codec:** blake3 keys, BLAKE2b P2SH, `num8` state integers, minimal-push
  arguments. Validated against the CLI vectors and the live genesis output. Good.
- **Transition engine (§B3) and registry applier (§B).** Good.
- **Webserver foundation:** `/names/status`, `/names/manifest`, `/names/:name` (503 while
  syncing), and the manifest reader for the live `genesis.txid` shape.

Still to do (Parts B-E):
- **The follower** (section 3 below): accepted transactions from the node into the applier, with
  reorg undo.
- **Tables:** gaps, names, offers, profiles, history.
- **The rest of Part D:**
  - `/names/by-owner/{address}`
  - `/names/gap/{keyHex}`
  - `/names/{name}/history`, `/names/{name}/offers`
  - `/offers/by-buyer/{address}`
  - `/market/listings`, `/market/activity`
  - `/names/expiring`
  - `/profiles/{address}`, `/identity/{address}`, `POST /identity/batch`
- **Part E push events.**
- **Part A:** a testnet-10 deployment of the whole indexer. No testnet config exists yet.

---

## 3. The follower: read the node, not the ingest database

The ingest that fills Postgres (`docker/kachat/app/run-ingest.sh`, simply-kaspa-indexer) runs with:
- `--disable=virtual_chain_processing,transaction_acceptance,...,transactions_inputs`
- `--exclude-fields=...,tx_in_previous_outpoint,tx_in_signature_script,...,tx_out_amount`
- `--retention=1h`

Every one of those is something the names module needs:
- **Signature scripts:** each spend's redeem script, dispatch tag and arguments (Part B3).
- **Previous outpoints:** which registry UTXO a transaction spends.
- **Output amounts:** offer values, sale payouts, the reclaim bond.
- **Acceptance and virtual-chain order:** reorg-safe ordering.
- **Covenant bindings:** a Toccata field the ingest does not store at all.
- **History older than an hour.**

Turning those fields back on would grow the shared database for everyone. So **follow the node
directly** in the names module, the way the `kachat-domains` CLI scanner and dotk-indexer both do:

- **Connection:** a wRPC (or gRPC) client to the same node. It needs `--utxoindex` and a
  Toccata-capable rusty-kaspa: the vectors use rev `a41a333`, and TN10 nodes run 2.0.1 / 2.1.0.
- **Call** `getVirtualChainFromBlockV2(startHash, includeAcceptedTransactions = true)` at high
  verbosity. Each accepted transaction then carries:
  - its inputs with previous outpoints and signature scripts;
  - its outputs with amounts, scripts and covenant bindings;
  - its payload.

  The response also lists the removed chain blocks (reorgs).
- **Start** at the manifest's `genesis.scanFrom`, then from your own stored checkpoint.
- **Which transactions are registry ones:**
  - those whose registry-covenant output (output 0 for a spend of a gap, the continuation for a
    name entry) carries `registryCovenantId`, **and** which spend a tracked UTXO;
  - plus transactions whose payload is a `kchat:1:offer:` marker that matches one of their
    outputs (Part B4);
  - plus spends of tracked offer outpoints.
- **Profiles** (`kchat:1:profile:`) can come from the same stream (payload plus the sender's
  inputs). They need the previous outpoint's address, the sender resolution the chat module
  already does.

---

## 4. Ideas from dotk-indexer (reference only, no code)

dotk.name open-sourced their `.k` indexer (github.com/supertypo/dotk-indexer).

**Its license is AGPL-3.0, with extra terms:**
- a "powered by dotk-indexer" credit is required;
- the name "dotk" can't be used.

**Do not copy, port or translate its code** into this repo: that would put this indexer under
the AGPL. Read it for design only. These ideas are worth implementing in your own words:

1. **An undo journal for reorgs.**
   - Each applied registry event stores the **previous state** of every row it changed: the
     name row, the gap rows it split or merged, offer rows.
   - The rows are keyed by block hash and sequence number.
   - When the node reports a removed chain block, those events are undone newest first,
     restoring the stored states exactly.
   - Prune the journal below a safe depth (finality).
2. **A self-test against the node.**
   - Every gap and name row implies a P2SH address: the script of `prefix ‖ state ‖ suffix`.
   - Periodically, and only while caught up with the chain, ask the node
     (`getUtxosByAddresses`) whether each served row's outpoint is still unspent at that address.
   - A row the node refutes is a bug: log it loudly, stop serving it (answer 503 or withhold
     that row), and re-derive it.
   - The guiding rule: *never serve a fact the chain can refute.*
3. **A checkpoint plus resume.** Store the last processed chain block, and resume from it on
   restart. A snapshot export (all rows plus the checkpoint) is optional, for bootstrapping a
   new instance quickly.
4. **Optional: a reclaim bot.**
   - `reclaim` pays the caller the freed gap value (about 1 TKAS, minus fee), and pays the bond
     back to the old owner.
   - An optional task could reclaim lapsed names, after `expiresAt + 10 days`, so they free up
     promptly.
   - It needs a funded key and a reviewed transaction builder. Off by default, and **ask the app
     owner before enabling it**.

---

## 5. Order of work to finish testnet

1. **The follower** (section 3), with the undo journal and checkpoint (section 4, items 1 and 3).
2. **Tables plus Part D read endpoints**, in this priority:
   1. `/names/{name}`, `/names/by-owner`, `/names/gap`
   2. `/identity/{address}` + `POST /identity/batch` + `/profiles/{address}`, with the profile
      ingest in the new format (section 1.1)
   3. `/names/{name}/offers`, `/offers/by-buyer`, `/market/listings`, `/market/activity`,
      `/names/expiring`, `/names/{name}/history`
3. **The self-test** (section 4, item 2). Only report `synced: true` in `/names/status` once
   caught up.
4. **Part A:** a testnet-10 deployment of the whole indexer. Send the app owner its base URL,
   and the app's testnet profile will point at it.
5. **Part E:** push events (offer received, sold, offer accepted, expiring, grace).
6. **End-to-end check:** register, renew, list, buy, offer and accept, release and reclaim in the
   iOS app on testnet. Each step should appear in `/names/{name}/history` and agree with the
   `kachat-domains` CLI's `status`.

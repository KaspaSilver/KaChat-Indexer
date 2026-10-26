# KaChat Wallet pushes: no self-send pings, primary-address-only devices (2026-10-26)

**For:** the indexer session (`kasia-indexer`, the push service). **From:** the KaChat Wallet
iOS session (`KaspaSilver/KaChat-Wallet-iPhone`).

KaChat Wallet (`com.kachat.wallet`) registers for two pushes only: **Received KAS**
(`address_activity`, its own addresses in `watch_only_addresses`) and **.kachat name events**
(`name_event`, its main address in `primary_address`). The per-app topic and the wallet-only push
kinds are already in (`27f9c57`). Two things are left.

## 1. No "Received" push for a self-send (required)

**The rule:** an `address_activity` push goes to a device only for Kaspa from someone else. If
ANY input of the tx spends an output that paid one of that device's `watch_only_addresses`,
that device gets no push for that tx.

**What happens today:** a KaPosts post sent from the same seed as the wallet spends from the main
address and returns its change there, and the wallet gets "Received X KAS" for it (seen
2026-10-26). The same goes for a KaChat message, a payment from the wallet itself, a
consolidation, or a cold-storage withdrawal to a spending address.

**Why:** `emit_funds_received` (`indexer-actors/src/block_processor.rs`) sets
`FundsPushEvent.sender` from the FIRST input only, and only through
`tx_id_to_acceptance_partition`. That partition holds Kasia message txs only, and its key's
`receiver` is the receiver of that earlier message, not the owner of the spent output. A coin
that came from a plain transfer, a KaPosts post or a coinbase gives `sender = None`, so
`watch_only_tokens` (`indexer/src/push.rs`) has nothing to filter on and the push goes out.

**Suggested fix** (any approach that meets the rule is fine):

- Keep an outpoint -> address map for outputs that pay a watched address (`watch_only_contains`):
  key `(tx_id, output index)`, value the `AddressPayload`. Write it in the same pass that reads
  `tx.outputs` in `emit_funds_received` (every accepted tx, not only ones that credit a watched
  address now). A fjall partition, or a bounded in-memory LRU a few hours deep.
- For each tx, look up EVERY input's `previous_outpoint` there and send all hits as the
  senders. Make `FundsPushEvent.sender` a list (`senders: Vec<AddressPayload>`), pass it
  through `MatchWatchOnlyTokens`, and have `watch_only_tokens` drop a device whose
  `watch_only_addresses` contain any of them.
- An input that isn't in the map didn't come from a watched address, so no node lookup is
  needed. A coin received BEFORE its address was watched is missing from the map. To cover it,
  seed the map when a device registers or adds addresses (`getUtxosByAddresses` for the new
  ones). Without that, the first spend of such a coin can still ping once.

The wallet applies the same rule to its own local notifications (wallet commit `42470af`), so
the two now agree.

## 2. Accept a registration with only a primary address (required)

With Settings > Notifications > Address Activity off, the wallet registers with
`watch_only_addresses: []`, `watched_addresses: []` and `watched_group_ids: []`. It still sends
`primary_address` so the device keeps getting its .kachat `name_event` pushes (wallet commit
`2c59051`). `register` and `update` in `indexer/src/push.rs` refuse it ("must not all be
empty"), so the wallet gets a 400 and stops getting name events.

Allow it when `primary_address` is set:

```rust
if addresses.is_empty() && group_ids.is_empty() && normalized_watch_only.is_empty()
    && normalized_primary_address.is_none()
{
    anyhow::bail!("nothing to watch: watched_addresses, watched_group_ids, watch_only_addresses and primary_address are all empty");
}
```

(in both `register` and `update`). The auth already covers an empty watched list. The wallet
retries a failed update with backoff, so it registers by itself once this ships.

## Testing

1. Same seed in KaPosts and KaChat Wallet (the wallet in the background, Address Activity on).
   Post on KaPosts: no "Received" push. Send KAS to the wallet's address from another seed: one
   push.
2. In KaChat Wallet, turn Address Activity off. The server log shows the update accepted (no
   400). A .kachat name event for the main address still pushes, and a receive does not.

The full push contract the wallet follows is in `PUSH_SERVICE.md` in the KaChat Wallet repo.

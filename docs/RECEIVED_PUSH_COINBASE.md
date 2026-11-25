# One "Received" push per block found, not one per merging block (2026-11-25)

**For:** the indexer session (`kasia-indexer/indexer-actors/src/block_processor.rs`,
`emit_funds_received`; the `address_activity` push). **From:** the K iOS session (owner report:
"when I find a block it pings me multiple times even though I only find one block").

> **Status:** open.

## What happens

`handle_transaction` calls `emit_funds_received` for every transaction of every block it sees,
including each block's coinbase, and its own comment says it fires "on block sighting ... not
strictly on acceptance". A block a miner finds is merged by many later blocks, and **every** one of
them carries a coinbase transaction (each with its own tx id) paying that miner for it. Only the
coinbase of the chain block that merges it is accepted. So one block found sends one
`address_activity` push per merging block: several "Received 1.94 KAS" for one reward. The
dispatcher's tx-id dedup can't catch it: the tx ids differ.

## What to change

1. **Coinbase transactions** (subnetwork id `0100000000000000000000000000000000000000`): don't push
   on block sighting. Push one only when the coinbase is **accepted** (the virtual chain's
   accepted transactions, which the acceptance partition already records), so a miner hears once
   per reward that actually counts.
2. Better still, for every transaction, not just coinbase: push on acceptance. A transaction reorged
   out today can still push a false "Received". If that costs too much latency for ordinary
   payments, doing (1) alone fixes this report.

## The app side (done, not yet committed)

K's own catch-up (on opening the app) announced every recent transaction paying the address,
accepted or not; it now skips `is_accepted == false`. K's live path already skipped coinbase.
Pushes, which arrive while K is closed, are the indexer's alone: K's notification extension
doesn't run for `address_activity`.

Add a Status note here with the commit when done.

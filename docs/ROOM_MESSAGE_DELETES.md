# Public chat rooms: a sender deletes their own message for everyone (2026-11-25)

**Status:** done, commit 0816402 (transaction processor). Deletes are recorded in
`kachat_room_deletes` and applied in either arrival order; the delete row stays in the room
history; no push; KaPlay's rooms take none (d65265a). The webserver needed no change. The
indexer has no panel-marked machinery rooms beyond KaPlay's three. Live after the next kachat image rebuild.

**For:** the indexer session: the transaction processor that writes room (broadcast) messages, the
webserver's room history read, and room pushes (`notify_broadcast`).
**From:** the K iOS session (`KaspaSilver/K-iPhone`; `MessageDeleteCodec` in Models.swift,
`PublicChatService.deletePublicChatMessage` / `applyIncomingDeletes`).

## What K sends

A room message whose content is exactly this JSON is a **delete for everyone**:

```json
{"type":"delete","targetTxId":"<64-hex txid of the message to delete>"}
```

- It is sent in the **same room** as the message it deletes, as an ordinary room message, like the
  room edit envelope `{"type":"edit","targetTxId":...,"text":...}`.
- The sender is the transaction's input address, as for every room message. The tx's signature is
  the authority, the way KaPost's `delete` action is signed by its author.

## What the indexer should do

On ingest of a room message whose content parses as above:

1. **Never in the machinery rooms.**
   - `chess-arena`, `chess-bots` and `kplay-arcade` (and any room the panel marks as machinery)
     take no deletes: a move, a result or a stamped score can't be taken back.
   - K ignores deletes there too.
2. **Only from the same sender.**
   - Look up `targetTxId` in that room. If it exists and its `sender_address` equals the delete's
     sender, remove the message.
   - Also remove the reactions and edits that target it.
   - Anything else is ignored: a different sender, another room, or an unknown target.
3. **A delete that arrives before its target.**
   - This happens with out-of-order blocks or a backfill.
   - Keep the delete (txid, target, sender) and apply it when the target is ingested.
4. **The delete row itself.**
   - Keep it in the history the room read serves, so other clients that cached the message drop
     it too. K drops it either way.
   - It is never shown as a message: K filters it like edits and reactions.
5. **No push for a delete:** skip `notify_broadcast` for it, as for edits and reactions. K's push
   extension also silences one if it comes, but a skipped push saves the device a wake-up.
6. **Moderation is unchanged:** the admin delete (`kachat-admin`) still removes any message. This
   adds the sender's own delete.

## What K does meanwhile

- **On the sender's phone:** the message goes once the delete is sent; nothing changes if it isn't.
- **On every K:**
  - a delete seen in a page, a sweep or the block stream removes the cached message, but only from
    the same sender;
  - it is remembered, so a page that still carries the message (before this change is deployed)
    can't bring it back.
- **Once the indexer removes the message**, K's absence check also drops it everywhere: a cached
  message missing from two pages that cover its time is deleted (`dropRowsTheIndexerNoLongerHas`).
  That also covers clients that never saw the delete row. The check refuses to delete more than a
  handful at once (over 3, and over a fifth of what a page covers), so a rebuilt or partial index
  leaves K's caches alone; keep the 30-day history whole when you can.

So this works in K today, without the indexer change, for anyone who sees the delete. The indexer
change makes it complete:
- the message stops being served to new readers and other clients;
- the absence check cleans up everyone else.

Add a Status note here with the commit when done.

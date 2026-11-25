# Public chat rooms: a sender deletes their own message for everyone (2026-11-25)

**Status:** done, commit 0816402 (transaction processor). Deletes are recorded in
`kachat_room_deletes` and applied in either arrival order; the delete row stays in the room
history; no push. The webserver needed no change. Live after the next kachat image rebuild.

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

1. **Only from the same sender.**
   - Look up `targetTxId` in that room. If it exists and its `sender_address` equals the delete's
     sender, remove the message.
   - Also remove the reactions and edits that target it.
   - Anything else is ignored: a different sender, another room, or an unknown target.
2. **A delete that arrives before its target.**
   - This happens with out-of-order blocks or a backfill.
   - Keep the delete (txid, target, sender) and apply it when the target is ingested.
3. **The delete row itself.**
   - Keep it in the history the room read serves, so other clients that cached the message drop
     it too. K drops it either way.
   - It is never shown as a message: K filters it like edits and reactions.
4. **No push for a delete:** skip `notify_broadcast` for it, as for edits and reactions. K's push
   extension also silences one if it comes, but a skipped push saves the device a wake-up.
5. **Moderation is unchanged:** the admin delete (`kachat-admin`) still removes any message. This
   adds the sender's own delete.

## What K does meanwhile

- **On the sender's phone:** the message goes at once. If the delete transaction never leaves the
  phone, the message comes back.
- **On every K:**
  - a delete seen in a page, a sweep or the block stream removes the cached message, but only from
    the same sender;
  - it is remembered, so a page that still carries the message (before this change is deployed)
    can't bring it back.
- **Once the indexer removes the message**, K's absence check also drops it everywhere: a cached
  message missing from a page that covers its time is deleted (`dropRowsTheIndexerNoLongerHas`).
  That also covers clients that never saw the delete row.

So this works in K today, without the indexer change, for anyone who sees the delete. The indexer
change makes it complete:
- the message stops being served to new readers and other clients;
- the absence check cleans up everyone else.

Add a Status note here with the commit when done.

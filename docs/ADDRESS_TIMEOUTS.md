# Address timeouts

A 24-hour block on one address. The owner asked for it on 2026-11-25.

**While the timeout runs:**
- Nothing the address sends is stored, exactly as for an address on the
  [blocklist](ADDRESS_BLOCKLIST.md): KaPosts, public chats, game stamps, profiles, and the chat
  indexer's direct chats, handshakes, payments and group messages.
- What it sends during the timeout is never stored, even after the timeout ends.
- Messages sent *to* the address are not affected.

**When the timeout starts**, what the address posted publicly in the last 24 hours (by block
time) is deleted:
- **KaPosts** (matched by the address's key):
  - posts, replies and quotes, with their polls, translations, mentions and hashtags;
  - votes, follows, tips, poll votes and profile broadcasts.
  - An older post it edited in those 24 hours gets the text of its last earlier edit back. If
    there is no earlier edit, the post is deleted, because an edit overwrites the original text.
  - Blocks and deletes stay.
- **Public chats and game stamps:** room messages, reactions and edits, including KaPlay's rooms.
- **Profiles:** the profile records. The shown profile goes back to the last earlier record; with
  none, it is removed.

Deleted posts and room messages are tombstoned (`kachat_removed_txs`), so a reindex never stores
them again. Its private chats are not deleted. Pushes already sent stay sent.

## Admin API (kachat-admin, behind the panel's `/kachat/api` proxy)

```
GET  /api/timeouts
  -> { "timeouts": [ { "address": "kaspa:q…", "note": "spam", "started_at": 1795665556368,
                       "expires_at": 1795751956368, "purged": 23 } ] }
     running timeouts only, newest first; times in ms; purged = rows removed so far

POST /api/timeouts/add     { "address": "kaspa:q…", "note": "optional" }
  -> { "address": "kaspa:q…", "started_at": …, "expires_at": …,
       "purged": { "kaposts": n, "public_chats": n, "games": n, "profiles": n }, "total": n }
  400 { "error": "…" }      not a valid address, or not this indexer's network

POST /api/timeouts/remove  { "address": "kaspa:q…" }
  -> { "deleted": 0 | 1 }
```

- **Re-adding a running timeout** restarts it: the 24 hours count from now again, the note is
  replaced, and the last 24 hours are purged again.
- **Removing a timeout** ends it early. What was deleted does not come back.
- **Ended timeouts** drop out of the list. Admin clears them within 30 s.
- **Counting:** `kaposts` counts rows, including the polls, translations and mentions that went
  with a post.
- **Second pass:** each process rereads the list about every 15 s, so admin runs the purge again
  20 s after the add. That pass adds to `purged` and is logged.
- **Blocklist overlap:** an address can be on the blocklist and in a timeout at once; the
  blocklist wins. Blocklisting an address purges everything it ever sent; a timeout only purges
  its last 24 hours of public posts.

## Propagation

- The list is the `kachat_address_timeouts` table.
- The processor and the profiles follower read it together with the blocklist every 15 s, and
  only use rows whose `expires_at` is still in the future.
- The chat indexer's `blocked_addresses.txt` lists running timeouts next to the blocklist. Admin
  rewrites it on add, on remove, and when a timeout ends.

# Address blocklist

The panel's **Indexer > Manage > Block**. Nothing a blocked address sends is stored, and adding an
address purges what is already stored from it.

## Admin API (kachat-admin, behind the panel's `/kachat/api` proxy)

```
GET  /api/blocklist
  -> { "addresses": [ { "address": "kaspa:q…", "note": "spam bot", "added_at": 1764000000000, "purged": 1234 } ] }
     newest first; purged = rows removed by every add of this address so far

POST /api/blocklist/add    { "address": "kaspa:q…", "note": "optional" }
  -> { "address": "kaspa:q…",
       "purged": { "kaposts": n, "public_chats": n, "chats": n, "groups": n, "profiles": n, "games": n },
       "total": n,
       "warning": "…" }      warning only when the chat indexer could not purge (see below)
  400 { "error": "…" }       not a valid address, or not this indexer's network (kaspa: / kaspatest:)

POST /api/blocklist/remove { "address": "kaspa:q…" }
  -> { "deleted": 0 | 1 }
```

- The address is stored in canonical form (lowercase). The note is trimmed and cut to 200
  characters.
- Adding an address that is already listed updates its note and purges again. This also clears
  anything stored in the seconds before every process reread the list.
- `chats` and `groups` count transactions. The other figures count rows.
- `warning`: the address is listed and blocked anyway, but its chat messages were not purged
  (the chat indexer was down or refused). Adding it again retries the purge.
- Removing an address only stops blocking. Purged data does not come back. KaPosts can be
  rebuilt with the KaPosts reindex, which skips addresses that are still blocked.

## What is blocked, and where

| Kind | Stored by | Matched on | Blocked at |
|---|---|---|---|
| KaPosts: posts, replies, quotes, votes, follows, blocks, tips, polls, edits, deletes, broadcasts | transaction processor | the author's x-only key, taken from the address | the same gate as the KaPosts denylist |
| Public chats, chess (`#chess-arena`, `#chess-bots`), arcade (`#kplay-arcade`) | transaction processor | the verified sender address | before the `kachat_broadcasts` insert, so there is no push either |
| Profiles | profiles follower | the address the profile is stamped to | before the `profile_saves` and `names_profiles` writes |
| Direct chats (comm/dm), handshakes, payments, self-stash, group messages and controls | chat indexer | the sender address | see below |

How the chat indexer blocks:
- At the store gate, when the sender is known. A group message is matched by the sender key it
  carries, so it is caught even before the sender is known.
- When a pending sender resolves: the record is dropped instead of completed.
- On the history import.
- In push delivery, as a backstop.

The `.kachat` names live in the separate names stack and are not covered.

## Propagation

- The list is the `kachat_address_blocklist` table.
- The processor and the profiles follower reread it every 15 s.
- The chat indexer rereads `/app/data/blocked_addresses.txt` (`KASIA_BLOCKLIST_FILE`) every
  15 s. Admin rewrites that file on every change and at start.
- On add, admin calls the chat indexer's `POST /personal/purge-sender` (`x-internal-secret`).
  That call blocks the address at once and returns `{"chats": n, "groups": n}`.
- Messages sent *to* a blocked address are not affected.
- The existing `/api/kaposts/denylist` (keyed by pubkey) is unchanged.

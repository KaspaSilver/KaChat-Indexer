# Word filter

Keeps public chats and KaPosts family friendly: a message containing a listed word is not stored,
so no app reading this indexer shows it. The owner asked for it on 2026-11-25.

**What it covers:**
- **Public chats:** the text of a room message, a reply (and the message it quotes) or an edit.
  Reactions, media and KaPlay's game rooms are not read.
- **KaPosts:** posts, replies, quotes, edits, polls (question and options) and profile broadcasts
  (nickname and message).
- **Not covered:** direct and group chats (encrypted, the chat indexer), profiles, `.kachat`
  names.

The message is still on the Kaspa chain: other indexers and apps can show it.

## Matching

- Not case-sensitive. Accents are dropped (é → e).
- Common letter swaps are undone: `0`→o `1`→i `3`→e `4`→a `5`→s `7`→t `@`→a `$`→s.
- Stretched letters match: "fuuuck", "shiiit", "asss".
- **Whole words only:** `ass` does not match "class" or "assistant".
- **A trailing `*` matches any ending:** `fuck*` matches fucks, fucking, fucker.
- **Several words match as a phrase:** `kill yourself`.
- Numbers, hex ids, addresses and words over 40 characters are skipped.

The matcher is `kachat-transaction-processor/src/word_filter.rs`. kachat-admin compiles the same
file, so a purge finds exactly what the processor would drop.

## The list

- **Table:** `kachat_word_filter (word, added_at)`.
- **Default list:** `kachat-transaction-processor/src/word_filter_default.txt`, 168 entries
  reviewed by the owner (profanity, sexual words, slurs, hate and violence phrases). The owner
  allowed `damn`, `crap` and `cracker`.
- **Seeding:** the processor copies the default list into the table once, on the first start
  that finds no `word_filter_seeded` key in `k_vars`. From then on the table is the list, even
  when emptied.
- **Reload:** the processor rereads the list and the switch every 15 s.

On the live data (2026-11-25), the default list catches 31 of 3,493 stored KaPosts and none of
1,143 room messages. Of those, 2 are "homo sapiens", 1 is `xxx` used as a placeholder token
name, and 1 is "a bit naked" about a book.

## Admin API (kachat-admin, behind the panel's `/kachat/api` proxy)

```
GET  /api/word-filter
  -> { "enabled": true, "words": [ { "word": "fuck*", "added_at": 1795645803744 } ] }
     alphabetical; enabled = the feature_word_filter switch

POST /api/word-filter/add     { "words": "scam*\nrug pull, another" }
  -> { "added": 2, "invalid": ["!!!"] }
     one entry per line or comma; stored trimmed, lowercase, single spaces; at most 1000 per call
     and 64 characters each; added = entries new to the list

POST /api/word-filter/remove  { "word": "rug pull" }
  -> { "deleted": 0 | 1 }

POST /api/word-filter/purge   { "dry_run": true }
  -> { "dry_run": true, "public_chats": n, "kaposts": n, "total": n,
       "matches": [ { "tx_id": "…", "kind": "public_chat" | "kapost", "channel": "kaspa",
                      "word": "fuck*", "preview": "…", "timestamp": 1795… } ] }
     the stored messages the list catches now, newest first (up to 50 listed). Without dry_run they are
     deleted and tombstoned (kachat_removed_txs), so they are never stored again. Works with the
     switch on or off.
```

`GET/POST /api/settings` has the switch: `feature_word_filter` (boolean, default on; partial POST,
as with `feature_kaplay`). Off = new messages are not checked; the list is kept.

- Adding a word does not touch stored messages. The panel offers the purge preview after a change.
- Removing a word does not bring purged messages back.

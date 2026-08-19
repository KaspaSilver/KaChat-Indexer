# KaChat — KaPost @mention notifications (CLIENT spec)

**Audience:** the AI/engineer building the KaChat clients (iOS, Android, desktop).
**TL;DR:** the indexer does **not** parse `@text` or call KNS. Mentions are **client-resolved** — you
put the mentioned user's **pubkey** into the post's existing `mentioned_pubkeys` field, and the
indexer turns that into a `contentType: "mention"` notification in `get-notifications`. That's the
whole contract.

The server side is done (the indexer now labels top-level post mentions as `"mention"`). The rest is
client work described below.

---

## 1. The model (important — differs from a "server parses @" assumption)

KaPosts carry an on-chain payload of the form:

```
post:<sender_pubkey>:<sender_signature>:<base64_message>:<mentioned_pubkeys_json>
reply:<sender_pubkey>:<sender_signature>:<post_id>:<base64_message>:<mentioned_pubkeys_json>
```

- `<mentioned_pubkeys_json>` is a JSON array of **hex pubkeys**, e.g. `["<pubkey_hex>", …]`.
- **You (the client) resolve `@domain → owner pubkey`** and put those pubkeys here. You already do
  this resolution for the `@` autocomplete (contacts that have a KNS domain), so you already have the
  pubkey — just include it.
- The **signature covers** `<...>:<base64_message>:<mentioned_pubkeys_json>` (mentions are inside the
  signed message), so compute the signature over the mentioned_pubkeys you include. (This already
  works today for existing mention/hashtag flows — match that.)

The indexer stores each `mentioned_pubkey` and, for **top-level posts**, exposes it as a `"mention"`
notification to that pubkey's owner. **No KNS or `@`-parsing happens server-side.**

Two server-side behaviors to know:
- **kchat-only:** @mention notifications fire **only for posts written under the `kchat:1:` prefix**.
  A legacy `k:1:` post still indexes, but its `mentioned_pubkeys` are ignored (no mention rows). So
  ship mentions together with — or after — the `kchat:` write migration.
- **Parity-safe:** the server matches mentions by the **x-only** coordinate (ignores the 02/03
  parity byte), so deriving the mention pubkey from an address as `02 + x-only` is always correct —
  odd-parity (`03…`) users still receive their mentions.

> Why not server-side KNS? The client already resolves for autocomplete; doing it again in the
> indexer would add a network dependency to block ingestion and duplicate work. Resolution stays
> client-side, at post time.

---

## 2. What the client must do to SEND a mention

When the user writes `@alice` (or `@alice.kas`) in a **new post** (or reply):

1. Resolve the domain to its owner pubkey via KNS (strip a trailing `.kas` for the lookup; treat
   `@alice` and `@alice.kas` as the same domain). You already do this for autocomplete.
2. Add that **pubkey (hex)** to the post's `mentioned_pubkeys` array (dedupe; skip the author's own
   pubkey — self-mentions are also filtered server-side, but don't bother sending them).
3. Sign + broadcast as usual. Done.

Parsing rules for extracting `@domain` from the text are **client-side** and must match your own
renderer/autocomplete (the indexer never sees the plaintext rules). Reference regex you already use:
`/(^|[\s([{<"'])@([a-z0-9-]+(?:\.[a-z0-9-]+)*)/gi`.

A mention whose domain doesn't resolve → just don't add a pubkey for it (it renders as plain text).

---

## 3. What the client RECEIVES (get-notifications)

`GET /get-notifications?requesterPubkey=<pubkey>&limit=100&before=<cursor>` — unchanged endpoint.
A mention now appears as a normal notification row with **`contentType: "mention"`**:

```json
{
  "id": "<post txid>",
  "userPublicKey": "<author pubkey — who mentioned you>",
  "contentType": "mention",
  "voteType": null,
  "contentId": "<post txid — tap opens this post>",
  "postContent": "<base64 of the post payload (marker included) for preview>",
  "timestamp": 1710000000000
}
```

Render it as "<name> mentioned you in a post" and deep-link `contentId` to the post. This flows
through the **same** notifications list as `vote`/`reply`/`quote`/`follow` — just handle the new
`contentType: "mention"`.

**Server-side guarantees (you can rely on these):**
- **Self-mention filtered:** the author never gets a mention row for their own post.
- **Blocked users filtered:** if you blocked the author, you won't get the row.
- **De-duped:** one row per (post txid, mentioned pubkey); re-ingesting the post won't duplicate.

---

## 4. Scope / edge cases

- **Top-level posts → `contentType: "mention"`.** This is the fully-supported case (matches the
  acceptance tests: `"gm @alice"` → alice gets a `mention`).
- **Replies:** a reply's `mentioned_pubkeys` currently drives the existing **`reply`** notification
  (that's how "someone replied to your post" already works). So an `@mention` placed in a *reply*
  surfaces to the mentioned user as `contentType: "reply"`, not `"mention"` — the indexer can't tell
  a reply-target from an in-reply `@mention` (both are just pubkeys in `mentioned_pubkeys`). If you
  need reply-`@mentions` to read as `"mention"` distinctly, tell us — it requires the client to
  signal *which* pubkeys are `@mentions` vs the reply target (a payload/format change), so we'd scope
  that separately. For v1, mention = top-level post.
- **Quotes:** a quote that references your post surfaces as `contentType: "quote"` (unchanged).

---

## 5. Acceptance (mirrors the server tests)

With A = KNS `alice`, B = KNS `bob`, and B including A's pubkey in `mentioned_pubkeys`:

1. B posts `"gm @alice"` → A's get-notifications has `{contentType:"mention", userPublicKey:B, contentId:<B post txid>}`. ✅
2. B posts `"gm @alice.kas"` → same (you resolved the same domain → same pubkey). ✅
3. `"email me a@b.com"` → you don't add a pubkey (not a mention per your regex) → no notification. ✅
4. `@nonexistentdomain` → doesn't resolve → you don't add a pubkey → no notification. ✅
5. `"@alice @alice hello"` → dedupe to one pubkey → server also de-dupes → one mention. ✅
6. A posts `"note to self @alice"` → self-mention, filtered server-side (and you can skip sending). ✅
7. Re-broadcast/re-ingest of the same post txid → server de-dupes → no duplicate. ✅

---

## 6. One-line summary for each platform

> To notify someone with `@`, resolve the domain to its pubkey (you already do this for
> autocomplete) and include that pubkey in the post's `mentioned_pubkeys`. The indexer returns it as
> `contentType: "mention"` in `get-notifications`. Don't expect the server to parse `@text` or call
> KNS.

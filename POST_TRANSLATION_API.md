# Post Translation API (`/translate`)

Server-side translation of KaPosts, served by the KaPosts webserver on the **same host and base URL
as the KaPost Indexer** (default `https://kachat.duckdns.org`; in the app: Settings → Connection
Settings → **KaPost Indexer**). No new client setting and no new domain. The clients
(`PostTranslationService` on iOS/Android) are the source of truth for the contract; this documents
what the server implements so the client teams can confirm the match.

Because a KaPost is **immutable**, a translation of `(txid, target)` is correct permanently. The
server caches it and serves every later reader of that post/language from the cache.

## `POST /translate`

Request:
```json
{
  "target": "en",
  "posts": [
    { "id": "1943b508…64hex", "text": "hola mundo" }
  ]
}
```
- `target` — **required**. Bare BCP-47 primary language subtag (`en`, `pt`, `zh`), lowercased. A
  region (`pt-BR`) is reduced to its primary subtag (`pt`).
- `posts` — **required**, 1–50 entries (shipping clients send exactly one).
- `posts[].id` — the post's transaction id (64 hex). **Optional**, but strongly preferred: with an
  id the server translates its own verified copy and caches the result.
- `posts[].text` — source text (marker already stripped). Optional when `id` is present; see the
  cache-poisoning rule below.

Response `200`:
```json
{
  "translations": [
    { "id": "1943b508…", "source": "es", "target": "en", "text": "hello world", "cached": true }
  ]
}
```
- `source` — detected source language (bare subtag). Client localizes the name ("Translated from …").
- `text` — the translation. When `source == target`, the input is returned unchanged with
  `"untranslated": true` (detection is a guess; echoing beats a failure banner).
- `cached` — observability only; clients may ignore it.
- Order is not significant; match on `id` (or on position when no id was sent).

**Per-entry failure does not fail the batch** — that entry carries an `error`+`code` and no `text`:
```json
{ "id": "6068917e…", "error": "Unsupported language pair.", "code": "UNSUPPORTED_PAIR" }
```

Whole-request errors use the standard KaPosts shape `{ "error": …, "code": … }` with HTTP 400/429.

**Error codes:** `MISSING_PARAMETER`, `INVALID_POST_ID`, `TOO_MANY_POSTS`, `TEXT_TOO_LONG`,
`UNSUPPORTED_PAIR`, `RATE_LIMITED`, `TRANSLATION_FAILED`.

## `GET /translate/languages`
```json
{ "source": ["ar","de","en","es","fr","ja","ko","pt","ru","zh"],
  "target": ["ar","de","en","es","fr","ja","ko","pt","ru","zh"] }
```
Bare subtags. A client may use this to avoid offering a link it knows will fail; fetch it at most
once per launch and fall back to "offer anyway" if unavailable. (The set is whatever the operator
has loaded — see `LT_LOAD_ONLY` below.)

## Server rules (as implemented)
- **Cache key `(txid, target)`**, immutable → no TTL, no invalidation.
- **Cache-poisoning rule:** the server caches **only its own verified copy** of a post (read from the
  indexer's `k_contents`, marker stripped). If the post is **not** held here, the request-supplied
  `text` is translated and returned but **never cached** under the txid.
- **Length cap** 25 000 chars (`TEXT_TOO_LONG`).
- **Detection** runs on URL/@mention-stripped text; the full text is what gets translated/returned
  (no trimming, whitespace-collapsing, or HTML-escaping).
- **Privacy:** no identity — the endpoint takes **no** `requesterPubkey`/auth/account id (the only
  KaPosts endpoint that must not). The server does not log request bodies or post text. Per-IP rate
  limit ≈ 60 requests or 600 posts / minute → `RATE_LIMITED`.

## Backend / operator notes
- Engine: self-hosted **LibreTranslate** (`docker/kachat/compose.yaml`), reachable by the webserver
  at `LIBRETRANSLATE_URL` (default `http://127.0.0.1:5000`). Languages preloaded via `LT_LOAD_ONLY`
  (default `en,es,pt,fr,de,ru,zh,ja,ko,ar,vi`). The contract is engine-agnostic — DeepL/Google could be
  swapped behind it without touching clients.
- Cache table `post_translations (post_id BYTEA, target_lang, source_lang, text, created_at)`,
  created by the processor's schema init.

## Not yet implemented (planned follow-ups)
- **`language` field on posts** (`get-posts`/`get-replies`) so clients can drop on-device
  language detection. Until then, clients keep their local detection.
- **Cache warming** (pre-translating posts before anyone asks).
- **Chain-read fallback** for posts outside the indexer's window (currently: supplied text is
  translated but not cached).

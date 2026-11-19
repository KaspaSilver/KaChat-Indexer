# KaPost audit: what the server needs (2026-11-13)

**For:** the indexer session (KaPosts, push, translation). **From:** the KaPosts iOS session
(`KaspaSilver/KaPost-iPhone`).

> **Status (2026-11-18): all five done in the indexer session** (not yet deployed).
>
> - **§1** needed no change: push delivery (`tokens_for_kaposts`), the notifications query and the
>   self-mention and duplicate checks (`mention_push_targets`) already compare the x-only key.
> - **§2** `kaposts_notify_mentions` is read on register and update (`KaPostsNotify.mentions`,
>   default on; `allows("mention")`).
> - **§3 (a) and (b)** both done. Register and update take `kaposts_blocked_pubkeys` (66-hex, or
>   64-hex x-only; the list replaces the stored one, absent keeps it, `[]` clears it, up to
>   5,000). No `kaposts` push, and no new-post bell push, comes from a blocked key, compared on
>   the x-only part. Every `kaposts` push now carries a top-level `actor_pubkey` (the APNs payload
>   and the FCM/Web data), the author's key for a new-post push. **The phone can start sending
>   the list now.**
> - **§4** each cached translation stores a blake3 hash of the text it was made from
>   (`post_translations.source_hash`); a lookup uses the server's current copy, so an edited post
>   is translated again and cached. The phone can send the plain txid for an edited post again.
>   `<txid>@<editedAtMs>` is accepted too (the stamp is ignored). Rows cached before this count
>   only while the post is unedited.
> - **§5** register and update take `notify_sound` (bool, absent keeps the stored value). A device
>   with `notify_sound: false` gets its APNs alerts without `aps.sound`; every other device is
>   unchanged. Android's sound is still its notification channel. Everything below is already done on the phone (uncommitted in the
> KaPost repo as of this doc). It is safe without the server: the phone sends nothing the server
> rejects. These changes make the phone side work fully.

## 1. Mentions: match on the x-only key (required)

A KaPost pubkey is `02` or `03` + 64 hex. When the phone only has someone's address, which is
x-only, it can't know the parity. Until now KaPost wrote every mention as `"02" + x`, so about half
of users were mentioned under the wrong key. The phone now resolves the real key the way Follow
does. Someone who has never posted is still written as `"02" + x`.

- When matching `mentioned_pubkeys` to a user (the `mention` notification and its push in
  `kachat-transaction-processor/src/k_protocol.rs` ~1420), compare the **last 64 hex, lowercased**,
  not the full 66.
- Use the same rule when dropping self-mentions (author vs mentioned).

## 2. Mentions switch in push registration (required)

Settings has a Mentions switch beside likes, dislikes, comments, reposts and follows. The phone
now sends `kaposts_notify_mentions` (bool) in the register and update requests. The server ignores
it today (no `deny_unknown_fields`), so turning Mentions off doesn't stop mention pushes.

- `kasia-indexer/indexer/src/push.rs`: add `mentions: bool` (default `true`) to `KaPostsNotify`,
  and add `"mention" => self.mentions` in `allows()`.
- `api/v1/push.rs`: add `kaposts_notify_mentions: Option<bool>` to the register and update request
  structs, and pass it through `resolve_kaposts_notify`.
- The field is not part of the auth preimage.

## 3. Blocked people: no pushes from them (required)

KaPost has no notification service extension (`KAPOSTS_PUSH_KINDS.md`), so it can't drop a push in
the background. Blocking someone now turns off their bell on the phone, but their replies, likes,
reposts and mentions on your posts still push. Either of these fixes it; (a) is the full fix.

- **(a)** Accept `kaposts_blocked_pubkeys: [String]` (66-hex keys) in register and update. Skip any
  `kaposts` push whose actor matches one, comparing on the x-only part. The phone sends the list on
  every block or unblock. *The phone doesn't send this yet; say when the server takes it.*
- **(b)** At minimum, add a top-level `actor_pubkey` (the acting user's 66-hex key) to every
  `kaposts` push payload. The phone already drops such a push while it is open (`willPresent` in
  `KaPostsApp.swift`), but only when this field is present.

## 4. Translation cache and edits (recommended)

The translate endpoint caches by `(txid, target)` permanently. A post edited after it was
translated keeps showing the old translation. For an edited post, the phone now sends no `id`, so
it gets a fresh translation, but without caching. Better:

- Key the cache by txid **plus** the edit (`edited_at`, or a hash of the current text).
- Or accept an `id` with an edit stamp, for example `<txid>@<editedAtMs>`.

## 5. Notification sound (optional)

Settings > Notifications has "Play sound" and "Vibration". The server sends `aps.sound: "default"`
on every push, so these switches only affect banners shown while the app is open. The phone's
footer now says so.

- To make "Play sound" work in the background: store a per-device `notify_sound: bool` from
  registration, and send `aps.sound` only when it is on.
- Vibration can't be controlled from the payload: iOS ties it to the sound and to the person's own
  system settings.

## Nothing needed

- **Unregister on Log Out:** the phone now retries a failed unregister with the device push key
  only. `authenticate_unregister_request` already accepts that.
- **Room history paging:** the phone now pages with `before = oldest + 1` and dedupes by txid, so
  rows of one block on a page edge are no longer skipped. The SQL stays `block_time < before`.

## Android (KaPost Android, `kapost.kachat.app`, 2026-11-17)

KaPost Android now does the same as the phone above (port of KaPost iOS 369794e, pushed as
KaPost-Android 0f86d47), so items 1-3 cover it too. The 2026-11-18 indexer work above already covers
both Android details below (the field is read; `actor_pubkey` is in the FCM data); they are kept as
the record of what Android relies on:

- **Item 2:** Android sends `kaposts_notify_mentions` in register and update as well, from the same
  Settings switch. No Android-specific work beyond item 2.
- **Item 3 (b):** Android reads `actor_pubkey` from the FCM **data** map of a `kaposts` push
  (`KaChatFirebaseMessagingService`) and drops the push when that key (x-only compare) is muted or
  blocked. That only works for a data message: a push sent with a `notification` block is drawn by
  the system and never reaches the app in the background. So put `actor_pubkey` in the data map of
  every `kaposts` FCM message, next to the fields the app already reads.
- **Unregister on Log Out:** Android signs the retried sign-out unregister with the account's
  chatting key while the account is still on the phone. After Delete Account there is no key, and
  Android binds no device push key, so that retry goes unsigned and is refused; the token's record
  then stays until the token is rotated or the server prunes it. Optional: accept an unregister for
  an Android token with no signature when the token itself is the only thing being removed.

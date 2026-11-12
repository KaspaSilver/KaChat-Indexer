# Room voice notes: one push per note, not one per part (2026-11-11)

**For:** the indexer session (`kachat-transaction-processor`, the broadcast push). **From:** the
KaPosts iOS session (`KaspaSilver/KaPost-iPhone`).

> **Status (2026-11-11): open.**

## The problem

KaPost (`com.kachat.kaposts`) now sends KaChat's long voice notes in Public Chats rooms. A note
longer than 14.5 s is split into parts, up to 12, each its own ordinary room message, one
transaction each, sent a few seconds apart. Every part is a normal file/audio envelope, so
`broadcast_preview` turns each one into "Voice message" and `notify_broadcast` pushes each one.
A 2:50 note rings a room's watchers **12 times**.

KaPost has no notification service extension (see `KAPOSTS_PUSH_KINDS.md`), so the phone can't
hold back the later parts itself. While the app is open it lists them quietly, but in the
background or closed iOS shows every push as it arrives. Room messages are plaintext, so the
indexer can see the part tag and is the one place this can be fixed.

## 1. Push only the first part of a note to arrive (required)

**The rule:** a broadcast whose content is a tagged voice part rings only when it is the first
part of its note the indexer has seen. Later parts of the same note save as usual (room history
is unchanged) but send no push.

**What a part looks like** (`content` after `kchat:1:bcast:<channel>:`), the same media-file
JSON as any voice message with one extra key between `mimeType` and `content`:

```json
{"type":"file","name":"voice-2of12.webm","size":30807,"mimeType":"audio/webm",
 "voicePart":{"id":"3f9a…(32 hex)","index":2,"count":12},
 "content":"data:audio/webm;base64,…"}
```

**Read the tag defensively** (the same rules every client uses):

- `type` is `file`, `mimeType` starts with `audio/`.
- `voicePart` is an object with a string `id` of 16 to 64 hex characters (compare lowercased),
  and integers `count` in 2...12 and `index` in 1...count.
- Anything else, or no `voicePart` key, means a plain voice message: push it as today.

**The note key:** (channel, sender address, lowercased `id`, `count`). Parts of one note always
share it. A different sender reusing an id is a different note.

**First to arrive, not `index == 1`:** parts go out in index order, so part 1 is almost always
first. But a part that failed and was retried can land after the others, and pushing only
`index == 1` would then leave the note silent. Keep a small in-memory set of note keys already
pushed, expiring each entry after ~15 minutes (a whole note is out within a minute or two;
retries come later but rarely past that). Check and insert in the push path. If the set is lost
on a restart, one extra push for a note mid-send is fine.

**Where:** `k_protocol.rs`, the broadcast save path (~line 1370), beside the existing
`is_reaction_content` / `is_edit_content` checks before `notify_broadcast`. A helper in
`push_notify.rs` next to those, for example `voice_part_key(content) -> Option<(String, u32)>`
returning the lowercased id and count, keeps the parsing testable. The body stays
"Voice message".

**What not to change:**

- Saving and serving room history: every part is still a message the room shows (the app
  joins them into one bubble).
- The push for an untagged voice message, or for a part with a malformed tag.
- `apns-collapse-id` (still the tx id of the part that rang).

## 2. Nothing for 1:1 or groups

KaChat's 1:1 and group voice parts are encrypted, so the server can't see their tag. KaChat's
own notification extension handles those (KaChat `CLAUDE.md`, "Parts arrive as bare txIds").

## Where the format is defined

KaChat iPhone `MESSAGING.md`, "Long voice notes (multi-part)": the tag, the parsing rules, and
"only the first part to arrive bumps unread and rings". KaPost's room numbers (14.5 s parts, up
to 2:50 for 0.976 KAS) are in KaPost's `CLAUDE.md`, "Voice notes in rooms", and
`VoiceNotePolicy.publicChat` in `KaPosts/Models/Models.swift`.

## App side

KaPost iPhone sends the parts (`PublicChatService.sendPublicChatVoiceParts`), shows them as one
bubble (`PublicChatVoiceParts`), and lists later parts quietly while open
(`RoomVoicePushBursts`, `KaPostsApp.swift`). Not yet committed in the KaPost repo as of this doc.

## Testing

- Unit tests for the tag reader: a valid part returns its key; index 0, index > count, count 1,
  count 13, a 10-character id, a non-hex id, a boolean or string `index`, an image `mimeType`,
  and no `voicePart` key each return none.
- Two parts with the same key: the first pushes, the second doesn't. The same id from another
  sender or another channel pushes. After the entry expires, the key pushes again.
- On a room with the bell on: send a 30 s note from KaPost (3 parts) and expect one push
  ("Voice message") on a watching device. A 10 s note (one untagged message) still pushes.

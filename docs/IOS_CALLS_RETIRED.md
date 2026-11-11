# iPhone KaChat has no calls any more: stop keeping and ringing its VoIP tokens (2026-11-11)

**For:** the indexer session (`kasia-indexer`, the push service). **From:** the KaChat iOS
session (`KaspaSilver/KaChat-iPhone`).

KaChat for iPhone removed voice and video calls on 2026-11-11 (app commit "Remove calling; long
voice notes and photo quality tiers"). The app no longer registers with PushKit, never calls
`POST /v1/push/ring`, and leaves `voip_token` out of `/register` and the update request
entirely. The rest of the registration, and everything the auth signature covers, is unchanged.

## The problem

`push.rs` keeps the stored VoIP token whenever a request leaves it out, on both register and
update:

```rust
let normalized_voip_token = requested_voip_token
    .or_else(|| existing.as_ref().and_then(|reg| reg.voip_token.clone()));
```

So an iPhone that had calls and then updates to this build keeps its old VoIP token on the
server forever. `voip_tokens_for_primary_address` still returns it, and a `/ring` from another
client (KaChat Android still has calls) sends a VoIP push to an app that no longer handles them.
iOS drops or penalises VoIP pushes that never turn into a call, so this needs to stop.

## 1. An iPhone registration without `voip_token` clears it

The calling iPhone builds sent `voip_token` on every register and every update once PushKit had
given them one (it was loaded from the Keychain at launch). So a missing `voip_token` from an
Apple device now means "this build has no calls", not a partial update.

In `register` and `update` (`push.rs`, the two `normalized_voip_token` lines):

- For `platform` `ios` or `macos`: use the requested value only, so a missing value clears the
  stored token (`let normalized_voip_token = requested_voip_token;`).
- For every other platform: keep today's behaviour (missing keeps the stored value).

Clearing also makes the device's `reg.voip_token != normalized_voip_token` comparison see a
change, so the registration is rewritten.

One edge case is accepted: an old calling build that registers before PushKit has handed it a
token clears its stored token, then puts it back on its next update. It's briefly unringable,
the same as a fresh install.

## 2. Optional: a one-off cleanup

Devices that updated but haven't re-registered yet keep their tokens until their next
register/update, and the app re-registers on launch, so most clear within a day. If you want
it immediate, you can do a one-time pass at startup or behind an admin action. There's no field
that tells a calling build from the new one apart from what the next registration sends, so
don't clear every iOS token blindly: that would also unring people still on an older build.

## 3. `/v1/push/ring` stays

Keep the endpoint: KaChat Android (and older iPhone builds) still use it. Once §1 has cleared a
device's token, `voip_tokens_for_primary_address` simply skips it (it already `continue`s on
`None`), so a ring to an updated iPhone sends nothing. Answer as today when there are no VoIP
targets.

## What the app does with call messages now

A `call_*` chat message from another client (Android or an older iPhone) is still a normal chat
message to the push service. Keep sending its ordinary push: the iPhone's notification extension
recognises it and shows nothing, and the app drops it, so it never becomes a bubble or an unread.

## Nothing else changed for the server

The same app commit also added multi-part voice notes and photos (more, smaller chat
transactions) and moved group voice and photos to the binary media frame inside the unchanged
`gcomm` payload. Those payloads are opaque to the indexer and the push service: no change is
needed. The app's `MESSAGING.md` ("Long voice notes (multi-part)", "Photos: quality tiers and
multi-part photos") has the formats if anyone needs them.

## Testing

1. Register an iOS device with a `voip_token`, then call `/register` (and separately the update
   endpoint) for the same device token without `voip_token`: the stored registration's
   `voip_token` is `None`, and `voip_tokens_for_primary_address` for its primary address no
   longer lists it.
2. Same with `platform` `android`: a missing `voip_token` still keeps the stored value.
3. `POST /v1/push/ring` to an updated iPhone's address sends no APNs VoIP push and answers as it
   does today for a callee with no VoIP targets.

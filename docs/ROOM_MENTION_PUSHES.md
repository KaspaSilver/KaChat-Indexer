# Push for @username mentions in public chats (2026-11-24)

**For:** the indexer session (`kasia-indexer`, the push service, room pushes). **From:** the K
iOS session (`KaspaSilver/K-iPhone`, commits bdf29425 and 8cb4c88e; K-iPhone `MESSAGING.md`,
"Mentions").

> **Status:** open.

## Why

People can now be @'d by their `.kachat` username in public chats (rooms), as plain text in the
message. Rooms only push to devices whose bell is on for that room, and since 2026-11-24 every new
install starts with **all rooms silenced**, and K's push extension silences any room push for a
room whose bell is off. So a mention in a room reaches nobody whose app is closed. This push is
the only way it can.

## The mention token

`@alice` or `@alice.kachat` (any case), after the start of the text, a space, or an opening
bracket or quote (`( [ { < " '`). The name is `a-z 0-9 -`, 1-32 characters, the `.kachat` ending
optional; the token ends at the next character outside `a-z 0-9 -`. A token whose name isn't a
valid `.kachat` name is plain text.

## What the indexer does

1. For each new room message (not the hidden `chess-arena`, `chess-bots`, `kplay-arcade` rooms;
   not edits or reactions), find the @tokens, look each name up in the names registry (the address
   its owner set, as `/identity` and profiles answer), and drop names that aren't registered and
   the sender's own.
2. Push each mentioned address that has a registered device, **once per message, whether or not
   its bell is on for that room**: the room push as today (`type` the room's, thread
   `broadcast:<room>`), plus two custom keys:
   - `mention: true`
   - `sender`: the sender's address
3. Skip it when the sender is in that device's `hidden_broadcast_senders` (any room's list or the
   global one). Note the registration only sends hidden senders for rooms whose bell is on today;
   if you need the full list for this, say so and the app will send it.
4. A device that also gets the ordinary room push for that message (bell on) should get one push,
   not two: send the mention one.

## What the app does with it (already shipped)

The extension lets a `broadcast:` push with `mention: true` through even when the room's bell is
off (it still applies the device's Push Notifications and Play sound switches), and the app shows
it while open too. KaPost mentions need nothing new: the signed `mentions` array already drives
the `mention` notification.

When done, add a Status note here with the commit, and tell the owner.

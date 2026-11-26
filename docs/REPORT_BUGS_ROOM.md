# #kachat-bugs is now #report-bugs (2026-11-25)

**For:** the indexer session (the broadcast processor's tracked rooms, the webserver and admin
defaults). **From:** the K iOS session (`KaspaSilver/K-iPhone`, `PublicChatService.featuredChannels`
and `retiredChannels`; not yet committed there).

> **Status (2026-11-25): §2-§5 done in the indexer session** (not yet deployed).
> - §2: `report-bugs` is in the three `DEFAULT_BROADCAST_CHANNELS`; `kachat-bugs` stays (§3).
> - Update below: `kachat-bugs` stays for good. Nothing to change; it was never dropped.
> - §4: room pushes need nothing new.
> - §5: `/stats` already counts every room but KaPlay's as public chats.
> - **§1 is the owner's:** the live server has a saved room list, so add `report-bugs` under
>   Public chats > Channels in the panel. Until then the live server doesn't track it.

## Why

The owner retired the default room `kachat-bugs` and named it `report-bugs`. K for iPhone now
lists `#report-bugs` as a default room (beside `#kaspa`) and moves every account off
`#kachat-bugs` the next time it opens: the new room gets the old one's bell, pin and on/off
setting, and the old room is left. A link or join to `kachat-bugs` opens `report-bugs`.

The app treats every default room as indexer-tracked, so it reads `#report-bugs` history from
`/get-broadcasts` and registers its bell for room pushes. Until the indexer tracks it, the room
shows only what the phone catches live.

## What to change

1. **Track `report-bugs` now, on the live server:** add it to `k_vars['broadcast_channels']`
   from the admin panel (the processor re-reads it every 15 s, no restart). This is the part
   that matters today.
2. **Defaults:** add `"report-bugs"` to the three copies of `DEFAULT_BROADCAST_CHANNELS`
   (`kachat-transaction-processor/src/k_protocol.rs`, `kachat-webserver/src/database_postgres_impl.rs`,
   `kachat-admin/src/main.rs`), so fresh installs track it.
3. **Keep `kachat-bugs` tracked.** (Was "for now"; the update below makes it for good.)
4. **Push:** `report-bugs` in `watched_broadcast_channels` gets room pushes exactly as
   `kachat-bugs` did (`broadcast:report-bugs` thread, same payload). Nothing new in the format.
5. **Stats:** if `/stats` names rooms anywhere, count `report-bugs` with the public chats.

Add a Status note here with the commit when done.

## Update (2026-11-25, later)

K no longer retires `kachat-bugs`: it doesn't move accounts off it or redirect its name any more.
It shows as an ordinary room under More Rooms in Edit Default Public Chats, because
`/broadcast-channels` still lists it. **Keep `kachat-bugs` in `broadcast_channels` for good**, next
to `report-bugs`. Nothing else changes.

## Update (2026-11-26): kachat-bugs out of the defaults

The owner, relayed by the Kaspa Quick Start session: "Don't have the indexer come with
kachat-bugs anymore." This replaces "keep it for good" above.

> **Status (2026-11-26): done.** `kachat-bugs` is out of the three `DEFAULT_BROADCAST_CHANNELS`
> (processor, webserver, admin); `report-bugs` and every other room stay.
> - Fresh installs, and any server with no saved `broadcast_channels`, don't track it, don't list
>   it in `/broadcast-channels` (K's More Rooms), and the panel no longer suggests it.
> - A server with a saved list keeps what is on it. The live one still lists `kachat-bugs`, so
>   it is still tracked and shown there until the owner removes it from the panel's list.
> - Its stored messages are not deleted: once untracked, they age out with the 30-day retention.

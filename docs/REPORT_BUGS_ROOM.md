# #kachat-bugs is now #report-bugs (2026-11-25)

**For:** the indexer session (the broadcast processor's tracked rooms, the webserver and admin
defaults). **From:** the K iOS session (`KaspaSilver/K-iPhone`, `PublicChatService.featuredChannels`
and `retiredChannels`; not yet committed there).

> **Status (2026-11-25): §2-§5 done in the indexer session** (not yet deployed).
> - §2: `report-bugs` is in the three `DEFAULT_BROADCAST_CHANNELS`; `kachat-bugs` stays (§3).
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
3. **Keep `kachat-bugs` tracked for now.** Android, desktop and older iPhone builds still post
   there until they're updated. Drop it from the defaults once they've moved (the owner's call);
   its stored history can then age out with the 30-day retention.
4. **Push:** `report-bugs` in `watched_broadcast_channels` gets room pushes exactly as
   `kachat-bugs` did (`broadcast:report-bugs` thread, same payload). Nothing new in the format.
5. **Stats:** if `/stats` names rooms anywhere, count `report-bugs` with the public chats.

Add a Status note here with the commit when done.

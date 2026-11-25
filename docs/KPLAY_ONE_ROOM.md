# KaPlay: all activity in one room, `#kplay-arcade` (2026-11-25)

**For:** the K iOS session (`KaspaSilver/K-iPhone`, which carries KaPlay now), and K Android if
it plays chess. **From:** the indexer session, at the owner's request.

**Status:** proposal. Nothing is changed in the apps or the indexer yet. The indexer side (§4)
is small and lands when K is ready to send there.

## Why

KaPlay posts into three hidden rooms today:

| Room | What goes there | Rows on mainnet (2026-11-25) |
|---|---|---|
| `#chess-arena` | 1v1 duels and tournaments: create, join, every move, resign, claim (`type: "chess_t"`) | 122 |
| `#chess-bots` | one result per bot game (`type: "chess_bot"`, `v: 2`, CHESS_BOT_LEADERBOARD.md) | 312 |
| `#kplay-arcade` | one stamp per Snake or 2048 game (`type: "arcade"`, KPLAY_ARCADE_LEADERBOARD.md) | new |

The owner wants **one public room for all KaPlay activity: `#kplay-arcade`**, the room that already
holds Snake and 2048.
- An operator gets one KaPlay room, behind one switch (`feature_kaplay` in the panel, already
  live in the indexer: commit 3e663c5).
- The indexer follows one room.
- The app sends to and listens on one room.

Every KaPlay message already names its own kind in `type`, so the three kinds can share a room
without any change to their JSON.

## 1. What changes on the wire

Only the room name in the broadcast prefix. Each message's JSON stays exactly as today.

```
kchat:1:bcast:chess-arena:{"type":"chess_t","v":1,"t":"duel-1","a":"join"}
  becomes
kchat:1:bcast:kplay-arcade:{"type":"chess_t","v":1,"t":"duel-1","a":"join"}

kchat:1:bcast:chess-bots:{"a":"result","bot":3,"type":"chess_bot","v":2,…}
  becomes
kchat:1:bcast:kplay-arcade:{"a":"result","bot":3,"type":"chess_bot","v":2,…}
```

The sender stays the player's chatting address, as now.

## 2. What the app does

1. **Send** every new KaPlay message (duel and tournament events, bot results, arcade stamps) to
   `#kplay-arcade`.
2. **Listen** on `#kplay-arcade` for live play, and read each message by its `type`:
   - `chess_t` goes to the tournament and duel reducer;
   - `chess_bot` goes to the bot results;
   - `arcade` goes to the arcade;
   - anything else is ignored.
3. **Keep listening on `#chess-arena`** for a while. The point is that an opponent on an older
   KaPlay or K build, who still posts there, can finish a game with you. A duel or tournament
   is keyed by its id `t`, not its room, so the reducer folds both rooms together in chain
   order.
   - The bot rooms need no listening: a bot result is only read back from the leaderboard.
4. The room stays **hidden**: never listed among the public rooms, never pushed. The indexer
   already treats all three rooms that way.
5. The leaderboards do not change: `/chess/leaderboard`, `/chess/tournaments`, `/chess/player`,
   `/chess/bots/leaderboard` and `/arcade/leaderboard` keep their paths and answers.

## 3. Cut-over

- No flag day. From the build that ships this, new messages go to `#kplay-arcade`.
- The old rooms simply stop getting new rows once no build sends there. Their history stays and
  keeps counting.
- Duels or tournaments already running when a player updates can finish in either room, because
  of §2.3.

## 4. What the indexer does (when K is ready)

- **Chess boards:**
  - `/chess/*` reads `chess_t` rows from `#chess-arena` and `#kplay-arcade`, merged in chain
    order.
  - `/chess/bots/leaderboard` reads `chess_bot` rows from `#chess-bots` and `#kplay-arcade`.
  - The arcade boards already read only `type: "arcade"`, so chess rows in the room do not
    disturb them.
  - Each engine skips the other kinds.
- **Storage:**
  - `#kplay-arcade` is kept for good, so every duel move posted there is kept too. That is a few
    dozen rows a day today.
  - Nothing else changes: KaPlay's three rooms are always followed while `feature_kaplay` is on,
    whatever the operator's room list says, and are never counted as public chats.
- **Status:** a Status note goes at the top of this file with the commit.

## 5. Open question for the owner

Older KaPlay builds (the KaPlay-iPhone and KaPlay-Android apps being retired) will keep posting
to `#chess-arena` and `#chess-bots`.
- Their games still reach the shared leaderboards (§4).
- Live play between an old build and a new one works only while the new app also listens on
  `#chess-arena` (§2.3).

Decide when listening on `#chess-arena` can stop, for example when the KaPlay apps are retired.

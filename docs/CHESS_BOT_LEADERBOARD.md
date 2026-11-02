# Chess bot leaderboards: track `chess-bots`, serve a board per bot (2026-11-02)

**For:** the indexer session (`kachat-webserver`, `kachat-transaction-processor`). **From:** the
KaPlay (KaChat Chess) iOS session (`KaspaSilver/KaPlay-iPhone`).

KaPlay's bot games now send one transaction when they end, holding the result and every move,
instead of one per move. KaPlay's Leaderboard tab shows a board per bot: each player's **fewest
moves to win**. The server has to store those transactions and serve the boards. Until it does,
each phone shows only its own wins.

## 1. Track the channel (required)

Add `chess-bots` to the tracked broadcast channels: `DEFAULT_BROADCAST_CHANNELS` in
`kachat-transaction-processor/src/k_protocol.rs`, plus the production list if it is set in the
admin dashboard. Keep it out of public chat statistics, the same way `chess-arena` is excluded in
`kachat-webserver/src/web_server.rs`.

**Keep these rows for good**, not 30 days: the all-time boards depend on them.

## 2. The message

Plaintext broadcast `kchat:1:bcast:chess-bots:<json>`, signed by the player:

```json
{"a":"result","bot":3,"c":"w","g":"3b0f…","m":"e2e4 e7e5 d1h5 b8c6 f1c4 g8f6 h5f7","r":"won","type":"chess_bot","v":2,"why":"checkmate"}
```

| Field | Meaning |
|---|---|
| `type`, `v`, `a` | `"chess_bot"`, `2`, `"result"`. Anything else is ignored, including the old `v: 1` `start` / `move` / `resign` rows |
| `g` | Game id, a lowercase UUID |
| `bot` | The bot's level, 1 to 5 |
| `c` | The player's colour, `"w"` or `"b"` |
| `r` | `"won"`, `"lost"` or `"drawn"`, for the player |
| `why` | `checkmate`, `resignation`, `stalemate`, `insufficient material`, `the fifty-move rule`, `threefold repetition` |
| `m` | All moves, both sides, space separated: `e2e4`, a promotion `e7e8q` |

The player is the transaction's sender. Only the first row per (sender, `g`) counts, in chain order.

## 3. The rules

A row counts toward `games` if its fields are valid and every move in `m` is legal from the
starting position, replayed with the rules engine already in `kachat-webserver/src/chess.rs`. A
resignation can stop at any point, and `m` can be empty.

It counts as a **win** only if `r` is `won`, `why` is `checkmate`, and the final position is
checkmate delivered by the player's colour.

**Moves** are the player's own: `(plies + 1) / 2` as white, `plies / 2` as black. So 7 plies as
white is 4 moves.

**Each board:** one row per address, that player's fewest moves against that bot in the period.
Sort by moves ascending, then earliest block time, then txid.

**Periods:** the same as `/chess/leaderboard`: `since` from `chess::LEADERBOARD_FRESH_FROM_MS`, or
the start of the current UTC month if later. A win counts by its block time.

**Not cheat proof, and that is accepted:** the bot plays with randomness on the player's phone, so
the server cannot tell whether it really chose those moves. Don't try to detect this.

## 4. The endpoint

`GET /chess/bots/leaderboard?period=month|all&limit=100` (missing `period` = `all`, `limit` is per
bot):

```json
{
  "period": "month",
  "since": 1793491200000,
  "generatedAt": 1793500000000,
  "bots": [
    {
      "level": 3,
      "games": 41,
      "players": [
        { "address": "kaspa:…", "moves": 14, "at": 1793495000000, "gameId": "3b0f…", "txId": "…" }
      ]
    }
  ]
}
```

- `games` counts every counted row for that bot in the period, wins or not. KaPlay lists a bot once
  its all-time `games` is above 0, so a bot played but never beaten still appears.
- List only bots with at least one game.
- KaPlay reads `period`, `bots[].level`, `games`, `players[].address`, `moves` and `at`. It ignores
  a response whose `period` differs from the one it asked for. `gameId` and `txId` are for later.

The app side: `ChessBotLeaderboardService.swift`. The full protocol: `ONLINE_CHESS.md` §7 in the
KaPlay repo.

## 5. Testing

- A win as white with `m` of 7 plies shows `moves: 4`.
- A second, longer win by the same address leaves the row unchanged; a shorter one replaces it.
- A row with an illegal move in `m` counts for nothing.
- A row with `r: won` and legal moves that don't end in checkmate counts toward `games` but not the
  board.
- `v: 1` rows change nothing.
- A second row with the same sender and `g` changes nothing.

# KaPlay arcade leaderboards: Snake and 2048 (2026-11-23)

**For:** the indexer session (the `#kplay-arcade` room and a new read endpoint). **From:** the K
iOS session (`KaspaSilver/K-iPhone`, commit 518fdc8c; the app-side spec is K-iPhone `ARCADE.md`,
copied below in full so this file stands on its own).

> **Status:** open. Until `/arcade/leaderboard` is served, the app shows each player's own stamped
> scores only.

## What the indexer does

1. Follow `#kplay-arcade` broadcasts as it does `#chess-bots` (sender = the player). The room is
   hidden in the app: never list it, never push it.
2. Replay each stamp with the rules in §2-§4 below and keep only those whose replay reaches the
   stamped `score` (one per game id `g`). The test vectors in §2-§4 must pass.
3. Serve `GET /arcade/leaderboard?game=snake|2048&period=month|all&limit=100` (§5): per address
   its best checked score in the period (`month` = the UTC calendar month as the chess boards
   count it, `all` = since the chess fresh-start cutoff), highest first, earlier stamp on a tie,
   with `games` (checked stamps in the period), `since`, and each row's `at` (block time, ms) and
   `tx`.

When done, add a Status note here with the commit, and tell the owner so the iPhone session can
mark it done in K-iPhone `INDEXER_HANDOFF.md`.

---

# KaPlay Arcade: Snake and 2048 on Kaspa

Native games in KaPlay (since 2026-11-23, owner's call), next to Chess. Each finished game is
stamped on Kaspa, the indexer checks every stamp by replaying it, and the boards rank the best
checked score per address. A score shared to KaPost carries a challenge link.

App code: `KaChat/Services/ArcadeEngines.swift` (the games, pure and deterministic),
`KaChat/Services/ArcadeServices.swift` (stamp, own scores, boards, challenge links),
`KaChat/Views/Games/ArcadeViews.swift` (screens).

## 1. The stamp

A plaintext broadcast in the hidden room **`#kplay-arcade`**, sent by the player's chatting
address (the same broadcast transaction a public room message is), once when a game ends with a
score above 0:

```json
{"g":"<game id>","game":"snake","m":"12R 30U 41L","score":7,"seed":"9f2c41d07a3b55e1","type":"arcade","v":1}
```

| field | meaning |
|---|---|
| `type` | `"arcade"` |
| `v` | `1` |
| `game` | `"snake"` or `"2048"` |
| `g` | game id, a lowercase UUID chosen by the phone; one stamp per id |
| `seed` | the game's seed, 16 lowercase hex digits (a `UInt64`) |
| `score` | the score the game reached |
| `m` | the moves: Snake's turns (§3), 2048's swipes (§4) |

Keys are sorted, no extra whitespace. The sender of the transaction is the player.

## 2. Chance: SplitMix64

All chance comes from SplitMix64 seeded with `seed`:

```
state = state + 0x9E3779B97F4A7C15        (wrapping)
z = state
z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9  (wrapping)
z = (z ^ (z >> 27)) * 0x94D049BB133111EB  (wrapping)
next = z ^ (z >> 31)
below(n) = next % n
```

Test vector: seed 42 gives `13679457532755275413`, then `2949826092126892291`.

## 3. Snake

- Board 16 x 16, x right, y down. The snake starts as (6,8) head, (5,8), (4,8), heading right.
- The apple: the empty cells listed row by row from the top left (y, then x), the apple on
  `empty[below(count)]`. Placed at the start and after every apple eaten.
- A tick: if a turn is recorded for this tick (the tick count before it), the heading becomes
  that direction (it is never the current heading or its opposite). Then the head moves one cell.
  - Off the board: game over.
  - Onto the apple: the snake grows (the tail stays), score + 1, a new apple.
  - Onto its body: game over. The tail cell counts only when eating (otherwise it moves away).
  - Otherwise the tail moves along.
- `m`: the turns, space separated, each the tick it was taken on followed by `U`, `D`, `L` or
  `R`, ticks strictly increasing: `"12R 30U 41L"`. Empty when it never turned.
- Replay: play ticks, taking the recorded turns, until game over. A turn that is the current
  heading or its opposite is invalid; a game that hasn't ended after 1,000,000 ticks is invalid.
- Test vector: seed `0000000000000001`, `m` = `"1D 3L"`: first apple (0,9), game over after
  11 ticks with score 0.

## 4. 2048

- Board 4 x 4, `tiles[y*4+x]`, 0 empty. Two tiles to start.
- A new tile: the empty cells row by row from the top left, the cell `empty[below(count)]`,
  value 4 when `below(10) == 0`, else 2.
- A move `U`, `D`, `L` or `R` slides every line toward that wall; equal neighbours merge once
  each, nearest the wall first; each merge adds the new tile's value to the score. A move that
  changes no tile is not a move: it is never recorded, and a stamp containing one is invalid.
  After every move a new tile appears. The game is over when no move changes the board.
- `m`: the moves as one string of letters, `"LURD..."`.
- Replay: play the moves; the score must equal `score`.
- Test vector: seed `0000000000000001` starts `[2,2,0,...0]`; `m` = `"LURDLURDLURD"` gives
  score 48 and tiles `[0,0,0,0, 0,0,0,4, 0,0,2,16, 2,2,4,2]`.

## 5. The boards (indexer)

`GET /arcade/leaderboard?game=snake|2048&period=month|all&limit=100`

```json
{"game":"snake","period":"month","since":1793000000000,"games":412,
 "players":[{"address":"kaspa:...","score":57,"at":1793123456789,"tx":"<txid>"}]}
```

- Counts only stamps that replay to their `score` (§3, §4), one per `g`, from the transaction's
  sender. `month` is the calendar month (UTC) the chess boards use; `all` is everything.
- One row per address: its best score in the period, the earlier stamp on a tie. Highest first.
- `games`: checked stamps in the period.
- The app falls back to the player's own stamped scores when this isn't served yet.

## 6. Challenge links

`kachatarcade://<game>?score=<n>&by=<address>&game=<game id>`

The game-over sheet's Share to KaPost writes "I scored 1,240 in Snake on K. Think you can beat
it?" and this link; a post shows it as a card (`KaPostArcadeChallengeCard`) whose Play opens the
game in KaPlay with "<who> scored 1,240 in Snake. Can you beat it?". Only the game has to be
valid; a bad score or address is dropped. The scheme is registered by the app.

## Known limits

- The phone picks the seed, so a player could look for a kind seed before playing.
- A bot can play perfectly; as with chess bots, a stamp proves the game was played as recorded,
  not by whom.
- Each stamp costs a network fee from the chatting address; a game with no KAS there isn't
  stamped (the sheet says why).

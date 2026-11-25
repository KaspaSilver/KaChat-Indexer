//! KaPlay Arcade leaderboards (docs/KPLAY_ARCADE_LEADERBOARD.md): each finished Snake or 2048
//! game is stamped in the hidden `#kplay-arcade` room; a stamp counts only when replaying its
//! seed and moves reaches its score. The boards rank each address's best checked score.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};

use crate::chess::{ArenaRow, period_since_ms};

pub const ARCADE_CHANNEL: &str = "kplay-arcade";

/// A stamp is a short JSON object; anything much larger isn't one.
const MAX_STAMP_CHARS: usize = 24_000;

/// A Snake game that hasn't ended after this many ticks is invalid (§3).
const SNAKE_MAX_TICKS: u64 = 1_000_000;

// ------------------------------------------------------------------ chance (§2) ----

/// SplitMix64: all chance in both games.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dir {
    U,
    D,
    L,
    R,
}

impl Dir {
    fn from_char(c: char) -> Option<Dir> {
        match c {
            'U' => Some(Dir::U),
            'D' => Some(Dir::D),
            'L' => Some(Dir::L),
            'R' => Some(Dir::R),
            _ => None,
        }
    }

    fn opposite(self) -> Dir {
        match self {
            Dir::U => Dir::D,
            Dir::D => Dir::U,
            Dir::L => Dir::R,
            Dir::R => Dir::L,
        }
    }

    fn step(self) -> (i32, i32) {
        match self {
            Dir::U => (0, -1),
            Dir::D => (0, 1),
            Dir::L => (-1, 0),
            Dir::R => (1, 0),
        }
    }
}

// ------------------------------------------------------------------ Snake (§3) ----

const SNAKE_SIZE: i32 = 16;

/// How a Snake game went: its score and how many ticks it lasted.
#[derive(Debug, PartialEq, Eq)]
pub struct SnakeEnd {
    pub score: u64,
    pub ticks: u64,
    first_apple: (i32, i32),
}

/// The turns of `m` (`"12R 30U 41L"`): ticks strictly increasing. None if malformed.
fn snake_turns(m: &str) -> Option<Vec<(u64, Dir)>> {
    let mut turns: Vec<(u64, Dir)> = Vec::new();
    for token in m.split(' ').filter(|t| !t.is_empty()) {
        let dir = Dir::from_char(token.chars().last()?)?;
        let digits = &token[..token.len() - 1];
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 7 {
            return None;
        }
        let tick: u64 = digits.parse().ok()?;
        if turns.last().is_some_and(|(last, _)| *last >= tick) {
            return None;
        }
        turns.push((tick, dir));
    }
    Some(turns)
}

/// The cell for a new apple: the empty cells row by row, `empty[below(count)]`. None when the
/// snake fills the board.
fn place_apple(rng: &mut SplitMix64, body: &[(i32, i32)]) -> Option<(i32, i32)> {
    let taken: HashSet<(i32, i32)> = body.iter().copied().collect();
    let empty: Vec<(i32, i32)> = (0..SNAKE_SIZE)
        .flat_map(|y| (0..SNAKE_SIZE).map(move |x| (x, y)))
        .filter(|cell| !taken.contains(cell))
        .collect();
    if empty.is_empty() {
        return None;
    }
    Some(empty[rng.below(empty.len() as u64) as usize])
}

/// Replay a Snake game to its end. None if the moves are invalid.
pub fn replay_snake(seed: u64, m: &str) -> Option<SnakeEnd> {
    let turns = snake_turns(m)?;
    let mut rng = SplitMix64(seed);
    // Head first.
    let mut body: std::collections::VecDeque<(i32, i32)> = [(6, 8), (5, 8), (4, 8)].into_iter().collect();
    let mut heading = Dir::R;
    let mut apple = place_apple(&mut rng, body.make_contiguous());
    let first_apple = apple?;
    let mut score = 0u64;
    let mut next_turn = 0usize;
    let mut tick = 0u64;
    loop {
        if tick >= SNAKE_MAX_TICKS {
            return None;
        }
        if let Some((at, dir)) = turns.get(next_turn).copied()
            && at == tick
        {
            if dir == heading || dir == heading.opposite() {
                return None;
            }
            heading = dir;
            next_turn += 1;
        }
        tick += 1;
        let (dx, dy) = heading.step();
        let (hx, hy) = body[0];
        let head = (hx + dx, hy + dy);
        if !(0..SNAKE_SIZE).contains(&head.0) || !(0..SNAKE_SIZE).contains(&head.1) {
            break;
        }
        if Some(head) == apple {
            body.push_front(head);
            score += 1;
            apple = place_apple(&mut rng, body.make_contiguous());
            if apple.is_none() {
                break;
            }
            continue;
        }
        // Not eating: the tail moves away, so only the rest of the body is in the way.
        if body.iter().take(body.len() - 1).any(|cell| *cell == head) {
            break;
        }
        body.pop_back();
        body.push_front(head);
    }
    Some(SnakeEnd { score, ticks: tick, first_apple })
}

// ------------------------------------------------------------------ 2048 (§4) ----

/// How a 2048 game went: its score and the final tiles (`tiles[y*4+x]`).
#[derive(Debug, PartialEq, Eq)]
pub struct End2048 {
    pub score: u64,
    pub tiles: [u32; 16],
}

/// A new tile on an empty cell (row by row, `empty[below(count)]`): 4 when `below(10) == 0`.
fn spawn_tile(rng: &mut SplitMix64, tiles: &mut [u32; 16]) {
    let empty: Vec<usize> = (0..16).filter(|i| tiles[*i] == 0).collect();
    if empty.is_empty() {
        return;
    }
    let cell = empty[rng.below(empty.len() as u64) as usize];
    tiles[cell] = if rng.below(10) == 0 { 4 } else { 2 };
}

/// The cells of each line, nearest the wall first.
fn lines(dir: Dir) -> [[usize; 4]; 4] {
    let mut out = [[0usize; 4]; 4];
    for (line, cells) in out.iter_mut().enumerate() {
        for (k, cell) in cells.iter_mut().enumerate() {
            let (x, y) = match dir {
                Dir::L => (k, line),
                Dir::R => (3 - k, line),
                Dir::U => (line, k),
                Dir::D => (line, 3 - k),
            };
            *cell = y * 4 + x;
        }
    }
    out
}

/// Slide every line toward the wall, merging equal neighbours once each, nearest the wall first.
/// Returns the points scored, or None when no tile moves.
fn slide(tiles: &mut [u32; 16], dir: Dir) -> Option<u64> {
    let before = *tiles;
    let mut points = 0u64;
    for cells in lines(dir) {
        let values: Vec<u32> = cells.iter().map(|c| tiles[*c]).filter(|v| *v != 0).collect();
        let mut merged: Vec<u32> = Vec::with_capacity(4);
        let mut i = 0;
        while i < values.len() {
            if i + 1 < values.len() && values[i] == values[i + 1] {
                let v = values[i] * 2;
                points += v as u64;
                merged.push(v);
                i += 2;
            } else {
                merged.push(values[i]);
                i += 1;
            }
        }
        for (k, cell) in cells.iter().enumerate() {
            tiles[*cell] = merged.get(k).copied().unwrap_or(0);
        }
    }
    (*tiles != before).then_some(points)
}

/// Replay a 2048 game. None if a move is malformed or changes no tile.
pub fn replay_2048(seed: u64, m: &str) -> Option<End2048> {
    let mut rng = SplitMix64(seed);
    let mut tiles = [0u32; 16];
    spawn_tile(&mut rng, &mut tiles);
    spawn_tile(&mut rng, &mut tiles);
    let mut score = 0u64;
    for c in m.chars() {
        let dir = Dir::from_char(c)?;
        score += slide(&mut tiles, dir)?;
        spawn_tile(&mut rng, &mut tiles);
    }
    Some(End2048 { score, tiles })
}

// ------------------------------------------------------------------ stamps (§1) ----

#[derive(Debug, Deserialize)]
struct Stamp {
    #[serde(rename = "type")]
    kind: String,
    v: u32,
    game: String,
    g: String,
    seed: String,
    score: u64,
    m: String,
}

/// A checked stamp: one finished game whose replay reaches its score.
#[derive(Debug, Clone)]
struct CheckedGame {
    game: &'static str,
    address: String,
    score: u64,
    at: i64,
    tx: String,
}

/// The game a stamp is for, if it is a well-formed stamp whose replay reaches its score.
fn check_stamp(content: &str) -> Option<(&'static str, String, u64)> {
    if content.len() > MAX_STAMP_CHARS || !content.trim_start().starts_with('{') {
        return None;
    }
    let stamp: Stamp = serde_json::from_str(content).ok()?;
    if stamp.kind != "arcade" || stamp.v != 1 || stamp.score == 0 || !crate::chess::is_game_id(&stamp.g) {
        return None;
    }
    if stamp.seed.len() != 16 || !stamp.seed.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let seed = u64::from_str_radix(&stamp.seed, 16).ok()?;
    let (game, score) = match stamp.game.as_str() {
        "snake" => ("snake", replay_snake(seed, &stamp.m)?.score),
        "2048" => ("2048", replay_2048(seed, &stamp.m)?.score),
        _ => return None,
    };
    (score == stamp.score).then_some((game, stamp.g, score))
}

// ------------------------------------------------------------------ boards (§5) ----

#[derive(Debug, Clone, serde::Serialize)]
pub struct ArcadeRow {
    pub address: String,
    pub score: u64,
    pub at: i64,
    pub tx: String,
}

#[derive(Debug, Clone, Default)]
pub struct ArcadeBoard {
    pub since: i64,
    /// Checked stamps in the period.
    pub games: u64,
    pub players: Vec<ArcadeRow>,
}

/// Both games' boards for both periods, from one replay of the room.
#[derive(Debug, Clone, Default)]
pub struct ArcadeSnapshot {
    boards: HashMap<(&'static str, &'static str), ArcadeBoard>,
}

impl ArcadeSnapshot {
    /// The board for `game` (`snake` or `2048`) and `period` (`month`, else all time).
    pub fn board(&self, game: &str, period: Option<&str>) -> Option<(ArcadeBoard, &'static str, &'static str)> {
        let game = match game.trim() {
            "snake" => "snake",
            "2048" => "2048",
            _ => return None,
        };
        let period = match period.map(|p| p.trim().to_lowercase()).as_deref() {
            Some("month") | Some("monthly") => "month",
            _ => "all",
        };
        Some((self.boards.get(&(game, period)).cloned().unwrap_or_default(), game, period))
    }
}

/// One row's verdict: a stamp whose replay reaches its score. A stamp never changes, so the
/// webserver keeps this per tx id and replays each row once.
#[derive(Debug, Clone)]
pub struct CheckedStamp {
    game: &'static str,
    g: String,
    score: u64,
    sender: String,
    at: i64,
}

/// The expensive half: replay the row's stamp. `None` if it is not a checked stamp.
pub fn check_row(row: &ArenaRow) -> Option<CheckedStamp> {
    let (game, g, score) = check_stamp(&row.content)?;
    Some(CheckedStamp { game, g, score, sender: row.sender.clone(), at: row.block_time })
}

/// Replay the room once. Only the first stamp per (sender, game id) counts, in chain order
/// (block time, then txid), as for the chess bot games.
#[cfg(test)]
pub fn compute(rows: Vec<ArenaRow>, now_ms: i64) -> ArcadeSnapshot {
    let checked: Vec<(String, CheckedStamp)> =
        rows.iter().filter_map(|row| check_row(row).map(|s| (row.tx_id.clone(), s))).collect();
    snapshot(checked.iter().map(|(tx, s)| (tx.as_str(), s)), now_ms)
}

/// The cheap half: both games' boards for both periods from the checked stamps, as
/// (tx id, stamp), in any order.
pub fn snapshot<'a>(checked: impl Iterator<Item = (&'a str, &'a CheckedStamp)>, now_ms: i64) -> ArcadeSnapshot {
    let mut checked: Vec<(&str, &CheckedStamp)> = checked.collect();
    checked.sort_by(|a, b| a.1.at.cmp(&b.1.at).then_with(|| a.0.cmp(b.0)));
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut games: Vec<CheckedGame> = Vec::new();
    for (tx, s) in checked {
        if !seen.insert((s.sender.as_str(), s.g.as_str())) {
            continue;
        }
        games.push(CheckedGame { game: s.game, address: s.sender.clone(), score: s.score, at: s.at, tx: tx.to_string() });
    }
    let mut boards = HashMap::new();
    for (period, since) in [("all", period_since_ms(false, now_ms)), ("month", period_since_ms(true, now_ms))] {
        for game in ["snake", "2048"] {
            boards.insert((game, period), board(&games, game, since));
        }
    }
    ArcadeSnapshot { boards }
}

/// Each address's best score in the period (the earlier stamp on a tie), highest first.
fn board(games: &[CheckedGame], game: &str, since: i64) -> ArcadeBoard {
    let mut best: HashMap<&str, &CheckedGame> = HashMap::new();
    let mut count = 0u64;
    for g in games.iter().filter(|g| g.game == game && g.at >= since) {
        count += 1;
        let better = best
            .get(g.address.as_str())
            .is_none_or(|b| g.score > b.score);
        if better {
            best.insert(g.address.as_str(), g);
        }
    }
    let mut players: Vec<ArcadeRow> = best
        .into_values()
        .map(|g| ArcadeRow { address: g.address.clone(), score: g.score, at: g.at, tx: g.tx.clone() })
        .collect();
    players.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.at.cmp(&b.at))
            .then_with(|| a.address.cmp(&b.address))
    });
    ArcadeBoard { since, games: count, players }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_matches_the_test_vector() {
        let mut rng = SplitMix64(42);
        assert_eq!(rng.next(), 13679457532755275413);
        assert_eq!(rng.next(), 2949826092126892291);
    }

    #[test]
    fn snake_matches_the_test_vector() {
        let end = replay_snake(1, "1D 3L").expect("valid game");
        assert_eq!(end.first_apple, (0, 9));
        assert_eq!(end.ticks, 11);
        assert_eq!(end.score, 0);
    }

    #[test]
    fn snake_rejects_bad_turns() {
        // Turning onto the current heading or back on itself, ticks out of order, junk.
        assert!(replay_snake(1, "1R").is_none());
        assert!(replay_snake(1, "1L").is_none());
        assert!(replay_snake(1, "3D 3L").is_none());
        assert!(replay_snake(1, "5D 3L").is_none());
        assert!(replay_snake(1, "D").is_none());
        assert!(replay_snake(1, "1X").is_none());
        // Never turning: straight into the right wall from (6,8), 10 ticks.
        assert_eq!(replay_snake(1, "").map(|e| e.ticks), Some(10));
    }

    #[test]
    fn snake_eats_and_grows() {
        // Find a seed whose first apple is on row 8 ahead of the snake, and eat it.
        let seed = (0..10_000u64)
            .find(|s| {
                let mut rng = SplitMix64(*s);
                let apple = place_apple(&mut rng, &[(6, 8), (5, 8), (4, 8)]).unwrap();
                apple.1 == 8 && apple.0 > 6
            })
            .expect("some seed puts the apple ahead");
        assert!(replay_snake(seed, "").expect("valid").score >= 1);
    }

    #[test]
    fn game_2048_matches_the_test_vector() {
        let mut rng = SplitMix64(1);
        let mut tiles = [0u32; 16];
        spawn_tile(&mut rng, &mut tiles);
        spawn_tile(&mut rng, &mut tiles);
        assert_eq!(&tiles[..2], &[2, 2]);
        assert!(tiles[2..].iter().all(|t| *t == 0));
        let end = replay_2048(1, "LURDLURDLURD").expect("valid game");
        assert_eq!(end.score, 48);
        assert_eq!(end.tiles, [0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 2, 16, 2, 2, 4, 2]);
    }

    #[test]
    fn game_2048_merges_once_nearest_the_wall_first() {
        let mut tiles = [2, 2, 2, 2, 4, 4, 8, 0, 2, 0, 2, 4, 0, 0, 0, 0];
        assert_eq!(slide(&mut tiles, Dir::L), Some(4 + 4 + 8 + 4));
        assert_eq!(&tiles[..12], &[4, 4, 0, 0, 8, 8, 0, 0, 4, 4, 0, 0]);
        let mut row = [2, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        slide(&mut row, Dir::R);
        assert_eq!(&row[..4], &[0, 0, 2, 4]);
        // A move that changes nothing is not a move.
        let mut stuck = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(slide(&mut stuck, Dir::L), None);
        // The test vector's last board is packed down: one more D changes no tile.
        assert!(replay_2048(1, "LURDLURDLURDD").is_none());
        assert!(replay_2048(1, "LX").is_none());
    }

    fn stamp(game: &str, g: &str, seed: &str, score: u64, m: &str) -> String {
        format!(r#"{{"g":"{g}","game":"{game}","m":"{m}","score":{score},"seed":"{seed}","type":"arcade","v":1}}"#)
    }

    #[test]
    fn boards_count_only_checked_stamps_best_per_address() {
        let g = |n: u8| format!("{:08x}-0000-4000-8000-{:012x}", n, n);
        let row = |tx: &str, sender: &str, at: i64, content: String| ArenaRow {
            tx_id: tx.into(),
            sender: sender.into(),
            block_time: at,
            content,
        };
        let now = 1_900_000_000_000;
        let at = now - 1_000;
        let good = stamp("2048", &g(1), "0000000000000001", 48, "LURDLURDLURD");
        let rows = vec![
            row("t1", "kaspa:alice", at, good.clone()),
            // Same game id again from Alice: not counted twice.
            row("t2", "kaspa:alice", at + 1, good.clone()),
            // Bob ties Alice later; a forged score; a bad seed.
            row("t3", "kaspa:bob", at + 2, stamp("2048", &g(2), "0000000000000001", 48, "LURDLURDLURD")),
            row("t4", "kaspa:carol", at, stamp("2048", &g(3), "0000000000000001", 4800, "LURDLURDLURD")),
            row("t5", "kaspa:dave", at, stamp("2048", &g(4), "000000000000000G", 48, "LURDLURDLURD")),
            // An uppercase game id, and a score of 0, are not stamps.
            row("t6", "kaspa:erin", at, stamp("2048", &g(0xab).to_uppercase(), "0000000000000001", 48, "LURDLURDLURD")),
            row("t7", "kaspa:erin", at, stamp("snake", &g(6), "0000000000000001", 0, "1D 3L")),
            row("t8", "kaspa:erin", at, "hello".into()),
        ];
        let snap = compute(rows, now);
        let (board, game, period) = snap.board("2048", Some("month")).unwrap();
        assert_eq!((game, period), ("2048", "month"));
        assert_eq!(board.games, 2);
        let who: Vec<(&str, u64, &str)> =
            board.players.iter().map(|p| (p.address.as_str(), p.score, p.tx.as_str())).collect();
        assert_eq!(who, vec![("kaspa:alice", 48, "t1"), ("kaspa:bob", 48, "t3")]);
        assert!(snap.board("snake", None).unwrap().0.players.is_empty());
        assert!(snap.board("tetris", None).is_none());
        // An old stamp counts for all time, not this month.
        let old = vec![row("t9", "kaspa:alice", period_since_ms(true, now) - 1, good)];
        let snap = compute(old, now);
        assert_eq!(snap.board("2048", Some("month")).unwrap().0.games, 0);
        let all = snap.board("2048", Some("all")).unwrap().0;
        assert_eq!(all.games, u64::from(period_since_ms(false, now) <= period_since_ms(true, now) - 1));
    }

    #[test]
    fn rows_judged_in_batches_give_the_full_replays_boards() {
        let g = |n: u8| format!("{:08x}-0000-4000-8000-{:012x}", n, n);
        let now = 1_900_000_000_000;
        let at = now - 1_000;
        let good = stamp("2048", &g(1), "0000000000000001", 48, "LURDLURDLURD");
        let rows = || {
            vec![
                // Alice's later copy arrives first; the earlier one (by block time) arrives late.
                (2, ArenaRow { tx_id: "t2".into(), sender: "kaspa:alice".into(), block_time: at + 5, content: good.clone() }),
                (3, ArenaRow { tx_id: "t3".into(), sender: "kaspa:bob".into(), block_time: at + 1, content: good.clone() }),
                (4, ArenaRow { tx_id: "t4".into(), sender: "kaspa:carol".into(), block_time: at, content: "spam".into() }),
                (5, ArenaRow { tx_id: "t1".into(), sender: "kaspa:alice".into(), block_time: at, content: good.clone() }),
            ]
        };
        let full = compute(rows().into_iter().map(|(_, r)| r).collect(), now);
        let mut memo = crate::snapshot::RowMemo::default();
        let mut batches = rows();
        let late = batches.split_off(3);
        let fresh = memo.unseen(batches);
        memo.judge(fresh, check_row);
        // The second read overlaps the first.
        let fresh = memo.unseen(rows().into_iter().skip(2).chain(late).collect());
        memo.judge(fresh, check_row);
        let incremental = snapshot(memo.counted(), now);
        let players = |s: &ArcadeSnapshot| {
            let b = s.board("2048", None).unwrap().0;
            (b.games, b.players.iter().map(|p| (p.address.clone(), p.tx.clone())).collect::<Vec<_>>())
        };
        assert_eq!(players(&incremental), players(&full));
        assert_eq!(players(&full).1, vec![("kaspa:alice".to_string(), "t1".to_string()), ("kaspa:bob".to_string(), "t3".to_string())]);
    }
}

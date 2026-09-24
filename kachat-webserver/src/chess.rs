//! Chess Tournaments (5.1) leaderboard engine — a faithful Rust port of the app's reference
//! reducer so the server lands on the same state every phone does. Ported verbatim (not
//! reinterpreted) from three files on `vsmirn0v/KaChat@main`:
//!   - KaChat/Utilities/ChessEngine.swift        (rules: move gen, checkmate, ...)
//!   - KaChat/Models/ChessTournamentModels.swift (wire message, tournament/game state)
//!   - KaChat/Services/ChessTournamentEngine.swift (the reducer + leaderboard)
//!
//! Input: the `#chess-arena` broadcast rows (kchat:1:bcast:chess-arena:<json>) in chain order.
//! Output: every tournament's bracket/results, and the aggregated leaderboard. See
//! CHESS_TOURNAMENTS.md §2-4, §6.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Rules engine (ChessEngine.swift)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    White,
    Black,
}
impl Color {
    fn opposite(self) -> Color {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PieceType {
    Pawn,
    Knight,
    Bishop,
    Rook,
    Queen,
    King,
}
impl PieceType {
    fn from_promotion_letter(letter: Option<&str>) -> Option<PieceType> {
        match letter.map(|s| s.to_lowercase()) {
            Some(ref s) if s == "q" => Some(PieceType::Queen),
            Some(ref s) if s == "r" => Some(PieceType::Rook),
            Some(ref s) if s == "b" => Some(PieceType::Bishop),
            Some(ref s) if s == "n" => Some(PieceType::Knight),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Piece {
    pub piece_type: PieceType,
    pub color: Color,
}

/// 0-indexed file (a=0..h=7) and rank (1=0..8=7). Uses i32 so offset math can go off-board.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Square {
    pub file: i32,
    pub rank: i32,
}
impl Square {
    fn new(file: i32, rank: i32) -> Square {
        Square { file, rank }
    }
    fn is_valid(&self) -> bool {
        self.file >= 0 && self.file <= 7 && self.rank >= 0 && self.rank <= 7
    }
    fn algebraic(&self) -> String {
        let file_char = (b'a' + self.file as u8) as char;
        format!("{}{}", file_char, self.rank + 1)
    }
    fn from_algebraic(s: &str) -> Option<Square> {
        let chars: Vec<char> = s.to_lowercase().chars().collect();
        if chars.len() != 2 {
            return None;
        }
        let file_ascii = chars[0] as u32;
        if !(97..=104).contains(&file_ascii) {
            return None;
        }
        let rank_digit = chars[1].to_digit(10)?;
        if !(1..=8).contains(&rank_digit) {
            return None;
        }
        Some(Square {
            file: file_ascii as i32 - 97,
            rank: rank_digit as i32 - 1,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Move {
    pub from: Square,
    pub to: Square,
    pub promotion: Option<PieceType>,
}

#[derive(Clone)]
pub struct Board {
    /// squares[rank][file], rank 0 = rank "1".
    pub squares: [[Option<Piece>; 8]; 8],
    pub side_to_move: Color,
    pub white_castle_k: bool,
    pub white_castle_q: bool,
    pub black_castle_k: bool,
    pub black_castle_q: bool,
    pub en_passant: Option<Square>,
}
impl Board {
    fn piece_at(&self, sq: Square) -> Option<Piece> {
        if !sq.is_valid() {
            return None;
        }
        self.squares[sq.rank as usize][sq.file as usize]
    }
    fn set_piece(&mut self, piece: Option<Piece>, sq: Square) {
        self.squares[sq.rank as usize][sq.file as usize] = piece;
    }
    fn can_castle_k(&self, color: Color) -> bool {
        if color == Color::White {
            self.white_castle_k
        } else {
            self.black_castle_k
        }
    }
    fn can_castle_q(&self, color: Color) -> bool {
        if color == Color::White {
            self.white_castle_q
        } else {
            self.black_castle_q
        }
    }
    fn set_castle_k(&mut self, value: bool, color: Color) {
        if color == Color::White {
            self.white_castle_k = value;
        } else {
            self.black_castle_k = value;
        }
    }
    fn set_castle_q(&mut self, value: bool, color: Color) {
        if color == Color::White {
            self.white_castle_q = value;
        } else {
            self.black_castle_q = value;
        }
    }
}

const KNIGHT_OFFSETS: [(i32, i32); 8] = [
    (1, 2),
    (2, 1),
    (2, -1),
    (1, -2),
    (-1, -2),
    (-2, -1),
    (-2, 1),
    (-1, 2),
];
const KING_OFFSETS: [(i32, i32); 8] = [
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
];
const DIAGONAL_DIRS: [(i32, i32); 4] = [(1, 1), (1, -1), (-1, 1), (-1, -1)];
const STRAIGHT_DIRS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

pub fn initial_board() -> Board {
    let mut squares: [[Option<Piece>; 8]; 8] = [[None; 8]; 8];
    let back_rank = [
        PieceType::Rook,
        PieceType::Knight,
        PieceType::Bishop,
        PieceType::Queen,
        PieceType::King,
        PieceType::Bishop,
        PieceType::Knight,
        PieceType::Rook,
    ];
    for file in 0..8 {
        squares[0][file] = Some(Piece {
            piece_type: back_rank[file],
            color: Color::White,
        });
        squares[1][file] = Some(Piece {
            piece_type: PieceType::Pawn,
            color: Color::White,
        });
        squares[6][file] = Some(Piece {
            piece_type: PieceType::Pawn,
            color: Color::Black,
        });
        squares[7][file] = Some(Piece {
            piece_type: back_rank[file],
            color: Color::Black,
        });
    }
    Board {
        squares,
        side_to_move: Color::White,
        white_castle_k: true,
        white_castle_q: true,
        black_castle_k: true,
        black_castle_q: true,
        en_passant: None,
    }
}

fn legal_moves(board: &Board) -> Vec<Move> {
    let mut moves: Vec<Move> = Vec::new();
    for rank in 0..8 {
        for file in 0..8 {
            let sq = Square::new(file, rank);
            if let Some(piece) = board.piece_at(sq) {
                if piece.color == board.side_to_move {
                    pseudo_legal_moves(piece, sq, board, &mut moves);
                }
            }
        }
    }
    moves
        .into_iter()
        .filter(|m| {
            let resulting = apply(*m, board);
            !is_king_in_check(board.side_to_move, &resulting)
        })
        .collect()
}

fn normalizing_promotion(m: Move, board: &Board) -> Move {
    if m.promotion.is_some() {
        return m;
    }
    let piece = match board.piece_at(m.from) {
        Some(p) if p.piece_type == PieceType::Pawn => p,
        _ => return m,
    };
    let back_rank = if piece.color == Color::White { 7 } else { 0 };
    if m.to.rank != back_rank {
        return m;
    }
    Move {
        from: m.from,
        to: m.to,
        promotion: Some(PieceType::Queen),
    }
}

fn is_legal(m: Move, board: &Board) -> bool {
    let normalized = normalizing_promotion(m, board);
    legal_moves(board).into_iter().any(|x| x == normalized)
}

fn is_king_in_check(color: Color, board: &Board) -> bool {
    match find_king(color, board) {
        Some(king_sq) => is_square_attacked(king_sq, color.opposite(), board),
        None => false,
    }
}

fn is_checkmate(board: &Board) -> bool {
    is_king_in_check(board.side_to_move, board) && legal_moves(board).is_empty()
}

fn is_stalemate(board: &Board) -> bool {
    !is_king_in_check(board.side_to_move, board) && legal_moves(board).is_empty()
}

fn is_insufficient_material(board: &Board) -> bool {
    let mut white_minors: Vec<Square> = Vec::new();
    let mut black_minors: Vec<Square> = Vec::new();
    for rank in 0..8 {
        for file in 0..8 {
            let sq = Square::new(file, rank);
            let piece = match board.piece_at(sq) {
                Some(p) => p,
                None => continue,
            };
            match piece.piece_type {
                PieceType::King => continue,
                PieceType::Bishop | PieceType::Knight => {
                    if piece.color == Color::White {
                        white_minors.push(sq);
                    } else {
                        black_minors.push(sq);
                    }
                }
                PieceType::Pawn | PieceType::Rook | PieceType::Queen => return false,
            }
        }
    }
    match (white_minors.len(), black_minors.len()) {
        (0, 0) => true,
        (1, 0) | (0, 1) => true,
        (1, 1) => {
            let white = white_minors[0];
            let black = black_minors[0];
            if board.piece_at(white).map(|p| p.piece_type) != Some(PieceType::Bishop)
                || board.piece_at(black).map(|p| p.piece_type) != Some(PieceType::Bishop)
            {
                return false;
            }
            is_light_square(white) == is_light_square(black)
        }
        _ => false,
    }
}

fn is_light_square(sq: Square) -> bool {
    (sq.file + sq.rank) % 2 == 1
}

fn find_king(color: Color, board: &Board) -> Option<Square> {
    for rank in 0..8 {
        for file in 0..8 {
            let sq = Square::new(file, rank);
            if let Some(p) = board.piece_at(sq) {
                if p.piece_type == PieceType::King && p.color == color {
                    return Some(sq);
                }
            }
        }
    }
    None
}

fn is_square_attacked(sq: Square, color: Color, board: &Board) -> bool {
    // Pawns
    let pawn_rank_offset = if color == Color::White { -1 } else { 1 };
    for file_offset in [-1, 1] {
        let from = Square::new(sq.file + file_offset, sq.rank + pawn_rank_offset);
        if let Some(p) = board.piece_at(from) {
            if p.piece_type == PieceType::Pawn && p.color == color {
                return true;
            }
        }
    }
    // Knights
    for (df, dr) in KNIGHT_OFFSETS {
        let from = Square::new(sq.file + df, sq.rank + dr);
        if let Some(p) = board.piece_at(from) {
            if p.piece_type == PieceType::Knight && p.color == color {
                return true;
            }
        }
    }
    // King
    for (df, dr) in KING_OFFSETS {
        let from = Square::new(sq.file + df, sq.rank + dr);
        if let Some(p) = board.piece_at(from) {
            if p.piece_type == PieceType::King && p.color == color {
                return true;
            }
        }
    }
    // Sliding
    for (df, dr) in DIAGONAL_DIRS {
        if sliding_attacker(sq, (df, dr), board, color, &[PieceType::Bishop, PieceType::Queen]) {
            return true;
        }
    }
    for (df, dr) in STRAIGHT_DIRS {
        if sliding_attacker(sq, (df, dr), board, color, &[PieceType::Rook, PieceType::Queen]) {
            return true;
        }
    }
    false
}

fn sliding_attacker(
    sq: Square,
    dir: (i32, i32),
    board: &Board,
    color: Color,
    types: &[PieceType],
) -> bool {
    let mut current = Square::new(sq.file + dir.0, sq.rank + dir.1);
    while current.is_valid() {
        if let Some(p) = board.piece_at(current) {
            return p.color == color && types.contains(&p.piece_type);
        }
        current = Square::new(current.file + dir.0, current.rank + dir.1);
    }
    false
}

fn pseudo_legal_moves(piece: Piece, sq: Square, board: &Board, out: &mut Vec<Move>) {
    match piece.piece_type {
        PieceType::Pawn => pawn_moves(piece.color, sq, board, out),
        PieceType::Knight => stepping_moves(&KNIGHT_OFFSETS, piece.color, sq, board, out),
        PieceType::Bishop => sliding_moves(&DIAGONAL_DIRS, piece.color, sq, board, out),
        PieceType::Rook => sliding_moves(&STRAIGHT_DIRS, piece.color, sq, board, out),
        PieceType::Queen => {
            sliding_moves(&DIAGONAL_DIRS, piece.color, sq, board, out);
            sliding_moves(&STRAIGHT_DIRS, piece.color, sq, board, out);
        }
        PieceType::King => king_moves(piece.color, sq, board, out),
    }
}

fn pawn_moves(color: Color, sq: Square, board: &Board, out: &mut Vec<Move>) {
    let direction = if color == Color::White { 1 } else { -1 };
    let start_rank = if color == Color::White { 1 } else { 6 };
    let back_rank = if color == Color::White { 7 } else { 0 };

    let add_move = |to: Square, out: &mut Vec<Move>| {
        if !to.is_valid() {
            return;
        }
        if to.rank == back_rank {
            for promo in [
                PieceType::Queen,
                PieceType::Rook,
                PieceType::Bishop,
                PieceType::Knight,
            ] {
                out.push(Move {
                    from: sq,
                    to,
                    promotion: Some(promo),
                });
            }
        } else {
            out.push(Move {
                from: sq,
                to,
                promotion: None,
            });
        }
    };

    let single_push = Square::new(sq.file, sq.rank + direction);
    if single_push.is_valid() && board.piece_at(single_push).is_none() {
        add_move(single_push, out);
        let double_push = Square::new(sq.file, sq.rank + direction * 2);
        if sq.rank == start_rank && board.piece_at(double_push).is_none() {
            out.push(Move {
                from: sq,
                to: double_push,
                promotion: None,
            });
        }
    }

    for file_offset in [-1, 1] {
        let target = Square::new(sq.file + file_offset, sq.rank + direction);
        if !target.is_valid() {
            continue;
        }
        if let Some(occupant) = board.piece_at(target) {
            if occupant.color != color {
                add_move(target, out);
            }
        } else if Some(target) == board.en_passant {
            out.push(Move {
                from: sq,
                to: target,
                promotion: None,
            });
        }
    }
}

fn stepping_moves(offsets: &[(i32, i32)], color: Color, sq: Square, board: &Board, out: &mut Vec<Move>) {
    for (df, dr) in offsets {
        let target = Square::new(sq.file + df, sq.rank + dr);
        if !target.is_valid() {
            continue;
        }
        if let Some(occupant) = board.piece_at(target) {
            if occupant.color == color {
                continue;
            }
        }
        out.push(Move {
            from: sq,
            to: target,
            promotion: None,
        });
    }
}

fn sliding_moves(directions: &[(i32, i32)], color: Color, sq: Square, board: &Board, out: &mut Vec<Move>) {
    for (df, dr) in directions {
        let mut target = Square::new(sq.file + df, sq.rank + dr);
        while target.is_valid() {
            if let Some(occupant) = board.piece_at(target) {
                if occupant.color != color {
                    out.push(Move {
                        from: sq,
                        to: target,
                        promotion: None,
                    });
                }
                break;
            }
            out.push(Move {
                from: sq,
                to: target,
                promotion: None,
            });
            target = Square::new(target.file + df, target.rank + dr);
        }
    }
}

fn king_moves(color: Color, sq: Square, board: &Board, out: &mut Vec<Move>) {
    stepping_moves(&KING_OFFSETS, color, sq, board, out);
    if is_square_attacked(sq, color.opposite(), board) {
        return;
    }
    let rank = if color == Color::White { 0 } else { 7 };
    if board.can_castle_k(color)
        && board.piece_at(Square::new(5, rank)).is_none()
        && board.piece_at(Square::new(6, rank)).is_none()
        && !is_square_attacked(Square::new(5, rank), color.opposite(), board)
        && !is_square_attacked(Square::new(6, rank), color.opposite(), board)
    {
        out.push(Move {
            from: sq,
            to: Square::new(6, rank),
            promotion: None,
        });
    }
    if board.can_castle_q(color)
        && board.piece_at(Square::new(3, rank)).is_none()
        && board.piece_at(Square::new(2, rank)).is_none()
        && board.piece_at(Square::new(1, rank)).is_none()
        && !is_square_attacked(Square::new(3, rank), color.opposite(), board)
        && !is_square_attacked(Square::new(2, rank), color.opposite(), board)
    {
        out.push(Move {
            from: sq,
            to: Square::new(2, rank),
            promotion: None,
        });
    }
}

fn apply(m: Move, board: &Board) -> Board {
    let mut result = board.clone();
    let piece = match result.piece_at(m.from) {
        Some(p) => p,
        None => return result,
    };

    let is_en_passant_capture = piece.piece_type == PieceType::Pawn
        && Some(m.to) == board.en_passant
        && result.piece_at(m.to).is_none();
    let is_castle = piece.piece_type == PieceType::King && (m.to.file - m.from.file).abs() == 2;

    result.set_piece(None, m.from);
    let moved_piece = Piece {
        piece_type: m.promotion.unwrap_or(piece.piece_type),
        color: piece.color,
    };
    result.set_piece(Some(moved_piece), m.to);

    if is_en_passant_capture {
        let captured_pawn_square = Square::new(m.to.file, m.from.rank);
        result.set_piece(None, captured_pawn_square);
    }

    if is_castle {
        let rank = m.from.rank;
        if m.to.file == 6 {
            result.set_piece(None, Square::new(7, rank));
            result.set_piece(
                Some(Piece {
                    piece_type: PieceType::Rook,
                    color: piece.color,
                }),
                Square::new(5, rank),
            );
        } else {
            result.set_piece(None, Square::new(0, rank));
            result.set_piece(
                Some(Piece {
                    piece_type: PieceType::Rook,
                    color: piece.color,
                }),
                Square::new(3, rank),
            );
        }
    }

    if piece.piece_type == PieceType::King {
        result.set_castle_k(false, piece.color);
        result.set_castle_q(false, piece.color);
    }
    revoke_castling_right_if_corner_touched(m.from, &mut result);
    revoke_castling_right_if_corner_touched(m.to, &mut result);

    if piece.piece_type == PieceType::Pawn && (m.to.rank - m.from.rank).abs() == 2 {
        result.en_passant = Some(Square::new(m.from.file, (m.from.rank + m.to.rank) / 2));
    } else {
        result.en_passant = None;
    }

    result.side_to_move = board.side_to_move.opposite();
    result
}

fn revoke_castling_right_if_corner_touched(sq: Square, board: &mut Board) {
    match (sq.file, sq.rank) {
        (0, 0) => board.white_castle_q = false,
        (7, 0) => board.white_castle_k = false,
        (0, 7) => board.black_castle_q = false,
        (7, 7) => board.black_castle_k = false,
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Wire protocol + state (ChessTournamentModels.swift)
// ---------------------------------------------------------------------------

pub const ARENA_CHANNEL: &str = "chess-arena";
const PLAYER_COUNT: usize = 8;
const CLOCK_MS: i64 = 5 * 60 * 1000;
/// A seat in a waiting room lasts this long; if the room hasn't filled it's given back.
const SEAT_TTL_MS: i64 = 5 * 60 * 1000;

const PUBLIC_ID_PREFIX: &str = "public-";
const DUEL_ID_PREFIX: &str = "duel-";
/// Creator code for private tournaments (§2.1). `k` on the wire is the first 24 hex chars of
/// SHA-256(code + ":" + id); the code itself never appears on chain. Keep in sync with the apps'
/// ChessTournamentCodec.privateCreateCode.
const PRIVATE_CREATE_CODE: &str = "KACHAT-CHESS";

fn number_of(id: &str, prefix: &str) -> Option<i64> {
    let rest = id.strip_prefix(prefix)?;
    let n: i64 = rest.parse().ok()?;
    if n >= 1 {
        Some(n)
    } else {
        None
    }
}
fn public_number(id: &str) -> Option<i64> {
    number_of(id, PUBLIC_ID_PREFIX)
}
fn duel_number(id: &str) -> Option<i64> {
    number_of(id, DUEL_ID_PREFIX)
}
fn public_id(n: i64) -> String {
    format!("{}{}", PUBLIC_ID_PREFIX, n)
}
fn duel_id(n: i64) -> String {
    format!("{}{}", DUEL_ID_PREFIX, n)
}
fn is_public_id(id: &str) -> bool {
    public_number(id).is_some() || duel_number(id).is_some()
}

fn create_key(code: &str, id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}", code.trim().to_uppercase(), id).as_bytes());
    let hex: String = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect();
    hex[..24].to_string()
}
fn is_valid_create_key(key: Option<&str>, id: &str) -> bool {
    match key {
        Some(k) => k == create_key(PRIVATE_CREATE_CODE, id),
        None => false,
    }
}

#[derive(Deserialize)]
struct TournamentMessage {
    #[serde(rename = "type", default)]
    msg_type: String,
    #[serde(default)]
    v: i64,
    t: String,
    a: String,
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    g: Option<String>,
    #[serde(default)]
    n: Option<i64>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    promo: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    text: Option<String>,
    /// Private tournaments only: proof the creator holds the creator code.
    #[serde(default)]
    k: Option<String>,
    /// create only: 2 (a 1v1) or 8 (a tournament). Absent = 8.
    #[serde(default)]
    p: Option<i64>,
}

/// Cheap gate first (runs over every arena row), then decode. Mirrors ChessTournamentCodec.decode.
fn decode(content: &str) -> Option<TournamentMessage> {
    if content.chars().count() > 2048 || !content.starts_with('{') || !content.contains("\"chess_t\"") {
        return None;
    }
    let message: TournamentMessage = serde_json::from_str(content).ok()?;
    if message.msg_type != "chess_t" || message.v != 1 || message.t.is_empty() || message.t.len() > 64 {
        return None;
    }
    Some(message)
}

/// One arena row that decoded as a tournament message.
pub struct ArenaEvent {
    pub tx_id: String,
    pub sender: String,
    pub block_time: i64,
    message: TournamentMessage,
}

/// Build an ArenaEvent from a raw broadcast row, or None if it isn't a tournament message.
pub fn arena_event(tx_id: String, sender: String, block_time: i64, content: &str) -> Option<ArenaEvent> {
    let message = decode(content)?;
    Some(ArenaEvent {
        tx_id,
        sender,
        block_time,
        message,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Checkmate,
    Resignation,
    Timeout,
    DrawTiebreak,
}

struct Game {
    round: i32,
    index: i32,
    white: String,
    black: String,
    board: Board,
    moves_count: usize,
    white_used_ms: i64,
    black_used_ms: i64,
    last_event_at: i64,
    winner: Option<String>,
    #[allow(dead_code)]
    outcome: Option<Outcome>,
    ended_at: Option<i64>,
    position_counts: HashMap<String, i32>,
    halfmove_clock: i32,
}
impl Game {
    fn is_over(&self) -> bool {
        self.winner.is_some()
    }
    fn side_to_move(&self) -> Color {
        self.board.side_to_move
    }
    fn player_to_move(&self) -> &str {
        if self.side_to_move() == Color::White {
            &self.white
        } else {
            &self.black
        }
    }
    fn address_of(&self, color: Color) -> String {
        if color == Color::White {
            self.white.clone()
        } else {
            self.black.clone()
        }
    }
    fn color_of(&self, address: &str) -> Option<Color> {
        if address == self.white {
            Some(Color::White)
        } else if address == self.black {
            Some(Color::Black)
        } else {
            None
        }
    }
    fn used_ms(&self, color: Color) -> i64 {
        if color == Color::White {
            self.white_used_ms
        } else {
            self.black_used_ms
        }
    }
}

struct Tournament {
    creator: String,
    created_at: i64,
    /// 2 for a 1v1, 8 for a tournament.
    capacity: i32,
    started_at: Option<i64>,
    cancelled: bool,
    players: Vec<String>,
    joined_at: HashMap<String, i64>,
    games: HashMap<String, Game>,
    white_count: HashMap<String, i32>,
}
impl Tournament {
    fn is_duel(&self) -> bool {
        self.capacity == 2
    }
    fn rounds(&self) -> i32 {
        if self.is_duel() {
            1
        } else {
            3
        }
    }
    fn final_game_id(&self) -> String {
        format!("{}-0", self.rounds())
    }
    fn is_full(&self) -> bool {
        self.players.len() >= self.capacity as usize
    }
    fn seed_of(&self, address: &str) -> Option<usize> {
        self.players.iter().position(|p| p == address).map(|i| i + 1)
    }
    fn game(&self, round: i32, index: i32) -> Option<&Game> {
        self.games.get(&format!("{}-{}", round, index))
    }
    fn champion(&self) -> Option<String> {
        self.games.get(&self.final_game_id()).and_then(|g| g.winner.clone())
    }
    fn is_finished(&self) -> bool {
        self.games
            .get(&self.final_game_id())
            .map(|g| g.is_over())
            .unwrap_or(false)
    }
    fn is_live(&self) -> bool {
        !self.cancelled && self.started_at.is_some() && !self.is_finished()
    }
    fn is_open(&self) -> bool {
        !self.cancelled && self.started_at.is_none()
    }
}

/// A leaderboard row (§6): two boards' worth of stats per address. `wins`/`losses` are the
/// combined totals; `duel*` are 1v1 games (duel-N + private 1v1), `tournamentGame*` are games
/// inside 8-player tournaments, and `tournamentsWon`/`tournamentsPlayed` count 8-player
/// tournaments only (a 1v1 never counts as a tournament).
pub struct LeaderboardRow {
    pub address: String,
    pub wins: i64,
    pub losses: i64,
    pub duel_wins: i64,
    pub duel_losses: i64,
    pub tournament_game_wins: i64,
    pub tournament_game_losses: i64,
    pub tournaments_played: i64,
    pub tournaments_won: i64,
    /// Whole 8-player tournaments the player was knocked out of (§6, KaChat@main 7049e70). The
    /// tournament board is now "whole tournaments only" — champions (won) vs knocked-out (lost).
    pub tournaments_lost: i64,
    pub last_played_at: i64,
}

// ---------------------------------------------------------------------------
// Reducer (ChessTournamentEngine.swift)
// ---------------------------------------------------------------------------

fn reduce(mut events: Vec<ArenaEvent>) -> HashMap<String, Tournament> {
    // Chain order: block time, then txid.
    events.sort_by(|a, b| {
        if a.block_time != b.block_time {
            a.block_time.cmp(&b.block_time)
        } else {
            a.tx_id.cmp(&b.tx_id)
        }
    });
    let mut tournaments: HashMap<String, Tournament> = HashMap::new();
    for event in events {
        apply_event(event, &mut tournaments);
    }
    tournaments
}

fn apply_event(event: ArenaEvent, tournaments: &mut HashMap<String, Tournament>) {
    let m = &event.message;
    match m.a.as_str() {
        "create" => {
            // Public rooms are never created by message. A private tournament (8) needs the
            // creator key; a private 1v1 (2) is open to anyone.
            let capacity: i32 = if m.p == Some(2) { 2 } else { PLAYER_COUNT as i32 };
            if tournaments.contains_key(&m.t) || is_public_id(&m.t) {
                return;
            }
            if !(capacity == 2 || is_valid_create_key(m.k.as_deref(), &m.t)) {
                return;
            }
            let mut joined = HashMap::new();
            joined.insert(event.sender.clone(), event.block_time);
            let t = Tournament {
                creator: event.sender.clone(),
                created_at: event.block_time,
                capacity,
                started_at: None,
                cancelled: false,
                players: vec![event.sender.clone()],
                joined_at: joined,
                games: HashMap::new(),
                white_count: HashMap::new(),
            };
            tournaments.insert(m.t.clone(), t);
        }
        "join" => {
            if !tournaments.contains_key(&m.t) {
                // The first join opens a public room — whichever number it names. There used to be
                // a rule that room N opened only once room N-1 was full, but that made every
                // viewer's result depend on holding the complete history back to room 1: a viewer
                // missing the early rooms (indexer window / retention / a late backfill) rejected
                // every later room outright. The app dropped that gate (KaChat@main 7049e70); an
                // indexer that keeps it undercounts exactly the 1v1s/tournaments the phones show.
                let capacity = if public_number(&m.t).is_some() {
                    PLAYER_COUNT as i32
                } else if duel_number(&m.t).is_some() {
                    2
                } else {
                    return;
                };
                tournaments.insert(
                    m.t.clone(),
                    Tournament {
                        creator: event.sender.clone(),
                        created_at: event.block_time,
                        capacity,
                        started_at: None,
                        cancelled: false,
                        players: Vec::new(),
                        joined_at: HashMap::new(),
                        games: HashMap::new(),
                        white_count: HashMap::new(),
                    },
                );
            }
            let t = match tournaments.get_mut(&m.t) {
                Some(t) if t.is_open() => t,
                _ => return,
            };
            // Seats that ran out while the room waited are given back first (judged at this
            // join's block time — the same on every phone), then the joiner is seated.
            expire_seats(t, event.block_time);
            if t.players.contains(&event.sender) {
                return;
            }
            t.players.push(event.sender.clone());
            t.joined_at.insert(event.sender.clone(), event.block_time);
            if t.players.len() == t.capacity as usize {
                start(t, event.block_time);
            }
        }
        "leave" => {
            let t = match tournaments.get_mut(&m.t) {
                Some(t) if t.is_open() => t,
                _ => return,
            };
            if let Some(idx) = t.players.iter().position(|p| p == &event.sender) {
                t.players.remove(idx);
                t.joined_at.remove(&event.sender);
            }
        }
        "cancel" => {
            if let Some(t) = tournaments.get_mut(&m.t) {
                if t.is_open() && !is_public_id(&m.t) && t.creator == event.sender {
                    t.cancelled = true;
                }
            }
        }
        "move" => apply_move(&event, tournaments),
        "resign" => {
            let t = match tournaments.get_mut(&m.t) {
                Some(t) if t.is_live() => t,
                _ => return,
            };
            let game_id = match &m.g {
                Some(g) => g.clone(),
                None => return,
            };
            let over = {
                let game = match t.games.get_mut(&game_id) {
                    Some(g) if !g.is_over() => g,
                    _ => return,
                };
                let color = match game.color_of(&event.sender) {
                    Some(c) => c,
                    None => return,
                };
                let winner = game.address_of(color.opposite());
                finish(game, winner, Outcome::Resignation, event.block_time);
                true
            };
            if over {
                advance_after(t, &game_id);
            }
        }
        "claim" => {
            let t = match tournaments.get_mut(&m.t) {
                Some(t) if t.is_live() => t,
                _ => return,
            };
            let game_id = match &m.g {
                Some(g) => g.clone(),
                None => return,
            };
            let over = {
                let game = match t.games.get_mut(&game_id) {
                    Some(g) if !g.is_over() => g,
                    _ => return,
                };
                match game.color_of(&event.sender) {
                    Some(c) if c != game.side_to_move() => {}
                    _ => return,
                }
                let elapsed = (event.block_time - game.last_event_at).max(0);
                let remaining = CLOCK_MS - game.used_ms(game.side_to_move());
                if elapsed < remaining {
                    return;
                }
                if game.side_to_move() == Color::White {
                    game.white_used_ms = CLOCK_MS;
                } else {
                    game.black_used_ms = CLOCK_MS;
                }
                finish(game, event.sender.clone(), Outcome::Timeout, event.block_time);
                true
            };
            if over {
                advance_after(t, &game_id);
            }
        }
        // "chat" and unknown: no effect on bracket/leaderboard state.
        _ => {}
    }
}

fn apply_move(event: &ArenaEvent, tournaments: &mut HashMap<String, Tournament>) {
    let m = &event.message;
    let t = match tournaments.get_mut(&m.t) {
        Some(t) if t.is_live() => t,
        _ => return,
    };
    let game_id = match &m.g {
        Some(g) => g.clone(),
        None => return,
    };
    let game_over = {
        let game = match t.games.get_mut(&game_id) {
            Some(g) if !g.is_over() => g,
            _ => return,
        };
        if game.player_to_move() != event.sender {
            return;
        }
        match m.n {
            Some(n) if n == game.moves_count as i64 + 1 => {}
            _ => return,
        }
        let (from, to) = match (
            m.from.as_deref().and_then(Square::from_algebraic),
            m.to.as_deref().and_then(Square::from_algebraic),
        ) {
            (Some(f), Some(t)) => (f, t),
            _ => return,
        };
        let elapsed = (event.block_time - game.last_event_at).max(0);
        let remaining = CLOCK_MS - game.used_ms(game.side_to_move());
        if elapsed >= remaining {
            return;
        }
        let mut mv = Move {
            from,
            to,
            promotion: PieceType::from_promotion_letter(m.promo.as_deref()),
        };
        mv = normalizing_promotion(mv, &game.board);
        if !is_legal(mv, &game.board) {
            return;
        }
        let piece = match game.board.piece_at(from) {
            Some(p) => p,
            None => return,
        };
        let is_en_passant = piece.piece_type == PieceType::Pawn
            && Some(to) == game.board.en_passant
            && game.board.piece_at(to).is_none();
        let captured: Option<PieceType> = match game.board.piece_at(to) {
            Some(p) => Some(p.piece_type),
            None if is_en_passant => Some(PieceType::Pawn),
            None => None,
        };
        let mover = game.side_to_move();
        game.board = apply(mv, &game.board);
        if mover == Color::White {
            game.white_used_ms += elapsed;
        } else {
            game.black_used_ms += elapsed;
        }
        game.last_event_at = event.block_time;
        game.moves_count += 1;
        game.halfmove_clock = if piece.piece_type == PieceType::Pawn || captured.is_some() {
            0
        } else {
            game.halfmove_clock + 1
        };
        let key = position_key(&game.board);
        *game.position_counts.entry(key.clone()).or_insert(0) += 1;
        let count = *game.position_counts.get(&key).unwrap_or(&0);

        if is_checkmate(&game.board) {
            let winner = game.address_of(mover);
            finish(game, winner, Outcome::Checkmate, event.block_time);
        } else if is_stalemate(&game.board) {
            finish_draw(game, event.block_time);
        } else if is_insufficient_material(&game.board) {
            finish_draw(game, event.block_time);
        } else if game.halfmove_clock >= 100 {
            finish_draw(game, event.block_time);
        } else if count >= 3 {
            finish_draw(game, event.block_time);
        }
        game.is_over()
    };
    if game_over {
        advance_after(t, &game_id);
    }
}

fn expire_seats(t: &mut Tournament, time: i64) {
    let kept: Vec<String> = t
        .players
        .iter()
        .filter(|p| t.joined_at.get(*p).copied().unwrap_or(t.created_at) + SEAT_TTL_MS > time)
        .cloned()
        .collect();
    if kept.len() == t.players.len() {
        return;
    }
    for gone in &t.players {
        if !kept.contains(gone) {
            t.joined_at.remove(gone);
        }
    }
    t.players = kept;
}

// MARK: Bracket

fn start(t: &mut Tournament, time: i64) {
    t.started_at = Some(time);
    let seeds = t.players.clone();
    let pairs: Vec<(usize, usize)> = if t.is_duel() {
        vec![(0, 1)]
    } else {
        vec![(0, 7), (1, 6), (2, 5), (3, 4)]
    };
    for (index, pair) in pairs.iter().enumerate() {
        let white = seeds[pair.0].clone();
        let black = seeds[pair.1].clone();
        let g = make_game(1, index as i32, white.clone(), black, time);
        t.games.insert(format!("1-{}", index), g);
        *t.white_count.entry(white).or_insert(0) += 1;
    }
}

fn advance_after(t: &mut Tournament, game_id: &str) {
    let rounds = t.rounds();
    let (round, index, ended_at) = {
        let game = match t.games.get(game_id) {
            Some(g) => g,
            None => return,
        };
        match game.ended_at {
            Some(e) if game.round < rounds => (game.round, game.index, e),
            _ => return,
        }
    };
    let next_round = round + 1;
    let next_index = index / 2;
    let feeder_a = t.game(round, next_index * 2).and_then(|g| g.winner.clone());
    let feeder_b = t.game(round, next_index * 2 + 1).and_then(|g| g.winner.clone());
    let (a, b) = match (feeder_a, feeder_b) {
        (Some(a), Some(b)) => (a, b),
        _ => return,
    };
    if t.game(next_round, next_index).is_some() {
        return;
    }
    let whites_a = *t.white_count.get(&a).unwrap_or(&0);
    let whites_b = *t.white_count.get(&b).unwrap_or(&0);
    let a_is_white = if whites_a != whites_b {
        whites_a < whites_b
    } else {
        t.seed_of(&a).unwrap_or(99) < t.seed_of(&b).unwrap_or(99)
    };
    let (white, black) = if a_is_white { (a, b) } else { (b, a) };
    let g = make_game(next_round, next_index, white.clone(), black, ended_at);
    t.games.insert(format!("{}-{}", next_round, next_index), g);
    *t.white_count.entry(white).or_insert(0) += 1;
}

fn make_game(round: i32, index: i32, white: String, black: String, time: i64) -> Game {
    let board = initial_board();
    let mut position_counts = HashMap::new();
    position_counts.insert(position_key(&board), 1);
    Game {
        round,
        index,
        white,
        black,
        board,
        moves_count: 0,
        white_used_ms: 0,
        black_used_ms: 0,
        last_event_at: time,
        winner: None,
        outcome: None,
        ended_at: None,
        position_counts,
        halfmove_clock: 0,
    }
}

fn finish(game: &mut Game, winner: String, outcome: Outcome, time: i64) {
    game.winner = Some(winner);
    game.outcome = Some(outcome);
    game.ended_at = Some(time);
}

/// A draw on the board: more clock left advances; equal -> black.
fn finish_draw(game: &mut Game, time: i64) {
    let white_left = CLOCK_MS - game.white_used_ms;
    let black_left = CLOCK_MS - game.black_used_ms;
    let winner = if white_left > black_left {
        game.white.clone()
    } else {
        game.black.clone()
    };
    finish(game, winner, Outcome::DrawTiebreak, time);
}

fn position_key(board: &Board) -> String {
    let mut key = String::new();
    for rank in 0..8 {
        for file in 0..8 {
            if let Some(piece) = board.squares[rank][file] {
                let letter = match piece.piece_type {
                    PieceType::Pawn => "p",
                    PieceType::Knight => "n",
                    PieceType::Bishop => "b",
                    PieceType::Rook => "r",
                    PieceType::Queen => "q",
                    PieceType::King => "k",
                };
                if piece.color == Color::White {
                    key.push_str(&letter.to_uppercase());
                } else {
                    key.push_str(letter);
                }
            } else {
                key.push('.');
            }
        }
    }
    key.push(if board.side_to_move == Color::White { 'w' } else { 'b' });
    key.push(if board.white_castle_k { 'K' } else { '-' });
    key.push(if board.white_castle_q { 'Q' } else { '-' });
    key.push(if board.black_castle_k { 'k' } else { '-' });
    key.push(if board.black_castle_q { 'q' } else { '-' });
    match board.en_passant {
        Some(sq) => key.push_str(&sq.algebraic()),
        None => key.push('-'),
    }
    key
}

// MARK: Leaderboard (§6 two boards, realized from the reduced state)

fn leaderboard(tournaments: &HashMap<String, Tournament>) -> Vec<LeaderboardRow> {
    let mut rows: HashMap<String, LeaderboardRow> = HashMap::new();
    fn row<'a>(rows: &'a mut HashMap<String, LeaderboardRow>, address: &str) -> &'a mut LeaderboardRow {
        rows.entry(address.to_string()).or_insert_with(|| LeaderboardRow {
            address: address.to_string(),
            wins: 0,
            losses: 0,
            duel_wins: 0,
            duel_losses: 0,
            tournament_game_wins: 0,
            tournament_game_losses: 0,
            tournaments_played: 0,
            tournaments_won: 0,
            tournaments_lost: 0,
            last_played_at: 0,
        })
    }
    for t in tournaments.values() {
        let started_at = match t.started_at {
            Some(s) => s,
            None => continue,
        };
        let is_duel = t.is_duel();
        // A 1v1 is not a tournament: it counts on the 1v1 board only. So the tournament-level
        // player pass (tournamentsPlayed + lastPlayedAt from the start) runs for 8-player
        // brackets only — a duel player's lastPlayedAt comes solely from their finished games.
        if !is_duel {
            for player in &t.players {
                let r = row(&mut rows, player);
                r.tournaments_played += 1;
                r.last_played_at = r.last_played_at.max(started_at);
            }
        }
        for game in t.games.values() {
            if !game.is_over() {
                continue;
            }
            let winner = match &game.winner {
                Some(w) => w.clone(),
                None => continue,
            };
            let loser = if winner == game.white {
                game.black.clone()
            } else {
                game.white.clone()
            };
            let ended = game.ended_at.unwrap_or(0);
            {
                let w = row(&mut rows, &winner);
                w.wins += 1;
                w.last_played_at = w.last_played_at.max(ended);
                if is_duel {
                    w.duel_wins += 1;
                } else {
                    w.tournament_game_wins += 1;
                }
            }
            {
                let l = row(&mut rows, &loser);
                l.losses += 1;
                l.last_played_at = l.last_played_at.max(ended);
                if is_duel {
                    l.duel_losses += 1;
                } else {
                    // A tournament game loss in single-elim IS elimination from the tournament.
                    l.tournament_game_losses += 1;
                    l.tournaments_lost += 1;
                }
            }
        }
        // Tournaments won: champion of an 8-player bracket only.
        if !is_duel {
            if let Some(champion) = t.champion() {
                row(&mut rows, &champion).tournaments_won += 1;
            }
        }
    }
    let mut out: Vec<LeaderboardRow> = rows.into_values().collect();
    // §6: most wins first, fewest losses breaking ties (the phone re-sorts each board itself).
    out.sort_by(|a, b| {
        if a.wins != b.wins {
            return b.wins.cmp(&a.wins);
        }
        if a.losses != b.losses {
            return a.losses.cmp(&b.losses);
        }
        if a.last_played_at != b.last_played_at {
            return b.last_played_at.cmp(&a.last_played_at);
        }
        a.address.cmp(&b.address)
    });
    out
}

// ---------------------------------------------------------------------------
// Public entry points for the HTTP layer
// ---------------------------------------------------------------------------

/// A raw chess-arena broadcast row (as read from kachat_broadcasts).
pub struct ArenaRow {
    pub tx_id: String,
    pub sender: String,
    pub block_time: i64,
    pub content: String,
}

/// The precomputed lobby row for GET /chess/tournaments.
pub struct TournamentSummary {
    pub id: String,
    pub status: String, // "open" | "live" | "done" | "cancelled"
    pub capacity: i32,  // 2 = 1v1, 8 = tournament
    pub players: Vec<String>,
    pub started_at: Option<i64>,
    pub champion: Option<String>,
}

/// Replay the arena once and return both the leaderboard and the lobby list.
pub fn compute_all(rows: Vec<ArenaRow>) -> (Vec<LeaderboardRow>, Vec<TournamentSummary>) {
    let events: Vec<ArenaEvent> = rows
        .into_iter()
        .filter_map(|r| arena_event(r.tx_id, r.sender, r.block_time, &r.content))
        .collect();
    let tournaments = reduce(events);
    let board = leaderboard(&tournaments);

    let mut lobby: Vec<TournamentSummary> = tournaments
        .into_iter()
        .map(|(id, t)| {
            let status = if t.cancelled {
                "cancelled"
            } else if t.is_finished() {
                "done"
            } else if t.is_live() {
                "live"
            } else {
                "open"
            };
            TournamentSummary {
                id,
                status: status.to_string(),
                capacity: t.capacity,
                players: t.players.clone(),
                started_at: t.started_at,
                champion: t.champion(),
            }
        })
        .collect();
    lobby.sort_by(|a, b| {
        b.started_at
            .unwrap_or(0)
            .cmp(&a.started_at.unwrap_or(0))
            .then(a.id.cmp(&b.id))
    });
    (board, lobby)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(tx: &str, sender: &str, bt: i64, json: &str) -> ArenaRow {
        ArenaRow {
            tx_id: tx.to_string(),
            sender: sender.to_string(),
            block_time: bt,
            content: json.to_string(),
        }
    }
    fn events(rows: Vec<ArenaRow>) -> HashMap<String, Tournament> {
        reduce(
            rows.into_iter()
                .filter_map(|r| arena_event(r.tx_id, r.sender, r.block_time, &r.content))
                .collect(),
        )
    }

    #[test]
    fn initial_position_has_twenty_legal_moves() {
        assert_eq!(legal_moves(&initial_board()).len(), 20);
    }

    #[test]
    fn fools_mate_is_checkmate() {
        let mut b = initial_board();
        for (f, t, who) in [
            ("f2", "f3", Color::White),
            ("e7", "e5", Color::Black),
            ("g2", "g4", Color::White),
            ("d8", "h4", Color::Black),
        ] {
            let mv = Move {
                from: Square::from_algebraic(f).unwrap(),
                to: Square::from_algebraic(t).unwrap(),
                promotion: None,
            };
            assert_eq!(b.side_to_move, who);
            assert!(is_legal(mv, &b));
            b = apply(mv, &b);
        }
        assert!(is_checkmate(&b));
    }

    #[test]
    fn public_room_fills_and_seeds_round_one() {
        // A public tournament room: no create; the first join opens it, the 8th starts it.
        let mut rows = Vec::new();
        for i in 1..=8 {
            rows.push(ev(&format!("t{}", i), &format!("p{}", i), i as i64, r#"{"type":"chess_t","v":1,"t":"public-1","a":"join"}"#));
        }
        let ts = events(rows);
        let t = ts.get("public-1").expect("room opened");
        assert_eq!(t.capacity, 8);
        assert_eq!(t.players.len(), 8);
        assert!(t.started_at.is_some());
        let g = t.game(1, 0).unwrap(); // seeds 1v8, lower seed white
        assert_eq!(g.white, "p1");
        assert_eq!(g.black, "p8");
    }

    #[test]
    fn duel_starts_on_second_join() {
        let rows = vec![
            ev("d1", "alice", 1, r#"{"type":"chess_t","v":1,"t":"duel-1","a":"join"}"#),
            ev("d2", "bob", 2, r#"{"type":"chess_t","v":1,"t":"duel-1","a":"join"}"#),
        ];
        let ts = events(rows);
        let t = ts.get("duel-1").expect("duel opened");
        assert_eq!(t.capacity, 2);
        assert!(t.started_at.is_some());
        let g = t.game(1, 0).unwrap();
        assert_eq!(g.white, "alice");
        assert_eq!(g.black, "bob");
        assert_eq!(t.final_game_id(), "1-0");
    }

    #[test]
    fn private_tournament_requires_valid_key() {
        let id = "abcd2345";
        let good = create_key(PRIVATE_CREATE_CODE, id);
        // wrong key -> ignored
        let bad = ev("c1", "host", 1, &format!(r#"{{"type":"chess_t","v":1,"t":"{}","a":"create","p":8,"k":"deadbeefdeadbeefdeadbeef"}}"#, id));
        assert!(events(vec![bad]).get(id).is_none());
        // right key -> opens
        let ok = ev("c2", "host", 1, &format!(r#"{{"type":"chess_t","v":1,"t":"{}","a":"create","p":8,"k":"{}"}}"#, id, good));
        let ts = events(vec![ok]);
        assert!(ts.get(id).is_some());
        assert_eq!(ts.get(id).unwrap().capacity, 8);
    }

    #[test]
    fn stale_seat_expires_before_a_late_join() {
        // p1 joins at t=0; p2 joins 6 minutes later (past the 5-min seat TTL) -> p1's seat is
        // gone, so the duel does not start with a ghost.
        let rows = vec![
            ev("a", "p1", 0, r#"{"type":"chess_t","v":1,"t":"duel-1","a":"join"}"#),
            ev("b", "p2", 6 * 60 * 1000, r#"{"type":"chess_t","v":1,"t":"duel-1","a":"join"}"#),
        ];
        let t = events(rows);
        let room = t.get("duel-1").unwrap();
        assert_eq!(room.players, vec!["p2".to_string()]);
        assert!(room.started_at.is_none());
    }
}

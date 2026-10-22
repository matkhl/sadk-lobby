//! Pawn chess ("Bauernschach"), the two-player tavern game: kings and seven
//! pawns each, a captured pawn changes sides and is put back on the board,
//! and a pawn on the far baseline or a checkmate wins (see docs/TABLE-GAMES.md).
//!
//! The client checks the moves it offers but leaves the result to the
//! server, so this mirrors its rules exactly (`MiniGamePawnChessProxy`,
//! `CMiniGameControllerPawnChess_Actor` in the demo exe):
//! * x is the forward axis: seat 0 starts on x = 0 and moves +x, seat 1
//!   starts on x = 7 and moves -x. A square is `x + 8 * y`.
//! * Pieces are never removed: piece 0 is seat 0's king, 1-7 its pawns,
//!   8 seat 1's king, 9-15 its pawns. A captured pawn keeps its index and
//!   gets the capturer's owner bit.

use crate::lobby_message::{BitReader, BitWriter};
use crate::minigame::{read_varint, write_varint};

pub mod id {
    /// `pwn`, `trgt`: move a piece to an empty square.
    pub const MOVE: u16 = 400;
    /// `pwn`, `trgt`, `cppwn`, `cptrgt`: capture an enemy pawn and put it
    /// back on the board as one's own.
    pub const CAPTURE: u16 = 401;
    /// `posy`: where the own king starts on the own baseline.
    pub const PLACE_KING: u16 = 402;
    /// `plridx`: the NEUES SPIEL button.
    pub const NEW_GAME: u16 = 403;
}

/// `phs`.
pub mod phase {
    pub const WAITING: u8 = 1;
    pub const PLACE_KING_0: u8 = 2;
    pub const PLACE_KING_1: u8 = 3;
    pub const MOVE_0: u8 = 4;
    pub const MOVE_1: u8 = 5;
    pub const OVER: u8 = 6;
}

/// `plrstt` bits for seat 0; seat 1 uses the same bits shifted left by 4.
pub mod status {
    pub const CHECK: u8 = 0x01;
    pub const MATED: u8 = 0x02;
    pub const WON: u8 = 0x04;
}

const KING: u8 = 0x40;
const OWNER: u8 = 0x80;
const NO_MARK: u8 = 0xff;
const KING_STEPS: [(i8, i8); 8] = [(1, -1), (1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0), (-1, -1), (0, -1)];

fn x(p: u8) -> i8 {
    (p & 7) as i8
}
fn y(p: u8) -> i8 {
    (p >> 3 & 7) as i8
}
fn owner(p: u8) -> usize {
    (p >> 7) as usize
}
fn is_king(p: u8) -> bool {
    p & KING != 0
}
fn square(x: i8, y: i8) -> Option<u8> {
    ((0..8).contains(&x) && (0..8).contains(&y)).then(|| (x + 8 * y) as u8)
}
/// Forward direction of a seat's pawns.
fn dir(seat: usize) -> i8 {
    if seat == 0 { 1 } else { -1 }
}
/// The x a seat's pawns must reach to win.
fn goal(seat: usize) -> i8 {
    if seat == 0 { 7 } else { 0 }
}

/// A client action, already split into its fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Move { piece: u8, to: u8 },
    Capture { piece: u8, to: u8, captured: u8, put: u8 },
    PlaceKing { y: u8 },
    NewGame { seat: u8 },
}

impl Action {
    pub fn parse(id: u16, r: &mut BitReader) -> Option<Self> {
        let mut u8_ = || r.bits(8).map(|v| v as u8);
        Some(match id {
            id::MOVE => Action::Move { piece: u8_()?, to: u8_()? },
            id::CAPTURE => Action::Capture { piece: u8_()?, to: u8_()?, captured: u8_()?, put: u8_()? },
            id::PLACE_KING => Action::PlaceKing { y: u8_()? },
            id::NEW_GAME => Action::NewGame { seat: u8_()? },
            _ => return None,
        })
    }
}

/// What an action did to the table's money.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Money {
    /// The seat put the stake into the pot (its king is placed).
    Stake(usize),
    /// The seat wins this many stakes from the pot.
    Pot { seat: usize, stakes: u32 },
}

/// Why an action was refused. The client is still sent the unchanged
/// state with a new `trnid`, or it would wait for an answer forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Illegal {
    NotYourTurn,
    BadMove,
    NoPlacement,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PawnChess {
    pub phase: u8,
    /// `trnid`: changes with every state the server sends after an action.
    pub turn: u8,
    /// `plrstt`.
    pub status: u8,
    /// `pwnps`.
    pub pieces: [u8; 16],
    /// Seats that pressed NEUES SPIEL since the game ended.
    rematch: [bool; 2],
}

impl Default for PawnChess {
    fn default() -> Self {
        Self { phase: phase::WAITING, turn: 0, status: 0, pieces: Self::layout(3, 4), rematch: [false; 2] }
    }
}

impl PawnChess {
    /// Both sides on their baselines with the kings at `y0` and `y1`.
    fn layout(y0: u8, y1: u8) -> [u8; 16] {
        let mut pieces = [0; 16];
        for (seat, king_y) in [(0usize, y0), (1, y1)] {
            let base = seat * 8;
            let (bx, ob) = if seat == 0 { (0u8, 0) } else { (7, OWNER) };
            pieces[base] = bx | king_y << 3 | KING | ob;
            for (i, py) in (0..8u8).filter(|&py| py != king_y).enumerate() {
                pieces[base + 1 + i] = bx | py << 3 | ob;
            }
        }
        pieces
    }

    fn bump(&mut self) {
        // 0xff is the client's "unlocked" marker: never send it.
        self.turn = (self.turn + 1) % 0xff;
    }

    /// Seat to move in phases 4 and 5.
    fn mover(&self) -> Option<usize> {
        match self.phase {
            phase::MOVE_0 => Some(0),
            phase::MOVE_1 => Some(1),
            _ => None,
        }
    }

    fn piece_on(&self, sq: u8) -> Option<usize> {
        self.pieces.iter().position(|&p| p & 0x3f == sq)
    }

    fn king(&self, seat: usize) -> u8 {
        *self.pieces.iter().find(|&&p| is_king(p) && owner(p) == seat).expect("each side has a king")
    }

    /// Squares attacked by `seat`'s pieces, occupied or not (the client's
    /// attack map in `sub_489040`).
    fn attacked_by(&self, seat: usize) -> u64 {
        let mut map = 0u64;
        for &p in self.pieces.iter().filter(|&&p| owner(p) == seat) {
            let steps: &[(i8, i8)] = if is_king(p) { &KING_STEPS } else { &[(1, 1), (1, -1)] };
            for &(dx, dy) in steps {
                let dx = if is_king(p) { dx } else { dx * dir(seat) };
                if let Some(sq) = square(x(p) + dx, y(p) + dy) {
                    map |= 1 << sq;
                }
            }
        }
        map
    }

    /// Pieces giving check to `seat`'s king.
    fn checkers(&self, seat: usize) -> Vec<usize> {
        let k = self.king(seat);
        let enemy = 1 - seat;
        (0..16)
            .filter(|&i| {
                let p = self.pieces[i];
                if owner(p) != enemy {
                    return false;
                }
                let (dx, dy) = (x(k) - x(p), y(k) - y(p));
                if is_king(p) { dx.abs() <= 1 && dy.abs() <= 1 } else { dx == dir(enemy) && dy.abs() == 1 }
            })
            .collect()
    }

    /// `pwchkmt`: the one pawn of `seat` the client lets capture the single
    /// checking pawn, with bit 0x10 when that capture goes toward y - 1.
    fn check_mark(&self, seat: usize) -> u8 {
        if self.phase != [phase::MOVE_0, phase::MOVE_1][seat] {
            return NO_MARK;
        }
        let checkers = self.checkers(seat);
        let [c] = checkers[..] else { return NO_MARK };
        let cp = self.pieces[c];
        if is_king(cp) {
            return NO_MARK;
        }
        (0..16)
            .find(|&i| {
                let p = self.pieces[i];
                owner(p) == seat
                    && !is_king(p)
                    && x(p) + dir(seat) == x(cp)
                    && (y(p) - y(cp)).abs() == 1
                    && !self.placements(seat, cp).is_empty()
            })
            .map_or(NO_MARK, |i| i as u8 | if y(cp) == y(self.pieces[i]) - 1 { 0x10 } else { 0 })
    }

    /// Where a pawn captured by `capturer` may go: the empty squares of its
    /// column, else of the next column toward the capturer's baseline.
    fn placements(&self, capturer: usize, captured: u8) -> Vec<u8> {
        let mut cx = x(captured);
        while (0..8).contains(&cx) {
            let free: Vec<u8> = (0..8).filter_map(|cy| square(cx, cy)).filter(|&sq| self.piece_on(sq).is_none()).collect();
            if !free.is_empty() {
                return free;
            }
            cx -= dir(capturer);
        }
        Vec::new()
    }

    /// Target squares of piece `i` for its owner to move, as the client
    /// offers them (`sub_522140`).
    fn targets(&self, i: usize) -> Vec<u8> {
        let p = self.pieces[i];
        let seat = owner(p);
        let mark = self.check_mark(seat);
        let in_check = !self.checkers(seat).is_empty();
        if is_king(p) {
            let attacked = self.attacked_by(1 - seat);
            return KING_STEPS
                .iter()
                .filter_map(|&(dx, dy)| square(x(p) + dx, y(p) + dy))
                .filter(|&sq| attacked & 1 << sq == 0)
                .filter(|&sq| self.piece_on(sq).is_none_or(|o| owner(self.pieces[o]) != seat))
                .collect();
        }
        if mark != NO_MARK && mark & 0xf == i as u8 {
            let dy = if mark & 0x10 != 0 { -1 } else { 1 };
            return square(x(p) + dir(seat), y(p) + dy).into_iter().collect();
        }
        if in_check {
            return Vec::new();
        }
        let fx = x(p) + dir(seat);
        let forward = square(fx, y(p)).filter(|&sq| self.piece_on(sq).is_none());
        let captures = [-1, 1]
            .into_iter()
            .filter_map(|dy| square(fx, y(p) + dy))
            .filter(|&sq| self.piece_on(sq).is_some_and(|o| owner(self.pieces[o]) != seat));
        forward.into_iter().chain(captures).collect()
    }

    fn has_move(&self, seat: usize) -> bool {
        (0..16).filter(|&i| owner(self.pieces[i]) == seat).any(|i| {
            self.targets(i).into_iter().any(|sq| match self.piece_on(sq) {
                Some(o) if !is_king(self.pieces[o]) => !self.placements(seat, self.pieces[o]).is_empty(),
                Some(_) => false,
                None => true,
            })
        })
    }

    /// The second seat is taken (or a rematch was agreed): kings first.
    pub fn start(&mut self) {
        self.phase = phase::PLACE_KING_0;
        self.status = 0;
        self.pieces = Self::layout(3, 4);
        self.rematch = [false; 2];
        self.bump();
    }

    /// A player left: the other one wins a running game, and the table
    /// waits for a new opponent.
    pub fn left(&mut self, seat: usize) -> Vec<Money> {
        let mut money = Vec::new();
        // Seat 0 pays when it places its king, seat 1 when it places its own.
        match self.phase {
            phase::PLACE_KING_1 => money.push(Money::Pot { seat: 1 - seat, stakes: 1 }),
            phase::MOVE_0 | phase::MOVE_1 => money.push(Money::Pot { seat: 1 - seat, stakes: 2 }),
            _ => {}
        }
        self.phase = phase::WAITING;
        self.status = 0;
        self.rematch = [false; 2];
        self.bump();
        money
    }

    /// Applies a seat's action. Any `Err` still needs the state resent:
    /// the turn counter has moved on.
    pub fn act(&mut self, seat: usize, action: Action) -> Result<Vec<Money>, Illegal> {
        let result = self.apply(seat, action);
        // NEUES SPIEL locks nothing, and a new trnid in phase 6 would post
        // the winner lines again; `start` bumps it when the game restarts.
        if !matches!(action, Action::NewGame { .. }) {
            self.bump();
        }
        result
    }

    fn apply(&mut self, seat: usize, action: Action) -> Result<Vec<Money>, Illegal> {
        match action {
            Action::PlaceKing { y } => {
                if self.phase != [phase::PLACE_KING_0, phase::PLACE_KING_1][seat] {
                    return Err(Illegal::NotYourTurn);
                }
                if y > 7 {
                    return Err(Illegal::BadMove);
                }
                if seat == 0 {
                    self.pieces = Self::layout(y, if y == 0 { 1 } else { 0 });
                    self.phase = phase::PLACE_KING_1;
                } else {
                    self.pieces = Self::layout(y0_of(self.pieces[0]), y);
                    self.phase = phase::MOVE_0;
                }
                Ok(vec![Money::Stake(seat)])
            }
            Action::NewGame { seat: claimed } => {
                if self.phase != phase::OVER || claimed as usize != seat {
                    return Err(Illegal::NotYourTurn);
                }
                self.rematch[seat] = true;
                if self.rematch == [true; 2] {
                    self.start();
                }
                Ok(Vec::new())
            }
            Action::Move { piece, to } => {
                if self.mover() != Some(seat) {
                    return Err(Illegal::NotYourTurn);
                }
                let i = self.own_piece(seat, piece)?;
                if !self.targets(i).contains(&to) || self.piece_on(to).is_some() {
                    return Err(Illegal::BadMove);
                }
                self.pieces[i] = self.pieces[i] & 0xc0 | to;
                Ok(self.after_move(seat))
            }
            Action::Capture { piece, to, captured, put } => {
                if self.mover() != Some(seat) {
                    return Err(Illegal::NotYourTurn);
                }
                let i = self.own_piece(seat, piece)?;
                let c = captured as usize;
                if !self.targets(i).contains(&to) || self.piece_on(to) != Some(c) || is_king(self.pieces[c]) {
                    return Err(Illegal::BadMove);
                }
                let free = self.placements(seat, self.pieces[c]);
                if free.is_empty() {
                    return Err(Illegal::NoPlacement);
                }
                if !free.contains(&put) {
                    return Err(Illegal::BadMove);
                }
                self.pieces[i] = self.pieces[i] & 0xc0 | to;
                self.pieces[c] = put | if seat == 1 { OWNER } else { 0 };
                Ok(self.after_move(seat))
            }
        }
    }

    fn own_piece(&self, seat: usize, piece: u8) -> Result<usize, Illegal> {
        let i = piece as usize;
        (i < 16 && owner(self.pieces[i]) == seat).then_some(i).ok_or(Illegal::BadMove)
    }

    /// Baseline, checkmate and stalemate after `seat` moved; otherwise the
    /// other seat's turn.
    fn after_move(&mut self, seat: usize) -> Vec<Money> {
        let other = 1 - seat;
        let shift = |s: usize| if s == 0 { 0 } else { 4 };
        let on_goal = self.pieces.iter().any(|&p| owner(p) == seat && !is_king(p) && x(p) == goal(seat));
        self.phase = [phase::MOVE_0, phase::MOVE_1][other];
        let in_check = !self.checkers(other).is_empty();
        if on_goal {
            self.phase = phase::OVER;
            self.status = status::WON << shift(seat);
        } else if !self.has_move(other) {
            // Checkmate, or a stalemate, which the client has no text for:
            // the side that cannot move loses either way.
            self.phase = phase::OVER;
            self.status = status::WON << shift(seat) | if in_check { status::MATED << shift(other) } else { 0 };
        } else {
            self.status = if in_check { status::CHECK << shift(other) } else { 0 };
        }
        if self.phase == phase::OVER {
            self.rematch = [false; 2];
            vec![Money::Pot { seat, stakes: 2 }]
        } else {
            Vec::new()
        }
    }

    /// The game's part of 217/218 (`sub_489040`), seat chips as
    /// `(seat, credits, account)`.
    pub fn write(&self, w: &mut BitWriter, chips: &[(u8, u32, u32)]) {
        let seat = self.mover();
        let mark = seat.map_or(NO_MARK, |s| self.check_mark(s));
        w.bits(self.phase as u32, 8).bits(self.turn as u32, 8).bits(mark as u32, 8).bits(self.status as u32, 8);
        for p in self.pieces {
            w.bits(p as u32, 8);
        }
        w.bits(chips.len() as u32, 8);
        for &(slot, credits, account) in chips {
            w.bits(slot as u32, 8);
            write_varint(w, credits);
            write_varint(w, account);
        }
    }
}

impl PawnChess {
    /// Reads a [`PawnChess::write`] section: the state and the seat chips.
    pub fn read(r: &mut BitReader) -> Option<(Self, Vec<(u8, u32, u32)>)> {
        let phase = r.bits(8)? as u8;
        let turn = r.bits(8)? as u8;
        let _mark = r.bits(8)?;
        let status = r.bits(8)? as u8;
        let mut pieces = [0; 16];
        for p in &mut pieces {
            *p = r.bits(8)? as u8;
        }
        let n = r.bits(8)?;
        let chips = (0..n).map(|_| Some((r.bits(8)? as u8, read_varint(r)?, read_varint(r)?))).collect::<Option<_>>()?;
        Some((Self { phase, turn, status, pieces, rematch: [false; 2] }, chips))
    }

    /// Every action `seat` may take now, as the client would offer them
    /// (a capture with the first free square for the captured pawn).
    pub fn legal_actions(&self, seat: usize) -> Vec<Action> {
        match self.phase {
            phase::PLACE_KING_0 | phase::PLACE_KING_1 if self.phase - phase::PLACE_KING_0 == seat as u8 => {
                (0..8).map(|y| Action::PlaceKing { y }).collect()
            }
            phase::OVER => vec![Action::NewGame { seat: seat as u8 }],
            _ if self.mover() == Some(seat) => (0..16)
                .filter(|&i| owner(self.pieces[i]) == seat)
                .flat_map(|i| self.targets(i).into_iter().map(move |sq| (i, sq)))
                .filter_map(|(i, sq)| match self.piece_on(sq) {
                    None => Some(Action::Move { piece: i as u8, to: sq }),
                    Some(c) if !is_king(self.pieces[c]) => {
                        let put = *self.placements(seat, self.pieces[c]).first()?;
                        Some(Action::Capture { piece: i as u8, to: sq, captured: c as u8, put })
                    }
                    Some(_) => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

impl Action {
    pub fn id(&self) -> u16 {
        match self {
            Action::Move { .. } => id::MOVE,
            Action::Capture { .. } => id::CAPTURE,
            Action::PlaceKing { .. } => id::PLACE_KING,
            Action::NewGame { .. } => id::NEW_GAME,
        }
    }

    /// The fields after the table key.
    pub fn write(&self, w: &mut BitWriter) {
        let fields: &[u8] = match *self {
            Action::Move { piece, to } => &[piece, to],
            Action::Capture { piece, to, captured, put } => &[piece, to, captured, put],
            Action::PlaceKing { y } => &[y],
            Action::NewGame { seat } => &[seat],
        };
        for &f in fields {
            w.bits(f as u32, 8);
        }
    }
}

fn y0_of(king0: u8) -> u8 {
    king0 >> 3 & 7
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(x: u8, y: u8) -> u8 {
        x + 8 * y
    }

    fn placed(y0: u8, y1: u8) -> PawnChess {
        let mut g = PawnChess::default();
        g.start();
        assert_eq!(g.act(0, Action::PlaceKing { y: y0 }), Ok(vec![Money::Stake(0)]));
        assert_eq!(g.phase, phase::PLACE_KING_1);
        assert_eq!(g.pieces[0], 0x40 | y0 << 3);
        assert_eq!(g.act(1, Action::PlaceKing { y: y1 }), Ok(vec![Money::Stake(1)]));
        g
    }

    /// The worked example from the spec: the layout bytes and a capture.
    #[test]
    fn layout_moves_and_capture() {
        let mut g = placed(3, 4);
        assert_eq!(g.phase, phase::MOVE_0);
        assert_eq!(g.pieces, [0x58, 0x00, 0x08, 0x10, 0x20, 0x28, 0x30, 0x38, 0xe7, 0x87, 0x8f, 0x97, 0x9f, 0xaf, 0xb7, 0xbf]);
        let turn = g.turn;
        for (seat, piece, to) in [(0, 3, 17), (1, 12, 30), (0, 3, 18), (1, 12, 29), (0, 3, 19), (1, 12, 28)] {
            assert_eq!(g.act(seat, Action::Move { piece, to }), Ok(vec![]), "{piece} to {to}");
        }
        assert_eq!(g.turn, turn + 6);
        assert_eq!((g.pieces[3], g.pieces[12]), (0x13, 0x9c));
        // Square 52 = (4, 6) is free in the captured pawn's column; (4, 3)
        // is where the capturer lands.
        assert_eq!(g.act(0, Action::Capture { piece: 3, to: 28, captured: 12, put: 28 }), Err(Illegal::BadMove));
        assert_eq!(g.act(0, Action::Capture { piece: 3, to: 28, captured: 12, put: 52 }), Ok(vec![]));
        assert_eq!((g.pieces[3], g.pieces[12]), (0x1c, 0x34));
        assert_eq!((g.phase, g.status), (phase::MOVE_1, 0));
    }

    #[test]
    fn refusals_still_move_the_turn_on() {
        let mut g = placed(3, 4);
        let turn = g.turn;
        assert_eq!(g.act(1, Action::Move { piece: 9, to: pos(6, 0) }), Err(Illegal::NotYourTurn));
        assert_eq!(g.act(0, Action::Move { piece: 9, to: pos(6, 0) }), Err(Illegal::BadMove));
        assert_eq!(g.act(0, Action::Move { piece: 1, to: pos(2, 0) }), Err(Illegal::BadMove));
        assert_eq!(g.turn, turn + 3);
        assert_eq!(g.phase, phase::MOVE_0);
        let mut g = PawnChess { turn: 254, ..PawnChess::default() };
        g.bump();
        assert_eq!(g.turn, 0);
    }

    /// The rest position with some pieces moved: piece 0 (seat 0's king)
    /// and 1-7 on (0, i), piece 8 (seat 1's king) and 9-15 on (7, i - 8).
    fn board(moved: &[(usize, u8, u8)]) -> [u8; 16] {
        let mut b: [u8; 16] = std::array::from_fn(|i| {
            let p = if i < 8 { pos(0, i as u8) } else { OWNER | pos(7, i as u8 - 8) };
            if i % 8 == 0 { p | KING } else { p }
        });
        for &(i, x, y) in moved {
            b[i] = b[i] & 0xc0 | pos(x, y);
        }
        b
    }

    fn at(pieces: [u8; 16], phase: u8) -> PawnChess {
        PawnChess { phase, pieces, ..PawnChess::default() }
    }

    #[test]
    fn baseline_wins() {
        // Seat 1's pawn 11 is out of the way, so pawn 1 can walk in.
        let mut g = at(board(&[(1, 6, 3), (11, 5, 5)]), phase::MOVE_0);
        assert_eq!(g.act(0, Action::Move { piece: 1, to: pos(7, 3) }), Ok(vec![Money::Pot { seat: 0, stakes: 2 }]));
        assert_eq!((g.phase, g.status), (phase::OVER, status::WON));
        // NEUES SPIEL from both seats starts over with the kings.
        let turn = g.turn;
        assert_eq!(g.act(0, Action::NewGame { seat: 1 }), Err(Illegal::NotYourTurn));
        assert_eq!(g.act(0, Action::NewGame { seat: 0 }), Ok(vec![]));
        assert_eq!((g.phase, g.turn), (phase::OVER, turn));
        assert_eq!(g.act(1, Action::NewGame { seat: 1 }), Ok(vec![]));
        assert_eq!((g.phase, g.status), (phase::PLACE_KING_0, 0));
        assert_ne!(g.turn, turn);
    }

    #[test]
    fn check_marks_the_capturing_pawn() {
        // Seat 0's pawn 1 steps to (3,4) and checks the king on (4,3);
        // seat 1's pawn 13 on (4,5) can take it toward y - 1.
        let mut g = at(board(&[(8, 4, 3), (1, 2, 4), (13, 4, 5)]), phase::MOVE_0);
        assert_eq!(g.act(0, Action::Move { piece: 1, to: pos(3, 4) }), Ok(vec![]));
        assert_eq!((g.phase, g.status), (phase::MOVE_1, status::CHECK << 4));
        let mut w = BitWriter::new();
        g.write(&mut w, &[]);
        assert_eq!(w.finish()[..4], [phase::MOVE_1, g.turn, 13 | 0x10, 0x10]);
        // Only the king and the marked pawn may move now.
        assert_eq!(g.act(1, Action::Move { piece: 9, to: pos(6, 1) }), Err(Illegal::BadMove));
        assert_eq!(g.act(1, Action::Capture { piece: 13, to: pos(3, 4), captured: 1, put: pos(3, 0) }), Ok(vec![]));
        assert_eq!((g.pieces[13], g.pieces[1]), (OWNER | pos(3, 4), OWNER | pos(3, 0)));
        assert_eq!((g.phase, g.status), (phase::MOVE_0, 0));
    }

    #[test]
    fn checkmate() {
        // Seat 1's king on (7,0) behind its pawn on (7,1); seat 0's king on
        // (5,0) covers (6,0) and (6,1). Pawn 3 steps to (6,1): check, the
        // checker is defended and no seat-1 pawn can take it (pawn 10 is
        // moved off (7,2)).
        let mut g = at(board(&[(0, 5, 0), (3, 5, 1), (10, 3, 6)]), phase::MOVE_0);
        assert_eq!(g.act(0, Action::Move { piece: 3, to: pos(6, 1) }), Ok(vec![Money::Pot { seat: 0, stakes: 2 }]));
        assert_eq!((g.phase, g.status), (phase::OVER, status::WON | status::MATED << 4));
    }

    #[test]
    fn check_with_an_escape_is_not_mate() {
        let mut g = at(board(&[(0, 4, 0), (3, 5, 1), (10, 3, 6)]), phase::MOVE_0);
        assert_eq!(g.act(0, Action::Move { piece: 3, to: pos(6, 1) }), Ok(vec![]));
        assert_eq!((g.phase, g.status), (phase::MOVE_1, status::CHECK << 4));
        // The king may take the undefended checker or step aside.
        assert_eq!(g.targets(8), [pos(6, 1), pos(6, 0)]);
    }

    #[test]
    fn stalemate_loses() {
        // Seat 1 has only its king left on (7,0); seat 0's pawn on (6,0)
        // covers (7,1). The king steps to (5,0) and covers (6,0) and (6,1).
        let mut b = board(&[(0, 4, 0), (1, 6, 0), (8, 7, 0)]);
        for (n, i) in (9..16).enumerate() {
            b[i] = pos(1, n as u8);
        }
        let mut g = at(b, phase::MOVE_0);
        assert_eq!(g.act(0, Action::Move { piece: 0, to: pos(5, 0) }), Ok(vec![Money::Pot { seat: 0, stakes: 2 }]));
        assert_eq!((g.phase, g.status), (phase::OVER, status::WON));
    }

    #[test]
    fn leaving_forfeits() {
        let mut g = placed(3, 4);
        assert_eq!(g.left(1), [Money::Pot { seat: 0, stakes: 2 }]);
        assert_eq!(g.phase, phase::WAITING);
        let mut g = PawnChess::default();
        g.start();
        g.act(0, Action::PlaceKing { y: 0 }).unwrap();
        assert_eq!(g.left(0), [Money::Pot { seat: 1, stakes: 1 }]);
    }

    /// Random games between two players who only pick offered actions
    /// always end, keep the pieces on distinct squares, and the state
    /// reads back as written.
    #[test]
    fn random_games_end_and_round_trip() {
        let mut seed = 12345u32;
        let mut rand = move |n: usize| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as usize % n
        };
        for _ in 0..300 {
            let mut g = PawnChess::default();
            g.start();
            for _ in 0..1000 {
                if g.phase == phase::OVER {
                    break;
                }
                let seat = match g.phase {
                    phase::PLACE_KING_0 | phase::MOVE_0 => 0,
                    _ => 1,
                };
                let actions = g.legal_actions(seat);
                assert!(!actions.is_empty(), "{g:?}");
                let a = actions[rand(actions.len())];
                let mut r = BitWriter::new();
                a.write(&mut r);
                let bytes = r.finish();
                assert_eq!(Action::parse(a.id(), &mut BitReader::new(&bytes)), Some(a));
                assert!(g.act(seat, a).is_ok(), "{a:?} on {g:?}");
                let mut sorted = g.pieces.map(|p| p & 0x3f);
                sorted.sort();
                assert!(sorted.windows(2).all(|w| w[0] != w[1]), "two pieces on one square: {g:?}");
                let mut w = BitWriter::new();
                g.write(&mut w, &[(0, 5, 7), (1, 300, 0)]);
                let bytes = w.finish();
                let (back, chips) = PawnChess::read(&mut BitReader::new(&bytes)).unwrap();
                assert_eq!((back.phase, back.turn, back.status, back.pieces), (g.phase, g.turn, g.status, g.pieces));
                assert_eq!(chips, [(0, 5, 7), (1, 300, 0)]);
            }
            assert_eq!(g.phase, phase::OVER, "a game that never ends: {g:?}");
        }
    }
}

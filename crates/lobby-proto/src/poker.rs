//! Texas Hold'em poker, no limit, for two to eight players (see
//! docs/TABLE-GAMES.md).
//!
//! The server deals, posts the blinds, runs the betting and pays the pots.
//! The client only animates: it deals when `phase` turns 5, flies the bets
//! into the pots in phase 8, and in phase 9 picks each pot's winners itself
//! by comparing `hndval` among the pot's `plrmsk` seats, so the server pays
//! by the same rule. Each player's own hole cards go to them alone in a 303
//! before every street's deal.
//!
//! The client locks its buttons after an action until `crntplyr`, `phase`
//! or `round` changes, so every action is taken: amounts are clamped and
//! actions that were not offered become the nearest one that was.

use crate::lobby_message::{BitReader, BitWriter};
use crate::minigame::{read_varint, write_varint};

pub mod id {
    /// Key, `actn` u16: fold, call, check or all-in.
    pub const ACT: u16 = 300;
    /// Key, `actn` u16, `wgr` var: bet or raise to `wgr` this street.
    pub const WAGER: u16 = 301;
    /// Key, `enbl` u8: sit out from the next hand (1) or play again (0).
    pub const SIT_OUT: u16 = 302;
    /// Server to one player: key, `crd0` u8, `crd1` u8, `hndval` u32.
    pub const HOLE_CARDS: u16 = 303;
}

/// Action bits (`actns`, `actn`, `lstactn`).
pub mod action {
    pub const CALL: u16 = 0x002;
    pub const FOLD: u16 = 0x004;
    pub const RAISE: u16 = 0x008;
    pub const CHECK: u16 = 0x010;
    pub const BET: u16 = 0x020;
    pub const SMALL_BLIND: u16 = 0x040;
    pub const BIG_BLIND: u16 = 0x080;
    pub const ALL_IN: u16 = 0x100;
}

/// Player state bits above the action in `actn`. The client acts on them
/// (0x8000/0x2000/0x1000: "wait for the next hand"; 0x1000/0x800: never
/// your turn); the names are ours.
pub mod flag {
    pub const FOLDED: u16 = 0x8000;
    pub const NOT_DEALT: u16 = 0x2000;
    pub const SITTING_OUT: u16 = 0x1000;
    pub const ALL_IN: u16 = 0x0800;
}

pub mod phase {
    pub const WAITING: u8 = 1;
    /// Deals `round`: hole cards (1), flop (2), turn (3) or river (4).
    pub const DEAL: u8 = 5;
    pub const BETTING: u8 = 6;
    /// The bets fly into the pots.
    pub const COLLECT: u8 = 8;
    pub const SHOWDOWN: u8 = 9;
}

pub const NONE: u8 = 0xff;
/// `shwcrds` when the last player left won without a showdown.
pub const NO_SHOWDOWN: u8 = 0xfe;
pub const SEATS: usize = 8;

/// The client's turn clock (it warns at 5 s); the server acts at the end.
pub const TURN_MS: u64 = 15_000;
pub const COLLECT_MS: u64 = 1_000;
pub const STREET_MS: u64 = 1_500;
/// Each extra street a run-out deals at once.
pub const RUN_OUT_MS: u64 = 500;
pub const SHOWDOWN_MS: u64 = 1_000;
pub const POT_MS: u64 = 2_000;
/// Before the first hand, once two players can play.
pub const START_MS: u64 = 2_000;

/// The preflop deal animation: 350 ms per occupied seat and card.
pub fn deal_ms(seats: usize) -> u64 {
    700 * (seats as u64 + 1) + 500
}

/// `hndval` of the best five of `cards` (2-7 cards; values ≥ 52 are
/// skipped): the category in bits 24-31, then up to five deciding ranks
/// (0 = '2' … 12 = 'A') in 4-bit steps from bit 16 down. Larger is better;
/// the client compares these numbers to pick winners.
pub fn hand_value(cards: &[u8]) -> u32 {
    let mut counts = [0u8; 13];
    let mut suits = [0u16; 4];
    for &c in cards.iter().filter(|&&c| c < 52) {
        counts[(c % 13) as usize] += 1;
        suits[(c / 13) as usize] |= 1 << (c % 13);
    }
    let pack = |category: u32, ranks: &[u8]| {
        ranks.iter().take(5).enumerate().fold(category << 24, |v, (i, &r)| v | (r as u32) << (16 - 4 * i))
    };
    fn straight(mask: u16) -> Option<u8> {
        (4..13u8).rev().find(|&top| mask >> (top - 4) & 0x1f == 0x1f).or((mask & 0x100f == 0x100f).then_some(3))
    }
    let ranks_desc = |mask: u16| (0..13u8).rev().filter(move |r| mask & 1 << r != 0);
    if let Some(top) = suits.iter().filter_map(|&s| straight(s)).max() {
        return pack(8, &[top]);
    }
    let with = |n: u8| (0..13u8).rev().filter(move |&r| counts[r as usize] == n);
    let kickers = |used: &[u8], n: usize| -> Vec<u8> {
        (0..13u8).rev().filter(|r| counts[*r as usize] > 0 && !used.contains(r)).take(n).collect()
    };
    let quads: Vec<u8> = with(4).collect();
    let trips: Vec<u8> = with(3).collect();
    let pairs: Vec<u8> = with(2).collect();
    if let Some(&q) = quads.first() {
        return pack(7, &[&[q][..], &kickers(&[q], 1)].concat());
    }
    if let Some(&t) = trips.first() {
        let pair = trips.get(1).into_iter().chain(pairs.first()).max();
        if let Some(&p) = pair {
            return pack(6, &[t, p]);
        }
    }
    if let Some(&s) = suits.iter().find(|s| s.count_ones() >= 5) {
        return pack(5, &ranks_desc(s).take(5).collect::<Vec<_>>());
    }
    let all = suits.iter().fold(0, |a, s| a | s);
    if let Some(top) = straight(all) {
        return pack(4, &[top]);
    }
    if let Some(&t) = trips.first() {
        return pack(3, &[&[t][..], &kickers(&[t], 2)].concat());
    }
    if let [a, b, ..] = pairs[..] {
        return pack(2, &[&[a, b][..], &kickers(&[a, b], 1)].concat());
    }
    if let Some(&p) = pairs.first() {
        return pack(1, &[&[p][..], &kickers(&[p], 3)].concat());
    }
    pack(0, &kickers(&[], 5))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Player {
    /// Dealt into the current hand.
    pub dealt: bool,
    pub folded: bool,
    pub all_in: bool,
    /// Sits out from the next hand; checks or folds at once until then.
    pub sit_out: bool,
    pub cards: [u8; 2],
    /// `wgr`: in front of the player this street.
    pub bet: u32,
    /// Put in the pots on earlier streets.
    pub committed: u32,
    /// The last action this street (the low bits of `actn`).
    pub action: u16,
    /// Has acted since the last full bet or raise: may not raise again.
    pub acted: bool,
    /// Turns timed out in a row.
    pub timeouts: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pot {
    pub amount: u32,
    /// `plrmsk`: the seats that can win it.
    pub eligible: u8,
    /// Phase 8: each seat's chips moving into it now.
    pub moves: [u32; SEATS],
}

/// What a step did besides changing the state: the 303s to send first,
/// as `(seat, cards, hndval)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Out {
    pub hole_cards: Vec<(usize, [u8; 2], u32)>,
}

/// A player's message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// 300: fold, call, check or all-in.
    Plain(u16),
    /// 301: bet or raise to the amount (the total for this street).
    Wager(u16, u32),
    /// 302.
    SitOut(bool),
}

impl Action {
    pub fn parse(id: u16, r: &mut BitReader) -> Option<Self> {
        match id {
            id::ACT => Some(Self::Plain(r.bits(16)? as u16)),
            id::WAGER => Some(Self::Wager(r.bits(16)? as u16, read_varint(r)?)),
            id::SIT_OUT => Some(Self::SitOut(r.bits(8)? != 0)),
            _ => None,
        }
    }

    pub fn id(&self) -> u16 {
        match self {
            Self::Plain(_) => id::ACT,
            Self::Wager(..) => id::WAGER,
            Self::SitOut(_) => id::SIT_OUT,
        }
    }

    pub fn write(&self, w: &mut BitWriter) {
        match *self {
            Self::Plain(a) => {
                w.bits(a as u32, 16);
            }
            Self::Wager(a, v) => {
                w.bits(a as u32, 16);
                write_varint(w, v);
            }
            Self::SitOut(on) => {
                w.bits(on as u32, 8);
            }
        }
    }
}

/// Only the seat whose turn it is may act, and only while betting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotYourTurn;

/// Chips behind each seat (`crdts`, not counting the bet in front); `None`
/// for an empty seat. The table owns them; the game moves them.
pub type Stacks = [Option<u32>];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Poker {
    /// `lmtmn`; the small blind is half.
    pub big_blind: u32,
    pub phase: u8,
    /// 0 none, 1 preflop, 2 flop, 3 turn, 4 river.
    pub round: u8,
    /// `egr`: a run-out's last street shown before it (with round 4).
    pub run_out_from: u8,
    pub run_out: bool,
    /// Phase 9: the hands are shown (else won by folds).
    pub reveal: bool,
    /// When the phase or turn began, and how long until the next step.
    pub since: u64,
    pub wait: u64,
    pub dealer: u8,
    pub big_blind_seat: u8,
    pub current: u8,
    pub actions: u16,
    /// `minwgr` and `mnrs`.
    pub min_wager: u32,
    pub min_raise: u32,
    /// `lstply`, `lstactn`, `lstvl`.
    pub last: (u8, u16, u32),
    pub board: [u8; 5],
    pub players: [Player; SEATS],
    pub pots: Vec<Pot>,
    /// Phase 8: the pots carry this street's moves.
    pub collecting: bool,
}

impl Poker {
    pub fn new(now: u64, big_blind: u32) -> Self {
        Self {
            big_blind: big_blind.max(2),
            phase: phase::WAITING,
            round: 0,
            run_out_from: 0,
            run_out: false,
            reveal: false,
            since: now,
            wait: START_MS,
            dealer: NONE,
            big_blind_seat: NONE,
            current: NONE,
            actions: 0,
            min_wager: 0,
            min_raise: 0,
            last: (NONE, 0, 0),
            board: [NONE; 5],
            players: Default::default(),
            pots: Vec::new(),
            collecting: false,
        }
    }

    fn small_blind(&self) -> u32 {
        self.big_blind / 2
    }

    fn live(&self, s: usize) -> bool {
        let p = &self.players[s];
        p.dealt && !p.folded
    }

    fn can_act(&self, s: usize) -> bool {
        self.live(s) && !self.players[s].all_in
    }

    fn live_count(&self) -> usize {
        (0..SEATS).filter(|&s| self.live(s)).count()
    }

    fn max_bet(&self) -> u32 {
        self.players.iter().map(|p| p.bet).max().unwrap_or(0)
    }

    /// The first seat after `from` (wrapping over `n` seats) that passes.
    fn next_seat(from: u8, n: usize, pred: impl Fn(usize) -> bool) -> Option<usize> {
        let from = if from == NONE { n - 1 } else { from as usize };
        (1..=n).map(|k| (from + k) % n).find(|&s| pred(s))
    }

    fn ready(&self, stacks: &Stacks) -> Vec<bool> {
        (0..stacks.len()).map(|s| stacks[s].is_some_and(|c| c >= self.big_blind) && !self.players[s].sit_out).collect()
    }

    fn visible_board(&self) -> &[u8] {
        match self.round {
            0 | 1 => &[],
            2 => &self.board[..3],
            3 => &self.board[..4],
            _ => &self.board,
        }
    }

    fn value(&self, s: usize) -> u32 {
        let p = &self.players[s];
        hand_value(&[&p.cards[..], self.visible_board()].concat())
    }

    /// The 303s for everyone still in the hand.
    fn hole_cards(&self) -> Out {
        Out { hole_cards: (0..SEATS).filter(|&s| self.live(s)).map(|s| (s, self.players[s].cards, self.value(s))).collect() }
    }

    /// The pots from what everyone put in (`committed`, plus the bets in
    /// front when `with_bets`): one per all-in level of the players still
    /// in. Chips above the highest of those (from players who left) go to
    /// the last pot.
    fn build_pots(&self, with_bets: bool) -> Vec<Pot> {
        let total = |s: usize| self.players[s].committed + if with_bets { self.players[s].bet } else { 0 };
        let before = |s: usize| self.players[s].committed;
        let mut levels: Vec<u32> = (0..SEATS).filter(|&s| self.live(s)).map(total).collect();
        levels.sort_unstable();
        levels.dedup();
        levels.retain(|&l| l > 0);
        let mut pots = Vec::new();
        let mut prev = 0;
        for &level in &levels {
            let slice = |v: u32| v.min(level) - v.min(prev);
            let mut pot = Pot::default();
            for s in 0..SEATS {
                pot.amount += slice(total(s));
                pot.moves[s] = slice(total(s)) - slice(before(s));
                if self.live(s) && total(s) >= level {
                    pot.eligible |= 1 << s;
                }
            }
            pots.push(pot);
            prev = level;
        }
        let rest: u32 = (0..SEATS).map(|s| total(s) - total(s).min(prev)).sum();
        if let Some(last) = pots.last_mut() {
            last.amount += rest;
            for s in 0..SEATS {
                last.moves[s] += total(s) - total(s).min(prev) - (before(s) - before(s).min(prev));
            }
        }
        if !with_bets {
            for p in &mut pots {
                p.moves = [0; SEATS];
            }
        }
        pots
    }

    /// The actions offered to seat `s` (at most three).
    fn offer(&self, s: usize, stacks: &Stacks) -> u16 {
        use action::*;
        let p = &self.players[s];
        let stack = stacks.get(s).copied().flatten().unwrap_or(0);
        let max = self.max_bet();
        let to_call = max - p.bet;
        if to_call > 0 {
            if stack <= to_call {
                FOLD | ALL_IN
            } else if p.acted {
                FOLD | CALL
            } else if stack + p.bet >= max + self.min_raise {
                RAISE | FOLD | CALL
            } else {
                FOLD | CALL | ALL_IN
            }
        } else {
            let (raise, need) = if max == 0 { (BET, self.big_blind) } else { (RAISE, max + self.min_raise) };
            if p.acted {
                FOLD | CHECK
            } else if stack + p.bet >= need {
                raise | FOLD | CHECK
            } else {
                FOLD | CHECK | ALL_IN
            }
        }
    }

    /// Checks when that is free, else folds (timeouts and sitting out).
    fn passive(&self, s: usize) -> u16 {
        if self.max_bet() > self.players[s].bet {
            action::FOLD
        } else {
            action::CHECK
        }
    }

    fn needs_to_act(&self, s: usize) -> bool {
        if !self.can_act(s) {
            return false;
        }
        let p = &self.players[s];
        let others = (0..SEATS).filter(|&o| o != s && self.can_act(o)).count();
        p.bet < self.max_bet() || (!p.acted && others > 0)
    }

    /// A new hand: moves the button, deals and posts the blinds.
    fn start_hand(&mut self, now: u64, stacks: &mut Stacks, deck: [u8; 52]) -> Out {
        let n = stacks.len();
        let ready = self.ready(stacks);
        for (s, p) in self.players.iter_mut().enumerate() {
            *p = Player { sit_out: p.sit_out, timeouts: p.timeouts, dealt: ready.get(s) == Some(&true), ..Player::default() };
        }
        let dealt: Vec<usize> = (0..n).filter(|&s| ready[s]).collect();
        let dealer = Self::next_seat(self.dealer, n, |s| ready[s]).unwrap_or(0);
        let small = if dealt.len() == 2 { dealer } else { Self::next_seat(dealer as u8, n, |s| ready[s]).unwrap_or(0) };
        let big = Self::next_seat(small as u8, n, |s| ready[s]).unwrap_or(0);
        let order: Vec<usize> = (1..=n).map(|k| (dealer + k) % n).filter(|&s| ready[s]).collect();
        let k = order.len();
        for (i, &s) in order.iter().enumerate() {
            self.players[s].cards = [deck[i], deck[i + k]];
        }
        self.board.copy_from_slice(&deck[2 * k..2 * k + 5]);
        self.dealer = dealer as u8;
        self.big_blind_seat = big as u8;
        self.round = 1;
        self.run_out = false;
        self.run_out_from = 0;
        self.reveal = false;
        self.pots.clear();
        self.collecting = false;
        self.current = NONE;
        self.actions = 0;
        self.min_raise = self.big_blind;
        self.put(small, self.small_blind(), stacks);
        self.players[small].action = action::SMALL_BLIND;
        let paid = self.put(big, self.big_blind, stacks);
        self.players[big].action = action::BIG_BLIND;
        self.min_wager = self.max_bet();
        self.last = (big as u8, action::BIG_BLIND, paid);
        self.enter(phase::DEAL, now, deal_ms(stacks.iter().flatten().count()));
        let mut out = self.hole_cards();
        // Clear old cards on the clients of those who sit this hand out.
        for s in (0..n).filter(|&s| stacks[s].is_some() && !ready[s]) {
            out.hole_cards.push((s, [NONE; 2], 0));
        }
        out
    }

    /// Moves up to `amount` from the seat's stack in front of it.
    fn put(&mut self, s: usize, amount: u32, stacks: &mut Stacks) -> u32 {
        let Some(Some(stack)) = stacks.get_mut(s) else { return 0 };
        let paid = amount.min(*stack);
        *stack -= paid;
        self.players[s].bet += paid;
        if *stack == 0 {
            self.players[s].all_in = true;
        }
        paid
    }

    fn enter(&mut self, phase: u8, now: u64, wait: u64) {
        self.phase = phase;
        self.since = now;
        self.wait = wait;
    }

    /// Passes the turn to the next seat after `from` that must act, or ends
    /// the street. Players sitting out act at once.
    fn next_turn(&mut self, from: u8, now: u64, stacks: &mut Stacks) -> Out {
        if self.live_count() <= 1 {
            return self.finish_street(now, stacks);
        }
        let Some(s) = Self::next_seat(from, SEATS, |s| self.needs_to_act(s)) else {
            return self.finish_street(now, stacks);
        };
        self.current = s as u8;
        self.actions = self.offer(s, stacks);
        self.enter(phase::BETTING, now, TURN_MS);
        if self.players[s].sit_out {
            return self.apply(s, self.passive(s), 0, now, stacks);
        }
        Out::default()
    }

    /// Takes seat `s`'s action (the one whose turn it is), made legal.
    fn apply(&mut self, s: usize, act: u16, wager: u32, now: u64, stacks: &mut Stacks) -> Out {
        use action::*;
        let stack = stacks.get(s).copied().flatten().unwrap_or(0);
        let max = self.max_bet();
        let bet = self.players[s].bet;
        let to_call = max - bet;
        let may_raise = self.actions & (BET | RAISE | ALL_IN) != 0 && !self.players[s].acted;
        let call = to_call.min(stack);
        let add = match act {
            FOLD => None,
            CHECK | CALL if to_call == 0 => Some(0),
            CALL => Some(call),
            ALL_IN if may_raise => Some(stack),
            ALL_IN => Some(call),
            BET | RAISE if may_raise => {
                let least = if max == 0 { self.big_blind } else { max + self.min_raise };
                let target = if wager == u32::MAX { least } else { wager.max(least) }.min(bet + stack);
                Some(if target <= max { call } else { target - bet })
            }
            BET | RAISE if to_call == 0 => Some(0),
            BET | RAISE => Some(call),
            _ if to_call == 0 => Some(0),
            _ => None,
        };
        let Some(add) = add else {
            let p = &mut self.players[s];
            p.folded = true;
            p.action = FOLD;
            self.last = (s as u8, FOLD, 0);
            return self.next_turn(s as u8, now, stacks);
        };
        self.put(s, add, stacks);
        let new_bet = self.players[s].bet;
        let shown = if self.players[s].all_in && add > 0 {
            (ALL_IN, add)
        } else if new_bet > max {
            (if max == 0 { BET } else { RAISE }, new_bet)
        } else if add > 0 {
            (CALL, add)
        } else {
            (CHECK, 0)
        };
        if new_bet > max {
            let increase = new_bet - max;
            if increase >= self.min_raise {
                self.min_raise = increase;
                for p in self.players.iter_mut() {
                    p.acted = false;
                }
            }
            self.min_wager = new_bet;
        }
        let p = &mut self.players[s];
        p.acted = true;
        p.action = shown.0;
        self.last = (s as u8, shown.0, shown.1);
        self.next_turn(s as u8, now, stacks)
    }

    /// No more betting this street: collect the bets (phase 8) if any.
    fn finish_street(&mut self, now: u64, stacks: &mut Stacks) -> Out {
        self.current = NONE;
        self.actions = 0;
        if self.players.iter().any(|p| p.bet > 0) {
            self.pots = self.build_pots(true);
            self.collecting = true;
            for p in self.players.iter_mut() {
                p.committed += std::mem::take(&mut p.bet);
            }
            self.enter(phase::COLLECT, now, COLLECT_MS);
            return Out::default();
        }
        self.after_collect(now, stacks)
    }

    /// The next street, a run-out to the river, or the showdown.
    fn after_collect(&mut self, now: u64, stacks: &mut Stacks) -> Out {
        self.collecting = false;
        self.pots = self.build_pots(false);
        if self.live_count() <= 1 {
            return self.showdown(now, stacks, false);
        }
        if self.round >= 4 {
            return self.showdown(now, stacks, true);
        }
        let movers = (0..SEATS).filter(|&s| self.can_act(s)).count();
        if movers <= 1 {
            let extra = 3 - self.round as u64;
            self.run_out = true;
            self.run_out_from = self.round;
            self.round = 4;
            self.enter(phase::DEAL, now, STREET_MS + RUN_OUT_MS * extra);
        } else {
            self.round += 1;
            self.enter(phase::DEAL, now, STREET_MS);
        }
        self.last = (NONE, 0, 0);
        self.min_wager = self.big_blind;
        self.min_raise = self.big_blind;
        for p in self.players.iter_mut() {
            p.action = 0;
            p.acted = false;
        }
        self.hole_cards()
    }

    /// Pays every pot to its best hands (or its only player) and shows the
    /// result.
    fn showdown(&mut self, now: u64, stacks: &mut Stacks, reveal: bool) -> Out {
        self.collecting = false;
        self.pots = self.build_pots(false);
        self.reveal = reveal;
        self.current = NONE;
        self.actions = 0;
        let n = stacks.len().max(1);
        for pot in &self.pots {
            let seats: Vec<usize> = (0..SEATS).filter(|s| pot.eligible & 1 << s != 0).collect();
            let best = seats.iter().map(|&s| if reveal { self.value(s) } else { 0 }).max().unwrap_or(0);
            let mut winners: Vec<usize> = seats.into_iter().filter(|&s| !reveal || self.value(s) == best).collect();
            // The odd chips go to the first winners after the button.
            winners.sort_by_key(|&s| (s + n - 1 - self.dealer as usize % n) % n);
            let Some(count) = (!winners.is_empty()).then_some(winners.len() as u32) else { continue };
            for (i, &s) in winners.iter().enumerate() {
                let share = pot.amount / count + u32::from((i as u32) < pot.amount % count);
                if let Some(Some(stack)) = stacks.get_mut(s) {
                    *stack += share;
                }
            }
        }
        let pots = self.pots.len() as u64;
        self.enter(phase::SHOWDOWN, now, SHOWDOWN_MS + POT_MS * pots);
        Out::default()
    }

    /// Back to waiting for players.
    fn idle(&mut self, now: u64) {
        for p in self.players.iter_mut() {
            *p = Player { sit_out: p.sit_out, timeouts: p.timeouts, ..Player::default() };
        }
        self.round = 0;
        self.run_out = false;
        self.reveal = false;
        self.pots.clear();
        self.collecting = false;
        self.board = [NONE; 5];
        self.current = NONE;
        self.actions = 0;
        self.last = (NONE, 0, 0);
        self.min_wager = 0;
        self.min_raise = 0;
        self.enter(phase::WAITING, now, START_MS);
    }

    /// Moves on when a phase's time is up; `deck` shuffles for a new hand.
    pub fn tick(&mut self, now: u64, stacks: &mut Stacks, deck: impl FnOnce() -> [u8; 52]) -> Option<Out> {
        if now.saturating_sub(self.since) < self.wait {
            return None;
        }
        Some(match self.phase {
            phase::WAITING if self.ready(stacks).iter().filter(|&&r| r).count() >= 2 => self.start_hand(now, stacks, deck()),
            phase::DEAL if self.run_out => self.showdown(now, stacks, true),
            phase::DEAL => {
                let from = if self.round == 1 { self.big_blind_seat } else { self.dealer };
                self.next_turn(from, now, stacks)
            }
            phase::BETTING => {
                let s = self.current as usize;
                let p = &mut self.players[s];
                p.timeouts += 1;
                if p.timeouts >= 2 {
                    p.sit_out = true;
                }
                self.apply(s, self.passive(s), 0, now, stacks)
            }
            phase::COLLECT => self.after_collect(now, stacks),
            phase::SHOWDOWN => {
                self.idle(now);
                self.wait = 0;
                if self.ready(stacks).iter().filter(|&&r| r).count() >= 2 {
                    self.start_hand(now, stacks, deck())
                } else {
                    Out::default()
                }
            }
            _ => return None,
        })
    }

    /// A player's 300-302.
    pub fn act(&mut self, seat: usize, a: Action, now: u64, stacks: &mut Stacks) -> Result<Out, NotYourTurn> {
        if let Action::SitOut(on) = a {
            self.players[seat].sit_out = on;
            self.players[seat].timeouts = 0;
            if on && self.phase == phase::BETTING && self.current as usize == seat {
                return Ok(self.apply(seat, self.passive(seat), 0, now, stacks));
            }
            return Ok(Out::default());
        }
        if self.phase != phase::BETTING || self.current as usize != seat {
            return Err(NotYourTurn);
        }
        self.players[seat].timeouts = 0;
        Ok(match a {
            Action::Plain(act) => self.apply(seat, act, 0, now, stacks),
            Action::Wager(act, wager) => self.apply(seat, act, wager, now, stacks),
            Action::SitOut(_) => unreachable!(),
        })
    }

    /// Someone sat down: dealt in from the next hand.
    pub fn joined(&mut self, seat: usize, now: u64) {
        let p = &mut self.players[seat];
        *p = Player { bet: p.bet, committed: p.committed, folded: true, ..Player::default() };
        if self.phase == phase::WAITING {
            self.since = now;
            self.wait = START_MS;
        }
    }

    /// Someone stood up (`stacks` without them): folded, their chips in
    /// the pots stay there. Returns a step when the state changed.
    pub fn left(&mut self, seat: usize, now: u64, stacks: &mut Stacks) -> Option<Out> {
        let in_hand = self.live(seat) && matches!(self.phase, phase::DEAL | phase::BETTING | phase::COLLECT);
        let p = &mut self.players[seat];
        p.sit_out = false;
        p.timeouts = 0;
        if !in_hand {
            p.dealt = false;
            return None;
        }
        p.folded = true;
        if self.phase == phase::BETTING && (self.current as usize == seat || self.live_count() <= 1) {
            return Some(self.next_turn(seat as u8, now, stacks));
        }
        Some(Out::default())
    }

    /// `actn` of a seat.
    fn actn(&self, s: usize) -> u16 {
        let p = &self.players[s];
        let mut a = p.action;
        if p.dealt && p.folded {
            a |= flag::FOLDED;
        }
        if !p.dealt && self.phase != phase::WAITING {
            a |= flag::NOT_DEALT;
        }
        if p.sit_out {
            a |= flag::SITTING_OUT;
        }
        if p.dealt && p.all_in {
            a |= flag::ALL_IN;
        }
        a
    }

    /// The game's part of 217 (`sub_487DE0`); chips as `(seat, credits,
    /// account)`.
    pub fn write(&self, w: &mut BitWriter, now: u64, chips: &[(u8, u32, u32)]) {
        let time = if self.phase == phase::BETTING { (now.saturating_sub(self.since) / 10).min(u16::MAX as u64) } else { 0 };
        let egr = if self.run_out { self.run_out_from } else { 0 };
        let show = match (self.phase, self.reveal) {
            (phase::SHOWDOWN, false) => NO_SHOWDOWN,
            _ => NONE,
        };
        w.bits(self.dealer as u32, 8).bits(chips.len() as u32, 8).bits(self.current as u32, 8);
        w.bits(self.actions as u32, 16).bits(self.phase as u32, 8).bits(self.round as u32, 8);
        w.bits(egr as u32, 8).bits(show as u32, 8).bits(time as u32, 16).u32(self.min_wager);
        w.bits(self.last.0 as u32, 8);
        write_varint(w, self.last.2);
        write_varint(w, self.min_raise);
        w.bits(self.last.1 as u32, 16);
        for &c in self.visible_board() {
            w.bits(c as u32, 8);
        }
        let mask = (0..SEATS).filter(|&s| self.players[s].bet > 0).fold(0, |m, s| m | 1 << s);
        w.bits(mask, 8);
        for p in self.players.iter().filter(|p| p.bet > 0) {
            write_varint(w, p.bet);
        }
        let shown = |s: usize| self.phase == phase::SHOWDOWN && self.reveal && self.pots.iter().any(|p| p.eligible & 1 << s != 0);
        for &(seat, credits, account) in chips {
            let s = seat as usize % SEATS;
            w.bits(seat as u32, 8).bool(shown(s));
            if shown(s) {
                let p = &self.players[s];
                w.bits(p.cards[0] as u32, 8).bits(p.cards[1] as u32, 8).u32(self.value(s));
            }
            w.bits(self.actn(s) as u32, 16);
            write_varint(w, credits);
            write_varint(w, account);
        }
        w.bits(self.pots.len() as u32 | if self.collecting { 0x80 } else { 0 }, 8);
        for pot in &self.pots {
            w.u32(pot.amount).bits(pot.eligible as u32, 8);
            if self.collecting {
                let mask = (0..SEATS).filter(|&s| pot.moves[s] > 0).fold(0, |m, s| m | 1 << s);
                w.bits(mask, 8);
                for &v in pot.moves.iter().filter(|&&v| v > 0) {
                    write_varint(w, v);
                }
            }
        }
    }
}

/// A 303's payload after the key.
pub fn write_hole_cards(w: &mut BitWriter, cards: [u8; 2], value: u32) {
    w.bits(cards[0] as u32, 8).bits(cards[1] as u32, 8).u32(value);
}

/// What a client reads from a [`Poker::write`] section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub dealer: u8,
    pub current: u8,
    pub actions: u16,
    pub phase: u8,
    pub round: u8,
    pub run_out_from: u8,
    pub show: u8,
    pub time: u16,
    pub min_wager: u32,
    pub last: (u8, u16, u32),
    pub min_raise: u32,
    pub board: Vec<u8>,
    pub bets: [u32; SEATS],
    /// `(seat, cards and hndval if sent, actn, crdts, accnt)`.
    pub players: Vec<(u8, Option<([u8; 2], u32)>, u16, u32, u32)>,
    pub pots: Vec<Pot>,
    pub collecting: bool,
}

impl View {
    pub fn read(r: &mut BitReader) -> Option<Self> {
        let mut v = View { dealer: r.bits(8)? as u8, ..View::default() };
        let count = r.bits(8)?;
        v.current = r.bits(8)? as u8;
        v.actions = r.bits(16)? as u16;
        v.phase = r.bits(8)? as u8;
        v.round = r.bits(8)? as u8;
        v.run_out_from = r.bits(8)? as u8;
        v.show = r.bits(8)? as u8;
        v.time = r.bits(16)? as u16;
        v.min_wager = r.u32()?;
        let last_seat = r.bits(8)? as u8;
        let last_value = read_varint(r)?;
        v.min_raise = read_varint(r)?;
        v.last = (last_seat, r.bits(16)? as u16, last_value);
        let cards = match v.round {
            0 | 1 => 0,
            2 => 3,
            3 => 4,
            _ => 5,
        };
        for _ in 0..cards {
            v.board.push(r.bits(8)? as u8);
        }
        let mask = r.bits(8)?;
        for s in (0..SEATS).filter(|s| mask & 1 << s != 0) {
            v.bets[s] = read_varint(r)?;
        }
        for _ in 0..count {
            let seat = r.bits(8)? as u8;
            let cards = if r.bool()? {
                Some(([r.bits(8)? as u8, r.bits(8)? as u8], r.u32()?))
            } else {
                None
            };
            v.players.push((seat, cards, r.bits(16)? as u16, read_varint(r)?, read_varint(r)?));
        }
        let pots = r.bits(8)?;
        v.collecting = pots & 0x80 != 0;
        for _ in 0..pots & 0x7f {
            let mut pot = Pot { amount: r.u32()?, eligible: r.bits(8)? as u8, ..Pot::default() };
            if v.collecting {
                let mask = r.bits(8)?;
                for s in (0..SEATS).filter(|s| mask & 1 << s != 0) {
                    pot.moves[s] = read_varint(r)?;
                }
            }
            v.pots.push(pot);
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    /// `"Ah"` → 25.
    fn card(c: &str) -> u8 {
        let b = c.as_bytes();
        let rank = b"23456789TJQKA".iter().position(|&r| r == b[0]).unwrap() as u8;
        let suit = b"dhsc".iter().position(|&s| s == b[1]).unwrap() as u8;
        suit * 13 + rank
    }

    fn cards(s: &str) -> Vec<u8> {
        s.split_whitespace().map(card).collect()
    }

    fn value(s: &str) -> u32 {
        hand_value(&cards(s))
    }

    #[test]
    fn hand_values() {
        assert_eq!([card("Ah"), card("Kd"), card("7s"), card("7c"), card("2c")], [25, 11, 31, 44, 39]);
        // The spec's values.
        assert_eq!(value("Ah Kd"), 0x000C_B000);
        assert_eq!(value("7s 7c"), 0x0105_0000);
        assert_eq!(value("Ah Kd Ad 7h 2c"), 0x010C_B500);
        assert_eq!(value("7s 7c Ad 7h 2c"), 0x0305_C000);
        // Every category, best five of seven.
        assert_eq!(value("Ah Kd Qs 9c 7h 4d 2s"), 0x000C_BA75);
        assert_eq!(value("Ah Ad Ks Kc 7h 7d 2s"), 0x020C_B500);
        assert_eq!(value("5h 4d 3s 2c Ah Kd Qs"), 0x0403_0000, "the wheel");
        assert_eq!(value("Th Jd Qs Kc Ah 2d 2s"), 0x040C_0000);
        assert_eq!(value("2h 7h 9h Jh Kh Ah Ad"), 0x050C_B975);
        assert_eq!(value("7h 7d 7s Kc Kh Ad Ac"), 0x0605_C000, "two pairs to pick from");
        assert_eq!(value("7h 7d 7s Kc Kh Kd Ac"), 0x060B_5000, "two trips");
        assert_eq!(value("9h 9d 9s 9c Kh 2d Ac"), 0x0707_C000);
        assert_eq!(value("5h 4h 3h 2h Ah Kd Qs"), 0x0803_0000);
        assert!(value("Ah Kh Qh Jh Th") > value("9s 9d 9h 9c As"));
        assert!(value("Ah Ad 3c") > value("Ks Kd Qc"));
    }

    /// Stacks for two seats.
    fn stacks(a: u32, b: u32) -> Vec<Option<u32>> {
        vec![Some(a), Some(b)]
    }

    fn section(g: &Poker, now: u64, st: &[Option<u32>]) -> Vec<u8> {
        let chips: Vec<_> = st.iter().enumerate().filter_map(|(s, c)| c.map(|c| (s as u8, c, 200))).collect();
        let mut w = BitWriter::new();
        g.write(&mut w, now, &chips);
        w.finish()
    }

    /// A worked hand, state sections byte for byte (bytes worked out from
    /// the client's reader, `sub_487DE0`).
    #[test]
    fn worked_hand() {
        // Seat 1 is dealt first (left of the button), then seat 0.
        let mut deck = cards("7s Ah 7c Kd Ad 7h 2c 3d 4d");
        deck.extend((0..52).filter(|c| !deck.contains(c)).collect::<Vec<_>>());
        let deck: [u8; 52] = deck.try_into().unwrap();
        let mut st = stacks(200, 200);
        let mut g = Poker::new(0, 10);
        assert_eq!(g.tick(1_999, &mut st, || unreachable!()), None);
        let out = g.tick(2_000, &mut st, || deck).unwrap();
        assert_eq!(out.hole_cards, [(0, [25, 11], 0x000C_B000), (1, [31, 44], 0x0105_0000)]);
        let body = |m: &str| hex(m)[7..].to_vec();
        assert_eq!(
            section(&g, 2_000, &st),
            body("A2 32 A5 0B 01 23 00 00 02 FF 00 00 05 01 00 FF 00 00 00 00 00 0A 01 8A 8A 00 80 03 85 8A
                  00 00 20 4E 1C E4 00 80 20 26 FA 72 00 00")
        );
        let t = 2_000 + deal_ms(2);
        assert!(g.tick(t - 1, &mut st, || unreachable!()).is_none());
        assert_eq!(g.tick(t, &mut st, || unreachable!()), Some(Out::default()));
        assert_eq!(
            section(&g, t, &st),
            body("A2 32 A5 0B 01 23 00 00 02 00 00 0E 06 01 00 FF 00 00 00 00 00 0A 01 8A 8A 00 80 03 85 8A
                  00 00 20 4E 1C E4 00 80 20 26 FA 72 00 00")
        );

        // A calls, B checks: the bets are collected.
        let msg = hex("01 23 00 00 02");
        let mut r = BitReader::new(&msg);
        r.bits(24);
        let a = Action::parse(id::ACT, &mut r).unwrap();
        assert_eq!(g.act(1, a, t, &mut st), Err(NotYourTurn));
        g.act(0, a, t, &mut st).unwrap();
        assert_eq!(
            section(&g, t, &st),
            body("A2 32 A5 0B 01 23 00 00 02 01 00 1C 06 01 00 FF 00 00 00 00 00 0A 00 85 8A 00 02 03 8A 8A
                  00 00 01 4D F4 E4 00 80 20 26 FA 72 00 00")
        );
        g.act(1, Action::Plain(action::CHECK), t, &mut st).unwrap();
        assert_eq!(
            section(&g, t, &st),
            body("A2 32 A5 0B 01 23 00 00 02 FF 00 00 08 01 00 FF 00 00 00 00 00 0A 01 45 00 08 00 00 00 00
                  A6 FA 72 00 40 02 13 7D 39 10 20 00 00 02 80 60 71 51 40")
        );

        // The flop, with new hand values for both.
        let t = t + COLLECT_MS;
        let out = g.tick(t, &mut st, || unreachable!()).unwrap();
        assert_eq!(out.hole_cards, [(0, [25, 11], 0x010C_B500), (1, [31, 44], 0x0305_C000)]);
        assert_eq!(
            section(&g, t, &st),
            body("A2 32 A5 0B 01 23 00 00 02 FF 00 00 05 02 00 FF 00 00 00 00 00 0A FF 45 00 00 06 09 13 80
                  00 00 00 26 FA 72 00 40 00 13 7D 39 00 20 00 00 02 80 60")
        );
        let t = t + STREET_MS;
        g.tick(t, &mut st, || unreachable!()).unwrap();
        assert_eq!(
            section(&g, t, &st),
            body("A2 32 A5 0B 01 23 00 00 02 01 00 34 06 02 00 FF 00 00 00 00 00 0A FF 45 00 00 06 09 13 80
                  00 00 00 26 FA 72 00 40 00 13 7D 39 00 20 00 00 02 80 60")
        );
        g.act(1, Action::Plain(action::CHECK), t, &mut st).unwrap();
        assert_eq!(
            section(&g, t, &st),
            body("A2 32 A5 0B 01 23 00 00 02 00 00 34 06 02 00 FF 00 00 00 00 00 0A 01 45 00 08 06 09 13 80
                  00 00 00 26 FA 72 00 40 02 13 7D 39 00 20 00 00 02 80 60")
        );
        let msg = hex("01 23 00 00 20 91 40");
        let mut r = BitReader::new(&msg);
        r.bits(24);
        let bet = Action::parse(id::WAGER, &mut r).unwrap();
        assert_eq!(bet, Action::Wager(action::BET, 20));
        g.act(0, bet, t, &mut st).unwrap();
        assert_eq!(
            section(&g, t, &st),
            body("A2 32 A5 0B 01 23 00 00 02 01 00 0E 06 02 00 FF 00 00 00 00 00 14 00 91 49 14 00 20 0C 12
                  27 01 91 40 00 01 04 D5 4E 40 08 00 42 6F A7 20 04 00 00 00 50 0C")
        );

        // B folds: A's bet is collected into a pot only A can win.
        g.act(1, Action::Plain(action::FOLD), t, &mut st).unwrap();
        assert_eq!(
            section(&g, t, &st),
            body("A2 32 A5 0B 01 23 00 00 02 FF 00 00 08 02 00 FF 00 00 00 00 00 14 01 48 A0 00 20 60 91 38
                  00 00 00 82 6A A7 20 05 00 09 37 D3 91 02 00 00 00 50 02 03 22 80")
        );
        let t = t + COLLECT_MS;
        g.tick(t, &mut st, || unreachable!()).unwrap();
        assert_eq!(st, stacks(210, 190));
        assert_eq!(
            section(&g, t, &st),
            body("A2 32 A5 0B 01 23 00 00 02 FF 00 00 09 02 00 FE 00 00 00 00 00 14 01 48 A0 00 20 60 91 38
                  00 00 00 82 74 A7 20 05 00 09 37 D3 90 02 00 00 00 50 02")
        );
        let data = section(&g, t, &st);
        let v = View::read(&mut BitReader::new(&data)).unwrap();
        assert_eq!((v.phase, v.show, v.pots[0].amount, v.players[1].2), (phase::SHOWDOWN, NO_SHOWDOWN, 40, 0x8004));

        // The next hand: B deals and posts the small blind.
        let t = t + SHOWDOWN_MS + POT_MS;
        let out = g.tick(t, &mut st, || deck).unwrap();
        assert_eq!(out.hole_cards.len(), 2);
        assert_eq!((g.dealer, g.phase, g.round), (1, phase::DEAL, 1));
        assert_eq!(st, stacks(200, 185));
    }

    /// Three players: an all-in builds a side pot; the run-out deals to the
    /// river and the best hands win each pot.
    #[test]
    fn side_pot_and_run_out() {
        // Dealt from seat 1: seat 1 gets Ah Ad, seat 2 Ks Kd, seat 0 2c 7d.
        let mut deck = cards("Ah Ks 2c Ad Kd 7d Kh 9c 4s 3h Jd");
        deck.extend((0..52).filter(|c| !deck.contains(c)).collect::<Vec<_>>());
        let deck: [u8; 52] = deck.try_into().unwrap();
        let mut st = vec![Some(100), Some(50), Some(100)];
        let mut g = Poker::new(0, 10);
        g.tick(START_MS, &mut st, || deck).unwrap();
        // Button seat 0, small blind 1, big blind 2; seat 0 acts first.
        assert_eq!((g.dealer, g.big_blind_seat), (0, 2));
        let mut t = START_MS + deal_ms(3);
        g.tick(t, &mut st, || unreachable!()).unwrap();
        assert_eq!((g.current, g.actions), (0, action::RAISE | action::FOLD | action::CALL));
        // Seat 0 raises to 30; seat 1 goes all in for 50; seat 2 calls;
        // seat 0 calls.
        g.act(0, Action::Wager(action::RAISE, 30), t, &mut st).unwrap();
        assert_eq!((g.current, g.actions), (1, action::RAISE | action::FOLD | action::CALL));
        g.act(1, Action::Plain(action::ALL_IN), t, &mut st).unwrap();
        assert_eq!(g.last, (1, action::ALL_IN, 45));
        assert_eq!(g.min_raise, 20, "a raise of 20 over 30 is a full raise");
        g.act(2, Action::Plain(action::CALL), t, &mut st).unwrap();
        // Over-wagering is clamped: seat 0 cannot raise past its stack.
        assert_eq!(g.current, 0);
        g.act(0, Action::Plain(action::CALL), t, &mut st).unwrap();
        assert_eq!((g.phase, g.collecting), (phase::COLLECT, true));
        assert_eq!(g.pots.len(), 1);
        assert_eq!(g.pots[0].amount, 150);
        t += COLLECT_MS;
        g.tick(t, &mut st, || unreachable!()).unwrap();
        assert_eq!(g.round, 2);
        t += STREET_MS;
        g.tick(t, &mut st, || unreachable!()).unwrap();
        // Postflop the first seat after the button acts: seat 1 is all in,
        // so seat 2. Seat 2 bets 50 (all in), seat 0 calls.
        assert_eq!(g.current, 2);
        g.act(2, Action::Wager(action::BET, 999), t, &mut st).unwrap();
        assert_eq!(g.last, (2, action::ALL_IN, 50));
        g.act(0, Action::Plain(action::CALL), t, &mut st).unwrap();
        assert_eq!(st, vec![Some(0), Some(0), Some(0)]);
        assert_eq!(g.pots.iter().map(|p| (p.amount, p.eligible)).collect::<Vec<_>>(), [(150, 0b111), (100, 0b101)]);
        t += COLLECT_MS;
        let out = g.tick(t, &mut st, || unreachable!()).unwrap();
        // Nobody can bet any more: deal turn and river at once.
        assert_eq!((g.phase, g.round, g.run_out_from), (phase::DEAL, 4, 2));
        assert_eq!(out.hole_cards.len(), 3);
        t += STREET_MS + RUN_OUT_MS;
        g.tick(t, &mut st, || unreachable!()).unwrap();
        // Board Kh 9c 4s 3h Jd: seat 2's trip kings beat the aces in both
        // pots.
        assert_eq!((g.phase, g.reveal), (phase::SHOWDOWN, true));
        assert_eq!(st, vec![Some(0), Some(0), Some(250)]);
        let mut w = BitWriter::new();
        g.write(&mut w, t, &[(0, 0, 100), (1, 0, 50), (2, 250, 100)]);
        let data = w.finish();
        let v = View::read(&mut BitReader::new(&data)).unwrap();
        assert_eq!(v.show, NONE);
        assert!(v.players.iter().all(|p| p.1.is_some()), "everyone in a pot shows");
        // Seats 0 and 1 are broke: no next hand.
        t += SHOWDOWN_MS + 2 * POT_MS;
        assert_eq!(g.tick(t, &mut st, || unreachable!()), Some(Out::default()));
        assert_eq!(g.phase, phase::WAITING);
    }

    #[test]
    fn timeouts_sit_out_and_leavers() {
        let mut st = vec![Some(100), Some(100), Some(100)];
        let mut g = Poker::new(0, 10);
        let deck: [u8; 52] = std::array::from_fn(|i| i as u8);
        g.tick(START_MS, &mut st, || deck).unwrap();
        let mut t = START_MS + deal_ms(3);
        g.tick(t, &mut st, || unreachable!()).unwrap();
        // Seat 0 lets the clock run out twice: it folds and sits out.
        assert_eq!(g.current, 0);
        assert!(g.tick(t + TURN_MS - 1, &mut st, || unreachable!()).is_none());
        t += TURN_MS;
        g.tick(t, &mut st, || unreachable!()).unwrap();
        assert!(g.players[0].folded);
        assert_eq!(g.players[0].timeouts, 1);
        // Seat 1 (small blind) asks to sit out on its turn: it folds at once.
        assert_eq!(g.current, 1);
        g.act(1, Action::SitOut(true), t, &mut st).unwrap();
        assert!(g.players[1].folded);
        // Seat 2 wins the blinds without a showdown.
        assert_eq!(g.phase, phase::COLLECT);
        t += COLLECT_MS;
        g.tick(t, &mut st, || unreachable!()).unwrap();
        assert_eq!((g.phase, g.reveal), (phase::SHOWDOWN, false));
        assert_eq!(st, vec![Some(100), Some(95), Some(105)]);
        // Seat 1 sits out the next hand; seats 0 and 2 play.
        t += SHOWDOWN_MS + POT_MS;
        g.tick(t, &mut st, || deck).unwrap();
        assert!(!g.players[1].dealt);
        assert_eq!(g.actn(1), flag::NOT_DEALT | flag::SITTING_OUT);
        t += deal_ms(3);
        g.tick(t, &mut st, || unreachable!()).unwrap();
        // Heads-up: the button (seat 2) is the small blind and acts first;
        // it leaves, and seat 0 wins.
        assert_eq!((g.dealer, g.current), (2, 2));
        st[2] = None;
        assert!(g.left(2, t, &mut st).is_some());
        assert_eq!(g.phase, phase::COLLECT);
        t += COLLECT_MS;
        g.tick(t, &mut st, || unreachable!()).unwrap();
        assert_eq!(st, vec![Some(105), Some(95), None]);
    }
}

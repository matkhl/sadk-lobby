//! Dice ("Würfeln"), the tavern game for one to four players: everyone bets
//! chips on the sums 2-12 of two dice, the dealer throws, and the house pays
//! (see docs/TABLE-GAMES.md).
//!
//! The client computes each player's payout itself when phase 3 starts
//! (`MiniGameDiceProxy`, `sub_4871B0`), so the server must use the same
//! formula and go through every phase: 1 bets (15 s, the client's own
//! countdown), 2 the dealer throws, 3 the dice are shown, 4 payout. Chips
//! that lie on the table stay in `crdts` (the client shows `crdts` minus
//! the bets) until the 4 → 1 update settles them.

use crate::lobby_message::{BitReader, BitWriter};
use crate::minigame::{read_varint, write_varint};

pub mod id {
    /// Key, `crdts` var, `bt00`..`bt10` vars: the sender's complete bets
    /// (absolute totals) after a click in phase 1.
    pub const BET: u16 = 220;
    /// Key only: the dealer throws (phase 2).
    pub const ROLL: u16 = 221;
}

/// `phase`.
pub mod phase {
    pub const BETS: u8 = 1;
    pub const ROLL: u8 = 2;
    pub const SHOW: u8 = 3;
    pub const PAYOUT: u8 = 4;
}

/// How long each phase lasts, in ms. Phase 1 is the client's countdown;
/// phase 2 ends earlier when the dealer throws.
pub const BETS_MS: u64 = 15_000;
pub const ROLL_MS: u64 = 10_000;
pub const SHOW_MS: u64 = 2_500;
pub const PAYOUT_MS: u64 = 4_000;

/// Bet fields: `bt[i]` is on the sum `i + 2`.
pub const FIELDS: usize = 11;
pub const SEATS: usize = 4;
pub type Bets = [u32; FIELDS];

/// The two colours alternate around 7, which belongs to neither.
fn colour(sum: u8) -> Option<bool> {
    match sum {
        2 | 4 | 6 | 9 | 11 => Some(true),
        3 | 5 | 8 | 10 | 12 => Some(false),
        _ => None,
    }
}

/// What a seat's bets return on `sum`, stake included, as the client
/// computes it: the exact sum pays 5×, the other fields of its colour give
/// the stake back, and a 7 pays only field 7, 3×.
pub fn payout(bets: &Bets, sum: u8) -> u32 {
    if sum == 7 {
        return 3 * bets[5];
    }
    (0..FIELDS)
        .map(|i| {
            let n = i as u8 + 2;
            if n == sum {
                5 * bets[i]
            } else if colour(n).is_some() && colour(n) == colour(sum) {
                bets[i]
            } else {
                0
            }
        })
        .sum()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dice {
    pub phase: u8,
    /// When the phase began, in the table clock's ms.
    pub since: u64,
    /// `dlr`: the seat that throws.
    pub dealer: u8,
    /// Drawn when betting closes, shown from phase 3 on.
    pub roll: Option<(u8, u8)>,
    pub bets: [Bets; SEATS],
}

impl Dice {
    /// A new round of betting, dealt by the lowest seat taken.
    pub fn new(now: u64, occupied: &[bool]) -> Self {
        let dealer = occupied.iter().position(|&o| o).unwrap_or(0) as u8;
        Self { phase: phase::BETS, since: now, dealer, roll: None, bets: [[0; FIELDS]; SEATS] }
    }

    fn sum(&self) -> Option<u8> {
        self.roll.map(|(a, b)| a + b)
    }

    /// The next taken seat after the dealer, wrapping (the dealer itself if
    /// nobody else sits there).
    fn next_dealer(&self, occupied: &[bool]) -> u8 {
        let n = occupied.len().max(1);
        (1..=n).map(|k| (self.dealer as usize + k) % n).find(|&s| occupied.get(s) == Some(&true)).unwrap_or(0) as u8
    }

    /// Message 220: the seat's complete bets, with at most `credits` on the
    /// table. Only in phase 1.
    pub fn bet(&mut self, seat: usize, bets: Bets, credits: u32) -> bool {
        let total: u64 = bets.iter().map(|&b| b as u64).sum();
        if self.phase != phase::BETS || seat >= SEATS || total > credits as u64 {
            return false;
        }
        self.bets[seat] = bets;
        true
    }

    /// Message 221: the dealer throws.
    pub fn throw(&mut self, seat: usize, now: u64) -> bool {
        if self.phase != phase::ROLL || seat != self.dealer as usize {
            return false;
        }
        self.phase = phase::SHOW;
        self.since = now;
        true
    }

    /// Moves on when a phase's time is up. Returns whether anything
    /// changed and, at the end of a round, each seat's change of chips.
    pub fn tick(&mut self, now: u64, occupied: &[bool], mut draw: impl FnMut() -> (u8, u8)) -> Option<Vec<(usize, i64)>> {
        let elapsed = now.saturating_sub(self.since);
        let mut settled = Vec::new();
        match self.phase {
            phase::BETS if elapsed >= BETS_MS => {
                self.roll = Some(draw());
                self.phase = phase::ROLL;
            }
            phase::ROLL if elapsed >= ROLL_MS => self.phase = phase::SHOW,
            phase::SHOW if elapsed >= SHOW_MS => self.phase = phase::PAYOUT,
            phase::PAYOUT if elapsed >= PAYOUT_MS => {
                let sum = self.sum().unwrap_or(0);
                for seat in 0..SEATS {
                    let staked: u64 = self.bets[seat].iter().map(|&b| b as u64).sum();
                    if staked > 0 {
                        settled.push((seat, payout(&self.bets[seat], sum) as i64 - staked as i64));
                    }
                }
                self.bets = [[0; FIELDS]; SEATS];
                self.roll = None;
                self.dealer = self.next_dealer(occupied);
                self.phase = phase::BETS;
            }
            _ => return None,
        }
        self.since = now;
        Some(settled)
    }

    /// A seat stood up (`occupied` without it): bets placed in phase 1 are
    /// void; later they are settled against the roll. Returns the seat's
    /// change of chips. A leaving dealer hands over, and throws first if
    /// the round waits for it.
    pub fn left(&mut self, seat: usize, now: u64, occupied: &[bool]) -> i64 {
        let bets = std::mem::replace(&mut self.bets[seat], [0; FIELDS]);
        let delta = match (self.phase, self.sum()) {
            (phase::BETS, _) | (_, None) => 0,
            (_, Some(sum)) => payout(&bets, sum) as i64 - bets.iter().map(|&b| b as i64).sum::<i64>(),
        };
        if seat == self.dealer as usize {
            self.dealer = self.next_dealer(occupied);
            if self.phase == phase::ROLL {
                self.phase = phase::SHOW;
                self.since = now;
            }
        }
        delta
    }

    /// The game's part of 217/218 (`sub_487740`); chips as `(seat, credits,
    /// account)`.
    pub fn write(&self, w: &mut BitWriter, now: u64, chips: &[(u8, u32, u32)]) {
        let dice = match (self.phase, self.roll) {
            (phase::SHOW | phase::PAYOUT, Some((a, b))) => a | b << 4,
            _ => 0,
        };
        let time = (now.saturating_sub(self.since) / 10).min(u16::MAX as u64);
        w.bits(dice as u32, 8).bits(self.dealer as u32, 8).bits(chips.len() as u32, 8).bits(self.phase as u32, 8);
        w.bits(time as u32, 16);
        for &(seat, credits, account) in chips {
            w.bits(seat as u32, 8);
            write_varint(w, credits);
            write_varint(w, account);
            for &b in &self.bets[seat as usize % SEATS] {
                write_varint(w, b);
            }
        }
    }

    /// Reads a [`Dice::write`] section: the state as of `now` (from `time`),
    /// and the seat chips.
    pub fn read(r: &mut BitReader, now: u64) -> Option<(Self, Vec<(u8, u32, u32)>)> {
        let dice = r.bits(8)? as u8;
        let dealer = r.bits(8)? as u8;
        let n = r.bits(8)?;
        let phase = r.bits(8)? as u8;
        let time = r.bits(16)? as u64;
        let mut bets = [[0; FIELDS]; SEATS];
        let mut chips = Vec::new();
        for _ in 0..n {
            let seat = r.bits(8)? as u8;
            chips.push((seat, read_varint(r)?, read_varint(r)?));
            for b in bets[seat as usize % SEATS].iter_mut() {
                *b = read_varint(r)?;
            }
        }
        let roll = (dice != 0).then_some((dice & 0xf, dice >> 4));
        Some((Self { phase, since: now.saturating_sub(time * 10), dealer, roll, bets }, chips))
    }
}

/// Message 220 after the key: the client's `crdts` (not trusted) and bets.
pub fn parse_bets(r: &mut BitReader) -> Option<(u32, Bets)> {
    let credits = read_varint(r)?;
    let mut bets = [0; FIELDS];
    for b in &mut bets {
        *b = read_varint(r)?;
    }
    Some((credits, bets))
}

pub fn write_bets(w: &mut BitWriter, credits: u32, bets: &Bets) {
    write_varint(w, credits);
    for &b in bets {
        write_varint(w, b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    fn bets(pairs: &[(u8, u32)]) -> Bets {
        let mut b = [0; FIELDS];
        for &(sum, v) in pairs {
            b[sum as usize - 2] = v;
        }
        b
    }

    #[test]
    fn payouts_follow_the_client() {
        // B's bets from the spec: 5 each on 8, 4 and 10; an 8 returns 30.
        let b = bets(&[(8, 5), (4, 5), (10, 5)]);
        assert_eq!(payout(&b, 8), 30);
        assert_eq!(payout(&b, 4), 25);
        assert_eq!(payout(&b, 7), 0);
        assert_eq!(payout(&bets(&[(7, 10)]), 7), 30);
        assert_eq!(payout(&bets(&[(7, 10)]), 8), 0);
        assert_eq!(payout(&bets(&[(2, 1), (11, 1), (12, 1)]), 2), 6);
    }

    /// The spec's worked round, byte for byte.
    #[test]
    fn worked_round() {
        let occupied = [true, true, false, false];
        let chips = |a: u32, b: u32| [(0u8, a, 100u32), (1, b, 100)];
        let section = |g: &Dice, now: u64, c: &[(u8, u32, u32)]| {
            let mut w = BitWriter::new();
            g.write(&mut w, now, c);
            w.finish()
        };
        let mut g = Dice::new(0, &occupied);
        assert_eq!(section(&g, 0, &chips(100, 100)), hex("00 00 02 01 00 00 00 96 49 64 00 00 32 c9 2c 80 00"));

        // A: 10 on 7; B: 5 on 8, 4 and 10, each click the full array.
        let msg1 = hex("01 23 00 96 40 45 00");
        let mut r = BitReader::new(&msg1);
        assert_eq!(r.bits(24), Some(0x012300));
        let (credits, a) = parse_bets(&mut r).unwrap();
        assert_eq!((credits, a), (100, bets(&[(7, 10)])));
        assert!(g.bet(0, a, 100));
        let msg2 = hex("01 23 00 96 42 14 42 a1 40");
        let mut r = BitReader::new(&msg2);
        r.bits(24);
        let (_, b) = parse_bets(&mut r).unwrap();
        assert_eq!(b, bets(&[(4, 5), (8, 5), (10, 5)]));
        assert!(g.bet(1, b, 100));
        assert!(!g.bet(1, bets(&[(4, 101)]), 100), "more than the chips");
        assert_eq!(section(&g, 7300, &chips(100, 100)), hex("00 00 02 01 02 da 00 96 49 64 04 50 00 65 92 59 08 51 0a 85 00"));

        // Betting closes at 15 s with a hidden 3 + 5; A throws.
        assert_eq!(g.tick(14_999, &occupied, || unreachable!()), None);
        assert_eq!(g.tick(15_000, &occupied, || (3, 5)), Some(vec![]));
        assert_eq!(section(&g, 15_000, &chips(100, 100)), hex("00 00 02 02 00 00 00 96 49 64 04 50 00 65 92 59 08 51 0a 85 00"));
        assert!(!g.throw(1, 16_000), "only the dealer throws");
        assert!(g.throw(0, 16_000));
        assert_eq!(section(&g, 16_000, &chips(100, 100)), hex("53 00 02 03 00 00 00 96 49 64 04 50 00 65 92 59 08 51 0a 85 00"));
        assert_eq!(g.tick(18_500, &occupied, || unreachable!()), Some(vec![]));
        assert_eq!(section(&g, 18_500, &chips(100, 100)), hex("53 00 02 04 00 00 00 96 49 64 04 50 00 65 92 59 08 51 0a 85 00"));

        // Settle 4 s later: A 100 - 10, B 100 - 15 + 30; seat 1 deals next.
        assert_eq!(g.tick(22_500, &occupied, || unreachable!()), Some(vec![(0, -10), (1, 15)]));
        assert_eq!(section(&g, 22_500, &chips(90, 115)), hex("00 01 02 01 00 00 00 95 a9 64 00 00 32 e7 2c 80 00"));

        let msg3 = hex("00 01 02 01 00 00 00 95 a9 64 00 00 32 e7 2c 80 00");
        let mut r = BitReader::new(&msg3);
        let (back, c) = Dice::read(&mut r, 22_500).unwrap();
        assert_eq!((back, c), (g, chips(90, 115).to_vec()));
    }

    #[test]
    fn idle_dealer_and_leavers() {
        let mut occupied = [true, true, true, false];
        let mut g = Dice::new(0, &occupied);
        assert!(g.bet(2, bets(&[(6, 4)]), 10));
        // Nobody throws: the dice show after 10 s anyway.
        g.tick(15_000, &occupied, || (2, 4));
        assert_eq!(g.tick(24_999, &occupied, || unreachable!()), None);
        g.tick(25_000, &occupied, || unreachable!());
        assert_eq!(g.phase, phase::SHOW);
        // Seat 2 leaves after the roll: settled against the 6 (4 * 5 - 4).
        occupied[2] = false;
        assert_eq!(g.left(2, 26_000, &occupied), 16);
        // The dealer leaves while it should throw: seat 1 takes over and
        // the dice show at once.
        let mut g = Dice::new(0, &[true, true, false, false]);
        g.tick(15_000, &occupied, || (1, 1));
        assert_eq!(g.left(0, 16_000, &[false, true, false, false]), 0);
        assert_eq!((g.dealer, g.phase), (1, phase::SHOW));
        // Bets placed in phase 1 by a leaver are void.
        let mut g = Dice::new(0, &[true, false, false, false]);
        g.bet(0, bets(&[(7, 3)]), 5);
        assert_eq!(g.left(0, 1_000, &[false; 4]), 0);
    }
}

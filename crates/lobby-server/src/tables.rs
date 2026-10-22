//! Table games in the village taverns (see `sadk_lobby_proto::minigame` and
//! docs/TABLE-GAMES.md): the tables, who sits at them, and the game each runs.
//!
//! Every village session watches all tables. A table is announced with a
//! 218 (settings only: seats in a 218 crash the client) and brought to life
//! by a 217 with its seats and game state; 217s then follow every change,
//! and a 216 removes it when its last player leaves. A newcomer gets the
//! 218 + 217 pair for each table after the world welcome and after the
//! avatars at the tables (village `join` sends those first).
//!
//! Each table has its own chat channel; the client joins it by itself when
//! it sees its own seat.

use crate::village::Event;
use sadk_lobby_proto::dice::{self, Dice};
use sadk_lobby_proto::lobby_message::{BitReader, BitWriter};
use sadk_lobby_proto::minigame::{self as mg, section, CreateTable, GameKind, Header, Join, Seat, Settings, TableKey};
use sadk_lobby_proto::pawnchess::{self, Money, PawnChess};
use sadk_lobby_proto::poker::{self, Poker};
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

/// `ActorID` type of a player's avatar (the id is the character id).
pub const ACTOR_AVATAR: u8 = 1;
/// Table chat channels are this plus the table key's `part`: clear of the
/// village (1) and zone (10 + scene) channels.
pub const CHAT_BASE: u32 = 1000;

struct Watcher {
    avatar: u32,
    tx: UnboundedSender<Event>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occupant {
    pub conn: u32,
    pub avatar: u32,
    /// `crdts`: the chips in front of the player now.
    pub credits: u32,
    /// `accnt`: all the chips the player brought (`stck` of 2001/209 and
    /// every 222).
    pub account: u32,
}

/// The game a table runs, with its state section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Game {
    PawnChess(PawnChess),
    Dice(Dice),
    Poker(Poker),
}

/// A message for one session (`Some(conn)`) or for everyone.
type Outgoing = (Option<u32>, u16, Vec<u8>);

pub struct Table {
    pub key: TableKey,
    pub kind: GameKind,
    pub funnies: bool,
    /// Tavern + 2.
    pub scene: u8,
    pub table: u8,
    pub chat: u32,
    pub settings: Settings,
    /// One entry per seat (`max_players`).
    pub seats: Vec<Option<Occupant>>,
    pub game: Game,
}

impl Table {
    fn header(&self, sections: u8) -> Header {
        Header { kind: self.kind, sections, funnies: self.funnies, scene: self.scene, table: self.table, chat: self.chat, key: self.key }
    }

    fn seat_list(&self) -> Vec<Seat> {
        self.seats
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.as_ref().map(|o| Seat { index: i as u8, actor_kind: ACTOR_AVATAR, actor: o.avatar }))
            .collect()
    }

    /// Header plus the named sections; `now` is the table clock in ms.
    pub fn encode(&self, sections: u8, now: u64) -> Vec<u8> {
        let mut w = BitWriter::new();
        self.header(sections).write(&mut w);
        if sections & section::SETTINGS != 0 {
            self.settings.write(&mut w);
        }
        if sections & section::SEATS != 0 {
            mg::write_seats(&mut w, &self.seat_list());
        }
        if sections & section::STATE != 0 {
            let chips: Vec<_> = self
                .seats
                .iter()
                .enumerate()
                .filter_map(|(i, o)| o.as_ref().map(|o| (i as u8, o.credits, o.account)))
                .collect();
            match &self.game {
                Game::PawnChess(g) => g.write(&mut w, &chips),
                Game::Dice(g) => g.write(&mut w, now, &chips),
                Game::Poker(g) => g.write(&mut w, now, &chips),
            }
        }
        w.finish()
    }

    /// The 218 + 217 pair that makes the table appear on a client.
    fn announce(&self, now: u64) -> [(u16, Vec<u8>); 2] {
        [(mg::id::TABLE_CREATE, self.encode(section::SETTINGS, now)), self.update(section::SEATS, now)]
    }

    /// A 217 with `sections` and the game state.
    fn update(&self, sections: u8, now: u64) -> (u16, Vec<u8>) {
        (mg::id::TABLE_UPDATE, self.encode(sections | section::STATE, now))
    }

    /// Each seat's `crdts` (`None` for an empty seat), for poker to move.
    fn stacks(&self) -> Vec<Option<u32>> {
        self.seats.iter().map(|o| o.as_ref().map(|o| o.credits)).collect()
    }

    fn set_stacks(&mut self, stacks: &[Option<u32>]) {
        for (o, c) in self.seats.iter_mut().zip(stacks) {
            if let (Some(o), Some(c)) = (o, c) {
                o.credits = *c;
            }
        }
    }

    /// A poker step's 303s, each for the player in that seat, then the 217
    /// for everyone.
    fn poker_step(&self, out: &poker::Out, now: u64) -> Vec<Outgoing> {
        let mut msgs: Vec<Outgoing> = Vec::new();
        for &(seat, cards, value) in &out.hole_cards {
            if let Some(Some(o)) = self.seats.get(seat) {
                let mut w = BitWriter::new();
                self.key.write(&mut w);
                poker::write_hole_cards(&mut w, cards, value);
                msgs.push((Some(o.conn), poker::id::HOLE_CARDS, w.finish()));
            }
        }
        let (id, data) = self.update(0, now);
        msgs.push((None, id, data));
        msgs
    }

    fn occupied(&self) -> Vec<bool> {
        self.seats.iter().map(Option::is_some).collect()
    }

    /// Adds `delta` to a seat's chips.
    fn adjust(&mut self, seat: usize, delta: i64) {
        if let Some(Some(o)) = self.seats.get_mut(seat) {
            o.credits = (o.credits as i64 + delta).clamp(0, u32::MAX as i64) as u32;
        }
    }

    pub fn seat_of(&self, conn: u32) -> Option<usize> {
        self.seats.iter().position(|o| o.as_ref().is_some_and(|o| o.conn == conn))
    }

    fn is_empty(&self) -> bool {
        self.seats.iter().all(Option::is_none)
    }

    /// Stake per game (`lmtmn`).
    fn stake_per_game(&self) -> u32 {
        self.settings.limit_min
    }

    fn pay(&mut self, money: &[Money]) {
        let stake = self.stake_per_game() as i64;
        for m in money {
            match *m {
                Money::Stake(seat) => self.adjust(seat, -stake),
                Money::Pot { seat, stakes } => self.adjust(seat, stake * stakes as i64),
            }
        }
    }

    /// Starts a game when every seat is taken and everyone can pay, and
    /// holds a new one back (phase 1) while someone cannot.
    fn check_start(&mut self) {
        let stake = self.stake_per_game();
        let ready = self.seats.iter().all(|o| o.as_ref().is_some_and(|o| o.credits >= stake));
        if let Game::PawnChess(g) = &mut self.game {
            match g.phase {
                pawnchess::phase::WAITING if ready => g.start(),
                pawnchess::phase::PLACE_KING_0 if !ready => g.phase = pawnchess::phase::WAITING,
                _ => {}
            }
        }
    }
}

/// Why a create, join or action was refused. The client gets 219 for a
/// create and 220 for a join either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    UnknownGame,
    NoSuchTavern,
    TableTaken,
    NoTableLeft,
    NoSuchTable,
    Full,
    WrongPassword,
    TooFewChips,
    AlreadySeated,
    NotSeated,
    UnknownAction,
    /// A dice bet or throw out of turn, or bets beyond the chips.
    NotNow,
    Illegal(pawnchess::Illegal),
}

pub struct Tables {
    tables: BTreeMap<TableKey, Table>,
    watchers: HashMap<u32, Watcher>,
    next_part: u16,
    /// The table clock: ms since `epoch`, plus `skipped` (tests move it on).
    epoch: Instant,
    skipped: u64,
}

impl Default for Tables {
    fn default() -> Self {
        Self { tables: BTreeMap::new(), watchers: HashMap::new(), next_part: 0, epoch: Instant::now(), skipped: 0 }
    }
}

/// A fair number below `n` (at most 256) from the OS random source.
fn random_below(n: u32) -> u32 {
    // Reject the top of the range, so every value is equally likely.
    let limit = 256 - 256 % n;
    loop {
        let mut b = [0u8; 1];
        if getrandom::getrandom(&mut b).is_err() {
            return std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos()) % n;
        }
        if (b[0] as u32) < limit {
            return b[0] as u32 % n;
        }
    }
}

/// Two fair dice.
fn roll_dice() -> (u8, u8) {
    (1 + random_below(6) as u8, 1 + random_below(6) as u8)
}

/// A shuffled deck (Fisher-Yates).
fn shuffle() -> [u8; 52] {
    let mut deck: [u8; 52] = std::array::from_fn(|i| i as u8);
    for i in (1..52).rev() {
        deck.swap(i, random_below(i as u32 + 1) as usize);
    }
    deck
}

impl Tables {
    fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + self.skipped
    }

    /// Moves the table clock on (tests).
    #[cfg(test)]
    fn skip(&mut self, ms: u64) {
        self.skipped += ms;
    }

    /// Runs the timed parts of the games (dice phases, poker hands); call
    /// a few times a second.
    pub fn tick(&mut self) {
        let now = self.now();
        let mut msgs = Vec::new();
        for t in self.tables.values_mut() {
            let occupied = t.occupied();
            let mut stacks = t.stacks();
            match &mut t.game {
                Game::Dice(g) => {
                    if let Some(settled) = g.tick(now, &occupied, roll_dice) {
                        for (seat, delta) in settled {
                            t.adjust(seat, delta);
                        }
                        let (id, data) = t.update(0, now);
                        msgs.push((None, id, data));
                    }
                }
                Game::Poker(g) => {
                    if let Some(out) = g.tick(now, &mut stacks, shuffle) {
                        t.set_stacks(&stacks);
                        msgs.extend(t.poker_step(&out, now));
                    }
                }
                Game::PawnChess(_) => {}
            }
        }
        self.send_all(msgs);
    }

    /// A village session starts watching: it gets every table.
    pub fn watch(&mut self, conn: u32, avatar: u32, tx: UnboundedSender<Event>) {
        let now = self.now();
        for t in self.tables.values() {
            for (id, data) in t.announce(now) {
                let _ = tx.send(Event::Village { id, data });
            }
        }
        self.watchers.insert(conn, Watcher { avatar, tx });
    }

    /// The session leaves the village: it stands up from its table. Call
    /// this before the avatar leaves the village, so no client keeps a
    /// seat for an avatar it no longer has.
    pub fn unwatch(&mut self, conn: u32) {
        self.leave(conn);
        self.watchers.remove(&conn);
    }

    pub fn get(&self, key: &TableKey) -> Option<&Table> {
        self.tables.get(key)
    }

    pub fn table_of(&self, conn: u32) -> Option<TableKey> {
        self.tables.values().find(|t| t.seat_of(conn).is_some()).map(|t| t.key)
    }

    fn taken(&self, scene: u8, table: u8) -> bool {
        self.tables.values().any(|t| t.scene == scene && t.table == table)
    }

    /// Message 2001: a new table with its creator in seat 0.
    pub fn create(&mut self, conn: u32, c: &CreateTable) -> Result<TableKey, Refused> {
        let kind = GameKind::from_u8(c.kind).ok_or(Refused::UnknownGame)?;
        if self.table_of(conn).is_some() {
            return Err(Refused::AlreadySeated);
        }
        if c.tavern > 1 {
            return Err(Refused::NoSuchTavern);
        }
        let limit_min = c.limit_min.max(1);
        if c.stake < limit_min {
            return Err(Refused::TooFewChips);
        }
        let scene = c.tavern + 2;
        let big_poker = kind == GameKind::Poker && c.max_players > 4;
        let table = if c.table == mg::ANY_TABLE {
            let mut free = if big_poker { 13..mg::TABLES } else { 0..13 };
            free.find(|&t| !self.taken(scene, t)).ok_or(Refused::NoTableLeft)?
        } else if c.table >= mg::TABLES || (c.table >= 13 && kind != GameKind::Poker) {
            return Err(Refused::NoSuchTable);
        } else if self.taken(scene, c.table) {
            return Err(Refused::TableTaken);
        } else {
            c.table
        };
        let max = match kind {
            GameKind::PawnChess => 2,
            GameKind::Dice => c.max_players.clamp(1, 4),
            GameKind::Poker if table >= 13 => c.max_players.clamp(2, 8),
            GameKind::Poker => c.max_players.clamp(2, 4),
        } as usize;
        // msgprt 0xffff is best avoided; 0 stays unused.
        self.next_part = self.next_part % 0xfffe + 1;
        let key = TableKey { part: self.next_part, round: 0 };
        let avatar = self.watchers.get(&conn).map_or(0, |w| w.avatar);
        let now = self.now();
        let mut seats = vec![None; max];
        seats[0] = Some(Occupant { conn, avatar, credits: c.stake, account: c.stake });
        let occupied: Vec<bool> = seats.iter().map(Option::is_some).collect();
        let mut t = Table {
            key,
            kind,
            funnies: c.play_money,
            scene,
            table,
            chat: CHAT_BASE + key.part as u32,
            settings: Settings { password: c.password, name: c.name.clone(), limit_min, limit_max: c.limit_max, max_players: max as u8 },
            seats,
            game: match kind {
                GameKind::PawnChess => Game::PawnChess(PawnChess::default()),
                GameKind::Dice => Game::Dice(Dice::new(now, &occupied)),
                GameKind::Poker => Game::Poker(Poker::new(now, limit_min)),
            },
        };
        t.check_start();
        for (id, data) in t.announce(now) {
            self.broadcast(id, data);
        }
        self.tables.insert(key, t);
        Ok(key)
    }

    /// Message 209: sit down at the first free seat.
    pub fn join(&mut self, conn: u32, j: &Join) -> Result<usize, Refused> {
        if self.table_of(conn).is_some() {
            return Err(Refused::AlreadySeated);
        }
        let avatar = self.watchers.get(&conn).map_or(0, |w| w.avatar);
        let now = self.now();
        let t = self.tables.get_mut(&j.key).ok_or(Refused::NoSuchTable)?;
        if t.settings.password.is_some_and(|crc| crc != j.password) {
            return Err(Refused::WrongPassword);
        }
        if j.stake < t.stake_per_game() {
            return Err(Refused::TooFewChips);
        }
        let seat = t.seats.iter().position(Option::is_none).ok_or(Refused::Full)?;
        t.seats[seat] = Some(Occupant { conn, avatar, credits: j.stake, account: j.stake });
        if let Game::Poker(g) = &mut t.game {
            g.joined(seat, now);
        }
        t.check_start();
        let (id, data) = t.update(section::SEATS, now);
        self.broadcast(id, data);
        Ok(seat)
    }

    /// Message 204 or leaving the village: stand up (a running game is
    /// lost); the last one out closes the table.
    pub fn leave(&mut self, conn: u32) -> Option<TableKey> {
        let key = self.table_of(conn)?;
        let now = self.now();
        let t = self.tables.get_mut(&key)?;
        let seat = t.seat_of(conn)?;
        let mut occupied = t.occupied();
        occupied[seat] = false;
        let mut stacks = t.stacks();
        stacks[seat] = None;
        let mut msgs = Vec::new();
        match &mut t.game {
            Game::PawnChess(g) => {
                let money = g.left(seat);
                t.pay(&money);
            }
            Game::Dice(g) => {
                let delta = g.left(seat, now, &occupied);
                t.adjust(seat, delta);
            }
            Game::Poker(g) => {
                let out = g.left(seat, now, &mut stacks);
                t.seats[seat] = None;
                t.set_stacks(&stacks);
                if let Some(out) = out.filter(|o| !o.hole_cards.is_empty()) {
                    msgs.extend(t.poker_step(&out, now).into_iter().filter(|m| m.0.is_some()));
                }
            }
        }
        t.seats[seat] = None;
        if t.is_empty() {
            let t = self.tables.remove(&key)?;
            self.broadcast(mg::id::TABLE_REMOVE, t.encode(0, now));
        } else {
            let (id, data) = t.update(section::SEATS, now);
            self.send_all(msgs);
            self.broadcast(id, data);
        }
        Some(key)
    }

    /// An action at the player's table (channel 5, after the table key):
    /// 222 adds chips, the rest belong to the game. The table's state goes
    /// out after every action, refused ones included: the client waits for
    /// a new `trnid` before it takes input again.
    pub fn act(&mut self, conn: u32, key: TableKey, id: u16, r: &mut BitReader) -> Result<(), Refused> {
        let now = self.now();
        let t = self.tables.get_mut(&key).ok_or(Refused::NoSuchTable)?;
        let seat = t.seat_of(conn).ok_or(Refused::NotSeated)?;
        let mut private = Vec::new();
        let mut stacks = t.stacks();
        let result = if id == mg::id::REBUY {
            let amount = r.u32().ok_or(Refused::UnknownAction)?;
            if let Some(Some(o)) = t.seats.get_mut(seat) {
                o.credits = o.credits.saturating_add(amount);
                o.account = o.account.saturating_add(amount);
            }
            Ok(())
        } else {
            match &mut t.game {
                Game::PawnChess(g) => {
                    let action = pawnchess::Action::parse(id, r).ok_or(Refused::UnknownAction)?;
                    match g.act(seat, action) {
                        Ok(money) => {
                            t.pay(&money);
                            Ok(())
                        }
                        Err(why) => Err(Refused::Illegal(why)),
                    }
                }
                Game::Dice(g) => {
                    let credits = t.seats[seat].as_ref().map_or(0, |o| o.credits);
                    let ok = match id {
                        dice::id::BET => {
                            let (_, bets) = dice::parse_bets(r).ok_or(Refused::UnknownAction)?;
                            g.bet(seat, bets, credits)
                        }
                        dice::id::ROLL => g.throw(seat, now),
                        _ => return Err(Refused::UnknownAction),
                    };
                    if ok {
                        Ok(())
                    } else {
                        Err(Refused::NotNow)
                    }
                }
                Game::Poker(g) => {
                    let action = poker::Action::parse(id, r).ok_or(Refused::UnknownAction)?;
                    let out = g.act(seat, action, now, &mut stacks).map_err(|_| Refused::NotNow)?;
                    t.set_stacks(&stacks);
                    private = t.poker_step(&out, now).into_iter().filter(|m| m.0.is_some()).collect();
                    Ok(())
                }
            }
        };
        t.check_start();
        let (id, data) = t.update(0, now);
        self.send_all(private);
        self.broadcast(id, data);
        result
    }

    fn broadcast(&self, id: u16, data: Vec<u8>) {
        for w in self.watchers.values() {
            let _ = w.tx.send(Event::Village { id, data: data.clone() });
        }
    }

    fn send_all(&self, msgs: Vec<Outgoing>) {
        for (to, id, data) in msgs {
            match to {
                Some(conn) => {
                    if let Some(w) = self.watchers.get(&conn) {
                        let _ = w.tx.send(Event::Village { id, data });
                    }
                }
                None => self.broadcast(id, data),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sadk_lobby_proto::minigame::TableMessage;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn drain(rx: &mut UnboundedReceiver<Event>) -> Vec<(u16, Vec<u8>)> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| match e {
                Event::Village { id, data } => (id, data),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    fn ids(rx: &mut UnboundedReceiver<Event>) -> Vec<u16> {
        drain(rx).into_iter().map(|(id, _)| id).collect()
    }

    fn create(kind: u8, max: u8, password: Option<u32>) -> CreateTable {
        CreateTable {
            kind,
            play_money: true,
            stake: 10,
            name: "Tisch".into(),
            limit_min: 1,
            limit_max: 0x7fff_ffff,
            max_players: max,
            tavern: 1,
            table: 0,
            password,
        }
    }

    #[test]
    fn create_join_leave_remove() {
        let mut t = Tables::default();
        let (a_tx, mut a) = unbounded_channel();
        let (b_tx, mut b) = unbounded_channel();
        t.watch(1, 10, a_tx);
        let key = t.create(1, &create(1, 4, None)).unwrap();
        let out = drain(&mut a);
        assert_eq!(out.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [mg::id::TABLE_CREATE, mg::id::TABLE_UPDATE]);
        // 218: settings only, funnies, scene 3, chat channel set.
        let m = TableMessage::read(&mut BitReader::new(&out[0].1)).unwrap();
        assert_eq!((m.header.sections, m.header.funnies, m.header.scene, m.header.table), (section::SETTINGS, true, 3, 0));
        assert_eq!(m.header.chat, CHAT_BASE + key.part as u32);
        assert!(m.seats.is_none());
        let m = TableMessage::read(&mut BitReader::new(&out[1].1)).unwrap();
        assert_eq!(m.seats.unwrap(), [Seat { index: 0, actor_kind: ACTOR_AVATAR, actor: 10 }]);

        t.watch(2, 20, b_tx);
        assert_eq!(ids(&mut b), [mg::id::TABLE_CREATE, mg::id::TABLE_UPDATE]);
        assert_eq!(t.join(2, &Join { key, stake: 10, password: 0 }), Ok(1));
        assert_eq!(ids(&mut a), [mg::id::TABLE_UPDATE]);
        assert_eq!(t.join(2, &Join { key, stake: 10, password: 0 }), Err(Refused::AlreadySeated));
        assert_eq!(t.create(2, &create(1, 4, None)), Err(Refused::AlreadySeated));

        t.leave(1);
        assert_eq!(ids(&mut b), [mg::id::TABLE_UPDATE, mg::id::TABLE_UPDATE]);
        t.unwatch(2);
        assert_eq!(ids(&mut a), [mg::id::TABLE_UPDATE, mg::id::TABLE_REMOVE]);
        assert!(t.get(&key).is_none());
    }

    #[test]
    fn refusals() {
        let mut t = Tables::default();
        let (tx, _rx) = unbounded_channel();
        for c in 1..=6 {
            t.watch(c, c * 10, tx.clone());
        }
        assert_eq!(t.create(1, &create(9, 2, None)), Err(Refused::UnknownGame));
        assert_eq!(t.create(1, &CreateTable { tavern: 2, ..create(3, 2, None) }), Err(Refused::NoSuchTavern));
        assert_eq!(t.create(1, &CreateTable { table: 13, ..create(3, 2, None) }), Err(Refused::NoSuchTable));
        assert_eq!(t.create(1, &CreateTable { stake: 0, ..create(3, 2, None) }), Err(Refused::TooFewChips));
        let key = t.create(1, &create(3, 2, Some(0xabcd))).unwrap();
        assert_eq!(t.create(2, &create(1, 4, None)), Err(Refused::TableTaken));
        assert_eq!(t.join(2, &Join { key, stake: 10, password: 1 }), Err(Refused::WrongPassword));
        assert_eq!(t.join(2, &Join { key, stake: 0, password: 0xabcd }), Err(Refused::TooFewChips));
        assert_eq!(t.join(2, &Join { key, stake: 10, password: 0xabcd }), Ok(1));
        assert_eq!(t.join(3, &Join { key, stake: 10, password: 0xabcd }), Err(Refused::Full));
        let other = TableKey { part: 999, round: 0 };
        assert_eq!(t.join(4, &Join { key: other, stake: 10, password: 0 }), Err(Refused::NoSuchTable));
    }

    #[test]
    fn the_server_picks_a_table() {
        let mut t = Tables::default();
        let (tx, _rx) = unbounded_channel();
        for c in 1..=4 {
            t.watch(c, c * 10, tx.clone());
        }
        let any = |kind, max| CreateTable { table: mg::ANY_TABLE, ..create(kind, max, None) };
        let a = t.create(1, &any(3, 2)).unwrap();
        let b = t.create(2, &any(2, 8)).unwrap();
        let c = t.create(3, &any(1, 4)).unwrap();
        assert_eq!([a, b, c].map(|k| t.get(&k).unwrap().table), [0, 13, 1]);
        assert_eq!(t.get(&b).unwrap().seats.len(), 8);
    }

    fn chess(t: &Tables, key: &TableKey) -> PawnChess {
        match &t.get(key).unwrap().game {
            Game::PawnChess(g) => g.clone(),
            other => panic!("{other:?}"),
        }
    }

    fn credits(t: &Tables, key: &TableKey) -> Vec<u32> {
        t.get(key).unwrap().seats.iter().flatten().map(|o| o.credits).collect()
    }

    fn act(t: &mut Tables, conn: u32, key: TableKey, id: u16, args: &[u8]) -> Result<(), Refused> {
        t.act(conn, key, id, &mut BitReader::new(args))
    }

    #[test]
    fn pawn_chess_table() {
        use pawnchess::phase;
        let mut t = Tables::default();
        let (tx, mut rx) = unbounded_channel();
        t.watch(1, 10, tx.clone());
        t.watch(2, 20, tx);
        // Stake per game is lmtmn = 1 here; both bring 10 chips.
        let key = t.create(1, &create(3, 4, None)).unwrap();
        assert_eq!(t.get(&key).unwrap().seats.len(), 2);
        assert_eq!(chess(&t, &key).phase, phase::WAITING);
        t.join(2, &Join { key, stake: 10, password: 0 }).unwrap();
        assert_eq!(chess(&t, &key).phase, phase::PLACE_KING_0);
        drain(&mut rx);

        assert_eq!(act(&mut t, 2, key, 402, &[3]), Err(Refused::Illegal(pawnchess::Illegal::NotYourTurn)));
        let out = drain(&mut rx);
        assert_eq!(out.len(), 2, "refusals resend the state to both watchers");
        let mut r = BitReader::new(&out[0].1);
        let m = TableMessage::read(&mut r).unwrap();
        assert_eq!(m.header.sections, section::STATE);
        let (g, chips) = PawnChess::read(&mut r).unwrap();
        assert_eq!((g.phase, chips), (phase::PLACE_KING_0, vec![(0, 10, 10), (1, 10, 10)]));

        act(&mut t, 1, key, 402, &[3]).unwrap();
        act(&mut t, 2, key, 402, &[4]).unwrap();
        assert_eq!(chess(&t, &key).phase, phase::MOVE_0);
        assert_eq!(credits(&t, &key), [9, 9]);
        act(&mut t, 1, key, 400, &[3, 17]).unwrap();
        assert_eq!(act(&mut t, 3, key, 400, &[12, 30]), Err(Refused::NotSeated));

        // Leaving mid-game hands the pot to the other player.
        t.leave(2);
        assert_eq!(credits(&t, &key), [11]);
        assert_eq!(chess(&t, &key).phase, phase::WAITING);
    }

    #[test]
    fn no_new_game_without_chips() {
        use pawnchess::phase;
        let mut t = Tables::default();
        let (tx, _rx) = unbounded_channel();
        t.watch(1, 10, tx.clone());
        t.watch(2, 20, tx);
        let key = t.create(1, &CreateTable { limit_min: 5, stake: 5, ..create(3, 2, None) }).unwrap();
        t.join(2, &Join { key, stake: 5, password: 0 }).unwrap();
        act(&mut t, 1, key, 402, &[3]).unwrap();
        act(&mut t, 2, key, 402, &[4]).unwrap();
        t.leave(2);
        assert_eq!(credits(&t, &key), [10]);
        // A newcomer with too few chips is refused; with enough the game
        // starts, and a rebuy is counted in accnt.
        assert_eq!(t.join(2, &Join { key, stake: 4, password: 0 }), Err(Refused::TooFewChips));
        t.join(2, &Join { key, stake: 5, password: 0 }).unwrap();
        assert_eq!(chess(&t, &key).phase, phase::PLACE_KING_0);
        act(&mut t, 2, key, mg::id::REBUY, &[0, 0, 0, 7]).unwrap();
        let o = t.get(&key).unwrap().seats[1].clone().unwrap();
        assert_eq!((o.credits, o.account), (12, 12));
    }

    fn dice_game(t: &Tables, key: &TableKey) -> Dice {
        match &t.get(key).unwrap().game {
            Game::Dice(g) => g.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dice_round() {
        use sadk_lobby_proto::dice::{self as d, phase};
        let mut t = Tables::default();
        let (tx, mut rx) = unbounded_channel();
        t.watch(1, 10, tx.clone());
        t.watch(2, 20, tx);
        // One player is enough for dice: betting starts at once.
        let key = t.create(1, &create(1, 4, None)).unwrap();
        assert_eq!(dice_game(&t, &key).phase, phase::BETS);
        t.join(2, &Join { key, stake: 10, password: 0 }).unwrap();
        drain(&mut rx);

        // Seat 0 bets 3 on 8 and 2 on 7, seat 1 tries more than its chips.
        let bet = |pairs: &[(usize, u32)], credits: u32| {
            let mut b = [0u32; d::FIELDS];
            for &(sum, v) in pairs {
                b[sum - 2] = v;
            }
            let mut w = BitWriter::new();
            d::write_bets(&mut w, credits, &b);
            w.finish()
        };
        act(&mut t, 1, key, d::id::BET, &bet(&[(8, 3), (7, 2)], 10)).unwrap();
        assert_eq!(act(&mut t, 2, key, d::id::BET, &bet(&[(8, 11)], 10)), Err(Refused::NotNow));
        assert_eq!(act(&mut t, 2, key, d::id::ROLL, &[]), Err(Refused::NotNow));
        let out = drain(&mut rx);
        let mut r = BitReader::new(&out[0].1);
        TableMessage::read(&mut r).unwrap();
        let (g, chips) = Dice::read(&mut r, 0).unwrap();
        assert_eq!(g.bets[0][6], 3);
        assert_eq!(chips, [(0, 10, 10), (1, 10, 10)], "bets stay in crdts until the round ends");

        // Betting closes after 15 s; the dealer (seat 0) throws.
        t.skip(d::BETS_MS);
        t.tick();
        assert_eq!(dice_game(&t, &key).phase, phase::ROLL);
        assert!(!drain(&mut rx).is_empty());
        act(&mut t, 1, key, d::id::ROLL, &[]).unwrap();
        let g = dice_game(&t, &key);
        let (a, b) = g.roll.unwrap();
        assert!((1..=6).contains(&a) && (1..=6).contains(&b));
        t.skip(d::SHOW_MS);
        t.tick();
        assert_eq!(dice_game(&t, &key).phase, phase::PAYOUT);
        t.skip(d::PAYOUT_MS);
        t.tick();
        let g = dice_game(&t, &key);
        assert_eq!((g.phase, g.dealer), (phase::BETS, 1));
        let won = d::payout(&{
            let mut b = [0; d::FIELDS];
            b[6] = 3;
            b[5] = 2;
            b
        }, a + b);
        assert_eq!(credits(&t, &key), [10 - 5 + won, 10]);
    }

    #[test]
    fn poker_hand() {
        use poker::{action, phase, Action};
        let mut t = Tables::default();
        let (a_tx, mut a) = unbounded_channel();
        let (b_tx, mut b) = unbounded_channel();
        t.watch(1, 10, a_tx);
        t.watch(2, 20, b_tx);
        let key = t.create(1, &CreateTable { limit_min: 2, ..create(2, 4, None) }).unwrap();
        t.join(2, &Join { key, stake: 10, password: 0 }).unwrap();
        drain(&mut a);
        drain(&mut b);
        let game = |t: &Tables| match &t.get(&key).unwrap().game {
            Game::Poker(g) => g.clone(),
            other => panic!("{other:?}"),
        };
        // The first hand is dealt 2 s after the second player sat down: each
        // player gets their own cards in a 303 before the 217.
        t.skip(poker::START_MS);
        t.tick();
        let out_a = drain(&mut a);
        assert_eq!(out_a.iter().map(|m| m.0).collect::<Vec<_>>(), [poker::id::HOLE_CARDS, mg::id::TABLE_UPDATE]);
        let g = game(&t);
        let mut r = BitReader::new(&out_a[0].1);
        assert_eq!(TableKey::read(&mut r), Some(key));
        assert_eq!([r.bits(8).unwrap() as u8, r.bits(8).unwrap() as u8], g.players[0].cards);
        let out_b = drain(&mut b);
        let mut r = BitReader::new(&out_b[0].1);
        TableKey::read(&mut r);
        assert_eq!(r.bits(8).unwrap() as u8, g.players[1].cards[0]);
        assert_eq!((g.phase, g.dealer), (phase::DEAL, 0));
        assert_eq!(credits(&t, &key), [9, 8]);

        // Seat 0 (button, small blind) acts first and folds; seat 1 wins
        // the blinds.
        t.skip(poker::deal_ms(2));
        t.tick();
        assert_eq!(game(&t).current, 0);
        let mut w = BitWriter::new();
        Action::Plain(action::FOLD).write(&mut w);
        assert_eq!(act(&mut t, 2, key, poker::id::ACT, &w.finish()), Err(Refused::NotNow));
        act(&mut t, 1, key, poker::id::ACT, &w.finish()).unwrap();
        assert_eq!(game(&t).phase, phase::COLLECT);
        t.skip(poker::COLLECT_MS);
        t.tick();
        assert_eq!(game(&t).phase, phase::SHOWDOWN);
        assert_eq!(credits(&t, &key), [9, 11]);

        // Leaving mid-hand folds: seat 1 (button now) leaves on its turn.
        t.skip(poker::SHOWDOWN_MS + poker::POT_MS);
        t.tick();
        t.skip(poker::deal_ms(2));
        t.tick();
        assert_eq!(game(&t).current, 1);
        t.leave(2);
        t.skip(poker::COLLECT_MS);
        t.tick();
        assert_eq!(credits(&t, &key), [9 - 2 + 3]);
    }
}

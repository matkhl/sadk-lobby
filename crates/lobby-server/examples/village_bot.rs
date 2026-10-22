//! A stand-in second player for testing the lobby village: logs in to the
//! lobby with `--login name:password` (the account must exist; a first
//! login with the game creates it), then to the village with a character's
//! id and a token sealed with the session key, as the game does, and walks
//! its avatar in a circle by sending message
//! 2000 three times a second, like the game does. It also logs in to usercomm,
//! joins the chat channels (1 is the village, "GLOBAL" in the game's chat; 10
//! is zone 0, "LOKAL") and says `--say` in each every `--every` seconds (an
//! empty `--say` keeps it quiet). With `--whisper-to <character id>` it
//! also whispers `--whisper` to that player as often. With `--mail-to
//! <character id>` it sends one letter (`--mail "title|text"`) through the
//! lobby's post office first, as the letterbox in the village does.
//! `--chess join` sits down at the first pawn chess table with a free seat
//! and `--chess create` opens one (`--table tavern,table`, 0 or 1 and
//! 0-12); either way it then plays random moves of those the client would
//! offer, and asks for a new game when one ends. `--dice join|create` does
//! the same for dice: a few chips on random sums each round, and a throw
//! when it is the dealer. `--poker join|create` plays Texas Hold'em: it
//! checks when it can, calls small bets, raises the minimum with two pair
//! or better, and folds the rest. Clients only seat avatars
//! they have spawned, so put the bot in the tavern with `--zone` (the
//! scene: tavern + 2) and an `--at` inside it.
//!
//! `--new-character Name --character-data <hex>` creates a character for
//! the account first (as "Avatar erstellen" does) and plays as it instead of
//! `--perm`; a first login with an unknown name registers the account.
//!
//! ```text
//! cargo run --example village_bot -- --msgdefs "<install>/bin/msgdefs.ini" --login besucher:test123 --perm 2
//!     [--lobby 127.0.0.1:17070] [--village 127.0.0.1:17071] [--usercomm 127.0.0.1:17072] [--at 810,650,967] [--radius 25] [--say text] [--every 20] [--channels 1,10]
//!     [--whisper-to 1] [--whisper text] [--mail-to 11] [--mail "Hallo|Wie geht's?"] [--chess join|create] [--dice join|create] [--poker join|create] [--table 1,0] [--zone 3]
//! ```
//!
//! `--at` is the circle centre in the 11-bit position units of message 2000
//! (the default is where a new retail avatar stands). `--perm` is the
//! character's id from `accounts.json`; the other players see its name and
//! look, and it shows online in their friend lists while the bot runs.

use sadk_lobby_proto::auth::{self, ClientExchange, Credentials, Token};
use sadk_lobby_proto::cell::{self, Cell};
use sadk_lobby_proto::frame::{self, Frame, FrameKind, Handshake};
use sadk_lobby_proto::lobby_message::{self as lm, BitReader, BitWriter, Location};
use sadk_lobby_proto::minigame::{self as mg, GameKind, TableKey, TableMessage};
use sadk_lobby_proto::dice::{self, Dice};
use sadk_lobby_proto::pawnchess::{phase, Action, PawnChess};
use sadk_lobby_proto::poker;
use sadk_lobby_proto::{Message, MsgDefs, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The chips the bot brings to a table: the lowest balance a character can
/// have, so the lobby never refuses it.
const BUY_IN: u32 = 10;

struct Options {
    msgdefs: String,
    lobby: String,
    login: (String, String),
    village: String,
    usercomm: String,
    perm: u32,
    at: [f32; 3],
    radius: f32,
    say: String,
    every: f32,
    channels: Vec<u32>,
    whisper_to: Option<u32>,
    whisper: String,
    mail_to: Option<u32>,
    mail: String,
    chess: Option<String>,
    dice: Option<String>,
    poker: Option<String>,
    table: (u8, u8),
    zone: u8,
    new_character: Option<(String, Vec<u8>)>,
}

fn options() -> Result<Options, String> {
    let mut o = Options {
        msgdefs: String::new(),
        lobby: "127.0.0.1:17070".into(),
        login: (String::new(), String::new()),
        village: "127.0.0.1:17071".into(),
        usercomm: "127.0.0.1:17072".into(),
        perm: 0,
        at: [810.0, 650.0, 967.0],
        radius: 25.0,
        say: "Hallo aus dem Siedlerdorf!".into(),
        every: 20.0,
        channels: vec![1, 10],
        whisper_to: None,
        whisper: "Psst, nur fuer dich!".into(),
        mail_to: None,
        mail: "Ein Brief|Gruesse aus dem Siedlerdorf!".into(),
        chess: None,
        dice: None,
        poker: None,
        table: (1, 0),
        zone: 0,
        new_character: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or(format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--msgdefs" => o.msgdefs = value,
            "--lobby" => o.lobby = value,
            "--login" => {
                let (name, password) = value.split_once(':').ok_or("--login needs name:password")?;
                o.login = (name.into(), password.into());
            }
            "--village" => o.village = value,
            "--usercomm" => o.usercomm = value,
            "--say" => o.say = value,
            "--whisper" => o.whisper = value,
            "--whisper-to" => o.whisper_to = Some(value.parse().map_err(|e| format!("--whisper-to: {e}"))?),
            "--mail-to" => o.mail_to = Some(value.parse().map_err(|e| format!("--mail-to: {e}"))?),
            "--mail" => o.mail = value,
            "--chess" if value == "join" || value == "create" => o.chess = Some(value),
            "--dice" if value == "join" || value == "create" => o.dice = Some(value),
            "--poker" if value == "join" || value == "create" => o.poker = Some(value),
            "--table" => {
                let (a, b) = value.split_once(',').ok_or("--table needs tavern,table")?;
                o.table = (a.trim().parse().map_err(|e| format!("--table: {e}"))?, b.trim().parse().map_err(|e| format!("--table: {e}"))?);
            }
            "--every" => o.every = value.parse().map_err(|e| format!("--every: {e}"))?,
            "--channels" => {
                o.channels = value.split(',').map(|c| c.trim().parse()).collect::<Result<_, _>>().map_err(|e| format!("--channels: {e}"))?
            }
            "--new-character" => o.new_character = Some((value, o.new_character.take().map(|c| c.1).unwrap_or_default())),
            "--character-data" => {
                let data = (0..value.len() / 2).map(|i| u8::from_str_radix(&value[2 * i..2 * i + 2], 16)).collect::<Result<Vec<_>, _>>().map_err(|e| format!("--character-data: {e}"))?;
                let name = o.new_character.take().map(|c| c.0).unwrap_or_default();
                o.new_character = Some((name, data));
            }
            "--zone" => o.zone = value.parse().map_err(|e| format!("--zone: {e}"))?,
            "--perm" => o.perm = value.parse().map_err(|e| format!("--perm: {e}"))?,
            "--radius" => o.radius = value.parse().map_err(|e| format!("--radius: {e}"))?,
            "--at" => {
                let v: Vec<f32> = value.split(',').map(|p| p.trim().parse()).collect::<Result<_, _>>().map_err(|e| format!("--at: {e}"))?;
                o.at = v.try_into().map_err(|_| "--at needs x,y,z".to_string())?;
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    if o.msgdefs.is_empty() || (o.perm == 0 && o.new_character.is_none()) || o.login.0.is_empty() {
        return Err("usage: village_bot --msgdefs <bin/msgdefs.ini> --login <name:password> --perm <character id> [--lobby host:port] [--village host:port] [--usercomm host:port] [--at x,y,z] [--radius n] [--say text] [--every s] [--channels a,b] [--whisper-to character id] [--whisper text] [--mail-to character id] [--mail title|text]".into());
    }
    Ok(o)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut o = options()?;
    let defs = MsgDefs::parse(&std::fs::read_to_string(&o.msgdefs).map_err(|e| format!("{}: {e}", o.msgdefs))?)?;
    // The lobby connection stays open: the session key lives as long as it.
    let mut lobby = Connection::lobby_login(&defs, &o.lobby, &o.login.0, &o.login.1)?;
    if let Some((name, data)) = &o.new_character {
        let fields = [("name", Value::text(name)), ("data", Value::Bytes(Some(data.clone()))), ("ticket_id", Value::Int(5))];
        lobby.connection.send("AddCharacter", &fields)?;
        let reply = lobby.connection.wait_for("AddResult")?;
        if reply.int("errorcode") != Some(0) {
            return Err(format!("AddCharacter: {reply}"));
        }
        o.perm = reply.int("id").unwrap_or(0) as u32;
        println!("created character {name} ({})", o.perm);
    }
    let mut village = Connection::login(&defs, &o.village, o.perm, &lobby)?;
    let mut usercomm = Connection::login(&defs, &o.usercomm, o.perm, &lobby)?;
    if let Some(to) = o.mail_to {
        let (title, text) = o.mail.split_once('|').unwrap_or((&o.mail, ""));
        // The game sends the text with its terminating zero.
        let mut text = text.as_bytes().to_vec();
        text.push(0);
        let letter = [
            ("delivery_target", Value::Int(to as i64)),
            ("creator", Value::Int(o.perm as i64)),
            ("creation_time", Value::Int(0)),
            ("title", Value::text(title)),
            ("message_text", Value::Bytes(Some(text))),
            ("data", Value::Bytes(Some(Vec::new()))),
            ("ticket_id", Value::Int(4)),
        ];
        lobby.connection.send("AddPrivateMessage", &letter)?;
        let reply = lobby.connection.wait_for("AddResult")?;
        println!("letter to {to}: {reply}");
    }

    // Print what the village tells us, on another thread.
    let mut reader = village.stream.try_clone().map_err(|e| e.to_string())?;
    let defs_r = defs.clone();
    let mut buf = std::mem::take(&mut village.buf);
    let (tables_tx, tables_rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(frame) = read(&mut reader, &mut buf) {
            match Message::decode(&defs_r, &frame.payload) {
                Ok(m) if m.name == "SendGameData" => {
                    let (_, _, id) = lm::split_header(m.int("msg_type").unwrap_or(0) as u32);
                    let data = m.bytes("data").unwrap_or_default().to_vec();
                    if id != lm::id::AVATAR_DATA {
                        println!("<- village message {id}: {data:02x?}");
                    }
                    if (mg::id::TABLE_REMOVE..=mg::id::TABLE_CREATE).contains(&id) || id == poker::id::HOLE_CARDS {
                        let _ = tables_tx.send((id, data));
                    }
                }
                Ok(m) => println!("<- {m}"),
                Err(e) => println!("<- undecodable: {e}"),
            }
        }
        println!("disconnected");
        std::process::exit(0);
    });

    // And what usercomm tells us: channel news and chat lines.
    let mut reader = usercomm.stream.try_clone().map_err(|e| e.to_string())?;
    let defs_r = defs.clone();
    let mut buf = std::mem::take(&mut usercomm.buf);
    std::thread::spawn(move || {
        while let Ok(frame) = read(&mut reader, &mut buf) {
            if !Cell::is_cell(&frame.payload) {
                match Message::decode(&defs_r, &frame.payload) {
                    Ok(m) => println!("<- usercomm {m}"),
                    Err(e) => println!("<- usercomm undecodable: {e}"),
                }
                continue;
            }
            match Cell::decode(&frame.payload) {
                Ok(Cell::Deliver { data, cell, from, .. }) => match Message::decode_propset(&defs_r, &data) {
                    Ok(m) => println!("<- cell {cell} from {from:#x}: {m}"),
                    Err(e) => println!("<- cell {cell} from {from:#x}: undecodable ({e})"),
                },
                Ok(other) => println!("<- {other:?}"),
                Err(e) => println!("<- undecodable cell: {e}"),
            }
        }
        println!("usercomm disconnected");
        std::process::exit(0);
    });
    for (ticket, &channel) in (4..).zip(&o.channels) {
        let join = [("cell_id", Value::Int(channel as i64)), ("ticket_id", Value::Int(ticket)), ("from_id", Value::Int(o.perm as i64))];
        usercomm.send("RequestJoinChannel", &join)?;
    }

    let mut chess = Chess::default();
    let mut dice_bot = DiceBot::default();
    let mut poker_bot = PokerBot::default();
    if o.poker.as_deref() == Some("create") {
        let mut w = BitWriter::new();
        // Poker for funnies, 10 chips, "Bot", big blind 2, four seats, no password.
        w.bits(GameKind::Poker as u32, 8).bits(1, 8).u32(BUY_IN).str("Bot").u32(2).u32(0x7fff_ffff);
        w.bits(4, 8).bits(o.table.0 as u32, 8).bits(o.table.1 as u32, 8).bits(0, 8).u32(0);
        village.send_village(2, mg::id::CREATE, w.finish())?;
        println!("-> opening a poker table at tavern {} table {}", o.table.0, o.table.1);
    }
    if o.dice.as_deref() == Some("create") {
        let mut w = BitWriter::new();
        // Dice for funnies, 10 chips, "Bot", minimum 1, four seats, no password.
        w.bits(GameKind::Dice as u32, 8).bits(1, 8).u32(BUY_IN).str("Bot").u32(1).u32(0x7fff_ffff);
        w.bits(4, 8).bits(o.table.0 as u32, 8).bits(o.table.1 as u32, 8).bits(0, 8).u32(0);
        village.send_village(2, mg::id::CREATE, w.finish())?;
        println!("-> opening a dice table at tavern {} table {}", o.table.0, o.table.1);
    }
    if o.chess.as_deref() == Some("create") {
        let mut w = BitWriter::new();
        // Pawn chess for funnies, 10 chips, "Bot", 2 a game, two seats, no password.
        w.bits(GameKind::PawnChess as u32, 8).bits(1, 8).u32(BUY_IN).str("Bot").u32(2).u32(0x7fff_ffff);
        w.bits(2, 8).bits(o.table.0 as u32, 8).bits(o.table.1 as u32, 8).bits(0, 8).u32(0);
        village.send_village(2, mg::id::CREATE, w.finish())?;
        println!("-> opening a pawn chess table at tavern {} table {}", o.table.0, o.table.1);
    }

    let start = Instant::now();
    let mut next_line = 3.0;
    loop {
        while let Ok((id, data)) = tables_rx.try_recv() {
            if id == poker::id::HOLE_CARDS {
                poker_bot.hole_cards(&data);
                continue;
            }
            chess.update(id, &data, o.perm);
            dice_bot.update(id, &data, o.perm);
            poker_bot.update(id, &data, o.perm);
        }
        if o.poker.is_some() {
            poker_bot.play(&mut village, o.poker.as_deref() == Some("join"))?;
        }
        if o.dice.is_some() {
            dice_bot.play(&mut village, o.dice.as_deref() == Some("join"))?;
        }
        if o.chess.is_some() {
            chess.play(&mut village, o.chess.as_deref() == Some("join"))?;
        }
        if start.elapsed().as_secs_f32() >= next_line {
            next_line += o.every.max(1.0);
            if !o.say.is_empty() {
                for &channel in &o.channels {
                    usercomm.say(channel, &o.say)?;
                }
            }
            if let (Some(to), Some(&channel)) = (o.whisper_to, o.channels.first()) {
                usercomm.whisper(channel, to, &o.whisper)?;
            }
        }
        let t = start.elapsed().as_secs_f32();
        let angle = t * 0.4;
        let pos = [o.at[0] + o.radius * angle.cos(), o.at[1], o.at[2] + o.radius * angle.sin()];
        // Heading along the circle, in 1/128 turns.
        let heading = (angle + std::f32::consts::FRAC_PI_2).rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU;
        let location = Location {
            tick: (start.elapsed().as_millis() as u16) | 1,
            pos: pos.map(|p| p.clamp(0.0, 2047.0) as u16),
            rot: (heading * 128.0) as u8 & 0x7f,
            zone: o.zone,
            ghost_zone: 15,
            running: false,
            flag: false,
        };
        let mut w = BitWriter::new();
        location.write(&mut w);
        let header = 0x2000 | lm::id::MOVE as i64;
        village.send("SendGameData", &[("msg_type", Value::Int(header)), ("data", Value::Bytes(Some(w.finish())))])?;
        std::thread::sleep(Duration::from_millis(330));
    }
}

/// The bot's view of the pawn chess tables, from 216-218.
#[derive(Default)]
struct Chess {
    /// Seats taken and seats in all, per table.
    tables: HashMap<TableKey, (usize, usize)>,
    /// Our table and seat.
    seat: Option<(TableKey, usize)>,
    asked: bool,
    game: Option<PawnChess>,
    /// The (phase, turn) we last answered, and when the state came.
    answered: Option<(u8, u8)>,
    seen: Option<Instant>,
    seed: u32,
}

impl Chess {
    fn update(&mut self, id: u16, data: &[u8], me: u32) {
        let mut r = BitReader::new(data);
        let Some(t) = TableMessage::read(&mut r) else { return println!("unreadable table message") };
        let key = t.header.key;
        if t.header.kind != GameKind::PawnChess {
            return;
        }
        if id == mg::id::TABLE_REMOVE {
            self.tables.remove(&key);
            if self.seat.is_some_and(|(k, _)| k == key) {
                self.seat = None;
            }
            return;
        }
        let entry = self.tables.entry(key).or_insert((0, 2));
        if let Some(s) = &t.settings {
            entry.1 = s.max_players as usize;
        }
        if let Some(seats) = &t.seats {
            entry.0 = seats.len();
            match seats.iter().find(|s| s.actor == me) {
                Some(s) => self.seat = Some((key, s.index as usize)),
                None if self.seat.is_some_and(|(k, _)| k == key) => self.seat = None,
                None => {}
            }
        }
        if t.header.sections & mg::section::STATE != 0 && self.seat.is_some_and(|(k, _)| k == key) {
            if let Some((g, chips)) = PawnChess::read(&mut r) {
                println!("pawn chess: phase {} turn {} status {:#04x} chips {chips:?}", g.phase, g.turn, g.status);
                self.game = Some(g);
                self.seen = Some(Instant::now());
            }
        }
    }

    fn play(&mut self, village: &mut Connection, join: bool) -> Result<(), String> {
        let Some((key, seat)) = self.seat else {
            let open = self.tables.iter().find(|(_, (taken, max))| taken < max).map(|(k, _)| *k);
            if let (true, false, Some(key)) = (join, self.asked, open) {
                self.asked = true;
                let mut w = BitWriter::new();
                key.write(&mut w);
                w.u32(BUY_IN).u32(0);
                println!("-> joining pawn chess table {key:?}");
                village.send_village(5, mg::id::JOIN, w.finish())?;
            }
            return Ok(());
        };
        let Some(g) = &self.game else { return Ok(()) };
        // Think for a moment, and a bit longer before a new game.
        let wait = if g.phase == phase::OVER { 5.0 } else { 1.5 };
        if self.answered == Some((g.phase, g.turn)) || self.seen.is_some_and(|t| t.elapsed().as_secs_f32() < wait) {
            return Ok(());
        }
        let actions = g.legal_actions(seat);
        if actions.is_empty() {
            return Ok(());
        }
        self.answered = Some((g.phase, g.turn));
        self.seed = self.seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        // Captures first, else any move.
        let captures: Vec<_> = actions.iter().filter(|a| matches!(a, Action::Capture { .. })).copied().collect();
        let pool = if captures.is_empty() { &actions } else { &captures };
        let action = pool[(self.seed >> 16) as usize % pool.len()];
        println!("-> {action:?}");
        let mut w = BitWriter::new();
        key.write(&mut w);
        action.write(&mut w);
        village.send_village(5, action.id(), w.finish())
    }
}

/// The bot's view of the dice tables.
#[derive(Default)]
struct DiceBot {
    tables: HashMap<TableKey, (usize, usize)>,
    seat: Option<(TableKey, usize)>,
    asked: bool,
    game: Option<Dice>,
    /// When the current phase began (our clock), and what we did in it.
    phase_seen: Option<(u8, Instant)>,
    acted: bool,
    seed: u32,
}

impl DiceBot {
    fn update(&mut self, id: u16, data: &[u8], me: u32) {
        let mut r = BitReader::new(data);
        let Some(t) = TableMessage::read(&mut r) else { return };
        let key = t.header.key;
        if t.header.kind != GameKind::Dice {
            return;
        }
        if id == mg::id::TABLE_REMOVE {
            self.tables.remove(&key);
            if self.seat.is_some_and(|(k, _)| k == key) {
                self.seat = None;
            }
            return;
        }
        let entry = self.tables.entry(key).or_insert((0, 4));
        if let Some(s) = &t.settings {
            entry.1 = s.max_players as usize;
        }
        if let Some(seats) = &t.seats {
            entry.0 = seats.len();
            match seats.iter().find(|s| s.actor == me) {
                Some(s) => self.seat = Some((key, s.index as usize)),
                None if self.seat.is_some_and(|(k, _)| k == key) => self.seat = None,
                None => {}
            }
        }
        if t.header.sections & mg::section::STATE != 0 && self.seat.is_some_and(|(k, _)| k == key) {
            if let Some((g, chips)) = Dice::read(&mut r, 0) {
                if self.phase_seen.map(|(p, _)| p) != Some(g.phase) {
                    println!("dice: phase {} dealer {} roll {:?} chips {chips:?}", g.phase, g.dealer, g.roll);
                    self.phase_seen = Some((g.phase, Instant::now()));
                    self.acted = false;
                }
                self.game = Some(g);
            }
        }
    }

    fn play(&mut self, village: &mut Connection, join: bool) -> Result<(), String> {
        let Some((key, seat)) = self.seat else {
            let open = self.tables.iter().find(|(_, (taken, max))| taken < max).map(|(k, _)| *k);
            if let (true, false, Some(key)) = (join, self.asked, open) {
                self.asked = true;
                let mut w = BitWriter::new();
                key.write(&mut w);
                w.u32(BUY_IN).u32(0);
                println!("-> joining dice table {key:?}");
                village.send_village(5, mg::id::JOIN, w.finish())?;
            }
            return Ok(());
        };
        let (Some(g), Some((_, since))) = (&self.game, self.phase_seen) else { return Ok(()) };
        if self.acted || since.elapsed().as_secs_f32() < 2.0 {
            return Ok(());
        }
        let mut w = BitWriter::new();
        key.write(&mut w);
        match g.phase {
            dice::phase::BETS => {
                self.seed = self.seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let mut bets = [0u32; dice::FIELDS];
                bets[(self.seed >> 16) as usize % dice::FIELDS] = 5;
                bets[(self.seed >> 8) as usize % dice::FIELDS] += 1;
                println!("-> bets {bets:?}");
                dice::write_bets(&mut w, 100, &bets);
                village.send_village(5, dice::id::BET, w.finish())?;
            }
            dice::phase::ROLL if g.dealer as usize == seat => {
                println!("-> throw");
                village.send_village(5, dice::id::ROLL, w.finish())?;
            }
            _ => {}
        }
        self.acted = true;
        Ok(())
    }
}

/// The bot's view of the poker tables.
#[derive(Default)]
struct PokerBot {
    tables: HashMap<TableKey, (usize, usize)>,
    seat: Option<(TableKey, usize)>,
    asked: bool,
    view: Option<poker::View>,
    /// Our cards and `hndval` from the last 303.
    hand: ([u8; 2], u32),
    /// `(crntplyr, phase, round)` and when it last changed; like the
    /// client, we act once per change.
    turn: Option<((u8, u8, u8), Instant)>,
    acted: bool,
}

/// `"Ah"` for a card byte.
fn card_name(c: u8) -> String {
    match c {
        0..=51 => format!("{}{}", b"23456789TJQKA"[c as usize % 13] as char, b"dhsc"[c as usize / 13] as char),
        52 => "??".into(),
        _ => "--".into(),
    }
}

impl PokerBot {
    fn hole_cards(&mut self, data: &[u8]) {
        let mut r = BitReader::new(data);
        let Some(key) = TableKey::read(&mut r) else { return };
        let (Some(a), Some(b), Some(value)) = (r.bits(8), r.bits(8), r.u32()) else { return };
        if self.seat.is_some_and(|(k, _)| k == key) {
            self.hand = ([a as u8, b as u8], value);
            println!("poker: my cards {} {}, hand {value:#010x}", card_name(a as u8), card_name(b as u8));
        }
    }

    fn update(&mut self, id: u16, data: &[u8], me: u32) {
        let mut r = BitReader::new(data);
        let Some(t) = TableMessage::read(&mut r) else { return };
        let key = t.header.key;
        if t.header.kind != GameKind::Poker {
            return;
        }
        if id == mg::id::TABLE_REMOVE {
            self.tables.remove(&key);
            if self.seat.is_some_and(|(k, _)| k == key) {
                self.seat = None;
            }
            return;
        }
        let entry = self.tables.entry(key).or_insert((0, 4));
        if let Some(s) = &t.settings {
            entry.1 = s.max_players as usize;
        }
        if let Some(seats) = &t.seats {
            entry.0 = seats.len();
            match seats.iter().find(|s| s.actor == me) {
                Some(s) => self.seat = Some((key, s.index as usize)),
                None if self.seat.is_some_and(|(k, _)| k == key) => self.seat = None,
                None => {}
            }
        }
        if t.header.sections & mg::section::STATE == 0 || !self.seat.is_some_and(|(k, _)| k == key) {
            return;
        }
        let Some(v) = poker::View::read(&mut r) else { return };
        let now = (v.current, v.phase, v.round);
        if self.turn.map(|(t, _)| t) != Some(now) {
            let board: Vec<String> = v.board.iter().map(|&c| card_name(c)).collect();
            let pots: Vec<u32> = v.pots.iter().map(|p| p.amount).collect();
            println!(
                "poker: phase {} round {} dealer {} current {} actions {:#x} board [{}] pots {pots:?} last {:?}",
                v.phase,
                v.round,
                v.dealer,
                v.current,
                v.actions,
                board.join(" "),
                v.last
            );
            if v.phase == poker::phase::SHOWDOWN {
                for (seat, cards, actn, credits, _) in &v.players {
                    if let Some((c, value)) = cards {
                        println!("poker:   seat {seat} shows {} {} ({value:#010x})", card_name(c[0]), card_name(c[1]));
                    }
                    println!("poker:   seat {seat} actn {actn:#06x} chips {credits}");
                }
            }
            self.turn = Some((now, Instant::now()));
            self.acted = false;
        }
        self.view = Some(v);
    }

    fn play(&mut self, village: &mut Connection, join: bool) -> Result<(), String> {
        use poker::action::*;
        let Some((key, seat)) = self.seat else {
            let open = self.tables.iter().find(|(_, (taken, max))| taken < max).map(|(k, _)| *k);
            if let (true, false, Some(key)) = (join, self.asked, open) {
                self.asked = true;
                let mut w = BitWriter::new();
                key.write(&mut w);
                w.u32(BUY_IN).u32(0);
                println!("-> joining poker table {key:?}");
                village.send_village(5, mg::id::JOIN, w.finish())?;
            }
            return Ok(());
        };
        let (Some(v), Some((_, since))) = (&self.view, self.turn) else { return Ok(()) };
        if self.acted || v.phase != poker::phase::BETTING || v.current as usize != seat || since.elapsed().as_secs_f32() < 1.5 {
            return Ok(());
        }
        let credits = v.players.iter().find(|p| p.0 as usize == seat).map_or(0, |p| p.3);
        let to_call = v.min_wager.saturating_sub(v.bets[seat]);
        let category = self.hand.1 >> 24;
        let a = v.actions;
        let action = if a & (RAISE | BET) != 0 && category >= 2 {
            let (kind, to) = if a & RAISE != 0 { (RAISE, v.min_wager + v.min_raise) } else { (BET, v.min_wager) };
            poker::Action::Wager(kind, to)
        } else if a & CHECK != 0 {
            poker::Action::Plain(CHECK)
        } else if a & CALL != 0 && (to_call <= credits / 3 || category >= 1) {
            poker::Action::Plain(CALL)
        } else if a & ALL_IN != 0 && category >= 1 {
            poker::Action::Plain(ALL_IN)
        } else {
            poker::Action::Plain(FOLD)
        };
        println!("-> poker {action:?}");
        let mut w = BitWriter::new();
        key.write(&mut w);
        action.write(&mut w);
        village.send_village(5, action.id(), w.finish())?;
        self.acted = true;
        Ok(())
    }
}

/// The lobby login: the account and the session key tokens are sealed with.
struct LobbySession<'a> {
    connection: Connection<'a>,
    account: u32,
    name: String,
    session_key: [u8; auth::KEY_LEN],
}

/// A logged-in connection to the lobby, the village or usercomm.
struct Connection<'a> {
    defs: &'a MsgDefs,
    stream: TcpStream,
    me: u32,
    perm: u32,
    buf: Vec<u8>,
}

impl<'a> Connection<'a> {
    fn connect(defs: &'a MsgDefs, addr: &str, perm: u32) -> Result<Self, String> {
        let mut stream = TcpStream::connect(addr).map_err(|e| format!("{addr}: {e}"))?;
        let mut user = [0u8; 32];
        user[..4].copy_from_slice(b"user");
        let hello = Handshake { connection: frame::CLIENT_UNASSIGNED, user, password: *b"password", trailer: 1 };
        write(&mut stream, Frame::new(frame::CLIENT_UNASSIGNED, frame::SERVER, FrameKind::Connect, hello.encode()))?;
        let mut buf = Vec::new();
        let connected = read(&mut stream, &mut buf)?;
        let me = Handshake::decode(&connected.payload).ok_or("bad Connected frame")?.connection;
        println!("connected to {addr} as {me}");
        Ok(Self { defs, stream, me, perm, buf })
    }

    /// Logs in to the lobby as the game's login dialog does: key exchange,
    /// then the encrypted name and password; the answer is the session key.
    fn lobby_login(defs: &'a MsgDefs, addr: &str, name: &str, password: &str) -> Result<LobbySession<'a>, String> {
        let mut c = Self::connect(defs, addr, 0)?;
        c.send("CheckVersion", &[("version", Value::Int(27)), ("ticket_id", Value::Int(1))])?;
        let (exchange, key) = ClientExchange::start();
        c.send("StartAuthenticateSession", &[("key", Value::Bytes(Some(key))), ("ticket_id", Value::Int(2))])?;
        let ack = c.wait_for("AckAuthenticateSession")?;
        let k = exchange.finish(ack.bytes("cipher").unwrap_or_default()).map_err(|e| e.to_string())?;
        let creds = Credentials { name: name.as_bytes().to_vec(), password: password.as_bytes().to_vec(), cd_key: None };
        let cipher = auth::seal(&k, &creds.encode()).map_err(|e| e.to_string())?;
        c.send("AuthenticateUser", &[("cipher", Value::Bytes(Some(cipher))), ("ticket_id", Value::Int(3))])?;
        let reply = c.wait_for("SessionKey")?;
        let session = auth::open(&k, reply.bytes("cipher").unwrap_or_default()).ok_or("bad SessionKey")?;
        let session_key = session.try_into().map_err(|_| "bad SessionKey length")?;
        let account = reply.int("perm_id").unwrap_or(0) as u32;
        println!("logged in to the lobby as {name} (account {account})");
        Ok(LobbySession { connection: c, account, name: name.into(), session_key })
    }

    /// Connects and logs in with a token for `perm`, as the game does after
    /// "Spiel starten": the token carries this connection's nonce and is
    /// sealed with the lobby's session key.
    fn login(defs: &'a MsgDefs, addr: &str, perm: u32, lobby: &LobbySession) -> Result<Self, String> {
        let mut c = Self::connect(defs, addr, perm)?;
        c.send("CheckVersion", &[("version", Value::Int(27)), ("ticket_id", Value::Int(1))])?;
        c.send("StartValidateTokenSession", &[("ticket_id", Value::Int(2))])?;
        let nonce = c.wait_for("AckValidateTokenSession")?.bytes("nonce").unwrap_or_default().to_vec();
        let token = Token { id: lobby.account, name: lobby.name.as_bytes().to_vec(), key_hash: vec![0; 64], value: 1, password: vec![], nonce };
        let cipher = token.seal(&lobby.session_key).map_err(|e| e.to_string())?;
        c.send("SendToken", &[("perm_id", Value::Int(perm as i64)), ("cipher", Value::Bytes(Some(cipher))), ("ticket_id", Value::Int(3))])?;
        let result = c.wait_for("AddResult")?;
        if result.int("errorcode") != Some(0) {
            return Err(format!("{addr} refused the token: {}", result.string("errormsg").unwrap_or_default()));
        }
        Ok(c)
    }

    /// Reads until a lobby message called `name` arrives; a `Result` with an
    /// error ends the wait with that error.
    fn wait_for(&mut self, name: &str) -> Result<Message, String> {
        loop {
            let frame = read(&mut self.stream, &mut self.buf)?;
            if Cell::is_cell(&frame.payload) {
                continue;
            }
            let msg = Message::decode(self.defs, &frame.payload).map_err(|e| e.to_string())?;
            if msg.name == name {
                return Ok(msg);
            }
            if msg.name == "Result" && msg.int("errorcode") != Some(0) {
                return Err(format!("waiting for {name}: {msg}"));
            }
        }
    }

    fn send(&mut self, name: &str, fields: &[(&str, Value)]) -> Result<(), String> {
        let msg = fields.iter().fold(Message::new(self.defs, name).expect(name), |m, (f, v)| m.with(f, v.clone()));
        self.send_payload(msg.encode(self.defs))
    }

    /// A village message on `channel` (bits 12-14 of `msg_type`).
    fn send_village(&mut self, channel: u16, id: u16, data: Vec<u8>) -> Result<(), String> {
        let header = (channel as i64) << 12 | id as i64;
        self.send("SendGameData", &[("msg_type", Value::Int(header)), ("data", Value::Bytes(Some(data)))])
    }

    fn send_payload(&mut self, payload: Vec<u8>) -> Result<(), String> {
        let mut f = Frame::new(self.me, frame::SERVER, FrameKind::Application, payload);
        f.flags = u32::MAX;
        write(&mut self.stream, f)
    }

    /// Routes a ChatMessage to a channel, as the game's chat box does.
    fn say(&mut self, channel: u32, text: &str) -> Result<(), String> {
        let line = Message::new(self.defs, "ChatMessage")
            .expect("ChatMessage")
            .with("txt", Value::text(text))
            .with("from_id", Value::Int(self.perm as i64));
        let data = line.encode_propset(self.defs);
        println!("-> cell {channel}: {text}");
        let route = Cell::Route { module: 0, message: line.id, except: cell::NOBODY, data, cell: channel, to_self: true, propset: true };
        self.send_payload(route.encode())
    }

    /// Whispers to a player, as the game's "/tell": a WhisperChatMessage
    /// naming the receiver, routed to a channel.
    fn whisper(&mut self, channel: u32, to: u32, text: &str) -> Result<(), String> {
        let line = Message::new(self.defs, "WhisperChatMessage")
            .expect("WhisperChatMessage")
            .with("mode", Value::Int(3))
            .with("txt", Value::text(text))
            .with("cell_id", Value::Int(channel as i64))
            .with("from_id", Value::Int(self.perm as i64))
            .with("perm_id", Value::Int(to as i64));
        let data = line.encode_propset(self.defs);
        println!("-> whisper to {to}: {text}");
        let route = Cell::Route { module: 0, message: line.id, except: cell::NOBODY, data, cell: channel, to_self: true, propset: true };
        self.send_payload(route.encode())
    }
}

fn write(stream: &mut TcpStream, frame: Frame) -> Result<(), String> {
    stream.write_all(&frame.encode()).map_err(|e| format!("send: {e}"))
}

fn read(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Frame, String> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some((frame, used)) = Frame::decode(buf).map_err(|e| e.to_string())? {
            buf.drain(..used);
            return Ok(frame);
        }
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

//! Table games ("Kneipenspiele") in the village taverns: dice, poker and
//! pawn chess. Evidence: demo exe 34811, `LobbyComm::MiniGameProxy` and its
//! subclasses; see docs/TABLE-GAMES.md for the protocol.
//!
//! The server keeps one object per table ("proxy" in the client) and syncs
//! it with village messages 218 (create), 217 (update) and 216 (remove).
//! Each carries a [`Header`] and then the sections its `mngt` flags name.
//! Client actions travel on village channel 5 and start with the table's
//! [`TableKey`].

use crate::lobby_message::{BitReader, BitWriter};

pub mod id {
    /// Server → client: a table appeared ([`super::Header`] + sections).
    pub const TABLE_CREATE: u16 = 218;
    /// Server → client: a table changed (same layout as 218).
    pub const TABLE_UPDATE: u16 = 217;
    /// Server → client: a table is gone ([`super::Header`] only).
    pub const TABLE_REMOVE: u16 = 216;
    /// Client → server (channel 2): create a table ([`super::CreateTable`]).
    pub const CREATE: u16 = 2001;
    /// Client → server (channel 5): join a table ([`super::Join`]).
    pub const JOIN: u16 = 209;
    /// Client → server (channel 5): leave the table.
    pub const LEAVE: u16 = 204;
    /// Client → server (channel 5): add `amnt` chips (u32) to the stack.
    pub const REBUY: u16 = 222;
    /// Server → client, no payload: "Kein Tisch mehr frei", the answer to a
    /// 2001 that opened no table.
    pub const NO_TABLE_LEFT: u16 = 219;
    /// Server → client, no payload: "Tisch voll", the answer to a 209 that
    /// seated nobody.
    pub const TABLE_FULL: u16 = 220;
}

/// `tblidx` of a 2001 sent from the tavern menu: the server picks a table.
pub const ANY_TABLE: u8 = 0xff;
/// Tables per tavern; 13 and 14 are the eight-seat poker tables.
pub const TABLES: u8 = 15;

/// Game type, the low four bits of `mngt` and the `type` of message 2001.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameKind {
    Dice = 1,
    Poker = 2,
    PawnChess = 3,
}

impl GameKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Dice),
            2 => Some(Self::Poker),
            3 => Some(Self::PawnChess),
            _ => None,
        }
    }
}

/// Which sections follow the header (high bits of `mngt`; 0x20 is the
/// currency, see [`Header::funnies`]).
pub mod section {
    /// Password flag and CRC, name, bet limits, seat count.
    pub const SETTINGS: u8 = 0x10;
    /// Who sits where.
    pub const SEATS: u8 = 0x40;
    /// The game's own state (per game type).
    pub const STATE: u8 = 0x80;
}

/// Writes the client's variable-length number: one bit "present", then the
/// count of 4-bit groups minus one in 3 bits, then the value (`sub_48E6B0`).
pub fn write_varint(w: &mut BitWriter, value: u32) {
    if value == 0 {
        w.bits(0, 1);
        return;
    }
    let nibbles = (32 - value.leading_zeros()).div_ceil(4);
    w.bits(1, 1).bits(nibbles - 1, 3).bits(value, nibbles * 4);
}

/// Reads a [`write_varint`] number (`sub_48E7D0`).
pub fn read_varint(r: &mut BitReader) -> Option<u32> {
    if r.bits(1)? == 0 {
        return Some(0);
    }
    let n = r.bits(3)? * 4 + 4;
    r.bits(n)
}

/// Identifies a table in every message: `msgprt` (16 bits) and `rnid`
/// (8 bits). The client keys its table objects by `part | round << 16`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableKey {
    pub part: u16,
    pub round: u8,
}

impl TableKey {
    pub fn write(&self, w: &mut BitWriter) {
        w.bits(self.part as u32, 16).bits(self.round as u32, 8);
    }

    pub fn read(r: &mut BitReader) -> Option<Self> {
        Some(Self { part: r.bits(16)? as u16, round: r.bits(8)? as u8 })
    }
}

/// The start of 216/217/218 (`sub_470930`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// `mngt` bits 0-3. Any other value crashes the client.
    pub kind: GameKind,
    /// [`section`] flags.
    pub sections: u8,
    /// `mngt` bit 5: played for funnies, not gold.
    pub funnies: bool,
    /// `scntbl` low four bits: the tavern's scene, tavern + 2.
    pub scene: u8,
    /// `scntbl` high four bits: the table in the tavern, 0-14.
    pub table: u8,
    /// `chtid`: the table's chat channel. The client ignores a 217 with 0.
    pub chat: u32,
    pub key: TableKey,
}

impl Header {
    pub fn write(&self, w: &mut BitWriter) {
        let currency = if self.funnies { 0x20 } else { 0 };
        w.bits((self.kind as u32) | (self.sections as u32 & 0xd0) | currency, 8);
        w.bits((self.scene as u32 & 0xf) | (self.table as u32 & 0xf) << 4, 8);
        write_varint(w, self.chat);
        self.key.write(w);
    }

    pub fn read(r: &mut BitReader) -> Option<Self> {
        let mngt = r.bits(8)? as u8;
        let scntbl = r.bits(8)? as u8;
        Some(Self {
            kind: GameKind::from_u8(mngt & 0xf)?,
            sections: mngt & 0xd0,
            funnies: mngt & 0x20 != 0,
            scene: scntbl & 0xf,
            table: scntbl >> 4,
            chat: read_varint(r)?,
            key: TableKey::read(r)?,
        })
    }
}

/// Section [`section::SETTINGS`] (`sub_470C30`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// CRC of the password, `None` for an open table.
    pub password: Option<u32>,
    pub name: String,
    pub limit_min: u32,
    pub limit_max: u32,
    pub max_players: u8,
}

impl Settings {
    pub fn write(&self, w: &mut BitWriter) {
        w.bits(self.password.is_some() as u32, 8).u32(self.password.unwrap_or(0));
        w.str(&self.name);
        write_varint(w, self.limit_min);
        write_varint(w, self.limit_max);
        w.bits(self.max_players as u32, 4);
    }

    pub fn read(r: &mut BitReader) -> Option<Self> {
        let has_password = r.bits(8)? != 0;
        let crc = r.u32()?;
        Some(Self {
            password: has_password.then_some(crc),
            name: r.str()?,
            limit_min: read_varint(r)?,
            limit_max: read_varint(r)?,
            max_players: r.bits(4)? as u8,
        })
    }
}

/// A seated player (`ActorID`: 2-bit type, varint id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seat {
    pub index: u8,
    pub actor_kind: u8,
    pub actor: u32,
}

/// Section [`section::SEATS`] (`sub_471690`): a count, then per entry a
/// "used" bit with seat index and actor. It is the complete list of taken
/// seats; "used" is always 1 (0 makes the client write out of bounds).
/// Every seated avatar must already be spawned on the client, and a 218
/// must not carry seats (the client crashes).
pub fn write_seats(w: &mut BitWriter, seats: &[Seat]) {
    w.bits(seats.len() as u32, 4);
    for s in seats {
        w.bits(1, 1).bits(s.index as u32, 4).bits(s.actor_kind as u32, 2);
        write_varint(w, s.actor);
    }
}

/// Reads a [`write_seats`] section.
pub fn read_seats(r: &mut BitReader) -> Option<Vec<Seat>> {
    let n = r.bits(4)?;
    let mut seats = Vec::new();
    for _ in 0..n {
        if r.bits(1)? == 0 {
            continue;
        }
        let index = r.bits(4)? as u8;
        let actor_kind = r.bits(2)? as u8;
        seats.push(Seat { index, actor_kind, actor: read_varint(r)? });
    }
    Some(seats)
}

/// A whole 216/217/218 up to the game's own state, which `r` is left at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableMessage {
    pub header: Header,
    pub settings: Option<Settings>,
    pub seats: Option<Vec<Seat>>,
}

impl TableMessage {
    pub fn read(r: &mut BitReader) -> Option<Self> {
        let header = Header::read(r)?;
        let settings = if header.sections & section::SETTINGS != 0 { Some(Settings::read(r)?) } else { None };
        let seats = if header.sections & section::SEATS != 0 { Some(read_seats(r)?) } else { None };
        Some(Self { header, settings, seats })
    }
}

/// Message 2001 (`sub_46C420`): the matchmaking dialog's "create".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateTable {
    pub kind: u8,
    /// Play for funnies instead of gold.
    pub play_money: bool,
    pub stake: u32,
    pub name: String,
    pub limit_min: u32,
    pub limit_max: u32,
    pub max_players: u8,
    pub tavern: u8,
    pub table: u8,
    pub password: Option<u32>,
}

impl CreateTable {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut r = BitReader::new(data);
        let kind = r.bits(8)? as u8;
        let play_money = r.bits(8)? != 0;
        let stake = r.u32()?;
        let name = r.str()?;
        let limit_min = r.u32()?;
        let limit_max = r.u32()?;
        let max_players = r.bits(8)? as u8;
        let tavern = r.bits(8)? as u8;
        let table = r.bits(8)? as u8;
        let has_password = r.bits(8)? != 0;
        let crc = r.u32()?;
        Some(Self { kind, play_money, stake, name, limit_min, limit_max, max_players, tavern, table, password: has_password.then_some(crc) })
    }
}

/// Message 209 on channel 5: join the table named by `key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Join {
    pub key: TableKey,
    pub stake: u32,
    pub password: u32,
}

impl Join {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut r = BitReader::new(data);
        let key = TableKey::read(&mut r)?;
        Some(Self { key, stake: r.u32()?, password: r.u32()? })
    }
}

/// The table key that starts every channel-5 action, and a reader for the rest.
pub fn action(data: &[u8]) -> Option<(TableKey, BitReader<'_>)> {
    let mut r = BitReader::new(data);
    let key = TableKey::read(&mut r)?;
    Some((key, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_matches_the_client() {
        for v in [0u32, 1, 15, 16, 255, 256, 0x7fff_ffff, u32::MAX] {
            let mut w = BitWriter::new();
            write_varint(&mut w, v);
            w.bits(0x5, 3);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            assert_eq!(read_varint(&mut r), Some(v));
            assert_eq!(r.bits(3), Some(5));
        }
        // 10 = one nibble: 1, 000, 1010.
        let mut w = BitWriter::new();
        write_varint(&mut w, 10);
        assert_eq!(w.finish(), [0b1000_1010]);
    }

    /// Message 2001 from the retail client: a dice table "BigJohn" for four
    /// at table 0 of tavern 1, stake 10, played for funnies, no password.
    #[test]
    fn parses_create_from_the_retail_client() {
        let data = [
            0x01, 0x01, 0x00, 0x00, 0x00, 0x0a, 0x07, b'B', b'i', b'g', b'J', b'o', b'h', b'n', 0x00, 0x00, 0x00, 0x01,
            0x7f, 0xff, 0xff, 0xff, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let c = CreateTable::parse(&data).unwrap();
        assert_eq!(
            c,
            CreateTable {
                kind: 1,
                play_money: true,
                stake: 10,
                name: "BigJohn".into(),
                limit_min: 1,
                limit_max: 0x7fff_ffff,
                max_players: 4,
                tavern: 1,
                table: 0,
                password: None,
            }
        );
    }

    /// The spec's example: a dice table for funnies at tavern 1 (scene 3),
    /// table 0, chat 1000, key 1/0, "BigJohn", minimum 1, four seats.
    #[test]
    fn header_and_settings_bytes() {
        let header = Header {
            kind: GameKind::Dice,
            sections: section::SETTINGS,
            funnies: true,
            scene: 3,
            table: 0,
            chat: 1000,
            key: TableKey { part: 1, round: 0 },
        };
        let settings = Settings { password: None, name: "BigJohn".into(), limit_min: 1, limit_max: 0x7fff_ffff, max_players: 4 };
        let mut w = BitWriter::new();
        header.write(&mut w);
        settings.write(&mut w);
        assert_eq!(
            w.finish(),
            [
                0x31, 0x03, 0xa3, 0xe8, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, b'B', b'i', b'g', b'J', b'o', b'h', b'n', 0x81,
                0xf7, 0xff, 0xff, 0xff, 0xf4
            ]
        );
        // Seats: one avatar 12345 in seat 0.
        let mut w = BitWriter::new();
        Header { sections: section::SEATS, ..header }.write(&mut w);
        write_seats(&mut w, &[Seat { index: 0, actor_kind: 1, actor: 12345 }]);
        assert_eq!(w.finish(), [0x61, 0x03, 0xa3, 0xe8, 0x00, 0x01, 0x00, 0x18, 0x36, 0x60, 0x72]);
    }

    #[test]
    fn table_message_round_trip() {
        let header = Header { kind: GameKind::Dice, sections: section::SETTINGS | section::SEATS, funnies: false, scene: 2, table: 12, chat: 77, key: TableKey { part: 5, round: 1 } };
        let settings = Settings { password: Some(0xdead_beef), name: "Tisch".into(), limit_min: 2, limit_max: 500, max_players: 4 };
        let seats = vec![Seat { index: 0, actor_kind: 1, actor: 11 }, Seat { index: 2, actor_kind: 1, actor: 300 }];
        let mut w = BitWriter::new();
        header.write(&mut w);
        settings.write(&mut w);
        write_seats(&mut w, &seats);
        let bytes = w.finish();
        let got = TableMessage::read(&mut BitReader::new(&bytes)).unwrap();
        assert_eq!(got, TableMessage { header, settings: Some(settings), seats: Some(seats) });
    }
}

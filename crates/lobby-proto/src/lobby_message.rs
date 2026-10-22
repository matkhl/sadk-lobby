//! Village-server payloads ("LobbyMessage" in the client).
//!
//! Once a village connection is logged in, both sides exchange
//! `SendGameData { msg_type, data }` (or `SendGameDataBundle`, a list of
//! `u32 msg_type | u32 len | data`). `msg_type` is a 16-bit header:
//!
//! * bit 15: typed mode (each field is preceded by type/label tags, a debug
//!   format we never send),
//! * bits 12-14: a channel number (the client sends on channel 2 and reads
//!   the server's messages whatever their channel),
//! * bits 0-11: the message id (1000 = world welcome, ...).
//!
//! `data` is a bit stream, most significant bit first, with values written
//! most significant bit first. Strings are an 8-bit length followed by that
//! many 8-bit characters.

/// Village message ids (dispatcher `sub_470260` and the senders in the demo
/// exe).
pub mod id {
    /// World info sent by the village after login: name, server perm id and
    /// the chat channels of the village zones.
    pub const WORLD_WELCOME: u16 = 1000;
    /// Server → client: data blocks of one avatar ([`super::avatar_data`]).
    /// Creates the avatar if the client does not know it yet.
    pub const AVATAR_DATA: u16 = 1001;
    /// Server → client: an avatar entered the village (`u32` avatar id).
    pub const AVATAR_LOGIN: u16 = 1002;
    /// Server → client: an avatar left the village (`u32` avatar id).
    pub const AVATAR_LOGOUT: u16 = 1003;
    /// Client → server, about three times a second: the own avatar's
    /// [`super::Location`].
    pub const MOVE: u16 = 2000;
    /// Client → server when it leaves the village for a match: `u32`
    /// 0xAFFEDEAD. The client then waits for its village connection to log
    /// out (`VillageServerConnection::LoggedOut`) before it goes on to the
    /// referee.
    pub const LEAVE: u16 = 2002;
    /// Server → client: `u32 code`; with [`super::LEAVE_OK_CODE`] the client
    /// logs off usercomm and closes its village connection itself, which is
    /// the clean log-out [`LEAVE`] waits for (`sub_46E350`). A close from
    /// the server's side counts as a lost connection and sends the client
    /// back to the character list.
    pub const LEAVE_OK: u16 = 1006;
    /// Client → server every 30 s: `u32` ping code.
    pub const PING: u16 = 3798;
    /// Server → client: the ping code echoed back.
    pub const PONG: u16 = 3799;
    /// Server → client: an avatar's level, experience and balance
    /// ([`super::Stats::message`]); the client shows and checks the own
    /// avatar's gold and funnies from it (`sub_46BEC0`).
    pub const STATS: u16 = 3201;
}

/// The code in [`id::LEAVE_OK`] that lets the client go.
pub const LEAVE_OK_CODE: u32 = 0xdeadbeef;

/// Bits of the 4-bit `dtblcks` mask that starts an [`id::AVATAR_DATA`] body.
pub mod block {
    pub const LOCATION: u8 = 1;
    pub const STYLE: u8 = 2;
    pub const ACTIVE_ITEMS: u8 = 4;
    pub const STATS: u8 = 8;
}

/// An avatar's position as the client packs it (`sub_48E300`), 66 bits:
/// `u16 tick`, x/y/z as 11-bit fractions of the world box, the heading in
/// 1/128 turns, the zone and "ghost zone" (4 bits each), then the running
/// flag and one more flag. Message 2000 is exactly this, and the location
/// block of message 1001 has the same layout, so the server relays the
/// values without knowing the world box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Location {
    /// The sender's 16-bit millisecond clock; always odd in message 2000.
    pub tick: u16,
    /// 11 bits each.
    pub pos: [u16; 3],
    /// 7 bits, 128 steps per turn.
    pub rot: u8,
    pub zone: u8,
    pub ghost_zone: u8,
    pub running: bool,
    pub flag: bool,
}

impl Location {
    pub fn read(r: &mut BitReader) -> Option<Self> {
        Some(Self {
            tick: r.bits(16)? as u16,
            pos: [r.bits(11)? as u16, r.bits(11)? as u16, r.bits(11)? as u16],
            rot: r.bits(7)? as u8,
            zone: r.bits(4)? as u8,
            ghost_zone: r.bits(4)? as u8,
            running: r.bool()?,
            flag: r.bool()?,
        })
    }

    /// Body of message 2000.
    pub fn parse(data: &[u8]) -> Option<Self> {
        Self::read(&mut BitReader::new(data))
    }

    pub fn write(&self, w: &mut BitWriter) {
        w.bits(self.tick as u32, 16);
        for p in self.pos {
            w.bits(p as u32, 11);
        }
        w.bits(self.rot as u32, 7).bits(self.zone as u32, 4).bits(self.ghost_zone as u32, 4).bool(self.running).bool(self.flag);
    }
}

/// An avatar's look, the style block of message 1001 (`sub_4816A0`): the
/// name, then one byte each for tribe << 4 | gender, hair, skin, shirt and
/// trouser colour and four extra colours.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub name: String,
    pub tribe_gender: u8,
    pub hair: u8,
    pub skin: u8,
    pub shirt: u8,
    pub trousers: u8,
    pub extra: [u8; 4],
}

impl Style {
    /// Builds the style from a character's `data` (as sent in AddCharacter),
    /// the way the client builds its own avatar's style (`sub_480F10`). See
    /// [`character_blocks`] for the layout. The client keeps 4 bits of every
    /// value.
    ///
    /// * Only block 0 set (what character creation sends): block 0 is the
    ///   creation block, `u32` version, tribe, gender, hair, skin, shirt,
    ///   trousers, then four extra colours when the version is not 0.
    /// * Otherwise block 0 is the appearance block (`u32` version, tribe,
    ///   gender) and block 1 the style block (`u32` version, hair, skin,
    ///   shirt, trousers, then four extra colours when the version is not 0).
    ///
    /// Data that does not split into blocks gives the default style.
    pub fn from_character(name: &str, data: &[u8]) -> Self {
        let mut style = Self { name: name.to_string(), ..Self::default() };
        let Some(blocks) = character_blocks(data) else { return style };
        // Field `i` of a block, 0 past its end.
        let field = |block: &[u8], i: usize| block.get(4 * i..4 * i + 4).map_or(0, |b| (u32::from_le_bytes(b.try_into().unwrap()) & 0xf) as u8);
        let versioned = |block: &[u8]| block.get(..4).is_some_and(|b| b != [0; 4]);
        let creation_only = !matches!(blocks[0].len(), 0 | 12) && blocks[1..].iter().all(|b| b.is_empty());
        let (colours, first) = if creation_only { (blocks[0], 3) } else { (blocks[1], 1) };
        style.tribe_gender = field(blocks[0], 1) << 4 | field(blocks[0], 2);
        style.hair = field(colours, first);
        style.skin = field(colours, first + 1);
        style.shirt = field(colours, first + 2);
        style.trousers = field(colours, first + 3);
        if versioned(colours) {
            style.extra = [field(colours, first + 4), field(colours, first + 5), field(colours, first + 6), field(colours, first + 7)];
        }
        style
    }

    pub fn write(&self, w: &mut BitWriter) {
        w.str(&self.name);
        for b in [self.tribe_gender, self.hair, self.skin, self.shirt, self.trousers] {
            w.bits(b as u32, 8);
        }
        for b in self.extra {
            w.bits(b as u32, 8);
        }
    }

    pub fn read(r: &mut BitReader) -> Option<Self> {
        let name = r.str()?;
        let mut b = [0u8; 9];
        for v in &mut b {
            *v = r.bits(8)? as u8;
        }
        Some(Self { name, tribe_gender: b[0], hair: b[1], skin: b[2], shirt: b[3], trousers: b[4], extra: [b[5], b[6], b[7], b[8]] })
    }
}

/// Splits a character's `data` into its six blocks the way tincat does
/// (tincat3.dll, the handlers of UserCharConn and CharacterData): six `u32`
/// block sizes (little-endian), then the blocks back to back. `None` when
/// the sizes do not add up to the data.
pub fn character_blocks(data: &[u8]) -> Option<[&[u8]; 6]> {
    let sizes: Vec<usize> = (0..6).map(|i| data.get(4 * i..4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)).collect::<Option<_>>()?;
    if sizes.iter().try_fold(24usize, |sum, s| sum.checked_add(*s))? != data.len() {
        return None;
    }
    let mut at = 24;
    Some(std::array::from_fn(|i| {
        let block = &data[at..at + sizes[i]];
        at += sizes[i];
        block
    }))
}

/// Level, experience and balance of a character: block 4 of its data
/// (`AvatarStatsBlockEx`, read by `sub_485A40`) and message 3201.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub level: u32,
    pub exp: u32,
    pub gold: u32,
    pub funnies: u32,
}

impl Default for Stats {
    /// The client's own defaults for a character without a stats block.
    fn default() -> Self {
        Self { level: 1, exp: 0, gold: 1000, funnies: 1000 }
    }
}

impl Stats {
    /// Block 4, version 1 (little-endian `u32`s): version, level, exp, gold,
    /// funnies. Version 2 adds the avatar's village position and heading,
    /// which the client then spawns at; version 1 leaves its default spot.
    pub fn block(&self) -> Vec<u8> {
        [1, self.level, self.exp, self.gold, self.funnies].iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Message 3201 for avatar `owner`: `ownr, lvl, exp, gold, glod`.
    pub fn message(&self, owner: u32) -> Vec<u8> {
        BitWriter::new().u32(owner).u32(self.level).u32(self.exp).u32(self.gold).u32(self.funnies).finish()
    }
}

/// A character's `data` as the client should get it: in the full block
/// layout, with `stats` as block 4.
///
/// The client reads gold and funnies from block 4 only in the full layout
/// (`sub_480F10`). Data with only a creation block (what character creation
/// sends, see [`Style::from_character`]) gets 50 gold and 50 funnies there,
/// so the creation block is split into the appearance block (`u32` version
/// 0, tribe, gender) and the style block (`u32` version, hair, skin, shirt,
/// trousers, the four extra colours when the version is not 0), which the
/// client reads into the same look. Data that does not split into blocks
/// gets the client's default look.
pub fn with_stats(data: &[u8], stats: &Stats) -> Vec<u8> {
    let mut blocks: [Vec<u8>; 6] = match character_blocks(data) {
        Some(b) => b.map(<[u8]>::to_vec),
        None => Default::default(),
    };
    let creation_only = !matches!(blocks[0].len(), 0 | 12) && blocks[1..].iter().all(Vec::is_empty);
    if creation_only {
        let creation = std::mem::take(&mut blocks[0]);
        let word = |i: usize| creation.get(4 * i..4 * i + 4).map_or([0; 4], |b| b.try_into().unwrap());
        blocks[0] = [[0; 4], word(1), word(2)].concat();
        blocks[1] = [&creation[..4.min(creation.len())], creation.get(12..).unwrap_or(&[])].concat();
    }
    blocks[4] = stats.block();
    let mut out: Vec<u8> = blocks.iter().flat_map(|b| (b.len() as u32).to_le_bytes()).collect();
    for b in &blocks {
        out.extend_from_slice(b);
    }
    out
}

/// Rewrites character data stored in the layout this server once assumed,
/// six times `u32 size` followed by that block, into tincat's layout (see
/// [`character_blocks`]). `None` when the data is already valid or is not
/// in the old layout either. The client reads old-layout data as garbage:
/// "Besucher" (44-byte creation block, tribe 1) came out as tribe 3, which
/// the match dialog shows as "!TRIBE_NAME_RANDOM" and which ends the match
/// while the map loads.
pub fn repair_character_data(data: &[u8]) -> Option<Vec<u8>> {
    if character_blocks(data).is_some() {
        return None;
    }
    let mut blocks = Vec::new();
    let mut at = 0usize;
    for _ in 0..6 {
        let size = u32::from_le_bytes(data.get(at..at + 4)?.try_into().unwrap()) as usize;
        blocks.push(data.get(at + 4..at.checked_add(4 + size)?)?);
        at += 4 + size;
    }
    if at != data.len() {
        return None;
    }
    let mut out: Vec<u8> = blocks.iter().flat_map(|b| (b.len() as u32).to_le_bytes()).collect();
    for b in blocks {
        out.extend_from_slice(b);
    }
    Some(out)
}

/// Message 1001: `u32` avatar id (the character id; the client recognises
/// its own avatar by it and skips that location), the 4-bit block mask, then
/// the blocks in mask order. Only location and style are written; the client
/// keeps its defaults for active items and stats.
pub fn avatar_data(avatar: u32, location: Option<&Location>, style: Option<&Style>) -> Vec<u8> {
    let mut w = BitWriter::new();
    let mask = location.map_or(0, |_| block::LOCATION) | style.map_or(0, |_| block::STYLE);
    w.u32(avatar).bits(mask as u32, 4);
    if let Some(l) = location {
        l.write(&mut w);
    }
    if let Some(s) = style {
        s.write(&mut w);
    }
    w.finish()
}

/// Body of messages holding one `u32`: 1002 / 1003 (avatar id) and the ping
/// code of 3798 / 3799.
pub fn single_u32(value: u32) -> Vec<u8> {
    BitWriter::new().u32(value).finish()
}

/// Data block of a village entry in `GameServerData`: the village protocol
/// number (the client only lists villages matching its own, 1000 for retail
/// build 34688 and 999 for the demo) and a flag the client stores with it.
pub fn village_info(protocol: u32, flag: bool) -> Vec<u8> {
    BitWriter::new().u32(protocol).bool(flag).finish()
}

/// Message 1000: world name, the village server's perm id and the chat
/// channels as (zone, channel id). The client joins the channel of zone 255.
pub fn world_welcome(world: &str, server_perm: u32, channels: &[(u32, u32)]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.str(world).u32(server_perm).u32(channels.len() as u32);
    for &(zone, channel) in channels {
        w.u32(zone).u32(channel);
    }
    w.finish()
}

/// Builds the 16-bit `msg_type` header for an untyped message.
pub fn header(id: u16) -> u16 {
    id & 0x0fff
}

/// Splits a header into (typed, channel, id).
pub fn split_header(msg_type: u32) -> (bool, u8, u16) {
    let h = msg_type as u16;
    (h & 0x8000 != 0, ((h >> 12) & 7) as u8, h & 0x0fff)
}

#[derive(Default, Debug, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes the low `n` bits of `value`, most significant first.
    pub fn bits(&mut self, value: u32, n: u32) -> &mut Self {
        for i in (0..n).rev() {
            if self.bits % 8 == 0 {
                self.bytes.push(0);
            }
            if value >> i & 1 != 0 {
                *self.bytes.last_mut().unwrap() |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.bits(value, 32)
    }

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.bits(value as u32, 1)
    }

    /// 8-bit length, then the characters (Latin-1, at most 255).
    pub fn str(&mut self, text: &str) -> &mut Self {
        let bytes = crate::message::latin1(text);
        let bytes = &bytes[..bytes.len().min(255)];
        self.bits(bytes.len() as u32, 8);
        for &b in bytes {
            self.bits(b as u32, 8);
        }
        self
    }

    pub fn finish(&self) -> Vec<u8> {
        self.bytes.clone()
    }
}

#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Reads `n` bits (MSB first); `None` past the end.
    pub fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.bytes.get(self.pos / 8)?;
            v = v << 1 | (byte >> (7 - self.pos % 8) & 1) as u32;
            self.pos += 1;
        }
        Some(v)
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.bits(32)
    }

    pub fn bool(&mut self) -> Option<bool> {
        self.bits(1).map(|b| b != 0)
    }

    pub fn str(&mut self) -> Option<String> {
        let len = self.bits(8)?;
        (0..len).map(|_| self.bits(8).map(|b| b as u8 as char)).collect()
    }

    pub fn remaining_bits(&self) -> usize {
        (self.bytes.len() * 8).saturating_sub(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_msb_first() {
        // Village data block in GameServerData: protocol 1000, flag set.
        assert_eq!(village_info(1000, true), [0, 0, 0x03, 0xe8, 0x80]);
    }

    #[test]
    fn encodes_world_welcome() {
        let data = world_welcome("Dorf", 9, &[(255, 1)]);
        assert_eq!(data.len(), 1 + 4 + 4 + 4 + 8);
        let mut r = BitReader::new(&data);
        assert_eq!(r.str().as_deref(), Some("Dorf"));
        assert_eq!((r.u32(), r.u32(), r.u32(), r.u32()), (Some(9), Some(1), Some(255), Some(1)));
        assert_eq!(r.remaining_bits(), 0);
    }

    #[test]
    fn round_trips_mixed_fields() {
        let mut w = BitWriter::new();
        w.str("Siedlerdorf").u32(7).bits(5, 3).bool(false).u32(0xdead_beef);
        let bytes = w.finish();
        assert_eq!(bytes[0], 11);
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.str().as_deref(), Some("Siedlerdorf"));
        assert_eq!(r.u32(), Some(7));
        assert_eq!(r.bits(3), Some(5));
        assert_eq!(r.bool(), Some(false));
        assert_eq!(r.u32(), Some(0xdead_beef));
        assert!(r.remaining_bits() < 8);
        assert_eq!(r.bits(8), None);
    }

    /// Message 2000 recorded from the retail client (avatar standing at the
    /// village start).
    const MOVE: [u8; 9] = [0x07, 0xdb, 0x65, 0x4a, 0x29, 0xe3, 0x80, 0x0f, 0x00];

    #[test]
    fn parses_client_move() {
        let l = Location::parse(&MOVE).unwrap();
        assert_eq!(l.tick, 0x07db);
        assert_eq!(l.pos, [0x32a, 0x28a, 0x3c7]);
        assert_eq!((l.rot, l.zone, l.ghost_zone, l.running, l.flag), (0, 0, 15, false, false));
        let mut w = BitWriter::new();
        l.write(&mut w);
        assert_eq!(w.finish(), MOVE);
    }

    #[test]
    fn avatar_data_carries_location_and_style() {
        let l = Location { tick: 0x1235, pos: [1, 2047, 1024], rot: 127, zone: 3, ghost_zone: 15, running: true, flag: false };
        let s = Style { name: "garibald".into(), tribe_gender: 0x21, hair: 3, skin: 4, shirt: 5, trousers: 6, extra: [7, 8, 9, 10] };
        let data = avatar_data(42, Some(&l), Some(&s));
        let mut r = BitReader::new(&data);
        assert_eq!(r.u32(), Some(42));
        assert_eq!(r.bits(4), Some((block::LOCATION | block::STYLE) as u32));
        assert_eq!(Location::read(&mut r), Some(l));
        assert_eq!(Style::read(&mut r), Some(s));
        assert!(r.remaining_bits() < 8);

        let moved = avatar_data(42, Some(&l), None);
        assert_eq!(moved.len(), (32 + 4 + 66 + 7) / 8);
        assert_eq!(moved[4] >> 4, block::LOCATION);
        assert_eq!(single_u32(42), [0, 0, 0, 42]);
    }

    /// Character data in tincat's layout: six block sizes, then the blocks.
    fn character(blocks: &[&[u32]]) -> Vec<u8> {
        let mut data: Vec<u8> = (0..6).flat_map(|i| (4 * blocks.get(i).map_or(0, |b| b.len()) as u32).to_le_bytes()).collect();
        for v in blocks.iter().flat_map(|b| b.iter()) {
            data.extend(v.to_le_bytes());
        }
        data
    }

    #[test]
    fn style_from_character_data() {
        // AddCharacter data of a retail avatar created with the defaults
        // (garibald in our accounts): only a 44-byte creation block, version
        // 1, everything else 0.
        let mut data = vec![0u8; 68];
        data[0] = 44;
        data[24] = 1;
        assert_eq!(Style::from_character("garibald", &data), Style { name: "garibald".into(), ..Style::default() });

        // Creation block with every field set; values keep 4 bits.
        let s = Style::from_character("x", &character(&[&[1, 2, 1, 3, 4, 5, 6, 7, 8, 9, 0x1a]]));
        assert_eq!((s.tribe_gender, s.hair, s.skin, s.shirt, s.trousers, s.extra), (0x21, 3, 4, 5, 6, [7, 8, 9, 0xa]));
        // Version 0: no extra colours.
        let s = Style::from_character("x", &character(&[&[0, 1, 0, 3, 4, 5, 6, 7, 8, 9, 10]]));
        assert_eq!((s.tribe_gender, s.trousers, s.extra), (0x10, 6, [0; 4]));

        // Appearance block plus style block.
        let s = Style::from_character("y", &character(&[&[1, 2, 1], &[1, 3, 4, 5, 6, 7, 8, 9, 10]]));
        assert_eq!((s.tribe_gender, s.hair, s.skin, s.shirt, s.trousers, s.extra), (0x21, 3, 4, 5, 6, [7, 8, 9, 10]));

        assert_eq!(Style::from_character("y", &[]), Style { name: "y".into(), ..Style::default() });
    }

    #[test]
    fn splits_character_blocks() {
        let data = character(&[&[1, 2], &[], &[3]]);
        let blocks = character_blocks(&data).unwrap();
        assert_eq!(blocks.map(<[u8]>::len), [8, 0, 4, 0, 0, 0]);
        assert_eq!(blocks[2], 3u32.to_le_bytes());
        assert!(character_blocks(&data[..data.len() - 1]).is_none());
        assert!(character_blocks(&[0; 23]).is_none());
        assert!(character_blocks(&[0xff; 24]).is_none());
        assert_eq!(repair_character_data(&data), None);
    }

    #[test]
    fn repairs_old_layout_character_data() {
        // "Besucher" as it was stored: one 44-byte creation block (version 1,
        // tribe 1, gender 1, hair 2, skin 1, shirt 5, trousers 3) in the old
        // "size, block" layout. tincat reads it as sizes 44, 1, 1, 1, 2, 1.
        let mut old = 44u32.to_le_bytes().to_vec();
        for v in [1u32, 1, 1, 2, 1, 5, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0] {
            old.extend(v.to_le_bytes());
        }
        assert!(character_blocks(&old).is_none());
        let fixed = repair_character_data(&old).unwrap();
        assert_eq!(fixed, character(&[&[1, 1, 1, 2, 1, 5, 3, 0, 0, 0, 0]]));
        let s = Style::from_character("Besucher", &fixed);
        assert_eq!((s.tribe_gender, s.hair, s.skin, s.shirt, s.trousers), (0x11, 2, 1, 5, 3));
        assert_eq!(repair_character_data(&old[..old.len() - 1]), None);
    }

    #[test]
    fn splits_headers() {
        assert_eq!(split_header(header(id::WORLD_WELCOME) as u32), (false, 0, 1000));
        assert_eq!(split_header(0x8000 | 0x2000 | 3930), (true, 2, 3930));
    }

    #[test]
    fn stats_go_into_block_4_of_the_full_layout() {
        // A creation block as character creation sends it: version 1,
        // tribe 2, gender 1, hair 3, skin 4, shirt 5, trousers 6, extras 7-10.
        let creation: Vec<u8> = [1u32, 2, 1, 3, 4, 5, 6, 7, 8, 9, 10].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut data: Vec<u8> = [44u32, 0, 0, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        data.extend_from_slice(&creation);
        let stats = Stats { gold: 1234, funnies: 77, ..Stats::default() };
        let full = with_stats(&data, &stats);
        let blocks = character_blocks(&full).unwrap();
        assert_eq!(blocks.map(<[u8]>::len), [12, 36, 0, 0, 20, 0]);
        assert_eq!(blocks[4], [1u32, 1, 0, 1234, 77].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>());
        // The look is unchanged.
        assert_eq!(Style::from_character("A", &full), Style::from_character("A", &data));
        assert_eq!(Style::from_character("A", &full).tribe_gender, 0x21);
        // Full data keeps its blocks; only block 4 changes.
        let again = with_stats(&full, &Stats::default());
        assert_eq!(character_blocks(&again).unwrap()[..4], blocks[..4]);
        assert_eq!(character_blocks(&with_stats(b"junk", &stats)).unwrap()[4].len(), 20);
        // 3201: ownr, lvl, exp, gold, glod, MSB first.
        assert_eq!(stats.message(9)[..8], [0, 0, 0, 9, 0, 0, 0, 1]);
    }
}

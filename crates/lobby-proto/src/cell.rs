//! Cell messages: the chat channel protocol of the usercomm server. A "cell"
//! is a chat channel. These payloads share the frame with the msgdefs
//! messages but start with their own magic:
//!
//! `u16 0x0062 | u16 0 | u16 id`, then the fields of that id.
//!
//! Lobby messages travel inside them as property sets
//! ([`Message::encode_propset`](crate::Message::encode_propset)): the client
//! routes a `ChatMessage` to a cell with [`Cell::Route`], and the server hands
//! it, `UserJoinedChannel` and `UserLeftChannel` to the members with
//! [`Cell::Deliver`]. Strings are `u32` length plus bytes.
//!
//! A whisper is routed to a channel as `WhisperChatMessage` naming the
//! receiver's perm id, but tincat3 only shows a delivered
//! `PrivateChatMessage`; [`private_line`] turns one into the other.
//!
//! Layouts are from a recorded session with the original server
//! (`Sacred2LobbyPacketDumps` in S2-modders/Settlers-AdK-lobby-emulator; the
//! same tincat3 library). That server wrote strings without NUL, but the SADK
//! retail client (build 34688) then drops their last character, so we write
//! the NUL as in other messages. Both forms decode.

use crate::message::{latin1, latin1_to_string, DecodeError};
use crate::{Message, MsgDefs, Value};

/// The `PrivateChatMessage` the server delivers for a client's
/// `WhisperChatMessage` (`perm_id` = receiver), from the sender `from` with
/// its name. Both receiver and sender are given it: the receiver's client
/// shows it from `from_name`, the sender's (seeing its own `from_id`) shows
/// it to the receiver's name. `None` if the defs lack the message or the
/// whisper names no receiver.
pub fn private_line(defs: &MsgDefs, whisper: &Message, from: u32, from_name: &str) -> Option<Message> {
    let to = whisper.int("perm_id")?;
    Some(
        Message::new(defs, "PrivateChatMessage")?
            .with("mode", Value::Int(whisper.int("mode").unwrap_or(0)))
            .with("txt", Value::text(&whisper.string("txt").unwrap_or_default()))
            .with("cell_id", Value::Int(whisper.int("cell_id").unwrap_or(0)))
            .with("from_id", Value::Int(from as i64))
            .with("to_id", Value::Int(to))
            .with("from_name", Value::text(from_name)),
    )
}

/// Payload magic of cell messages.
pub const MAGIC: u16 = 0x0062;

/// Cell message ids.
pub mod id {
    /// Server → client: a channel exists (sent for every channel right after
    /// connecting).
    pub const CHANNEL_INFO: u16 = 0;
    /// Server → client: a channel is gone (or the client left it).
    pub const CHANNEL_LEFT: u16 = 1;
    /// Client → server: deliver a property set to a channel's members.
    pub const ROUTE: u16 = 2;
    /// Server → client: a property set from a channel.
    pub const DELIVER: u16 = 3;
    /// Client → server: create a channel.
    pub const CREATE_CHANNEL: u16 = 7;
    /// Server → client: the client is now in a channel.
    pub const JOINED: u16 = 9;
    /// Server → client: result of a channel request, by ticket.
    pub const STATUS: u16 = 11;
}

/// `except` in [`Cell::Route`] when nobody is left out.
pub const NOBODY: u32 = u32::MAX;

/// Description of a channel in [`Cell::ChannelInfo`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelData {
    /// "SC" in every recorded channel.
    pub publish: String,
    pub name: String,
    pub subject: String,
    pub creator: String,
    pub password: String,
    pub protected: bool,
    pub persistent: bool,
    pub autodelete: bool,
    pub hidden: bool,
    pub creator_pid: u32,
}

impl ChannelData {
    /// A permanent public channel owned by the server.
    pub fn public(name: &str, subject: &str) -> Self {
        Self { publish: "SC".into(), name: name.into(), subject: subject.into(), creator: "Admin".into(), persistent: true, ..Self::default() }
    }

    fn write(&self, w: &mut Writer) {
        for s in [&self.publish, &self.name, &self.subject, &self.creator, &self.password] {
            w.str(s);
        }
        for b in [self.protected, self.persistent, self.autodelete, self.hidden] {
            w.u8(b as u8);
        }
        w.u32(self.creator_pid);
    }

    fn read(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(Self {
            publish: r.str()?,
            name: r.str()?,
            subject: r.str()?,
            creator: r.str()?,
            password: r.str()?,
            protected: r.u8()? != 0,
            persistent: r.u8()? != 0,
            autodelete: r.u8()? != 0,
            hidden: r.u8()? != 0,
            creator_pid: r.u32()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    ChannelInfo { channel: ChannelData, cell: u32, ticket: u32 },
    ChannelLeft { cell: u32, ticket: u32, option: u16 },
    /// `message` is the lobby message id of `data`, a property set when
    /// `propset` is set. `except` is a perm id left out ([`NOBODY`]);
    /// `to_self` asks for a copy back.
    Route { module: u16, message: u16, except: u32, data: Vec<u8>, cell: u32, to_self: bool, propset: bool },
    /// `from` is the sender's perm id, or [`frame::SERVER`](crate::frame::SERVER)
    /// for the server's own notices (whose `cell` is 0; the property set
    /// names the channel).
    Deliver { message: u16, data: Vec<u8>, cell: u32, from: u32, propset: bool },
    Joined { cell: u32, ticket: u32, option: u16 },
    /// `result` 0 is success.
    Status { cell: u32, ticket: u32, result: u16 },
    /// Any other id, body undecoded.
    Other { id: u16, body: Vec<u8> },
}

impl Cell {
    pub fn id(&self) -> u16 {
        match self {
            Cell::ChannelInfo { .. } => id::CHANNEL_INFO,
            Cell::ChannelLeft { .. } => id::CHANNEL_LEFT,
            Cell::Route { .. } => id::ROUTE,
            Cell::Deliver { .. } => id::DELIVER,
            Cell::Joined { .. } => id::JOINED,
            Cell::Status { .. } => id::STATUS,
            Cell::Other { id, .. } => *id,
        }
    }

    /// True if a frame payload is a cell message.
    pub fn is_cell(payload: &[u8]) -> bool {
        payload.starts_with(&MAGIC.to_le_bytes())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer(Vec::with_capacity(32));
        w.u16(MAGIC);
        w.u16(0);
        w.u16(self.id());
        match self {
            Cell::ChannelInfo { channel, cell, ticket } => {
                let mut data = Writer(Vec::new());
                channel.write(&mut data);
                w.block(&data.0);
                w.u32(*cell);
                w.u32(*ticket);
            }
            Cell::ChannelLeft { cell, ticket, option } | Cell::Joined { cell, ticket, option } => {
                w.u32(*cell);
                w.u32(*ticket);
                w.u16(*option);
            }
            Cell::Route { module, message, except, data, cell, to_self, propset } => {
                w.u16(*module);
                w.u16(*message);
                w.u32(*except);
                w.block(data);
                w.u32(*cell);
                w.u8(*to_self as u8);
                w.u8(*propset as u8);
            }
            Cell::Deliver { message, data, cell, from, propset } => {
                w.u16(*message);
                w.block(data);
                w.u32(*cell);
                w.u32(*from);
                w.u8(*propset as u8);
            }
            Cell::Status { cell, ticket, result } => {
                w.u32(*cell);
                w.u32(*ticket);
                w.u16(*result);
            }
            Cell::Other { body, .. } => w.0.extend_from_slice(body),
        }
        w.0
    }

    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader { buf: payload, pos: 0 };
        let magic = r.u16()?;
        if magic != MAGIC {
            return Err(DecodeError::BadMagic(magic));
        }
        let _kind = r.u16()?;
        let id = r.u16()?;
        Ok(match id {
            id::CHANNEL_INFO => {
                let data = r.block()?;
                let channel = ChannelData::read(&mut Reader { buf: data, pos: 0 })?;
                Cell::ChannelInfo { channel, cell: r.u32()?, ticket: r.u32()? }
            }
            id::CHANNEL_LEFT => Cell::ChannelLeft { cell: r.u32()?, ticket: r.u32()?, option: r.u16()? },
            id::JOINED => Cell::Joined { cell: r.u32()?, ticket: r.u32()?, option: r.u16()? },
            id::ROUTE => Cell::Route {
                module: r.u16()?,
                message: r.u16()?,
                except: r.u32()?,
                data: r.block()?.to_vec(),
                cell: r.u32()?,
                to_self: r.u8()? != 0,
                propset: r.u8()? != 0,
            },
            id::DELIVER => Cell::Deliver { message: r.u16()?, data: r.block()?.to_vec(), cell: r.u32()?, from: r.u32()?, propset: r.u8()? != 0 },
            id::STATUS => Cell::Status { cell: r.u32()?, ticket: r.u32()?, result: r.u16()? },
            id => Cell::Other { id, body: payload[r.pos..].to_vec() },
        })
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn block(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        let mut bytes = latin1(s);
        bytes.push(0);
        self.block(&bytes);
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, at: &'static str) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or(DecodeError::Truncated { at })?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1, "byte")?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2, "short")?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4, "long")?.try_into().unwrap()))
    }
    fn block(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.u32()? as usize;
        self.take(len, "block")
    }
    fn str(&mut self) -> Result<String, DecodeError> {
        let raw = self.block()?;
        Ok(latin1_to_string(raw.strip_suffix(&[0]).unwrap_or(raw)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::tests::hex;
    use crate::frame::SERVER;

    const DEFS: &str = r#"# ChatMessage
[NETMSG_TYPE_2]
type="UNSHORT"
mode="UNLONG"
txt="STRING 256"
ticket_id="UNLONG"
from_id="UNLONG"

# PrivateChatMessage
[NETMSG_TYPE_3]
type="UNSHORT"
mode="UNLONG"
txt="STRING 256"
cell_id="UNLONG"
from_id="UNLONG"
to_id="UNLONG"
from_name="STRING 256"

# UserJoinedChannel
[NETMSG_TYPE_5]
type="UNSHORT"
perm_id="UNLONG"
cell_id="UNLONG"
nick="STRING 256"

# WhisperChatMessage
[NETMSG_TYPE_30]
type="UNSHORT"
mode="UNLONG"
txt="STRING 256"
cell_id="UNLONG"
from_id="UNLONG"
perm_id="UNLONG"
"#;

    /// The client says "hello world" in channel 2 (recorded, Sacred 2 dump 4).
    const ROUTE: &str = "62000000020000000200ffffffff1d0000000200000000000b00000068656c6c6f20776f726c6400\n        00000006000000020000000101";
    /// The server hands it to the members, from perm id 6.
    const DELIVER: &str = "62000000030002001d0000000200000000000b00000068656c6c6f20776f726c6400000000060000\n        00020000000600000001";
    /// "tester" (perm 6) joined channel 1, from the server.
    const JOINED_NOTICE: &str = "620000000300050014000000050006000000010000000600000074657374657200000000ccffffef\n        01";
    /// The SADK retail client (perm 1) whispers "psst" to perm 2 in channel 1
    /// (recorded against sadk-lobby).
    const WHISPER: &str = "62000000020000001e00ffffffff1b0000001e0003000000050000007073737400010000000100000002000000010000000101";
    /// Channel 2, "Lobby", sent right after connecting.
    const LOBBY_INFO: &str = "62000000000030000000020000005343050000004c6f6262790d0000004c6f626279204368616e6e\n        656c000000000000000000010000000000000200000000000000";

    fn h(s: &str) -> Vec<u8> {
        hex(&s.replace(' ', ""))
    }

    #[test]
    fn decodes_recorded_chat_line() {
        let defs = MsgDefs::parse(DEFS).unwrap();
        let route = Cell::decode(&h(ROUTE)).unwrap();
        let Cell::Route { module: 0, message: 2, except: NOBODY, ref data, cell: 2, to_self: true, propset: true } = route else {
            panic!("{route:?}")
        };
        let chat = Message::decode_propset(&defs, data).unwrap();
        assert_eq!(chat.name, "ChatMessage");
        assert_eq!((chat.string("txt").unwrap().as_str(), chat.int("from_id")), ("hello world", Some(6)));
        assert_eq!(Message::decode_propset(&defs, &chat.encode_propset(&defs)).unwrap(), chat);
        assert_eq!(route.encode(), h(ROUTE));

        let deliver = Cell::Deliver { message: 2, data: data.clone(), cell: 2, from: 6, propset: true };
        assert_eq!(deliver.encode(), h(DELIVER));
        assert_eq!(Cell::decode(&h(DELIVER)).unwrap(), deliver);
    }

    #[test]
    fn encodes_recorded_server_messages() {
        let defs = MsgDefs::parse(DEFS).unwrap();
        let joined = Message::new(&defs, "UserJoinedChannel")
            .unwrap()
            .with("perm_id", Value::Int(6))
            .with("cell_id", Value::Int(1))
            .with("nick", Value::text("tester"));
        let notice = Cell::Deliver { message: 5, data: joined.encode_propset(&defs), cell: 0, from: SERVER, propset: true };
        // As recorded, but "tester" with its NUL (and the block one longer).
        assert_eq!(notice.encode(), h("620000000300050015000000050006000000010000000700000074657374657200000000 00ccffffef01"));
        let Cell::Deliver { data, .. } = Cell::decode(&h(JOINED_NOTICE)).unwrap() else { panic!() };
        assert_eq!(Message::decode_propset(&defs, &data).unwrap(), joined);

        // The recorded Lobby channel names no creator.
        let lobby = ChannelData { creator: String::new(), ..ChannelData::public("Lobby", "Lobby Channel") };
        let info = Cell::ChannelInfo { channel: lobby, cell: 2, ticket: 0 };
        assert_eq!(Cell::decode(&h(LOBBY_INFO)).unwrap(), info);
        let encoded = info.encode();
        assert_eq!(encoded.len(), h(LOBBY_INFO).len() + 5, "five strings, each with a NUL");
        assert_eq!(Cell::decode(&encoded).unwrap(), info);

        assert_eq!(Cell::Joined { cell: 1, ticket: 0, option: 0 }.encode(), h("62000000090001000000000000000000"));
        assert_eq!(Cell::Status { cell: 1, ticket: 2, result: 0 }.encode(), h("620000000b00010000000200000000 00"));
    }

    #[test]
    fn turns_a_whisper_into_a_private_line() {
        let defs = MsgDefs::parse(DEFS).unwrap();
        let Cell::Route { message: 30, except: NOBODY, data, cell: 1, to_self: true, propset: true, .. } = Cell::decode(&h(WHISPER)).unwrap() else {
            panic!()
        };
        let whisper = Message::decode_propset(&defs, &data).unwrap();
        assert_eq!(whisper.name, "WhisperChatMessage");
        assert_eq!((whisper.int("mode"), whisper.string("txt").unwrap().as_str()), (Some(3), "psst"));
        assert_eq!((whisper.int("cell_id"), whisper.int("from_id"), whisper.int("perm_id")), (Some(1), Some(1), Some(2)));

        let line = private_line(&defs, &whisper, 1, "siedler").unwrap();
        let deliver = Cell::Deliver { message: line.id, data: line.encode_propset(&defs), cell: 1, from: 1, propset: true };
        let Cell::Deliver { message: 3, data, .. } = Cell::decode(&deliver.encode()).unwrap() else { panic!() };
        let got = Message::decode_propset(&defs, &data).unwrap();
        assert_eq!(got, line);
        assert_eq!((got.int("mode"), got.string("txt").unwrap().as_str(), got.int("cell_id")), (Some(3), "psst", Some(1)));
        assert_eq!((got.int("from_id"), got.int("to_id"), got.string("from_name").unwrap().as_str()), (Some(1), Some(2), "siedler"));

        assert!(private_line(&MsgDefs::parse("").unwrap(), &whisper, 1, "x").is_none());
    }

    #[test]
    fn rejects_short_and_foreign_payloads() {
        assert!(Cell::decode(&h("6200000002000000")).is_err());
        assert_eq!(Cell::decode(&h("b6261100")), Err(DecodeError::BadMagic(0x26b6)));
        assert!(Cell::is_cell(&h(ROUTE)) && !Cell::is_cell(&h("b6261100")));
        assert_eq!(Cell::decode(&h("620000002a000102")).unwrap(), Cell::Other { id: 42, body: vec![1, 2] });
    }
}

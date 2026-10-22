//! Wire format of the SADK lobby (tincat3).
//!
//! Layers:
//! * [`frame`]: the 28-byte transport header (magic `0xDABAFBEF`, sender,
//!   receiver, kind, flags, size, CRC) plus the connect handshake.
//! * [`msgdefs`] / [`message`]: application payloads, described field by field
//!   by the game's own `bin/msgdefs.ini` (`[NETMSG_TYPE_<id>]` sections).
//! * [`cell`]: the chat channel ("cell") messages of the usercomm server.
//!
//! No game data is embedded; the schema is read from the user's install.

pub mod auth;
pub mod cell;
pub mod crc;
pub mod dice;
pub mod frame;
pub mod lobby_message;
pub mod message;
pub mod minigame;
pub mod msgdefs;
pub mod pawnchess;
pub mod poker;
pub mod referee;

pub use frame::{Frame, FrameKind, Handshake};
pub use message::{Message, Value};
pub use msgdefs::{FieldType, MessageDef, MsgDefs};

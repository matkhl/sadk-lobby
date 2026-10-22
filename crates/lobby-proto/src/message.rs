//! Application payloads: `u16 magic | u16 id`, then the fields of
//! `NETMSG_TYPE_<id>` in msgdefs order (the first field, `type`, repeats the id).
//! All integers are little-endian. Clients pad payloads with stale buffer
//! bytes after the last field, so trailing data is ignored.
//!
//! The magic tracks the protocol revision: retail build 34688 sends 0x26B6,
//! an older recorded client 0x27D8. Replies should echo the client's magic.

use crate::msgdefs::{FieldType, MsgDefs};
use std::fmt;

/// Magic sent by the retail build 34688 client.
pub const RETAIL_MAGIC: u16 = 0x26B6;
/// Magic in an older recorded client session.
pub const OLDER_MAGIC: u16 = 0x27D8;
pub const KNOWN_MAGICS: [u16; 2] = [RETAIL_MAGIC, OLDER_MAGIC];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bool(bool),
    /// String bytes without the terminating NUL; `None` is the empty/absent
    /// string (length 0 on the wire). The game uses Latin-1 style 8-bit text.
    Str(Option<Vec<u8>>),
    Bytes(Option<Vec<u8>>),
}

impl Value {
    pub fn text(s: &str) -> Self {
        Value::Str(Some(latin1(s)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub magic: u16,
    pub id: u16,
    pub name: String,
    /// Field values in schema order, excluding `type`.
    pub fields: Vec<(String, Value)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated { at: &'static str },
    BadMagic(u16),
    UnknownType(u16),
    TypeMismatch { header: u16, field: u16 },
    StringTooLong { field: String, len: usize, max: u32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { at } => write!(f, "payload truncated in {at}"),
            Self::BadMagic(m) => write!(f, "unknown payload magic {m:#06x}"),
            Self::UnknownType(t) => write!(f, "message type {t} is not in msgdefs"),
            Self::TypeMismatch { header, field } => write!(f, "header type {header} but type field {field}"),
            Self::StringTooLong { field, len, max } => write!(f, "{field}: {len} bytes exceeds {max}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Converts text to the game's 8-bit encoding (characters above U+00FF become '?').
pub fn latin1(s: &str) -> Vec<u8> {
    s.chars().map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' }).collect()
}

pub fn latin1_to_string(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
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
    fn block(&mut self) -> Result<Option<&'a [u8]>, DecodeError> {
        let len = self.u32()? as usize;
        if len == 0 {
            return Ok(None);
        }
        Ok(Some(self.take(len, "string/memblock")?))
    }
}

impl Message {
    /// Builds a message by schema name; unspecified fields encode as 0/empty.
    pub fn new(defs: &MsgDefs, name: &str) -> Option<Self> {
        let def = defs.by_name(name)?;
        Some(Self { magic: RETAIL_MAGIC, id: def.id, name: def.name.clone(), fields: Vec::new() })
    }

    pub fn with(mut self, field: &str, value: Value) -> Self {
        self.set(field, value);
        self
    }

    pub fn set(&mut self, field: &str, value: Value) {
        match self.fields.iter_mut().find(|(n, _)| n == field) {
            Some(slot) => slot.1 = value,
            None => self.fields.push((field.to_string(), value)),
        }
    }

    pub fn get(&self, field: &str) -> Option<&Value> {
        self.fields.iter().find(|(n, _)| n == field).map(|(_, v)| v)
    }

    pub fn int(&self, field: &str) -> Option<i64> {
        match self.get(field)? {
            Value::Int(v) => Some(*v),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    /// String field bytes (empty for an absent string).
    pub fn bytes(&self, field: &str) -> Option<&[u8]> {
        match self.get(field)? {
            Value::Str(s) | Value::Bytes(s) => Some(s.as_deref().unwrap_or(&[])),
            _ => None,
        }
    }

    pub fn string(&self, field: &str) -> Option<String> {
        self.bytes(field).map(latin1_to_string)
    }

    pub fn decode(defs: &MsgDefs, payload: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader { buf: payload, pos: 0 };
        let magic = r.u16()?;
        if !KNOWN_MAGICS.contains(&magic) {
            return Err(DecodeError::BadMagic(magic));
        }
        let id = r.u16()?;
        Self::decode_fields(defs, magic, id, &mut r)
    }

    /// Decodes a property set: a message embedded in a usercomm cell message
    /// (see `cell`). It has no magic and no header id, so it starts with the
    /// `type` field. A string's NUL is optional, as in other messages.
    pub fn decode_propset(defs: &MsgDefs, data: &[u8]) -> Result<Self, DecodeError> {
        let id = Reader { buf: data, pos: 0 }.u16()?;
        Self::decode_fields(defs, RETAIL_MAGIC, id, &mut Reader { buf: data, pos: 0 })
    }

    fn decode_fields(defs: &MsgDefs, magic: u16, id: u16, r: &mut Reader) -> Result<Self, DecodeError> {
        let def = defs.get(id).ok_or(DecodeError::UnknownType(id))?;
        let mut fields = Vec::with_capacity(def.fields.len());
        for field in &def.fields {
            let value = match field.ty {
                FieldType::UnByte => Value::Int(r.u8()? as i64),
                FieldType::SiByte => Value::Int(r.u8()? as i8 as i64),
                FieldType::UnShort => Value::Int(r.u16()? as i64),
                FieldType::SiShort => Value::Int(r.u16()? as i16 as i64),
                FieldType::UnLong => Value::Int(r.u32()? as i64),
                FieldType::SiLong => Value::Int(r.u32()? as i32 as i64),
                FieldType::LBool => Value::Bool(r.u8()? != 0),
                FieldType::String(max) => {
                    let raw = r.block()?;
                    if let Some(raw) = raw {
                        if raw.len() > max as usize {
                            return Err(DecodeError::StringTooLong { field: field.name.clone(), len: raw.len(), max });
                        }
                    }
                    Value::Str(raw.map(|s| s.strip_suffix(&[0]).unwrap_or(s).to_vec()))
                }
                FieldType::MemBlock(_) => Value::Bytes(r.block()?.map(<[u8]>::to_vec)),
            };
            if field.name == "type" {
                let Value::Int(t) = value else { unreachable!() };
                if t != id as i64 {
                    return Err(DecodeError::TypeMismatch { header: id, field: t as u16 });
                }
                continue;
            }
            fields.push((field.name.clone(), value));
        }
        Ok(Self { magic, id, name: def.name.clone(), fields })
    }

    /// Encodes in schema order. Panics only if `self.id` is not in `defs`,
    /// which [`Message::new`] rules out.
    pub fn encode(&self, defs: &MsgDefs) -> Vec<u8> {
        let def = defs.get(self.id).expect("message id from these msgdefs");
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&self.magic.to_le_bytes());
        out.extend_from_slice(&self.id.to_le_bytes());
        for field in &def.fields {
            let value = if field.name == "type" { Some(&Value::Int(self.id as i64)) } else { self.get(&field.name) };
            let int = match value {
                Some(Value::Int(v)) => *v,
                Some(Value::Bool(b)) => *b as i64,
                _ => 0,
            };
            match field.ty {
                FieldType::UnByte | FieldType::SiByte | FieldType::LBool => out.push(int as u8),
                FieldType::UnShort | FieldType::SiShort => out.extend_from_slice(&(int as u16).to_le_bytes()),
                FieldType::UnLong | FieldType::SiLong => out.extend_from_slice(&(int as u32).to_le_bytes()),
                FieldType::String(max) => match value {
                    Some(Value::Str(Some(s))) => {
                        // Keep room for the NUL inside the declared maximum.
                        let s = &s[..s.len().min((max as usize).saturating_sub(1))];
                        out.extend_from_slice(&(s.len() as u32 + 1).to_le_bytes());
                        out.extend_from_slice(s);
                        out.push(0);
                    }
                    _ => out.extend_from_slice(&0u32.to_le_bytes()),
                },
                FieldType::MemBlock(_) => match value {
                    Some(Value::Bytes(Some(b))) => {
                        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                        out.extend_from_slice(b);
                    }
                    _ => out.extend_from_slice(&0u32.to_le_bytes()),
                },
            }
        }
        out
    }

    /// Encodes as a property set (see [`Message::decode_propset`]): the
    /// message without magic and header id. Strings keep their NUL; the
    /// retail client drops the last byte of every string otherwise.
    pub fn encode_propset(&self, defs: &MsgDefs) -> Vec<u8> {
        self.encode(defs).split_off(4)
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({}) {{", self.name, self.id)?;
        for (i, (name, value)) in self.fields.iter().enumerate() {
            let sep = if i == 0 { " " } else { ", " };
            match value {
                Value::Int(v) => write!(f, "{sep}{name}: {v}")?,
                Value::Bool(b) => write!(f, "{sep}{name}: {b}")?,
                Value::Str(None) | Value::Bytes(None) => write!(f, "{sep}{name}: -")?,
                Value::Str(Some(s)) => write!(f, "{sep}{name}: {:?}", latin1_to_string(s))?,
                Value::Bytes(Some(b)) => write!(f, "{sep}{name}: [{} bytes]", b.len())?,
            }
        }
        write!(f, " }}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::tests::hex;
    use crate::msgdefs::tests::SAMPLE;

    /// RequestLogin as sent by a real SADK client, including its stale padding.
    const LOGIN: &str = "d827040004000b0000006c6f6c6f6c6f6c6f6c6f00070000004156545e58410011000000\
        a796a597aaaeaca99c9e98a5a8a8ae9a000100fc2300000100000000c8180000201800000000000005";

    #[test]
    fn decodes_captured_login() {
        let defs = MsgDefs::parse(SAMPLE).unwrap();
        let msg = Message::decode(&defs, &hex(LOGIN)).unwrap();
        assert_eq!((msg.magic, msg.name.as_str()), (OLDER_MAGIC, "RequestLogin"));
        assert_eq!(msg.string("nick").unwrap(), "lololololo");
        assert_eq!(msg.bytes("password").unwrap().len(), 6);
        assert_eq!(msg.bytes("cd_key").unwrap().len(), 16);
        assert_eq!(msg.int("keypool"), Some(1));
        assert_eq!(msg.int("patchlevel"), Some(9212));
        assert_eq!(msg.int("ticket_id"), Some(1));
    }

    /// CheckVersion as sent by the retail 34688 client to our server.
    #[test]
    fn decodes_retail_check_version() {
        let defs = MsgDefs::parse(
            "# CheckVersion
[NETMSG_TYPE_188]
type=\"UNSHORT\"
version=\"SISHORT\"
subversion=\"SISHORT\"
ticket_id=\"UNLONG\"",
        )
        .unwrap();
        let msg = Message::decode(&defs, &hex("b626bc00bc001b00000001000000")).unwrap();
        assert_eq!(msg.magic, RETAIL_MAGIC);
        assert_eq!((msg.int("version"), msg.int("subversion"), msg.int("ticket_id")), (Some(27), Some(0), Some(1)));
        assert_eq!(msg.encode(&defs), hex("b626bc00bc001b00000001000000"));
    }

    #[test]
    fn round_trips_every_field_type() {
        let defs = MsgDefs::parse(SAMPLE).unwrap();
        let msg = Message::new(&defs, "Mixed")
            .unwrap()
            .with("flag", Value::Bool(true))
            .with("delta", Value::Int(-7))
            .with("small", Value::Int(-1))
            .with("data", Value::Bytes(Some(vec![1, 2, 3])));
        let back = Message::decode(&defs, &msg.encode(&defs)).unwrap();
        assert_eq!(back, msg);

        let result = Message::new(&defs, "Result")
            .unwrap()
            .with("errorcode", Value::Int(0x3d))
            .with("errormsg", Value::text("Wrong password"))
            .with("ticket_id", Value::Int(9));
        let bytes = result.encode(&defs);
        assert_eq!(&bytes[..7], &[0xb6, 0x26, 42, 0, 42, 0, 0x3d]);
        let back = Message::decode(&defs, &bytes).unwrap();
        assert_eq!(back.string("errormsg").unwrap(), "Wrong password");
        assert_eq!(back.int("ticket_id"), Some(9));
    }

    #[test]
    fn rejects_garbage() {
        let defs = MsgDefs::parse(SAMPLE).unwrap();
        assert_eq!(Message::decode(&defs, &[0x34, 0x12, 4, 0]), Err(DecodeError::BadMagic(0x1234)));
        assert_eq!(Message::decode(&defs, &[0xd8, 0x27, 9, 0]), Err(DecodeError::UnknownType(9)));
        assert!(matches!(Message::decode(&defs, &hex("d8270400")), Err(DecodeError::Truncated { .. })));
        // A string length that runs past the payload must not panic.
        assert!(Message::decode(&defs, &hex("d82704000400ffffff7f")).is_err());
    }
}

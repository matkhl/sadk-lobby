//! Transport frames. Every TCP message is a 28-byte little-endian header
//! followed by `size` payload bytes:
//!
//! ```text
//! u32 magic 0xDABAFBEF | u32 from | u32 to | u32 kind | u32 flags | u32 size | u32 crc
//! ```
//!
//! A client starts as `CLIENT_UNASSIGNED` and sends a [`FrameKind::Connect`]
//! handshake; the server answers [`FrameKind::Connected`] with the connection
//! id the client then uses as `from`.

use crate::crc::checksum;

pub const MAGIC: u32 = 0xDABA_FBEF;
pub const HEADER_LEN: usize = 28;
pub const CLIENT_UNASSIGNED: u32 = 0xEFFF_FFEE;
pub const SERVER: u32 = 0xEFFF_FFCC;
/// Upper bound for a payload; real lobby messages are a few hundred bytes.
pub const MAX_PAYLOAD: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Application,
    Connect,
    Connected,
    Ping,
    Other(u32),
}

impl FrameKind {
    pub fn from_u32(v: u32) -> Self {
        match v {
            2 => Self::Application,
            3 => Self::Connect,
            5 => Self::Connected,
            11 => Self::Ping,
            v => Self::Other(v),
        }
    }
    pub fn to_u32(self) -> u32 {
        match self {
            Self::Application => 2,
            Self::Connect => 3,
            Self::Connected => 5,
            Self::Ping => 11,
            Self::Other(v) => v,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub from: u32,
    pub to: u32,
    pub kind: FrameKind,
    /// Clients send 0xFFFFFFFF on application frames, the server 0.
    pub flags: u32,
    pub payload: Vec<u8>,
    /// False when the received CRC did not match (frame is still returned).
    pub checksum_ok: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    BadMagic(u32),
    TooLarge(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic(m) => write!(f, "bad frame magic {m:#010x}"),
            Self::TooLarge(n) => write!(f, "frame payload too large ({n} bytes)"),
        }
    }
}

impl std::error::Error for FrameError {}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
}

impl Frame {
    pub fn new(from: u32, to: u32, kind: FrameKind, payload: Vec<u8>) -> Self {
        Self { from, to, kind, flags: 0, payload, checksum_ok: true }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        for v in [
            MAGIC,
            self.from,
            self.to,
            self.kind.to_u32(),
            self.flags,
            self.payload.len() as u32,
            checksum(&self.payload),
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decodes one frame from the front of `buf`. Returns `Ok(None)` until the
    /// whole frame has arrived, otherwise the frame and the bytes it used.
    pub fn decode(buf: &[u8]) -> Result<Option<(Frame, usize)>, FrameError> {
        if buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let magic = u32_at(buf, 0);
        if magic != MAGIC {
            return Err(FrameError::BadMagic(magic));
        }
        let size = u32_at(buf, 20) as usize;
        if size > MAX_PAYLOAD {
            return Err(FrameError::TooLarge(size));
        }
        if buf.len() < HEADER_LEN + size {
            return Ok(None);
        }
        let payload = buf[HEADER_LEN..HEADER_LEN + size].to_vec();
        let frame = Frame {
            from: u32_at(buf, 4),
            to: u32_at(buf, 8),
            kind: FrameKind::from_u32(u32_at(buf, 12)),
            flags: u32_at(buf, 16),
            checksum_ok: checksum(&payload) == u32_at(buf, 24),
            payload,
        };
        Ok(Some((frame, HEADER_LEN + size)))
    }
}

/// Payload of the Connect / Connected frames (0x34 bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    pub connection: u32,
    pub user: [u8; 32],
    pub password: [u8; 8],
    pub trailer: u32,
}

impl Handshake {
    pub const LEN: usize = 0x34;
    /// The original server's reply always carried this in the password slot.
    pub const SERVER_PASSWORD: [u8; 8] = [0x2d, 0, 0, 0, 0, 0, 0, 0];

    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() != Self::LEN || u32_at(payload, 0) != MAGIC {
            return None;
        }
        Some(Self {
            connection: u32_at(payload, 4),
            user: payload[8..40].try_into().unwrap(),
            password: payload[40..48].try_into().unwrap(),
            trailer: u32_at(payload, 48),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&self.connection.to_le_bytes());
        out.extend_from_slice(&self.user);
        out.extend_from_slice(&self.password);
        out.extend_from_slice(&self.trailer.to_le_bytes());
        out
    }

    /// The server's answer: same handshake, with the assigned connection id.
    pub fn accept(&self, connection: u32) -> Self {
        Self { connection, password: Self::SERVER_PASSWORD, ..self.clone() }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Recorded SADK connect and the original server's reply (S2-modders dump).
    const CONNECT: &str = "effbbadaeeffffefccffffef030000000000000034000000973a483b
        effbbadaeeffffef757365720000000064020000040000000000000064020000
        040000000000000070617373776f720001000000";
    const CONNECTED: &str = "effbbadaccffffef050000000500000000000000340000005f7c97ee
        effbbada05000000757365720000000064020000040000000000000064020000
        04000000000000002d0000000000000001000000";

    #[test]
    fn decodes_captured_connect_and_rebuilds_reply() {
        let bytes = hex(CONNECT);
        let (frame, used) = Frame::decode(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert!(frame.checksum_ok);
        assert_eq!((frame.from, frame.to, frame.kind), (CLIENT_UNASSIGNED, SERVER, FrameKind::Connect));
        let hs = Handshake::decode(&frame.payload).unwrap();
        assert_eq!(hs.connection, CLIENT_UNASSIGNED);
        assert_eq!(&hs.user[..4], b"user");

        // The recorded server answer is exactly what we would send for id 5.
        let reply = Frame::new(SERVER, 5, FrameKind::Connected, hs.accept(5).encode());
        assert_eq!(reply.encode(), hex(CONNECTED));
    }

    #[test]
    fn waits_for_partial_frames() {
        let bytes = hex(CONNECT);
        assert_eq!(Frame::decode(&bytes[..10]).unwrap(), None);
        assert_eq!(Frame::decode(&bytes[..bytes.len() - 1]).unwrap(), None);
        let mut bad = bytes.clone();
        bad[0] = 0;
        assert!(matches!(Frame::decode(&bad), Err(FrameError::BadMagic(_))));
    }
}

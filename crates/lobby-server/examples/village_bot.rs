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
//!
//! ```text
//! cargo run --example village_bot -- --msgdefs "<install>/bin/msgdefs.ini" --login besucher:test123 --perm 2
//!     [--lobby 127.0.0.1:17070] [--village 127.0.0.1:17071] [--usercomm 127.0.0.1:17072] [--at 810,650,967] [--radius 25] [--say text] [--every 20] [--channels 1,10]
//!     [--whisper-to 1] [--whisper text] [--mail-to 11] [--mail "Hallo|Wie geht's?"]
//! ```
//!
//! `--at` is the circle centre in the 11-bit position units of message 2000
//! (the default is where a new retail avatar stands). `--perm` is the
//! character's id from `accounts.json`; the other players see its name and
//! look, and it shows online in their friend lists while the bot runs.

use sadk_lobby_proto::auth::{self, ClientExchange, Credentials, Token};
use sadk_lobby_proto::cell::{self, Cell};
use sadk_lobby_proto::frame::{self, Frame, FrameKind, Handshake};
use sadk_lobby_proto::lobby_message::{self as lm, BitWriter, Location};
use sadk_lobby_proto::{Message, MsgDefs, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

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
            "--every" => o.every = value.parse().map_err(|e| format!("--every: {e}"))?,
            "--channels" => {
                o.channels = value.split(',').map(|c| c.trim().parse()).collect::<Result<_, _>>().map_err(|e| format!("--channels: {e}"))?
            }
            "--perm" => o.perm = value.parse().map_err(|e| format!("--perm: {e}"))?,
            "--radius" => o.radius = value.parse().map_err(|e| format!("--radius: {e}"))?,
            "--at" => {
                let v: Vec<f32> = value.split(',').map(|p| p.trim().parse()).collect::<Result<_, _>>().map_err(|e| format!("--at: {e}"))?;
                o.at = v.try_into().map_err(|_| "--at needs x,y,z".to_string())?;
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    if o.msgdefs.is_empty() || o.perm == 0 || o.login.0.is_empty() {
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
    let o = options()?;
    let defs = MsgDefs::parse(&std::fs::read_to_string(&o.msgdefs).map_err(|e| format!("{}: {e}", o.msgdefs))?)?;
    // The lobby connection stays open: the session key lives as long as it.
    let mut lobby = Connection::lobby_login(&defs, &o.lobby, &o.login.0, &o.login.1)?;
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
    std::thread::spawn(move || {
        while let Ok(frame) = read(&mut reader, &mut buf) {
            match Message::decode(&defs_r, &frame.payload) {
                Ok(m) if m.name == "SendGameData" => {
                    let (_, _, id) = lm::split_header(m.int("msg_type").unwrap_or(0) as u32);
                    if id != lm::id::AVATAR_DATA {
                        println!("<- village message {id}: {:02x?}", m.bytes("data").unwrap_or_default());
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

    let start = Instant::now();
    let mut next_line = 3.0;
    loop {
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
            zone: 0,
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

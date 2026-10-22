//! One client connection: handshake, then request/response on lobby messages.

use crate::accounts::{Account, Character, List, LoginError};
use crate::village::Event;
use crate::{Role, Server};
use sadk_lobby_proto::auth::{self, Credentials};
use sadk_lobby_proto::cell::{self, Cell, ChannelData};
use sadk_lobby_proto::frame::{self, Frame, FrameKind, Handshake};
use sadk_lobby_proto::lobby_message as lm;
use sadk_lobby_proto::minigame as mg;
use sadk_lobby_proto::{Message, Value};
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

mod matches;
mod referee;
mod social;

/// Result codes the client knows (from the original server's behaviour).
mod code {
    pub const OK: i64 = 0;
    pub const NICK_TAKEN: i64 = 0x29;
    pub const AUTH_FAILED: i64 = 0x3d;
}

/// Wrong passwords a lobby connection may try before it is closed.
const MAX_FAILED_LOGINS: u32 = 5;
/// Server id of our village in the server list and in ConnectionData.
const VILLAGE_SERVER_ID: u32 = 1;
/// Chat command that sets the address a player's matches are listed with.
const MATCH_ADDRESS_COMMAND: &str = "!adresse";

/// Village protocol number the retail client (build 34688) accepts; the demo
/// expects 999. Sent in the GameServerData data block.
const VILLAGE_PROTOCOL: u32 = 1000;
/// Name shown for our one lobby village.
const VILLAGE_NAME: &str = "Siedlerdorf";
/// Chat channel of the whole village (world welcome zone 255), shown in the
/// chat's "GLOBAL" tab.
const VILLAGE_CHANNEL: u32 = 1;
/// Zones 0 to 13 each get a local chat channel, `LOCAL_CHANNELS + zone`,
/// shown in the "LOKAL" tab, which the chat opens on. The client joins the
/// channel of the zone its avatar is in (0 so far); without one it asks for
/// channel 0 and its LOKAL tab stays empty.
const LOCAL_CHANNELS: u32 = 10;
const LOCAL_ZONES: u32 = 14;

/// (zone, chat channel) pairs announced in the world welcome.
fn chat_channels() -> impl Iterator<Item = (u32, u32)> {
    std::iter::once((255, VILLAGE_CHANNEL)).chain((0..LOCAL_ZONES).map(|zone| (zone, LOCAL_CHANNELS + zone)))
}

/// Who a token login stands for: the owning account and the id and name the
/// client goes by in the village and the chat (its character's).
struct Identity {
    account: u32,
    id: u32,
    name: String,
}

pub struct Session {
    server: Arc<Server>,
    id: u32,
    role: Role,
    peer: String,
    /// Logged-in identity: the account (perm id, nick) on the lobby
    /// connection, the character (id, name) on usercomm and the village.
    user: Option<(u32, String)>,
    /// Perm id of the account behind `user`.
    account: Option<u32>,
    /// Payload magic of this client's protocol revision, echoed in replies.
    magic: u16,
    /// Twofish key agreed in StartAuthenticateSession.
    auth_key: Option<[u8; auth::KEY_LEN]>,
    /// Village message ids already reported as unhandled.
    seen_village_ids: HashSet<u16>,
    /// News about other avatars (village sessions only); `run` takes the
    /// receiver.
    events_tx: UnboundedSender<Event>,
    events: Option<UnboundedReceiver<Event>>,
    /// Avatar (character id) this village session plays, once logged in.
    avatar: Option<u32>,
    /// Tick of the client's last message 2000 and when it arrived, to stamp
    /// relayed positions with this client's clock.
    clock: Option<(u16, Instant)>,
    /// The client's address and the server address it connected to (see
    /// `crate::address`); unset in unit tests, which then use the listen
    /// address.
    peer_ip: Option<IpAddr>,
    local_ip: Option<IpAddr>,
    /// Nonce of this connection's AckValidateTokenSession, which its token
    /// must carry.
    token_nonce: Option<Vec<u8>>,
    /// Wrong passwords on this connection so far.
    failed_logins: u32,
    /// Set to close the connection once the queued replies are sent.
    closing: bool,
    out: Vec<u8>,
}

impl Session {
    pub fn new(server: Arc<Server>, id: u32, role: Role, peer: String) -> Self {
        let (events_tx, events) = unbounded_channel();
        Self {
            server,
            id,
            role,
            peer,
            user: None,
            account: None,
            magic: sadk_lobby_proto::message::RETAIL_MAGIC,
            auth_key: None,
            seen_village_ids: HashSet::new(),
            events_tx,
            events: Some(events),
            avatar: None,
            clock: None,
            peer_ip: None,
            local_ip: None,
            token_nonce: None,
            failed_logins: 0,
            closing: false,
            out: Vec::new(),
        }
    }

    /// Records where the client connects from and which of the server's
    /// addresses it reached.
    pub fn reached_at(mut self, peer: IpAddr, local: IpAddr) -> Self {
        self.peer_ip = Some(peer);
        self.local_ip = Some(local).filter(|ip| !ip.is_unspecified());
        self
    }

    /// The server's address as this client should use it (village,
    /// usercomm, referee); see `crate::address`.
    fn server_ip(&self) -> IpAddr {
        let local = self.local_ip.unwrap_or_else(|| self.server.village.ip());
        let public = *self.server.public_ip.read().unwrap();
        match self.peer_ip {
            Some(peer) => crate::address::server_for(peer, local, public),
            None => local,
        }
    }

    /// A match host's address as this client should use it; one the host
    /// set itself (`explicit`, see `match_address_command`) stays as it is.
    fn match_host_ip(&self, host: &str, explicit: bool) -> String {
        let (Ok(host_ip), Some(peer), false) = (host.parse::<IpAddr>(), self.peer_ip, explicit) else {
            return host.to_string();
        };
        let public = *self.server.public_ip.read().unwrap();
        crate::address::host_for(host_ip, peer, self.server_ip(), public).to_string()
    }

    pub async fn run(mut self, mut stream: TcpStream) -> std::io::Result<()> {
        let result = self.serve(&mut stream).await;
        // Off the table first: clients must not keep a seat for an avatar
        // they were told is gone.
        if self.role == Role::Village {
            self.server.tables.lock().unwrap().unwatch(self.id);
        }
        if self.avatar.is_some() {
            self.server.world.lock().unwrap().leave(self.id);
        }
        if self.role == Role::Usercomm {
            self.leave_all_channels();
        }
        if self.role == Role::Lobby {
            self.leave_matches();
            self.server.lobby_users.lock().unwrap().log_out(self.id);
            self.server.tokens.lock().unwrap().lobby_closed(self.id, Instant::now());
        }
        if self.role == Role::Referee {
            self.server.referee_games.lock().unwrap().leave(self.id);
        }
        result
    }

    async fn serve(&mut self, stream: &mut TcpStream) -> std::io::Result<()> {
        let mut events = self.events.take().expect("session runs once");
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        loop {
            tokio::select! {
                n = stream.read(&mut chunk) => {
                    let n = n?;
                    if n == 0 {
                        return Ok(());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    loop {
                        match Frame::decode(&buf) {
                            Ok(Some((frame, used))) => {
                                buf.drain(..used);
                                self.handle_frame(frame);
                            }
                            Ok(None) => break,
                            Err(e) => {
                                warn!(conn = self.id, "{e}; closing");
                                return Ok(());
                            }
                        }
                    }
                }
                Some(event) = events.recv() => self.handle_event(event),
            }
            if !self.out.is_empty() {
                stream.write_all(&self.out).await?;
                self.out.clear();
            }
            if self.closing {
                warn!(conn = self.id, peer = %self.peer, "closing the connection");
                return Ok(());
            }
        }
    }

    fn handle_frame(&mut self, frame: Frame) {
        if !frame.checksum_ok {
            warn!(conn = self.id, "frame checksum mismatch (kept)");
        }
        match frame.kind {
            FrameKind::Connect => match Handshake::decode(&frame.payload) {
                Some(hs) => {
                    info!(conn = self.id, role = ?self.role, peer = %self.peer, "handshake");
                    let reply = hs.accept(self.id).encode();
                    self.out.extend(Frame::new(frame::SERVER, self.id, FrameKind::Connected, reply).encode());
                    if self.role == Role::Usercomm {
                        // The original chat server listed its channels right away.
                        for (zone, cell) in chat_channels() {
                            let name = if zone == 255 { VILLAGE_NAME.to_string() } else { format!("{VILLAGE_NAME} {zone}") };
                            let channel = ChannelData::public(&name, "Lobby-Dorf");
                            self.send_cell(Cell::ChannelInfo { channel, cell, ticket: 0 });
                        }
                    }
                }
                None => warn!(conn = self.id, len = frame.payload.len(), "malformed handshake"),
            },
            FrameKind::Application if Cell::is_cell(&frame.payload) => match Cell::decode(&frame.payload) {
                Ok(cell) => {
                    debug!(conn = self.id, "<- cell {}", hex_preview(&frame.payload));
                    self.handle_cell(cell);
                }
                Err(e) => warn!(conn = self.id, "undecodable cell message ({e}): {}", hex_preview(&frame.payload)),
            },
            FrameKind::Application => match Message::decode(&self.server.defs, &frame.payload) {
                Ok(msg) => {
                    if msg.magic != self.magic {
                        info!(conn = self.id, "client protocol magic {:#06x}", msg.magic);
                        self.magic = msg.magic;
                    }
                    if msg.name == "SendGameData" {
                        // Village traffic, several messages a second per avatar.
                        debug!(conn = self.id, "<- {msg}");
                    } else {
                        info!(conn = self.id, "<- {msg}");
                    }
                    self.handle_message(msg);
                }
                Err(e) => warn!(conn = self.id, "undecodable payload ({e}): {}", hex_preview(&frame.payload)),
            },
            FrameKind::Ping => debug!(conn = self.id, "ping"),
            other => warn!(conn = self.id, ?other, "unknown frame kind: {}", hex_preview(&frame.payload)),
        }
    }

    fn send(&mut self, mut msg: Message) {
        msg.magic = self.magic;
        if msg.name == "GameServerData" && msg.int("server_type") == Some(crate::games::MATCH_SERVER_TYPE) {
            // A match is listed with its host's address as this client can reach it.
            let id = msg.int("server_id").unwrap_or(0) as u32;
            let explicit = self.server.games.lock().unwrap().get(id).is_some_and(|g| g.explicit_address);
            let ip = self.match_host_ip(&msg.string("ip").unwrap_or_default(), explicit);
            msg.set("ip", Value::text(&ip));
        }
        info!(conn = self.id, "-> {msg}");
        let payload = msg.encode(&self.server.defs);
        self.out.extend(Frame::new(frame::SERVER, self.id, FrameKind::Application, payload).encode());
    }

    fn message(&self, name: &str) -> Message {
        Message::new(&self.server.defs, name).unwrap_or_else(|| panic!("{name} missing from msgdefs"))
    }

    fn result(&mut self, ticket: i64, errorcode: i64, text: Option<&str>) {
        let mut msg = self.message("Result").with("errorcode", Value::Int(errorcode)).with("ticket_id", Value::Int(ticket));
        if let Some(text) = text {
            msg.set("errormsg", Value::text(text));
        }
        self.send(msg);
    }

    fn handle_message(&mut self, msg: Message) {
        let ticket = msg.int("ticket_id").unwrap_or(0);
        // Village, usercomm and referee serve only clients that passed the
        // token login.
        let login = matches!(msg.name.as_str(), "CheckVersion" | "StartValidateTokenSession" | "SendToken" | "TANLogin");
        if self.role != Role::Lobby && self.user.is_none() && !login {
            warn!(conn = self.id, "{} before the token login (refused)", msg.name);
            if msg.get("ticket_id").is_some() {
                self.result(ticket, code::AUTH_FAILED, Some("Not logged in"));
            }
            return;
        }
        if self.match_message(&msg, ticket) {
            return;
        }
        match msg.name.as_str() {
            "CheckVersion" => self.result(ticket, code::OK, None),
            "RequestCreateAccount" => self.create_account(&msg, ticket),
            "RequestLogin" => self.login(&msg, ticket),
            "StartAuthenticateSession" => self.start_auth(&msg, ticket),
            "AuthenticateUser" => self.authenticate(&msg, ticket, false),
            "SelfRegistration" => self.authenticate(&msg, ticket, true),
            "PropertyGet" | "PropertySetAbsolute" | "PropertySetRelative" => self.property(&msg, ticket),
            "AddCharacter" => self.add_character(&msg, ticket),
            "RequestUserCharList" => self.character_list(&msg, ticket),
            "RequestCharacters" => self.find_character(&msg, ticket),
            "ChangeCharacter" | "RemoveCharacter" => self.edit_character(&msg, ticket),
            "RegObserverServerList" => self.server_list(&msg, ticket),
            "AssignServer" => self.assign_server(&msg, ticket),
            "StartValidateTokenSession" => {
                let nonce = auth::new_session_key().map(|k| k[..16].to_vec()).unwrap_or_default();
                self.token_nonce = Some(nonce.clone());
                let ack = self.message("AckValidateTokenSession").with("nonce", Value::Bytes(Some(nonce))).with("ticket_id", Value::Int(ticket));
                self.send(ack);
            }
            "SendToken" => self.validate_token(&msg, ticket),
            "TANLogin" => self.validate_token(&msg, ticket),
            "RequestConnectionData" => self.connection_data(&msg, ticket),
            "RequestJoinChannel" => self.join_channel(msg.int("cell_id").unwrap_or(0) as u32, ticket as u32),
            "RequestLeaveChannel" => self.leave_channel(msg.int("cell_id").unwrap_or(0) as u32, ticket as u32),
            "SendGameData" => {
                let data = msg.bytes("data").unwrap_or_default().to_vec();
                self.game_data(msg.int("msg_type").unwrap_or(0) as u32, &data);
            }
            "RequestUserBuddyList" => self.send_list(List::Buddies, ticket),
            "RequestUserIgnoreList" => self.send_list(List::Ignored, ticket),
            "AddUserBuddy" | "RemoveUserBuddy" | "AddUserIgnore" | "RemoveUserIgnore" => self.edit_list(&msg, ticket),
            "RegObserverBuddylist" | "DeregObserverBuddylist" => {
                // Friend news goes to every lobby connection anyway.
                let reply = self.message("AddResult").with("errorcode", Value::Int(code::OK)).with("id", Value::Int(0)).with("ticket_id", Value::Int(ticket));
                self.send(reply);
            }
            "RequestPrivateMessageList" => self.mail_list(&msg, ticket),
            "AddPrivateMessage" => self.send_mail(&msg, ticket),
            "ChangePrivateMessage" | "RemovePrivateMessage" => self.edit_mail(&msg, ticket),
            "RequestMOTD" => {
                let text = self.server.motd.replace("%name%", self.user.as_ref().map_or("", |u| u.1.as_str()));
                let motd = self.message("MOTD").with("txt", Value::text(&text)).with("ticket_id", Value::Int(ticket));
                self.send(motd);
            }
            // Unknown requests still get an OK so the client does not hang;
            // the warning lists what to implement next.
            _ if msg.get("ticket_id").is_some() => {
                warn!(conn = self.id, "unhandled {} (answered with OK)", msg.name);
                self.result(ticket, code::OK, None);
            }
            _ => warn!(conn = self.id, "unhandled {}", msg.name),
        }
    }

    /// RegObserverServerList: type 4 lists the lobby villages ("Lobby-Dorf"),
    /// type 5 the open games. We host one village ourselves.
    fn server_list(&mut self, msg: &Message, ticket: i64) {
        if msg.int("server_type") == Some(4) {
            let port = self.server.village.port();
            let village = self
                .message("GameServerData")
                .with("server_id", Value::Int(VILLAGE_SERVER_ID as i64))
                .with("name", Value::text(VILLAGE_NAME))
                .with("description", Value::text("Lokales Lobby-Dorf"))
                .with("ip", Value::text(&self.server_ip().to_string()))
                .with("port", Value::Int(port as i64))
                .with("server_type", Value::Int(4))
                .with("server_subtype", Value::Int(2))
                .with("max_players", Value::Int(100))
                .with("cur_players", Value::Int(0))
                .with("running", Value::Bool(true))
                .with("data", Value::Bytes(Some(lm::village_info(VILLAGE_PROTOCOL, true))))
                .with("ticket_id", Value::Int(ticket));
            self.send(village);
        }
        self.result(ticket, code::OK, None);
    }

    /// AssignServer: the client asks where its usercomm (chat / lobby world)
    /// server is. We are that server ourselves.
    fn assign_server(&mut self, msg: &Message, ticket: i64) {
        let port = self.server.usercomm.port();
        let data = self
            .message("UsercommServerData")
            .with("server_id", Value::Int(1))
            .with("ip", Value::text(&self.server_ip().to_string()))
            .with("port", Value::Int(port as i64))
            .with("server_type", Value::Int(msg.int("server_type").unwrap_or(0)))
            .with("ticket_id", Value::Int(ticket));
        self.send(data);
        if msg.int("server_type") == Some(4) && msg.int("server_subtype") == Some(4) {
            self.assign_referee(ticket);
        }
        self.result(ticket, code::OK, None);
    }

    /// SendToken (usercomm, village, referee) / TANLogin on a fresh
    /// connection: the client proves it is logged in. Its `perm_id` is the id
    /// of the character picked in "Charakterauswahl", so this is where the
    /// server learns which avatar the player uses (see `token_identity`).
    /// tincat only treats the login as done on an `AddResult` carrying that
    /// id; a plain `Result` for this ticket is ignored. The token must check
    /// out (see `check_token`).
    fn validate_token(&mut self, msg: &Message, ticket: i64) {
        let id = msg.int("perm_id").unwrap_or(0) as u32;
        let identity = self.token_identity(id).ok_or("Unknown user").and_then(|who| self.check_token(msg, &who).map(|()| who));
        let mut reply = self.message("AddResult").with("ticket_id", Value::Int(ticket));
        match identity {
            Ok(Identity { account, id: me, name }) => {
                info!(conn = self.id, id, account, "{}: {name}", msg.name);
                if self.role == Role::Usercomm {
                    self.server.chat.lock().unwrap().log_in(self.id, me, &name, self.events_tx.clone());
                }
                self.user = Some((me, name));
                self.account = Some(account);
                reply.set("errorcode", Value::Int(code::OK));
                reply.set("id", Value::Int(id as i64));
                self.send(reply);
                if self.role == Role::Usercomm {
                    self.buddy_status_changed(me);
                }
                if self.role == Role::Village {
                    self.world_welcome();
                    self.enter_village(me);
                }
                if self.role == Role::Referee {
                    self.referee_login(id);
                }
                return;
            }
            Err(why) => {
                warn!(conn = self.id, id, peer = %self.peer, "{} refused: {why}", msg.name);
                reply.set("errorcode", Value::Int(code::AUTH_FAILED));
                reply.set("errormsg", Value::text(why));
            }
        }
        self.send(reply);
    }

    /// Checks a token login for `who`. `TANLogin` must bring an unused TAN
    /// the account got for this server in `ConnectionData`. `SendToken` must
    /// open with one of the account's live session keys (see
    /// `crate::tokens`) into a token that names the account (by name, and by
    /// its id or one of its characters') and carries the nonce this
    /// connection was given in `AckValidateTokenSession`; Twofish
    /// CTR has no MAC, so those fields are what proves the key was right.
    fn check_token(&mut self, msg: &Message, who: &Identity) -> Result<(), &'static str> {
        let now = Instant::now();
        if msg.name == "TANLogin" {
            let server = if self.role == Role::Referee { crate::referee::SERVER_ID } else { VILLAGE_SERVER_ID };
            let nonce = msg.bytes("nonce").unwrap_or_default();
            let ok = self.server.tokens.lock().unwrap().take_tan(who.account, server, nonce, now);
            return if ok { Ok(()) } else { Err("Invalid TAN") };
        }
        let nonce = self.token_nonce.take().ok_or("No token session")?;
        // tincat seals its perm id into the token: the account's from
        // SessionKey, until an AddResult of a token login hands it the
        // character's id (see `validate_token`), which later tokens carry.
        let (nick, ids) = {
            let accounts = self.server.accounts.lock().unwrap();
            let nick = accounts.account(who.account).map(|a| a.nick.clone()).unwrap_or_default();
            (nick, accounts.character_ids(who.account))
        };
        let keys = self.server.tokens.lock().unwrap().keys(who.account, now);
        if keys.is_empty() {
            return Err("Session expired");
        }
        let cipher = msg.bytes("cipher").unwrap_or_default();
        for key in keys {
            let Some(token) = auth::open(&key, cipher).and_then(|plain| auth::Token::parse(&plain)) else {
                continue;
            };
            let name = sadk_lobby_proto::message::latin1_to_string(&token.name);
            if (token.id != who.account && !ids.contains(&token.id)) || !name.eq_ignore_ascii_case(&nick) {
                continue;
            }
            debug!(conn = self.id, id = token.id, value = token.value, "token of {name}");
            return if token.nonce == nonce { Ok(()) } else { Err("Stale token") };
        }
        Err("Invalid token")
    }

    /// Who a token login is: the retail client sends its character's id. An
    /// account id (village_bot, or a client that has no character) stands
    /// for that account's first character, or the account itself.
    fn token_identity(&self, id: u32) -> Option<Identity> {
        let accounts = self.server.accounts.lock().unwrap();
        if let Some((owner, c)) = accounts.character(id, "") {
            return Some(Identity { account: owner.id, id: c.id, name: c.name.clone() });
        }
        let account = accounts.account(id)?;
        Some(match account.characters.first() {
            Some(c) => Identity { account: id, id: c.id, name: c.name.clone() },
            None => Identity { account: id, id, name: account.nick.clone() },
        })
    }

    /// Sends a village message (see `lobby_message`) as SendGameData.
    fn send_village(&mut self, id: u16, data: Vec<u8>) {
        info!(conn = self.id, "-> village message {id} ({} bytes)", data.len());
        let msg = self
            .message("SendGameData")
            .with("msg_type", Value::Int(lm::header(id) as i64))
            .with("data", Value::Bytes(Some(data)));
        self.send(msg);
    }

    /// `send_village` without the info lines, for the frequent position relays.
    fn send_quiet(&mut self, id: u16, data: Vec<u8>) {
        let mut msg = self
            .message("SendGameData")
            .with("msg_type", Value::Int(lm::header(id) as i64))
            .with("data", Value::Bytes(Some(data)));
        msg.magic = self.magic;
        let payload = msg.encode(&self.server.defs);
        self.out.extend(Frame::new(frame::SERVER, self.id, FrameKind::Application, payload).encode());
    }

    /// Village message 1000, right after the village login: world name, the
    /// village's perm id and its chat channels per zone (see `chat_channels`).
    /// The client labels chat lines from that perm id "[SYSTEM]", so it must
    /// not be a player's: it is the server's frame id, which also sends the
    /// channel notices.
    fn world_welcome(&mut self) {
        let channels: Vec<_> = chat_channels().collect();
        let data = lm::world_welcome(VILLAGE_NAME, frame::SERVER, &channels);
        self.send_village(lm::id::WORLD_WELCOME, data);
    }

    /// Joins the shared village as the character the client logged in with
    /// (see `token_identity`); the avatar id is the character id, which is
    /// how the client recognises its own avatar.
    fn enter_village(&mut self, char_id: u32) {
        let character = {
            let accounts = self.server.accounts.lock().unwrap();
            accounts.character(char_id, "").map(|(_, c)| (c.id, lm::Style::from_character(&c.name, &c.data)))
        };
        let Some((avatar, style)) = character.filter(|_| char_id != 0) else {
            return warn!(conn = self.id, char_id, "no such character; others will not see this avatar");
        };
        info!(conn = self.id, avatar, "entering the village as {}", style.name);
        self.avatar = Some(avatar);
        self.server.world.lock().unwrap().join(self.id, avatar, style, self.events_tx.clone());
        self.server.tables.lock().unwrap().watch(self.id, avatar, self.events_tx.clone());
    }

    /// Village message from the client: 2000 (own position) is shared with
    /// the other avatars and 3798 (ping) answered. Any other id is warned
    /// about once and logged at debug level afterwards.
    fn game_data(&mut self, msg_type: u32, data: &[u8]) {
        let (typed, channel, id) = lm::split_header(msg_type);
        if self.role == Role::Referee && !typed {
            return self.referee_message(id, data);
        }
        match id {
            lm::id::MOVE if !typed => match lm::Location::parse(data) {
                Some(location) => {
                    debug!(conn = self.id, ?location, "move");
                    self.clock = Some((location.tick, Instant::now()));
                    if self.avatar.is_some() {
                        self.server.world.lock().unwrap().moved(self.id, location);
                    }
                }
                None => warn!(conn = self.id, "short village message 2000: {}", hex_preview(data)),
            },
            lm::id::LEAVE if !typed => {
                info!(conn = self.id, "leaving the village for a match");
                self.server.tables.lock().unwrap().unwatch(self.id);
                if self.avatar.take().is_some() {
                    self.server.world.lock().unwrap().leave(self.id);
                }
                self.send_village(lm::id::LEAVE_OK, lm::single_u32(lm::LEAVE_OK_CODE));
            }
            lm::id::PING if !typed => {
                let code = lm::BitReader::new(data).u32().unwrap_or(0);
                debug!(conn = self.id, "village ping {code:#010x}");
                self.send_village(lm::id::PONG, lm::single_u32(code));
            }
            mg::id::CREATE if !typed => self.create_table(data),
            mg::id::JOIN | mg::id::LEAVE | mg::id::REBUY | 220 | 221 | 300..=302 | 400..=403 if !typed && channel == 5 => self.table_message(id, data),
            _ if self.seen_village_ids.insert(id) => {
                warn!(conn = self.id, typed, channel, "unhandled village message {id}: {}", hex_preview(data));
            }
            _ => debug!(conn = self.id, "village message {id}: {}", hex_preview(data)),
        }
    }

    /// Message 2001 from the matchmaking dialog: open a table and sit down.
    /// A refusal is answered with 219 ("Kein Tisch mehr frei"): the client
    /// holds the avatar still until it hears either way.
    fn create_table(&mut self, data: &[u8]) {
        let result = mg::CreateTable::parse(data).ok_or(None).and_then(|c| {
            let result = self.server.tables.lock().unwrap().create(self.id, &c);
            result.map(|key| (key, c)).map_err(Some)
        });
        match result {
            Ok((key, c)) => info!(conn = self.id, ?key, "table {:?} opened: game {}, tavern {}, table {}, {} seats", c.name, c.kind, c.tavern, c.table, c.max_players),
            Err(why) => {
                warn!(conn = self.id, ?why, "table not opened: {}", hex_preview(data));
                self.send_village(mg::id::NO_TABLE_LEFT, Vec::new());
            }
        }
    }

    /// A table action on channel 5: join (209), leave (204), chips (222)
    /// or a game move. A refused join is answered with 220 ("Tisch voll").
    fn table_message(&mut self, id: u16, data: &[u8]) {
        let mut tables = self.server.tables.lock().unwrap();
        match id {
            mg::id::JOIN => {
                let result = mg::Join::parse(data).ok_or(None).and_then(|j| tables.join(self.id, &j).map(|seat| (j, seat)).map_err(Some));
                drop(tables);
                match result {
                    Ok((j, seat)) => info!(conn = self.id, key = ?j.key, seat, "sat down at a table"),
                    Err(why) => {
                        warn!(conn = self.id, ?why, "not seated: {}", hex_preview(data));
                        self.send_village(mg::id::TABLE_FULL, Vec::new());
                    }
                }
            }
            mg::id::LEAVE => match tables.leave(self.id) {
                Some(key) => info!(conn = self.id, ?key, "stood up from a table"),
                None => debug!(conn = self.id, "table leave without a seat"),
            },
            _ => match mg::action(data) {
                Some((key, mut r)) => match tables.act(self.id, key, id, &mut r) {
                    Ok(()) => debug!(conn = self.id, ?key, "table action {id}: {}", hex_preview(data)),
                    Err(why) => warn!(conn = self.id, ?key, ?why, "table action {id} refused: {}", hex_preview(data)),
                },
                None => warn!(conn = self.id, "short table action {id}: {}", hex_preview(data)),
            },
        }
    }

    /// Another avatar appeared, moved or left: tell this client.
    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Appeared { avatar, style, location } => {
                info!(conn = self.id, avatar, "{} is in the village", style.name);
                self.send_village(lm::id::AVATAR_LOGIN, lm::single_u32(avatar));
                let location = self.local_time(location);
                self.send_village(lm::id::AVATAR_DATA, lm::avatar_data(avatar, Some(&location), Some(&style)));
            }
            Event::Moved { avatar, location } => {
                let location = self.local_time(location);
                let data = lm::avatar_data(avatar, Some(&location), None);
                debug!(conn = self.id, "-> village message {} for avatar {avatar}", lm::id::AVATAR_DATA);
                self.send_quiet(lm::id::AVATAR_DATA, data);
            }
            Event::Left { avatar } => {
                info!(conn = self.id, avatar, "avatar left the village");
                self.send_village(lm::id::AVATAR_LOGOUT, lm::single_u32(avatar));
            }
            Event::Cell(cell) => self.send_cell(cell),
            Event::Lobby(msg) => self.send(msg),
            Event::Village { id, data } => self.send_village(id, data),
        }
    }

    fn send_cell(&mut self, cell: Cell) {
        match &cell {
            Cell::Deliver { message, data, cell: c, from, .. } => {
                let shown = Message::decode_propset(&self.server.defs, data).map_or_else(|_| format!("message {message}"), |m| m.to_string());
                info!(conn = self.id, "-> cell {c} from {from:#x}: {shown}");
            }
            other => info!(conn = self.id, "-> cell {other:?}"),
        }
        self.out.extend(Frame::new(frame::SERVER, self.id, FrameKind::Application, cell.encode()).encode());
    }

    /// A property set for a cell message.
    fn propset(&self, name: &str, fields: &[(&str, Value)]) -> Vec<u8> {
        let msg = fields.iter().fold(self.message(name), |m, (f, v)| m.with(f, v.clone()));
        msg.encode_propset(&self.server.defs)
    }

    /// The server's notice that a user joined or left a channel, as the
    /// original server sent it: from the server, outer cell 0.
    fn channel_notice(&self, name: &str, cell: u32, perm: u32, nick: Option<&str>) -> Cell {
        let mut fields = vec![("perm_id", Value::Int(perm as i64)), ("cell_id", Value::Int(cell as i64))];
        if let Some(nick) = nick {
            fields.push(("nick", Value::text(nick)));
        }
        let message = self.server.defs.by_name(name).map_or(0, |d| d.id);
        Cell::Deliver { message, data: self.propset(name, &fields), cell: 0, from: frame::SERVER, propset: true }
    }

    /// RequestJoinChannel (usercomm): the client enters a chat channel, e.g.
    /// the village channel from the world welcome. It hears ChannelJoined,
    /// the result for its ticket and who is there (itself last); the others
    /// hear about it.
    fn join_channel(&mut self, cell: u32, ticket: u32) {
        let Some((perm, nick)) = self.user.clone() else {
            warn!(conn = self.id, cell, "channel join before login");
            return self.send_cell(Cell::Status { cell, ticket, result: 1 });
        };
        let others = self.server.chat.lock().unwrap().join(cell, self.id, perm, &nick, self.events_tx.clone());
        info!(conn = self.id, cell, "{nick} joined chat channel {cell} ({} there before)", others.len());
        self.send_cell(Cell::Joined { cell, ticket: 0, option: 0 });
        self.send_cell(Cell::Status { cell, ticket, result: 0 });
        for (other, other_nick) in &others {
            let notice = self.channel_notice("UserJoinedChannel", cell, *other, Some(other_nick));
            self.send_cell(notice);
        }
        let notice = self.channel_notice("UserJoinedChannel", cell, perm, Some(&nick));
        self.send_cell(notice.clone());
        let me = self.id;
        self.server.chat.lock().unwrap().send(cell, &notice, |conn, _| conn != me);
    }

    fn leave_channel(&mut self, cell: u32, ticket: u32) {
        let left = self.server.chat.lock().unwrap().leave(cell, self.id);
        self.send_cell(Cell::Status { cell, ticket, result: 0 });
        if let Some(perm) = left {
            info!(conn = self.id, cell, "left chat channel {cell}");
            let notice = self.channel_notice("UserLeftChannel", cell, perm, None);
            self.server.chat.lock().unwrap().send(cell, &notice, |_, _| true);
        }
    }

    /// A usercomm connection closes: it leaves its channels, and its
    /// character's friends see it go offline.
    fn leave_all_channels(&mut self) {
        let me = self.server.chat.lock().unwrap().user(self.id);
        let left = self.server.chat.lock().unwrap().leave_all(self.id);
        for (cell, perm) in left {
            let notice = self.channel_notice("UserLeftChannel", cell, perm, None);
            self.server.chat.lock().unwrap().send(cell, &notice, |_, _| true);
        }
        if let Some(me) = me.filter(|&me| !self.server.chat.lock().unwrap().is_online(me)) {
            self.buddy_status_changed(me);
        }
    }

    /// A cell message from a usercomm client. Route carries a chat line
    /// (ChatMessage property set) for a channel; it goes to the channel's
    /// members as it is, from the sender's perm id. A whisper is routed the
    /// same way but goes to its receiver only (see `whisper`).
    fn handle_cell(&mut self, msg: Cell) {
        match msg {
            Cell::Route { message, data, cell, except, to_self, propset, .. } => {
                let Some((perm, nick)) = self.user.clone() else {
                    return warn!(conn = self.id, cell, "chat before login");
                };
                let line = Message::decode_propset(&self.server.defs, &data).ok().filter(|_| propset);
                match &line {
                    Some(m) => info!(conn = self.id, cell, "<- cell route from {nick}: {m}"),
                    None => info!(conn = self.id, cell, "<- cell route of message {message} ({} bytes)", data.len()),
                }
                if let Some(command) = line.as_ref().filter(|m| m.name == "ChatMessage").and_then(|m| m.string("txt")) {
                    let command = command.trim_matches(|c: char| c == '\0' || c.is_whitespace());
                    if let Some(args) = command.strip_prefix(MATCH_ADDRESS_COMMAND) {
                        return self.match_address_command(cell, args.trim());
                    }
                }
                if let Some(whisper) = line.filter(|m| m.name == "WhisperChatMessage") {
                    return self.whisper(&whisper, cell, perm, &nick, to_self);
                }
                let chat = self.server.chat.lock().unwrap();
                if !chat.is_member(cell, self.id) {
                    warn!(conn = self.id, cell, "{nick} talks in chat channel {cell} without joining it");
                }
                let deliver = Cell::Deliver { message, data, cell, from: perm, propset };
                let me = self.id;
                chat.send(cell, &deliver, |conn, member| (conn != me || to_self) && member != except);
            }
            other => warn!(conn = self.id, "unhandled cell message {other:?}"),
        }
    }

    /// A whisper: WhisperChatMessage routed to a channel, naming the
    /// receiver's id (a character's, like every chat id). tincat3 drops a
    /// delivered WhisperChatMessage, so the receiver gets it as the
    /// PrivateChatMessage it shows, from the sender's id and name; the
    /// sender gets the same when it asks for its own copy, and shows it as
    /// said to the receiver. A receiver who is offline is named to the
    /// sender in a "[SYSTEM]" line (see `system_line`); one who ignores the
    /// sender gets nothing, and the sender is not told.
    fn whisper(&mut self, whisper: &Message, cell: u32, perm: u32, nick: &str, to_self: bool) {
        let Some(line) = cell::private_line(&self.server.defs, whisper, perm, nick) else {
            return warn!(conn = self.id, "whisper without a receiver: {whisper}");
        };
        let to = line.int("to_id").unwrap_or(0) as u32;
        let deliver = Cell::Deliver { message: line.id, data: line.encode_propset(&self.server.defs), cell, from: perm, propset: true };
        if self.server.accounts.lock().unwrap().ignores(to, perm) {
            info!(conn = self.id, to, "{nick} whispers to {to}, who ignores them");
        } else if self.server.chat.lock().unwrap().send_to_user(to, &deliver) == 0 {
            info!(conn = self.id, to, "{nick} whispers to {to}, who is not on usercomm");
            let text = match self.token_identity(to) {
                Some(who) => format!("{} ist nicht online.", who.name),
                None => "Diesen Spieler gibt es nicht.".to_string(),
            };
            return self.system_line(cell, &text);
        }
        if to_self && to != perm {
            self.send_cell(deliver);
        }
    }

    /// `!adresse [ip[:port] | aus]` in the chat: shows or sets the address
    /// other players get for the matches this player hosts, for hosts the
    /// lobby sees at another address than the one that works (a VPN, a
    /// tunnel; see docs/PLAYING-ONLINE.md). Only the speaker sees the answer.
    fn match_address_command(&mut self, cell: u32, args: &str) {
        let Some(account) = self.account else { return };
        let reply = if args.is_empty() {
            let set = self.server.accounts.lock().unwrap().account(account).and_then(|a| a.match_address.clone());
            match set {
                Some(address) => format!("Deine Spiele werden mit {address} gelistet. \"{MATCH_ADDRESS_COMMAND} aus\" nimmt das zurück."),
                None => format!("Deine Spiele werden mit der Adresse gelistet, die der Server sieht. \"{MATCH_ADDRESS_COMMAND} <IP>[:Port]\" ändert das."),
            }
        } else {
            let address = match args {
                "aus" | "off" => Ok(None),
                text => crate::address::parse_match_address(text).map(|(ip, port)| Some(port.map_or(ip.clone(), |p| format!("{ip}:{p}")))).ok_or(()),
            };
            match address {
                Ok(address) => match self.server.accounts.lock().unwrap().set_match_address(account, address.clone()) {
                    Ok(_) => {
                        info!(conn = self.id, account, ?address, "match address set");
                        match address {
                            Some(a) => format!("Deine nächsten Spiele werden mit {a} gelistet."),
                            None => "Deine nächsten Spiele werden mit der Adresse gelistet, die der Server sieht.".to_string(),
                        }
                    }
                    Err(e) => {
                        warn!(conn = self.id, "saving accounts failed: {e}");
                        "Das hat nicht geklappt (Serverfehler).".to_string()
                    }
                },
                Err(()) => format!("Das ist keine IPv4-Adresse: \"{args}\" (Beispiel: {MATCH_ADDRESS_COMMAND} 100.101.1.2 oder 203.0.113.7:24680)."),
            }
        };
        self.system_line(cell, &reply);
    }

    /// A chat line in `cell` from the server to this client alone; the
    /// client labels lines from the server's id "[SYSTEM]".
    fn system_line(&mut self, cell: u32, text: &str) {
        let line = self.propset("ChatMessage", &[("mode", Value::Int(0)), ("txt", Value::text(text)), ("from_id", Value::Int(frame::SERVER as i64))]);
        let message = self.server.defs.by_name("ChatMessage").map_or(2, |d| d.id);
        self.send_cell(Cell::Deliver { message, data: line, cell, from: frame::SERVER, propset: true });
    }

    /// Restamps a relayed position with this client's own clock (its last
    /// tick plus the time since), so the client applies it now. Clients that
    /// have not sent a tick yet get the sender's.
    fn local_time(&self, mut location: lm::Location) -> lm::Location {
        if let Some((tick, at)) = self.clock {
            location.tick = tick.wrapping_add(at.elapsed().as_millis() as u16) | 1;
        }
        location
    }

    /// RequestConnectionData: the client asks how to reach a server from the
    /// list (our village, or the referee) and gets its address plus a
    /// one-time nonce, the TAN for `TANLogin` there.
    fn connection_data(&mut self, msg: &Message, ticket: i64) {
        let Some(account) = self.account else {
            return self.result(ticket, code::AUTH_FAILED, Some("Not logged in"));
        };
        let referee = msg.int("server_id") == Some(crate::referee::SERVER_ID as i64);
        let (server, port) = if referee { (crate::referee::SERVER_ID, self.server.referee.port()) } else { (VILLAGE_SERVER_ID, self.server.village.port()) };
        let nonce = self.server.tokens.lock().unwrap().issue_tan(account, server, Instant::now()).unwrap_or_default();
        let reply = self
            .message("ConnectionData")
            .with("perm_id", Value::Int(msg.int("perm_id").unwrap_or(0)))
            .with("server_id", Value::Int(msg.int("server_id").unwrap_or(0)))
            .with("ip", Value::text(&self.server_ip().to_string()))
            .with("port", Value::Int(port as i64))
            .with("nonce", Value::Bytes(Some(nonce)))
            .with("errorcode", Value::Int(code::OK))
            .with("ticket_id", Value::Int(ticket));
        self.send(reply);
    }

    fn start_auth(&mut self, msg: &Message, ticket: i64) {
        let key = msg.bytes("key").unwrap_or_default();
        debug!(conn = self.id, "client key {}", hex_preview(key));
        match auth::acknowledge(key) {
            Ok((cipher, k)) => {
                self.auth_key = Some(k);
                let ack = self
                    .message("AckAuthenticateSession")
                    .with("cipher", Value::Bytes(Some(cipher)))
                    .with("ticket_id", Value::Int(ticket));
                self.send(ack);
            }
            Err(e) => {
                warn!(conn = self.id, "key exchange failed: {e}");
                self.result(ticket, code::AUTH_FAILED, Some("Key exchange failed"));
            }
        }
    }

    /// AuthenticateUser / SelfRegistration: encrypted name and password. Logging
    /// in with an unknown name creates the account (the client's own
    /// registration button opened a web page on the original service).
    fn authenticate(&mut self, msg: &Message, ticket: i64, registration: bool) {
        let Some(key) = self.auth_key else {
            return self.result(ticket, code::AUTH_FAILED, Some("No key exchange"));
        };
        let creds = auth::open(&key, msg.bytes("cipher").unwrap_or_default())
            .ok_or(auth::AuthError::BadCredentials)
            .and_then(|plain| Credentials::parse(&plain, registration));
        let creds = match creds {
            Ok(c) => c,
            Err(e) => {
                warn!(conn = self.id, "credentials: {e}");
                return self.result(ticket, code::AUTH_FAILED, Some("Encryption failure"));
            }
        };
        let nick = sadk_lobby_proto::message::latin1_to_string(&creds.name);
        if nick.trim().is_empty() || creds.password.is_empty() {
            return self.result(ticket, code::AUTH_FAILED, Some("Empty name or password"));
        }
        let outcome = {
            let mut accounts = self.server.accounts.lock().unwrap();
            match accounts.login(&nick, &creds.password) {
                Ok(a) if !registration => Ok((a.id, a.nick.clone(), false)),
                Ok(_) => Err((code::NICK_TAKEN, "Name already in use")),
                Err(LoginError::WrongPassword) if registration => Err((code::NICK_TAKEN, "Name already in use")),
                Err(LoginError::WrongPassword) => Err((code::AUTH_FAILED, "Wrong password")),
                Err(LoginError::UnknownUser) => match accounts.create(&nick, &creds.password) {
                    Ok(Some(a)) => Ok((a.id, a.nick, true)),
                    Ok(None) => Err((code::NICK_TAKEN, "Name already in use")),
                    Err(e) => {
                        warn!(conn = self.id, "saving accounts failed: {e}");
                        Err((code::AUTH_FAILED, "Server error"))
                    }
                },
            }
        };
        let (id, nick, created) = match outcome {
            Ok(user) => user,
            Err((code, text)) => return self.login_failed(ticket, code, text),
        };
        info!(conn = self.id, id, created, "authenticated: {nick}");
        self.user = Some((id, nick));
        self.account = Some(id);
        self.lobby_login(id);
        let sealed = self.server.tokens.lock().unwrap().issue(id, self.id).and_then(|session| auth::seal(&key, &session));
        match sealed {
            Ok(cipher) => {
                let reply = self
                    .message("SessionKey")
                    .with("perm_id", Value::Int(id as i64))
                    .with("cipher", Value::Bytes(Some(cipher)))
                    .with("ticket_id", Value::Int(ticket));
                self.send(reply);
            }
            Err(e) => {
                warn!(conn = self.id, "session key: {e}");
                self.result(ticket, code::AUTH_FAILED, Some("Server error"));
            }
        }
    }

    /// A refused login. After `MAX_FAILED_LOGINS` wrong passwords the
    /// connection is closed, so guessing takes a new key exchange each time.
    fn login_failed(&mut self, ticket: i64, errorcode: i64, text: &str) {
        self.result(ticket, errorcode, Some(text));
        if errorcode == code::AUTH_FAILED {
            self.failed_logins += 1;
            warn!(conn = self.id, peer = %self.peer, failed = self.failed_logins, "login refused: {text}");
            self.closing |= self.failed_logins >= MAX_FAILED_LOGINS;
        }
    }

    fn property(&mut self, msg: &Message, ticket: i64) {
        let (kategory, index) = (msg.int("kategory").unwrap_or(0) as i32, msg.int("index").unwrap_or(0) as i32);
        let value = match (&self.user, msg.name.as_str()) {
            (None, _) => 0,
            (Some((id, _)), "PropertyGet") => self.server.accounts.lock().unwrap().property(*id, kategory, index),
            (Some((id, _)), name) => {
                let value = msg.int("value").unwrap_or(0) as i32;
                let set = self.server.accounts.lock().unwrap().set_property(*id, kategory, index, value, name == "PropertySetRelative");
                set.unwrap_or_else(|e| {
                    warn!(conn = self.id, "saving property failed: {e}");
                    0
                })
            }
        };
        let data = self
            .message("PropertyData")
            .with("kategory", Value::Int(kategory as i64))
            .with("index", Value::Int(index as i64))
            .with("value", Value::Int(value as i64))
            .with("ticket_id", Value::Int(ticket));
        self.send(data);
        self.result(ticket, code::OK, None);
    }

    fn add_character(&mut self, msg: &Message, ticket: i64) {
        let name = msg.string("name").unwrap_or_default();
        let data = msg.bytes("data").unwrap_or_default().to_vec();
        if lm::character_blocks(&data).is_none() {
            warn!(conn = self.id, "AddCharacter data does not split into six blocks: {data:02x?}");
        }
        let added = match &self.user {
            Some((id, _)) if !name.trim().is_empty() => self.server.accounts.lock().unwrap().add_character(*id, name.trim(), data),
            _ => Ok(None),
        };
        let (errorcode, text, char_id) = match added {
            Ok(Some(c)) => {
                info!(conn = self.id, char_id = c.id, "character created: {}", c.name);
                (code::OK, None, c.id)
            }
            Ok(None) => (code::NICK_TAKEN, Some("Name already in use"), 0),
            Err(e) => {
                warn!(conn = self.id, "saving character failed: {e}");
                (code::AUTH_FAILED, Some("Server error"), 0)
            }
        };
        let mut reply = self
            .message("AddResult")
            .with("errorcode", Value::Int(errorcode))
            .with("id", Value::Int(char_id as i64))
            .with("ticket_id", Value::Int(ticket));
        if let Some(text) = text {
            reply.set("errormsg", Value::text(text));
        }
        self.send(reply);
    }

    /// UserCharConn and CharacterData share their layout.
    fn character_message(&self, kind: &str, owner: &Account, c: &Character, ticket: i64) -> Message {
        self.message(kind)
            .with("char_id", Value::Int(c.id as i64))
            .with("name", Value::text(&c.name))
            .with("owner_id", Value::Int(owner.id as i64))
            .with("owner_name", Value::text(&owner.nick))
            .with("status", Value::Int(1))
            .with("data", Value::Bytes(Some(c.data.clone())))
            .with("ticket_id", Value::Int(ticket))
    }

    fn character_list(&mut self, msg: &Message, ticket: i64) {
        let user = msg.int("user_id").unwrap_or(0) as u32;
        let replies: Vec<Message> = {
            let accounts = self.server.accounts.lock().unwrap();
            match accounts.account(user) {
                Some(owner) => owner.characters.iter().map(|c| self.character_message("UserCharConn", owner, c, ticket)).collect(),
                None => Vec::new(),
            }
        };
        for reply in replies {
            self.send(reply);
        }
        self.result(ticket, code::OK, None);
    }

    fn find_character(&mut self, msg: &Message, ticket: i64) {
        let id = msg.int("char_id").unwrap_or(0) as u32;
        let name = msg.string("name").unwrap_or_default();
        let reply = {
            let accounts = self.server.accounts.lock().unwrap();
            accounts.character(id, &name).map(|(owner, c)| self.character_message("CharacterData", owner, c, ticket))
        };
        match reply {
            Some(reply) => {
                self.send(reply);
                self.result(ticket, code::OK, None);
            }
            None => self.result(ticket, code::AUTH_FAILED, Some("Unknown character")),
        }
    }

    fn edit_character(&mut self, msg: &Message, ticket: i64) {
        let Some((owner, _)) = self.user.clone() else {
            return self.result(ticket, code::AUTH_FAILED, Some("Not logged in"));
        };
        let id = msg.int("char_id").unwrap_or(0) as u32;
        let done = {
            let mut accounts = self.server.accounts.lock().unwrap();
            if msg.name == "RemoveCharacter" {
                accounts.remove_character(owner, id)
            } else {
                let name = msg.string("name");
                let data = match msg.get("data") {
                    Some(Value::Bytes(Some(b))) => Some(b.clone()),
                    _ => None,
                };
                accounts.change_character(owner, id, name.as_deref(), data)
            }
        };
        match done {
            Ok(true) => self.result(ticket, code::OK, None),
            Ok(false) => self.result(ticket, code::AUTH_FAILED, Some("Unknown character")),
            Err(e) => {
                warn!(conn = self.id, "saving character failed: {e}");
                self.result(ticket, code::AUTH_FAILED, Some("Server error"));
            }
        }
    }

    fn create_account(&mut self, msg: &Message, ticket: i64) {
        let nick = msg.string("nick").unwrap_or_default();
        let password = msg.bytes("password").unwrap_or_default().to_vec();
        if nick.trim().is_empty() {
            return self.result(ticket, code::AUTH_FAILED, Some("Empty name"));
        }
        let created = self.server.accounts.lock().unwrap().create(&nick, &password);
        match created {
            Ok(Some(account)) => {
                info!(conn = self.id, id = account.id, "account created: {nick}");
                self.account = Some(account.id);
                self.user = Some((account.id, account.nick));
                self.result(ticket, code::OK, None);
            }
            Ok(None) => self.result(ticket, code::NICK_TAKEN, Some("Name already in use")),
            Err(e) => {
                warn!(conn = self.id, "saving accounts failed: {e}");
                self.result(ticket, code::AUTH_FAILED, Some("Server error"));
            }
        }
    }

    fn login(&mut self, msg: &Message, ticket: i64) {
        let nick = msg.string("nick").unwrap_or_default();
        let password = msg.bytes("password").unwrap_or_default();
        let outcome = self.server.accounts.lock().unwrap().login(&nick, password).map(|a| (a.id, a.nick.clone()));
        match outcome {
            Ok(user) => {
                info!(conn = self.id, id = user.0, "login: {}", user.1);
                self.account = Some(user.0);
                self.user = Some(user);
                self.result(ticket, code::OK, None);
            }
            Err(LoginError::UnknownUser) => self.result(ticket, code::AUTH_FAILED, Some("Unknown user")),
            Err(LoginError::WrongPassword) => self.login_failed(ticket, code::AUTH_FAILED, "Wrong password"),
        }
    }
}

#[cfg(test)]
impl Session {
    /// Logs this village, usercomm or referee session in the way the game
    /// does: `StartValidateTokenSession`, then `SendToken` for `perm` with a
    /// token for its account, sealed with a session key the account gets
    /// now (as from a lobby login). Only the `SendToken` replies stay queued.
    pub(crate) fn token_login(&mut self, perm: i64, ticket: i64) {
        let start = self.message("StartValidateTokenSession").with("ticket_id", Value::Int(ticket));
        self.handle_message(start);
        self.out.clear();
        let nonce = self.token_nonce.clone().unwrap_or_default();
        let cipher = self.token_identity(perm as u32).map(|who| {
            let nick = self.server.accounts.lock().unwrap().account(who.account).unwrap().nick.clone();
            let key = self.server.tokens.lock().unwrap().issue(who.account, 1_000_000 + who.account).unwrap();
            let token = auth::Token { id: who.account, name: nick.into_bytes(), key_hash: vec![0; 64], value: 1, password: vec![], nonce };
            token.seal(&key).unwrap()
        });
        let login = self
            .message("SendToken")
            .with("perm_id", Value::Int(perm))
            .with("cipher", Value::Bytes(cipher))
            .with("ticket_id", Value::Int(ticket));
        self.handle_message(login);
    }
}

fn hex_preview(bytes: &[u8]) -> String {
    let shown: String = bytes.iter().take(64).map(|b| format!("{b:02x}")).collect();
    if bytes.len() > 64 { format!("{shown}... ({} bytes)", bytes.len()) } else { shown }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Accounts;

    /// The retail msgdefs.ini sections the village handlers use.
    pub(super) const DEFS: &str = r#"# ChatMessage
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
# UserLeftChannel
[NETMSG_TYPE_6]
type="UNSHORT"
perm_id="UNLONG"
cell_id="UNLONG"
# WhisperChatMessage
[NETMSG_TYPE_30]
type="UNSHORT"
mode="UNLONG"
txt="STRING 256"
cell_id="UNLONG"
from_id="UNLONG"
perm_id="UNLONG"
# RequestJoinChannel
[NETMSG_TYPE_17]
type="UNSHORT"
cell_id="UNLONG"
ticket_id="UNLONG"
option="UNSHORT"
password="STRING 32"
from_id="UNLONG"
# Result
[NETMSG_TYPE_42]
type="UNSHORT"
errorcode="UNBYTE"
errormsg="STRING 32"
ticket_id="UNLONG"
# SendGameData
[NETMSG_TYPE_74]
type="UNSHORT"
msg_type="UNLONG"
data="MEMBLOCK 0"
# AddResult
[NETMSG_TYPE_153]
type="UNSHORT"
errorcode="UNBYTE"
errormsg="STRING 32"
id="UNLONG"
ticket_id="UNLONG"
# GameServerData
[NETMSG_TYPE_170]
type="UNSHORT"
server_id="UNLONG"
name="STRING 128"
owner_id="UNLONG"
description="STRING 128"
ip="STRING 128"
port="UNLONG"
password_required="LBOOL"
server_type="UNBYTE"
server_subtype="UNBYTE"
version="STRING 32"
max_players="UNSHORT"
cur_players="UNSHORT"
max_spectators="UNSHORT"
cur_spectators="UNSHORT"
ai_players="UNSHORT"
room_id="UNLONG"
level="UNBYTE"
game_mode="UNBYTE"
hardcore="LBOOL"
map="STRING 128"
running="LBOOL"
locked_config="LBOOL"
data="MEMBLOCK 0"
ticket_id="UNLONG"
# RegObserverServerList
[NETMSG_TYPE_171]
type="UNSHORT"
send_all="LBOOL"
server_type="UNBYTE"
room_id="UNLONG"
level="UNBYTE"
game_mode="UNBYTE"
hardcore="UNBYTE"
selection="UNLONG"
ticket_id="UNLONG"
# AssignServer
[NETMSG_TYPE_189]
type="UNSHORT"
server_type="UNBYTE"
server_subtype="UNBYTE"
ticket_id="UNLONG"
# UsercommServerData
[NETMSG_TYPE_192]
type="UNSHORT"
server_id="UNLONG"
ip="STRING 128"
port="UNLONG"
server_type="UNBYTE"
version="STRING 32"
data="MEMBLOCK 0"
ticket_id="UNLONG"
# StartValidateTokenSession
[NETMSG_TYPE_211]
type="UNSHORT"
ticket_id="UNLONG"
# AckValidateTokenSession
[NETMSG_TYPE_212]
type="UNSHORT"
nonce="MEMBLOCK 0"
ticket_id="UNLONG"
# SendToken
[NETMSG_TYPE_213]
type="UNSHORT"
perm_id="UNLONG"
cipher="MEMBLOCK 0"
ticket_id="UNLONG"
# TANLogin
[NETMSG_TYPE_224]
type="UNSHORT"
perm_id="UNLONG"
nonce="MEMBLOCK 0"
ticket_id="UNLONG"
# RequestConnectionData
[NETMSG_TYPE_221]
type="UNSHORT"
perm_id="UNLONG"
server_id="UNLONG"
ticket_id="UNLONG"
# ConnectionData
[NETMSG_TYPE_222]
type="UNSHORT"
perm_id="UNLONG"
server_id="UNLONG"
ip="STRING 128"
port="UNLONG"
nonce="MEMBLOCK 0"
errorcode="UNBYTE"
errormsg="STRING 32"
ticket_id="UNLONG"
"#;

    fn server(accounts: Accounts) -> Arc<Server> {
        Server::test(DEFS, accounts)
    }

    fn accounts(name: &str) -> Accounts {
        let path = std::env::temp_dir().join(format!("sadk-session-{name}-{}", std::process::id())).join("accounts.json");
        let _ = std::fs::remove_file(&path);
        Accounts::open(path).unwrap()
    }

    fn session(role: Role, name: &str) -> (Session, u32) {
        let mut accounts = accounts(name);
        let id = accounts.create("Siedler", b"x").unwrap().unwrap().id;
        (Session::new(server(accounts), 7, role, "test".into()), id)
    }

    /// Village messages (id, data) among the queued replies.
    fn village_messages(s: &mut Session) -> Vec<(u16, Vec<u8>)> {
        sent(s)
            .into_iter()
            .filter(|m| m.name == "SendGameData")
            .map(|m| (lm::split_header(m.int("msg_type").unwrap() as u32).2, m.bytes("data").unwrap().to_vec()))
            .collect()
    }

    /// Feeds a client village message into the session.
    fn game_data(s: &mut Session, id: u16, data: Vec<u8>) {
        let msg = s.message("SendGameData").with("msg_type", Value::Int(0x2000 | id as i64)).with("data", Value::Bytes(Some(data)));
        s.handle_message(msg);
    }

    /// Hands every pending village event to the session, as `run` does.
    fn deliver(s: &mut Session) {
        let events = s.events.as_mut().unwrap();
        let pending: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        for event in pending {
            s.handle_event(event);
        }
    }

    fn request(s: &Session, name: &str, fields: &[(&str, i64)]) -> Message {
        fields.iter().fold(s.message(name), |m, &(f, v)| m.with(f, Value::Int(v)))
    }

    /// Decodes the lobby messages the session queued for the client (cell
    /// messages are dropped).
    fn sent(s: &mut Session) -> Vec<Message> {
        frames(s).into_iter().filter(|p| !Cell::is_cell(p)).map(|p| Message::decode(&s.server.defs, &p).unwrap()).collect()
    }

    /// Decodes the cell messages the session queued (lobby messages are dropped).
    fn cells(s: &mut Session) -> Vec<Cell> {
        frames(s).into_iter().filter(|p| Cell::is_cell(p)).map(|p| Cell::decode(&p).unwrap()).collect()
    }

    fn frames(s: &mut Session) -> Vec<Vec<u8>> {
        let mut out = std::mem::take(&mut s.out);
        let mut payloads = Vec::new();
        while let Some((frame, used)) = Frame::decode(&out).unwrap() {
            payloads.push(frame.payload);
            out.drain(..used);
        }
        payloads
    }

    #[test]
    fn lists_the_village() {
        let (mut s, _) = session(Role::Lobby, "list");
        s.handle_message(request(&s, "RegObserverServerList", &[("server_type", 4), ("ticket_id", 3)]));
        let out = sent(&mut s);
        assert_eq!(out.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["GameServerData", "Result"]);
        let village = &out[0];
        assert_eq!((village.int("server_type"), village.int("server_subtype")), (Some(4), Some(2)));
        assert_eq!((village.string("ip").unwrap().as_str(), village.int("port")), ("127.0.0.1", Some(17071)));
        assert_eq!(village.get("running"), Some(&Value::Bool(true)));
        assert_eq!(village.bytes("data").unwrap(), lm::village_info(VILLAGE_PROTOCOL, true));
        assert_eq!(village.int("ticket_id"), Some(3));
    }

    #[test]
    fn assigns_usercomm_and_hands_out_the_village() {
        let (mut s, id) = session(Role::Lobby, "assign");
        s.handle_message(request(&s, "RequestConnectionData", &[("perm_id", id as i64), ("server_id", 1), ("ticket_id", 2)]));
        assert_eq!(sent(&mut s)[0].int("errorcode"), Some(code::AUTH_FAILED), "not logged in");
        s.account = Some(id);
        s.handle_message(request(&s, "AssignServer", &[("server_type", 4), ("server_subtype", 4), ("ticket_id", 1)]));
        s.handle_message(request(&s, "RequestConnectionData", &[("perm_id", id as i64), ("server_id", 1), ("ticket_id", 2)]));
        let out = sent(&mut s);
        assert_eq!(out.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["UsercommServerData", "GameServerData", "Result", "ConnectionData"]);
        assert_eq!(out[0].int("port"), Some(17072));
        let connection = &out[3];
        assert_eq!((connection.int("perm_id"), connection.int("port"), connection.int("errorcode")), (Some(id as i64), Some(17071), Some(0)));
        let tan = connection.bytes("nonce").unwrap().to_vec();
        assert_eq!(tan.len(), 16);

        // The nonce is a TAN for one village login.
        let mut village = Session::new(s.server.clone(), 8, Role::Village, "test".into());
        let tan_login = |v: &Session, tan: &[u8]| v.message("TANLogin").with("perm_id", Value::Int(id as i64)).with("nonce", Value::Bytes(Some(tan.to_vec()))).with("ticket_id", Value::Int(3));
        village.handle_message(tan_login(&village, &[0; 16]));
        assert_eq!(sent(&mut village)[0].int("errorcode"), Some(code::AUTH_FAILED), "a made-up TAN");
        village.handle_message(tan_login(&village, &tan));
        assert_eq!(sent(&mut village)[0].int("errorcode"), Some(code::OK));
        let mut again = Session::new(s.server.clone(), 9, Role::Village, "test".into());
        again.handle_message(tan_login(&again, &tan));
        assert_eq!(sent(&mut again)[0].int("errorcode"), Some(code::AUTH_FAILED), "a used TAN");
    }

    #[test]
    fn village_token_login_sends_world_welcome() {
        let (mut s, id) = session(Role::Village, "village");
        s.token_login(id as i64, 5);
        let out = sent(&mut s);
        assert_eq!(out.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["AddResult", "SendGameData"]);
        assert_eq!((out[0].int("errorcode"), out[0].int("id"), out[0].int("ticket_id")), (Some(0), Some(id as i64), Some(5)));
        assert_eq!(lm::split_header(out[1].int("msg_type").unwrap() as u32), (false, 0, lm::id::WORLD_WELCOME));
        let mut welcome = lm::BitReader::new(out[1].bytes("data").unwrap());
        assert_eq!(welcome.str().as_deref(), Some(VILLAGE_NAME));
        assert_eq!((welcome.u32(), welcome.u32()), (Some(frame::SERVER), Some(1 + LOCAL_ZONES)));
        assert_eq!((welcome.u32(), welcome.u32()), (Some(255), Some(VILLAGE_CHANNEL)));
        assert_eq!((welcome.u32(), welcome.u32()), (Some(0), Some(LOCAL_CHANNELS)));
    }

    /// Two village clients: each sees the other's avatar once it reported a
    /// position, follows its moves and sees it leave.
    #[test]
    fn village_avatars_see_each_other() {
        let mut accounts = accounts("two-players");
        // One 44-byte creation block (version 1, shirt colour 3).
        let mut look = vec![0u8; 68];
        look[0] = 44;
        look[24] = 1;
        look[44] = 3;
        let (a_id, b_id) = (accounts.create("A", b"x").unwrap().unwrap().id, accounts.create("B", b"x").unwrap().unwrap().id);
        let a_char = accounts.add_character(a_id, "Ada", look).unwrap().unwrap().id;
        let b_char = accounts.add_character(b_id, "Bert", vec![0; 68]).unwrap().unwrap().id;
        let server = server(accounts);
        let mut a = Session::new(server.clone(), 1, Role::Village, "a".into());
        let mut b = Session::new(server.clone(), 2, Role::Village, "b".into());
        a.token_login(a_id as i64, 1);
        let here = lm::Location { tick: 0x0101, pos: [810, 650, 967], ghost_zone: 15, ..lm::Location::default() };
        let mut w = lm::BitWriter::new();
        here.write(&mut w);
        game_data(&mut a, lm::id::MOVE, w.finish());
        sent(&mut a);

        // B logs in later and hears about A at once.
        b.token_login(b_id as i64, 1);
        sent(&mut b);
        deliver(&mut b);
        let out = village_messages(&mut b);
        assert_eq!(out.iter().map(|m| m.0).collect::<Vec<_>>(), [lm::id::AVATAR_LOGIN, lm::id::AVATAR_DATA]);
        assert_eq!(out[0].1, lm::single_u32(a_char));
        let mut r = lm::BitReader::new(&out[1].1);
        assert_eq!((r.u32(), r.bits(4)), (Some(a_char), Some((lm::block::LOCATION | lm::block::STYLE) as u32)));
        assert_eq!(lm::Location::read(&mut r), Some(here));
        let style = lm::Style::read(&mut r).unwrap();
        assert_eq!((style.name.as_str(), style.shirt), ("Ada", 3));

        // B's first move announces B to A; A's next move reaches B stamped
        // with B's clock.
        let b_here = lm::Location { tick: 0x7001, ..here };
        let mut w = lm::BitWriter::new();
        b_here.write(&mut w);
        game_data(&mut b, lm::id::MOVE, w.finish());
        deliver(&mut a);
        let out = village_messages(&mut a);
        assert_eq!(out.iter().map(|m| m.0).collect::<Vec<_>>(), [lm::id::AVATAR_LOGIN, lm::id::AVATAR_DATA]);
        assert_eq!(out[0].1, lm::single_u32(b_char));

        let there = lm::Location { tick: 0x0201, pos: [811, 650, 967], running: true, ..here };
        let mut w = lm::BitWriter::new();
        there.write(&mut w);
        game_data(&mut a, lm::id::MOVE, w.finish());
        deliver(&mut b);
        let out = village_messages(&mut b);
        assert_eq!(out.len(), 1);
        let mut r = lm::BitReader::new(&out[0].1);
        assert_eq!((r.u32(), r.bits(4)), (Some(a_char), Some(lm::block::LOCATION as u32)));
        let relayed = lm::Location::read(&mut r).unwrap();
        assert_eq!(lm::Location { tick: there.tick, ..relayed }, there);
        assert!(relayed.tick.wrapping_sub(b_here.tick) < 1000, "tick {:#06x} not on B's clock", relayed.tick);

        // A disconnects (what `run` does on the way out).
        server.world.lock().unwrap().leave(a.id);
        deliver(&mut b);
        assert_eq!(village_messages(&mut b), [(lm::id::AVATAR_LOGOUT, lm::single_u32(a_char))]);
    }

    #[test]
    fn answers_village_pings() {
        let (mut s, id) = session(Role::Village, "ping");
        s.token_login(id as i64, 1);
        sent(&mut s);
        game_data(&mut s, lm::id::PING, vec![0x0b, 0x09, 0x48, 0xae]);
        assert_eq!(village_messages(&mut s), [(lm::id::PONG, vec![0x0b, 0x09, 0x48, 0xae])]);
    }

    /// 2002 (off to a match): the avatar leaves the world, and the village
    /// tells the client to log off (1006).
    #[test]
    fn takes_out_a_player_leaving_for_a_match() {
        let mut accounts = accounts("leave");
        let id = accounts.create("Siedler", b"x").unwrap().unwrap().id;
        accounts.add_character(id, "garibald", vec![0; 68]).unwrap();
        let mut s = Session::new(server(accounts), 7, Role::Village, "test".into());
        s.token_login(id as i64, 1);
        sent(&mut s);
        assert!(s.avatar.is_some());
        game_data(&mut s, lm::id::LEAVE, vec![0xaf, 0xfe, 0xde, 0xad]);
        assert!(s.avatar.is_none());
        assert_eq!(village_messages(&mut s), [(lm::id::LEAVE_OK, lm::single_u32(0xdeadbeef))]);
    }

    #[test]
    fn rejects_unknown_tokens_and_skips_welcome_off_village() {
        let (mut s, id) = session(Role::Usercomm, "usercomm");
        s.token_login(id as i64 + 100, 1);
        assert_eq!(sent(&mut s)[0].int("errorcode"), Some(code::AUTH_FAILED));
        s.token_login(id as i64, 2);
        let out = sent(&mut s);
        assert_eq!(out.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["AddResult"]);
        assert_eq!(out[0].int("errorcode"), Some(code::OK));
    }

    /// A token must open with a live session key of the account, name the
    /// account and carry this connection's nonce.
    #[test]
    fn token_logins_are_verified() {
        let (lobby, id) = session(Role::Lobby, "verify");
        let server = lobby.server.clone();
        let key = server.tokens.lock().unwrap().issue(id, lobby.id).unwrap();
        let login = |conn: u32, token: &auth::Token, key: &[u8; auth::KEY_LEN]| {
            let mut s = Session::new(server.clone(), conn, Role::Usercomm, "uc".into());
            s.handle_message(s.message("StartValidateTokenSession").with("ticket_id", Value::Int(1)));
            let nonce = sent(&mut s)[0].bytes("nonce").unwrap().to_vec();
            let token = auth::Token { nonce: if token.nonce.is_empty() { nonce } else { token.nonce.clone() }, ..token.clone() };
            let cipher = token.seal(key).unwrap();
            s.handle_message(s.message("SendToken").with("perm_id", Value::Int(id as i64)).with("cipher", Value::Bytes(Some(cipher))).with("ticket_id", Value::Int(2)));
            let reply = sent(&mut s).remove(0);
            (reply.int("errorcode"), reply.string("errormsg").unwrap_or_default())
        };
        let good = auth::Token { id, name: b"siedler".to_vec(), key_hash: vec![1; 64], value: 1, password: vec![], nonce: vec![] };
        assert_eq!(login(10, &good, &key), (Some(code::OK), String::new()), "names are not case sensitive");
        // After a token login the client's tokens carry the character id it was given.
        let character = server.accounts.lock().unwrap().add_character(id, "garibald", vec![]).unwrap().unwrap().id;
        assert_eq!(login(17, &auth::Token { id: character, ..good.clone() }, &key).0, Some(code::OK));

        let forged = auth::new_session_key().unwrap();
        assert_eq!(login(11, &good, &forged).1, "Invalid token");
        assert_eq!(login(12, &auth::Token { id: id + 100, ..good.clone() }, &key).1, "Invalid token");
        assert_eq!(login(13, &auth::Token { name: b"besucher".to_vec(), ..good.clone() }, &key).1, "Invalid token");
        assert_eq!(login(14, &auth::Token { nonce: vec![9; 16], ..good.clone() }, &key).1, "Stale token");

        // SendToken without StartValidateTokenSession has no nonce to match.
        let mut s = Session::new(server.clone(), 15, Role::Usercomm, "uc".into());
        s.handle_message(s.message("SendToken").with("perm_id", Value::Int(id as i64)).with("cipher", Value::Bytes(Some(good.seal(&key).unwrap()))).with("ticket_id", Value::Int(2)));
        assert_eq!(sent(&mut s)[0].string("errormsg").unwrap(), "No token session");

        // The key dies with the lobby connection, after the grace time.
        let closed = Instant::now() - crate::tokens::DEFAULT_GRACE - std::time::Duration::from_secs(1);
        server.tokens.lock().unwrap().lobby_closed(lobby.id, closed);
        assert_eq!(login(16, &good, &key).1, "Session expired");
    }

    /// Before the token login, village, usercomm and the referee answer
    /// nothing but the login itself.
    #[test]
    fn refuses_requests_before_the_token_login() {
        let (mut s, _) = session(Role::Village, "gate");
        s.handle_message(request(&s, "RegObserverServerList", &[("server_type", 4), ("ticket_id", 4)]));
        game_data(&mut s, lm::id::PING, lm::single_u32(7));
        let out = sent(&mut s);
        assert_eq!(out.iter().map(|m| (m.name.as_str(), m.int("errorcode"))).collect::<Vec<_>>(), [("Result", Some(code::AUTH_FAILED))]);
    }
    /// The property set inside a Deliver, decoded.
    fn delivered(s: &Session, cell: &Cell) -> Message {
        let Cell::Deliver { data, propset: true, .. } = cell else { panic!("not a delivery: {cell:?}") };
        Message::decode_propset(&s.server.defs, data).unwrap()
    }

    fn usercomm(server: &Arc<Server>, conn: u32, perm: u32) -> Session {
        let mut s = Session::new(server.clone(), conn, Role::Usercomm, "uc".into());
        s.token_login(perm as i64, 1);
        sent(&mut s);
        s
    }

    /// "!adresse ..." in the chat sets the player's match address; the
    /// answer goes to the speaker only, and the others hear nothing.
    #[test]
    fn match_address_chat_command() {
        let mut accounts = accounts("command");
        let (a_id, b_id) = (accounts.create("Ada", b"x").unwrap().unwrap().id, accounts.create("Bert", b"x").unwrap().unwrap().id);
        let server = server(accounts);
        let (mut a, mut b) = (usercomm(&server, 1, a_id), usercomm(&server, 2, b_id));
        for (s, id) in [(&mut a, a_id), (&mut b, b_id)] {
            s.handle_message(request(s, "RequestJoinChannel", &[("cell_id", 10), ("ticket_id", 4), ("from_id", id as i64)]));
        }
        deliver(&mut a);
        deliver(&mut b);
        cells(&mut a);
        cells(&mut b);
        let say = |a: &mut Session, text: &str| {
            let line = a.propset("ChatMessage", &[("txt", Value::text(text)), ("from_id", Value::Int(a_id as i64))]);
            let route = Cell::Route { module: 0, message: 2, except: sadk_lobby_proto::cell::NOBODY, data: line, cell: 10, to_self: true, propset: true };
            a.handle_frame(Frame::new(1, frame::SERVER, FrameKind::Application, route.encode()));
            let out = cells(a);
            assert_eq!(out.len(), 1);
            assert!(matches!(out[0], Cell::Deliver { cell: 10, from: frame::SERVER, .. }));
            delivered(a, &out[0]).string("txt").unwrap()
        };
        let address = |server: &Arc<Server>| server.accounts.lock().unwrap().account(a_id).unwrap().match_address.clone();

        assert!(say(&mut a, "!adresse 100.101.1.2:24680").contains("100.101.1.2:24680"));
        assert_eq!(address(&server).as_deref(), Some("100.101.1.2:24680"));
        assert!(say(&mut a, "!adresse").contains("100.101.1.2:24680"));
        assert!(say(&mut a, "!adresse lobby.example.org").starts_with("Das ist keine IPv4-Adresse"));
        assert_eq!(address(&server).as_deref(), Some("100.101.1.2:24680"));
        say(&mut a, "!adresse aus");
        assert_eq!(address(&server), None);
        deliver(&mut b);
        assert!(cells(&mut b).is_empty(), "nobody else hears the commands");
    }

    #[test]
    fn usercomm_lists_the_chat_channels_on_connect() {
        let (mut s, _) = session(Role::Usercomm, "uc-connect");
        let hello = Handshake { connection: frame::CLIENT_UNASSIGNED, user: [0; 32], password: *b"password", trailer: 1 };
        s.handle_frame(Frame::new(frame::CLIENT_UNASSIGNED, frame::SERVER, FrameKind::Connect, hello.encode()));
        let out = cells(&mut s);
        assert_eq!(out.len(), 1 + LOCAL_ZONES as usize);
        let Cell::ChannelInfo { channel, cell: VILLAGE_CHANNEL, ticket: 0 } = &out[0] else { panic!("{out:?}") };
        assert_eq!(channel.name, VILLAGE_NAME);
        assert!(matches!(&out[1], Cell::ChannelInfo { cell: LOCAL_CHANNELS, .. }));
    }

    /// Two usercomm clients join the village channel, see each other, chat
    /// and see the other leave.
    #[test]
    fn chat_channel_members_talk() {
        let mut accounts = accounts("chat");
        let (a_id, b_id) = (accounts.create("Ada", b"x").unwrap().unwrap().id, accounts.create("Bert", b"x").unwrap().unwrap().id);
        let server = server(accounts);
        let (mut a, mut b) = (usercomm(&server, 1, a_id), usercomm(&server, 2, b_id));

        a.handle_message(request(&a, "RequestJoinChannel", &[("cell_id", 1), ("ticket_id", 4), ("from_id", a_id as i64)]));
        let out = cells(&mut a);
        assert_eq!(out[..2], [Cell::Joined { cell: 1, ticket: 0, option: 0 }, Cell::Status { cell: 1, ticket: 4, result: 0 }]);
        assert_eq!(out.len(), 3);
        let me = delivered(&a, &out[2]);
        assert_eq!((me.name.as_str(), me.int("perm_id"), me.int("cell_id"), me.string("nick").unwrap().as_str()), ("UserJoinedChannel", Some(a_id as i64), Some(1), "Ada"));
        assert!(matches!(out[2], Cell::Deliver { cell: 0, from: frame::SERVER, .. }));

        // B hears about A, then itself; A hears about B.
        b.handle_message(request(&b, "RequestJoinChannel", &[("cell_id", 1), ("ticket_id", 2), ("from_id", b_id as i64)]));
        let out = cells(&mut b);
        let joined: Vec<_> = out[2..].iter().map(|c| delivered(&b, c).int("perm_id")).collect();
        assert_eq!(joined, [Some(a_id as i64), Some(b_id as i64)]);
        deliver(&mut a);
        let out = cells(&mut a);
        assert_eq!(out.len(), 1);
        assert_eq!(delivered(&a, &out[0]).string("nick").unwrap(), "Bert");

        // A says something; with "self" set both get it, from A's perm id.
        let line = a.propset("ChatMessage", &[("txt", Value::text("Hallo Dorf")), ("from_id", Value::Int(a_id as i64))]);
        let route = Cell::Route { module: 0, message: 2, except: sadk_lobby_proto::cell::NOBODY, data: line.clone(), cell: 1, to_self: true, propset: true };
        a.handle_frame(Frame::new(1, frame::SERVER, FrameKind::Application, route.encode()));
        for s in [&mut a, &mut b] {
            deliver(s);
            assert_eq!(cells(s), [Cell::Deliver { message: 2, data: line.clone(), cell: 1, from: a_id, propset: true }]);
        }
        // Without "self" only B gets it.
        let route = Cell::Route { module: 0, message: 2, except: sadk_lobby_proto::cell::NOBODY, data: line, cell: 1, to_self: false, propset: true };
        a.handle_frame(Frame::new(1, frame::SERVER, FrameKind::Application, route.encode()));
        deliver(&mut a);
        deliver(&mut b);
        assert!(cells(&mut a).is_empty());
        assert_eq!(cells(&mut b).len(), 1);

        // B disconnects (what `run` does on the way out).
        b.leave_all_channels();
        deliver(&mut a);
        let out = cells(&mut a);
        let left = delivered(&a, &out[0]);
        assert_eq!((left.name.as_str(), left.int("perm_id"), left.int("cell_id")), ("UserLeftChannel", Some(b_id as i64), Some(1)));
    }

    /// A whisper reaches its receiver alone, as a PrivateChatMessage with the
    /// sender's name, and comes back to the sender, who asks for its own
    /// copy. One to a player who is offline or unknown gets the sender
    /// a "[SYSTEM]" line instead; one to a player who ignores the sender is
    /// dropped without a word.
    #[test]
    fn whispers_reach_only_the_receiver() {
        let mut accounts = accounts("whisper");
        let ids: Vec<u32> = ["Ada", "Bert", "Cleo"]
            .iter()
            .map(|n| {
                let owner = accounts.create(n, b"x").unwrap().unwrap().id;
                accounts.add_character(owner, n, vec![]).unwrap().unwrap().id
            })
            .collect();
        let cleo = accounts.owner(ids[2]).unwrap().id;
        let server = server(accounts);
        let mut users: Vec<Session> = ids.iter().enumerate().map(|(i, &id)| usercomm(&server, i as u32 + 1, id)).collect();
        for (s, &id) in users.iter_mut().zip(&ids) {
            s.handle_message(request(s, "RequestJoinChannel", &[("cell_id", 10), ("ticket_id", 4), ("from_id", id as i64)]));
        }
        for s in &mut users {
            deliver(s);
            cells(s);
        }

        let whisper = |s: &Session, to: u32| {
            let data = s.propset(
                "WhisperChatMessage",
                &[("mode", Value::Int(3)), ("txt", Value::text("psst")), ("cell_id", Value::Int(10)), ("from_id", Value::Int(ids[0] as i64)), ("perm_id", Value::Int(to as i64))],
            );
            let route = Cell::Route { module: 0, message: 30, except: sadk_lobby_proto::cell::NOBODY, data, cell: 10, to_self: true, propset: true };
            Frame::new(1, frame::SERVER, FrameKind::Application, route.encode())
        };
        let frame = whisper(&users[0], ids[1]);
        users[0].handle_frame(frame);
        for s in &mut users {
            deliver(s);
        }
        let (a, b, c) = (cells(&mut users[0]), cells(&mut users[1]), cells(&mut users[2]));
        assert!(c.is_empty(), "{c:?}");
        assert_eq!(a, b);
        assert!(matches!(b[..], [Cell::Deliver { message: 3, cell: 10, propset: true, from, .. }] if from == ids[0]));
        let line = delivered(&users[1], &b[0]);
        assert_eq!((line.name.as_str(), line.string("txt").unwrap().as_str(), line.int("mode")), ("PrivateChatMessage", "psst", Some(3)));
        assert_eq!((line.int("from_id"), line.int("to_id"), line.string("from_name").unwrap().as_str()), (Some(ids[0] as i64), Some(ids[1] as i64), "Ada"));

        // Cleo ignores Ada: only Ada's own copy comes back.
        server.accounts.lock().unwrap().set_listed(cleo, List::Ignored, ids[0], true).unwrap();
        let frame = whisper(&users[0], ids[2]);
        users[0].handle_frame(frame);
        for s in &mut users {
            deliver(s);
        }
        let (a, b, c) = (cells(&mut users[0]), cells(&mut users[1]), cells(&mut users[2]));
        assert!(b.is_empty() && c.is_empty(), "{b:?} {c:?}");
        assert_eq!(delivered(&users[0], &a[0]).int("to_id"), Some(ids[2] as i64));

        // Bert goes offline, and 999 never existed: Ada hears so from the server.
        users[1].leave_all_channels();
        for s in &mut users {
            deliver(s);
            cells(s);
        }
        for (to, notice) in [(ids[1], "Bert ist nicht online."), (999, "Diesen Spieler gibt es nicht.")] {
            let frame = whisper(&users[0], to);
            users[0].handle_frame(frame);
            for s in &mut users {
                deliver(s);
            }
            let a = cells(&mut users[0]);
            assert!(matches!(a[..], [Cell::Deliver { message: 2, cell: 10, from: frame::SERVER, propset: true, .. }]), "{a:?}");
            let line = delivered(&users[0], &a[0]);
            assert_eq!((line.string("txt").unwrap().as_str(), line.int("from_id")), (notice, Some(frame::SERVER as i64)));
            assert!(cells(&mut users[1]).is_empty() && cells(&mut users[2]).is_empty());
        }
    }

    /// The retail client logs in to usercomm, the village and the referee
    /// with the id of its character; an account's id stands for its first
    /// character.
    #[test]
    fn token_logins_name_the_character() {
        let mut accounts = accounts("token");
        let owner = accounts.create("siedler", b"x").unwrap().unwrap().id;
        let first = accounts.add_character(owner, "garibald", vec![]).unwrap().unwrap().id;
        let second = accounts.add_character(owner, "Hilde", vec![]).unwrap().unwrap().id;
        let server = server(accounts);
        for (conn, sent_id, me, name) in [(1, second, second, "Hilde"), (2, owner, first, "garibald")] {
            let mut s = Session::new(server.clone(), conn, Role::Usercomm, "uc".into());
            s.token_login(sent_id as i64, 3);
            let out = sent(&mut s);
            assert_eq!((out[0].name.as_str(), out[0].int("errorcode"), out[0].int("id")), ("AddResult", Some(code::OK), Some(sent_id as i64)));
            assert_eq!((s.account, s.user.clone()), (Some(owner), Some((me, name.to_string()))));
            assert!(server.chat.lock().unwrap().is_online(me));
        }
        let mut s = Session::new(server.clone(), 3, Role::Usercomm, "uc".into());
        s.token_login(999, 3);
        assert_eq!(sent(&mut s)[0].int("errorcode"), Some(code::AUTH_FAILED));
    }
}

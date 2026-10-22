//! Open matches ("game servers"). A player who hosts a match runs the game
//! server inside its own client (tincat's net driver, port 5479) and
//! registers it here with `AddGameServer`; the lobby lists it to everyone
//! observing server type 5, hands joiners the host's address and a one-time
//! nonce (`RequestConnectionData` → `ConnectionData`) and tells the host who
//! is coming (`TANConnectionRequest`). The match itself never touches the
//! lobby; the host reports players (`EnterServer` / `LeaveServer`), setting
//! changes (`ChangeGameServer`) and the end (`RemoveServer`).

use crate::village::Event;
use sadk_lobby_proto::{Message, MsgDefs, Value};
use std::collections::{BTreeMap, HashMap};
use tokio::sync::mpsc::UnboundedSender;

/// Server type of matches in `RegObserverServerList` and `GameServerData`
/// (4 is the lobby village).
pub const MATCH_SERVER_TYPE: i64 = 5;
/// Match ids start here so they never collide with the village (server 1).
const FIRST_ID: u32 = 1000;

/// Fields a host may set in `AddGameServer` / `ChangeGameServer` that
/// `GameServerData` repeats under the same name.
const LISTED: [&str; 16] = [
    "name",
    "description",
    "port",
    "server_type",
    "server_subtype",
    "version",
    "max_players",
    "max_spectators",
    "ai_players",
    "room_id",
    "level",
    "game_mode",
    "hardcore",
    "map",
    "running",
    "locked_config",
];

/// `ChangeGameServer.property_mask` bits, in the message's field order
/// (name = bit 0). Fields whose bit is clear keep their value; the retail
/// client always sends mask 0 with every field filled in, which counts as
/// "all fields".
const CHANGEABLE: [&str; 13] = [
    "name",
    "description",
    "cipher",
    "max_players",
    "max_spectators",
    "ai_players",
    "room_id",
    "level",
    "game_mode",
    "hardcore",
    "map",
    "running",
    "locked_config",
];
/// `data` follows `locked_config` in ChangeGameServer.
const CHANGE_DATA_BIT: u32 = 1 << CHANGEABLE.len();

pub struct Game {
    pub id: u32,
    pub owner: u32,
    pub owner_nick: String,
    /// Lobby connection of the host.
    pub host_conn: u32,
    pub ip: String,
    /// The host set its address itself (`!adresse`); it is listed as it is.
    pub explicit_address: bool,
    /// Listed fields by name (see `LISTED`), plus `data`.
    pub fields: HashMap<String, Value>,
    /// Shown as a lock in the list. The client sends a 145-byte `cipher`
    /// with or without a password and checks the password itself on the
    /// host, so this stays false until we know how a password shows.
    pub password_required: bool,
    /// Perm ids the host reported with `EnterServer`.
    pub players: Vec<u32>,
    host_tx: UnboundedSender<Event>,
}

impl Game {
    pub fn int(&self, field: &str) -> i64 {
        match self.fields.get(field) {
            Some(Value::Int(v)) => *v,
            Some(Value::Bool(b)) => *b as i64,
            _ => 0,
        }
    }

    pub fn running(&self) -> bool {
        self.int("running") != 0
    }

    /// The listing entry, as observers and `RegObserverServerList` get it.
    pub fn server_data(&self, defs: &MsgDefs, ticket: i64) -> Message {
        let mut msg = Message::new(defs, "GameServerData").expect("GameServerData in msgdefs");
        msg.set("server_id", Value::Int(self.id as i64));
        msg.set("owner_id", Value::Int(self.owner as i64));
        msg.set("ip", Value::text(&self.ip));
        msg.set("password_required", Value::Bool(self.password_required));
        msg.set("cur_players", Value::Int(self.players.len().max(1) as i64));
        for field in LISTED.iter().chain(["data"].iter()) {
            if let Some(v) = self.fields.get(*field) {
                msg.set(field, v.clone());
            }
        }
        msg.set("ticket_id", Value::Int(ticket));
        msg
    }

    /// Hands a lobby message to the host's session.
    pub fn tell_host(&self, msg: Message) -> bool {
        self.host_tx.send(Event::Lobby(msg)).is_ok()
    }
}

/// Who registers a match.
pub struct Host<'a> {
    pub perm: u32,
    pub nick: &'a str,
    pub conn: u32,
    /// The host's address as the lobby sees it; used when the client leaves
    /// `ip` empty (it does).
    pub ip: &'a str,
    /// The address (and port) the host set with `!adresse`, which wins.
    pub address: Option<(&'a str, Option<u16>)>,
    pub tx: UnboundedSender<Event>,
}

#[derive(Default)]
pub struct Games {
    next_id: u32,
    games: BTreeMap<u32, Game>,
    /// Lobby connections observing the match list, with their senders.
    observers: HashMap<u32, UnboundedSender<Event>>,
}

impl Games {
    /// Registers a match from `AddGameServer`. One per host connection: an
    /// older one of the same host is dropped (and returned).
    pub fn create(&mut self, host: Host<'_>, msg: &Message) -> (&Game, Option<Game>) {
        let replaced = self.games.values().find(|g| g.host_conn == host.conn).map(|g| g.id).and_then(|id| self.games.remove(&id));
        self.next_id = self.next_id.max(FIRST_ID) + 1;
        let id = self.next_id - 1;
        let mut fields = HashMap::new();
        for field in LISTED.iter().chain(["data"].iter()) {
            if let Some(v) = msg.get(field) {
                fields.insert(field.to_string(), v.clone());
            }
        }
        let mut ip = msg.string("ip").filter(|ip| !ip.is_empty()).unwrap_or_else(|| host.ip.to_string());
        if let Some((address, port)) = host.address {
            ip = address.to_string();
            if let Some(port) = port {
                fields.insert("port".into(), Value::Int(port as i64));
            }
        }
        let game = Game {
            id,
            owner: host.perm,
            owner_nick: host.nick.to_string(),
            host_conn: host.conn,
            ip,
            explicit_address: host.address.is_some(),
            fields,
            password_required: false,
            players: vec![host.perm],
            host_tx: host.tx,
        };
        self.games.insert(id, game);
        (&self.games[&id], replaced)
    }

    /// Applies `ChangeGameServer` to the match this connection hosts.
    pub fn change(&mut self, conn: u32, msg: &Message) -> Option<&Game> {
        let game = self.games.values_mut().find(|g| g.host_conn == conn)?;
        let mask = match msg.int("property_mask").unwrap_or(0) as u32 {
            0 => u32::MAX,
            mask => mask,
        };
        for (bit, field) in CHANGEABLE.iter().enumerate() {
            if mask & (1 << bit) == 0 || *field == "cipher" {
                continue;
            }
            if let Some(v) = msg.get(field) {
                game.fields.insert(field.to_string(), v.clone());
            }
        }
        if mask & CHANGE_DATA_BIT != 0 {
            if let Some(v) = msg.get("data") {
                game.fields.insert("data".into(), v.clone());
            }
        }
        Some(game)
    }

    /// `RemoveServer` from the host, or the host disconnected.
    pub fn remove(&mut self, conn: u32, id: u32) -> Option<Game> {
        if self.games.get(&id)?.host_conn != conn {
            return None;
        }
        self.games.remove(&id)
    }

    /// Every match the connection hosts (it went away).
    pub fn remove_host(&mut self, conn: u32) -> Vec<Game> {
        let ids: Vec<u32> = self.games.values().filter(|g| g.host_conn == conn).map(|g| g.id).collect();
        ids.iter().filter_map(|id| self.games.remove(id)).collect()
    }

    /// `EnterServer` / `LeaveServer` from the host about a player.
    pub fn set_player(&mut self, conn: u32, perm: u32, present: bool) -> Option<&Game> {
        let game = self.games.values_mut().find(|g| g.host_conn == conn)?;
        game.players.retain(|&p| p != perm);
        if present {
            game.players.push(perm);
        }
        Some(game)
    }

    pub fn get(&self, id: u32) -> Option<&Game> {
        self.games.get(&id)
    }

    /// The match a connection hosts.
    pub fn hosted_by(&self, conn: u32) -> Option<&Game> {
        self.games.values().find(|g| g.host_conn == conn)
    }

    /// The match a player is in (as host or reported by the host).
    pub fn playing(&self, perm: u32) -> Option<&Game> {
        self.games.values().find(|g| g.players.contains(&perm))
    }

    pub fn list(&self) -> impl Iterator<Item = &Game> {
        self.games.values()
    }

    pub fn observe(&mut self, conn: u32, tx: UnboundedSender<Event>) {
        self.observers.insert(conn, tx);
    }

    pub fn unobserve(&mut self, conn: u32) -> bool {
        self.observers.remove(&conn).is_some()
    }

    /// Sends a message to every observer of the match list.
    pub fn notify(&self, msg: &Message) -> usize {
        self.observers.values().filter(|tx| tx.send(Event::Lobby(msg.clone())).is_ok()).count()
    }
}

/// `RemoveServer` as the observers get it.
pub fn removed(defs: &MsgDefs, game: &Game) -> Message {
    Message::new(defs, "RemoveServer")
        .expect("RemoveServer in msgdefs")
        .with("server_id", Value::Int(game.id as i64))
        .with("running", Value::Bool(game.running()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    pub(crate) const DEFS: &str = r#"# AddGameServer
[NETMSG_TYPE_168]
type="UNSHORT"
name="STRING 128"
description="STRING 128"
ip="STRING 128"
port="UNLONG"
server_type="UNBYTE"
server_subtype="UNBYTE"
cipher="MEMBLOCK 0"
version="STRING 32"
max_players="UNSHORT"
max_spectators="UNSHORT"
ai_players="UNSHORT"
room_id="UNLONG"
level="UNBYTE"
game_mode="UNBYTE"
hardcore="LBOOL"
map="STRING 128"
running="LBOOL"
locked_config="LBOOL"
automatic_join="LBOOL"
data="MEMBLOCK 0"
ticket_id="UNLONG"
# RemoveServer
[NETMSG_TYPE_169]
type="UNSHORT"
server_id="UNLONG"
running="LBOOL"
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
# ChangeGameServer
[NETMSG_TYPE_177]
type="UNSHORT"
name="STRING 128"
description="STRING 128"
cipher="MEMBLOCK 0"
max_players="UNSHORT"
max_spectators="UNSHORT"
ai_players="UNSHORT"
room_id="UNLONG"
level="UNBYTE"
game_mode="UNBYTE"
hardcore="LBOOL"
map="STRING 128"
running="LBOOL"
locked_config="LBOOL"
data="MEMBLOCK 0"
property_mask="UNLONG"
ticket_id="UNLONG"
"#;

    fn drain(rx: &mut UnboundedReceiver<Event>) -> Vec<Message> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| match e {
                Event::Lobby(m) => m,
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    /// The AddGameServer the retail client sent when hosting (build 34688).
    pub(crate) fn add(defs: &MsgDefs) -> Message {
        Message::new(defs, "AddGameServer")
            .unwrap()
            .with("name", Value::text("garibaldwill spielen"))
            .with("description", Value::text("34688|2|1|0|0|garibald"))
            .with("ip", Value::Str(None))
            .with("port", Value::Int(5479))
            .with("server_type", Value::Int(5))
            .with("server_subtype", Value::Int(1))
            .with("version", Value::text("1.0"))
            .with("max_players", Value::Int(2))
            .with("max_spectators", Value::Int(1))
            .with("map", Value::text("MP_2P_fata_morgana"))
            .with("running", Value::Bool(false))
            .with("ticket_id", Value::Int(20))
    }

    #[test]
    fn lists_changes_and_removes_a_match() {
        let defs = MsgDefs::parse(DEFS).unwrap();
        let mut games = Games::default();
        let (host_tx, mut host_rx) = unbounded_channel();
        let (obs_tx, mut obs) = unbounded_channel();
        games.observe(9, obs_tx);

        let (game, replaced) = games.create(Host { perm: 1, nick: "siedler", conn: 3, ip: "127.0.0.1", address: None, tx: host_tx }, &add(&defs));
        assert!(replaced.is_none());
        assert_eq!((game.id, game.ip.as_str()), (FIRST_ID, "127.0.0.1"));
        let data = game.server_data(&defs, 0);
        let bytes = data.encode(&defs);
        let back = Message::decode(&defs, &bytes).unwrap();
        assert_eq!(back.string("name").unwrap(), "garibaldwill spielen");
        assert_eq!((back.int("server_id"), back.int("owner_id"), back.int("port")), (Some(FIRST_ID as i64), Some(1), Some(5479)));
        assert_eq!((back.int("server_type"), back.int("max_players"), back.int("cur_players")), (Some(5), Some(2), Some(1)));
        assert_eq!(back.string("map").unwrap(), "MP_2P_fata_morgana");
        assert_eq!(back.string("ip").unwrap(), "127.0.0.1");
        games.notify(&data);
        assert_eq!(drain(&mut obs), [data]);

        // Only the map changes (bit 10); the name stays.
        let change = Message::new(&defs, "ChangeGameServer")
            .unwrap()
            .with("name", Value::text("ignored"))
            .with("map", Value::text("MP_4P_other"))
            .with("property_mask", Value::Int(1 << 10));
        assert!(games.change(4, &change).is_none(), "only the host changes its match");
        let game = games.change(3, &change).unwrap();
        assert_eq!(game.server_data(&defs, 0).string("map").unwrap(), "MP_4P_other");
        assert_eq!(game.server_data(&defs, 0).string("name").unwrap(), "garibaldwill spielen");

        // Mask 0, as the retail client sends it: every field is new.
        let change = Message::new(&defs, "ChangeGameServer")
            .unwrap()
            .with("name", Value::text("garibald will spielen"))
            .with("description", Value::text("34688|2|1|0|0|garibald|Besucher"))
            .with("cipher", Value::Bytes(Some(vec![7; 145])))
            .with("max_players", Value::Int(2))
            .with("max_spectators", Value::Int(2))
            .with("game_mode", Value::Int(1))
            .with("map", Value::text("MP_2P_fata_morgana"))
            .with("property_mask", Value::Int(0));
        let data = games.change(3, &change).unwrap().server_data(&defs, 0);
        assert_eq!(data.string("name").unwrap(), "garibald will spielen");
        assert_eq!(data.string("description").unwrap(), "34688|2|1|0|0|garibald|Besucher");
        assert_eq!((data.int("game_mode"), data.int("max_spectators")), (Some(1), Some(2)));
        assert_eq!(data.string("map").unwrap(), "MP_2P_fata_morgana");
        assert_eq!(data.get("password_required"), Some(&Value::Bool(false)), "a cipher alone is no password");

        let game = games.set_player(3, 2, true).unwrap();
        assert_eq!(game.players, [1, 2]);
        assert_eq!(games.playing(2).map(|g| g.id), Some(FIRST_ID));
        games.set_player(3, 2, false);
        assert!(games.playing(2).is_none());

        assert!(games.get(FIRST_ID).unwrap().tell_host(removed(&defs, games.get(FIRST_ID).unwrap())));
        assert_eq!(drain(&mut host_rx)[0].name, "RemoveServer");
        assert!(games.remove(4, FIRST_ID).is_none());
        assert_eq!(games.remove(3, FIRST_ID).map(|g| g.id), Some(FIRST_ID));
        assert_eq!(games.list().count(), 0);
    }

    #[test]
    fn one_match_per_host_and_cleanup_on_disconnect() {
        let defs = MsgDefs::parse(DEFS).unwrap();
        let mut games = Games::default();
        let (tx, _rx) = unbounded_channel();
        let first = games.create(Host { perm: 1, nick: "a", conn: 3, ip: "10.0.0.1", address: None, tx: tx.clone() }, &add(&defs)).0.id;
        let (second, replaced) = games.create(Host { perm: 1, nick: "a", conn: 3, ip: "10.0.0.1", address: None, tx: tx.clone() }, &add(&defs));
        assert_eq!((second.id, replaced.map(|g| g.id)), (first + 1, Some(first)));
        games.create(Host { perm: 2, nick: "b", conn: 4, ip: "10.0.0.2", address: None, tx }, &add(&defs));
        assert_eq!(games.remove_host(3).len(), 1);
        assert_eq!(games.list().map(|g| g.owner).collect::<Vec<_>>(), [2]);
    }
}

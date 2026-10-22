//! Lobby requests about matches (see `crate::games`), handled on the lobby
//! connection of the host or of a joining player.

use super::{code, Session};
use crate::games::{self, Host, MATCH_SERVER_TYPE};
use sadk_lobby_proto::auth;
use sadk_lobby_proto::{Message, Value};
use tracing::{info, warn};

/// Error the original lobby gave for an unknown game server.
const UNKNOWN_SERVER: i64 = 0x84;

impl Session {
    /// Dispatches a match request; returns false for messages it does not
    /// handle (so `handle_message` goes on).
    pub(super) fn match_message(&mut self, msg: &Message, ticket: i64) -> bool {
        match msg.name.as_str() {
            "RegObserverServerList" if msg.int("server_type") == Some(MATCH_SERVER_TYPE) => self.observe_matches(ticket),
            "DeregObserverServerList" => {
                self.server.games.lock().unwrap().unobserve(self.id);
                self.result(ticket, code::OK, None);
            }
            "AddGameServer" => self.add_match(msg, ticket),
            "ChangeGameServer" => self.change_match(msg, ticket),
            "RemoveServer" => self.remove_match(msg.int("server_id").unwrap_or(0) as u32, ticket),
            "EnterServer" | "LeaveServer" => self.match_player(msg, ticket),
            "RequestConnectionData" if self.server.games.lock().unwrap().get(msg.int("server_id").unwrap_or(0) as u32).is_some() => {
                self.join_match(msg, ticket)
            }
            "CheckPlayerOnServerAndGetInfos" => self.check_player(msg, ticket),
            _ => return false,
        }
        true
    }

    /// RegObserverServerList for type 5: the open matches now, and every
    /// change later (GameServerData / RemoveServer with ticket 0).
    fn observe_matches(&mut self, ticket: i64) {
        let list: Vec<Message> = {
            let mut games = self.server.games.lock().unwrap();
            games.observe(self.id, self.events_tx.clone());
            games.list().map(|g| g.server_data(&self.server.defs, ticket)).collect()
        };
        info!(conn = self.id, "observing the match list ({} open)", list.len());
        for msg in list {
            self.send(msg);
        }
        self.result(ticket, code::OK, None);
    }

    /// AddGameServer: the client hosts a match. It gets the new server id in
    /// an AddResult; the observers get the listing.
    fn add_match(&mut self, msg: &Message, ticket: i64) {
        let Some((perm, nick)) = self.user.clone() else {
            return self.result(ticket, code::AUTH_FAILED, Some("Not logged in"));
        };
        let ip = match self.peer_ip {
            Some(ip) => ip.to_string(),
            None => self.peer.rsplit_once(':').map_or(self.peer.as_str(), |(ip, _)| ip).to_string(),
        };
        let set = self.account.and_then(|a| self.server.accounts.lock().unwrap().account(a).and_then(|a| a.match_address.clone()));
        let set = set.as_deref().and_then(crate::address::parse_match_address);
        let address = set.as_ref().map(|(ip, port)| (ip.as_str(), *port));
        let (id, listing, replaced) = {
            let mut games = self.server.games.lock().unwrap();
            let host = Host { perm, nick: &nick, conn: self.id, ip: &ip, address, tx: self.events_tx.clone() };
            let (game, replaced) = games.create(host, msg);
            let replaced = replaced.map(|old| games::removed(&self.server.defs, &old));
            (game.id, game.server_data(&self.server.defs, 0), replaced)
        };
        info!(conn = self.id, id, "{nick} hosts match {:?} on {ip}:{}", listing.string("name").unwrap_or_default(), listing.int("port").unwrap_or(0));
        let reply = self
            .message("AddResult")
            .with("errorcode", Value::Int(code::OK))
            .with("id", Value::Int(id as i64))
            .with("ticket_id", Value::Int(ticket));
        self.send(reply);
        let games = self.server.games.lock().unwrap();
        if let Some(removed) = replaced {
            games.notify(&removed);
        }
        games.notify(&listing);
    }

    fn change_match(&mut self, msg: &Message, ticket: i64) {
        let listing = {
            let mut games = self.server.games.lock().unwrap();
            games.change(self.id, msg).map(|g| g.server_data(&self.server.defs, 0))
        };
        match listing {
            Some(listing) => {
                info!(conn = self.id, mask = format_args!("{:#x}", msg.int("property_mask").unwrap_or(0)), "match changed: {listing}");
                self.result(ticket, code::OK, None);
                self.server.games.lock().unwrap().notify(&listing);
            }
            None => self.result(ticket, UNKNOWN_SERVER, Some("No game server")),
        }
    }

    fn remove_match(&mut self, id: u32, ticket: i64) {
        let removed = self.server.games.lock().unwrap().remove(self.id, id);
        match removed {
            Some(game) => {
                info!(conn = self.id, id, "match closed");
                self.result(ticket, code::OK, None);
                self.server.games.lock().unwrap().notify(&games::removed(&self.server.defs, &game));
            }
            None => self.result(ticket, UNKNOWN_SERVER, Some("No game server")),
        }
    }

    /// The host's lobby connection closed: its matches go too.
    pub(super) fn leave_matches(&mut self) {
        let mut games = self.server.games.lock().unwrap();
        games.unobserve(self.id);
        for game in games.remove_host(self.id) {
            info!(conn = self.id, id = game.id, "host left; match closed");
            games.notify(&games::removed(&self.server.defs, &game));
        }
    }

    /// EnterServer / LeaveServer: the host reports a player joining or
    /// leaving its match; the player count in the listing follows.
    fn match_player(&mut self, msg: &Message, ticket: i64) {
        let perm = msg.int("perm_id").unwrap_or(0) as u32;
        let present = msg.name == "EnterServer";
        let listing = {
            let mut games = self.server.games.lock().unwrap();
            games.set_player(self.id, perm, present).map(|g| g.server_data(&self.server.defs, 0))
        };
        match listing {
            Some(listing) => {
                info!(conn = self.id, perm, present, "match players: {:?}", listing.int("cur_players"));
                self.result(ticket, code::OK, None);
                self.server.games.lock().unwrap().notify(&listing);
            }
            None => {
                warn!(conn = self.id, perm, "{} without a hosted match", msg.name);
                self.result(ticket, code::OK, None);
            }
        }
    }

    /// RequestConnectionData for a match: the joiner gets the host's address
    /// and a nonce; the host gets a TANConnectionRequest with the same nonce
    /// and the joiner's character.
    fn join_match(&mut self, msg: &Message, ticket: i64) {
        let id = msg.int("server_id").unwrap_or(0) as u32;
        let Some((perm, nick)) = self.user.clone() else {
            return self.result(ticket, code::AUTH_FAILED, Some("Not logged in"));
        };
        let nonce = auth::new_session_key().map(|k| k[..16].to_vec()).unwrap_or_default();
        let joiner_ip = self.peer.rsplit_once(':').map_or(self.peer.as_str(), |(ip, _)| ip).to_string();
        let character = {
            let accounts = self.server.accounts.lock().unwrap();
            accounts.account(perm).and_then(|a| a.characters.first()).map(|c| (c.id, c.name.clone(), c.data.clone()))
        };
        let (char_id, char_name, data) = character.unwrap_or((0, nick.clone(), Vec::new()));
        let tan = self
            .message("TANConnectionRequest")
            .with("ip", Value::text(&joiner_ip))
            .with("nonce", Value::Bytes(Some(nonce.clone())))
            .with("char_id", Value::Int(char_id as i64))
            .with("name", Value::text(&char_name))
            .with("owner_id", Value::Int(perm as i64))
            .with("owner_name", Value::text(&nick))
            .with("data", Value::Bytes(Some(data)));
        let host = {
            let games = self.server.games.lock().unwrap();
            games.get(id).map(|g| {
                let mut tan = tan;
                tan.magic = self.magic;
                (self.match_host_ip(&g.ip, g.explicit_address), g.int("port"), g.tell_host(tan))
            })
        };
        let Some((ip, port, told)) = host else {
            return self.result(ticket, UNKNOWN_SERVER, Some("GameServer could not be found"));
        };
        info!(conn = self.id, id, told, "{nick} joins match {id} at {ip}:{port}");
        let reply = self
            .message("ConnectionData")
            .with("perm_id", Value::Int(perm as i64))
            .with("server_id", Value::Int(id as i64))
            .with("ip", Value::text(&ip))
            .with("port", Value::Int(port))
            .with("nonce", Value::Bytes(Some(nonce)))
            .with("errorcode", Value::Int(code::OK))
            .with("ticket_id", Value::Int(ticket));
        self.send(reply);
    }

    /// CheckPlayerOnServerAndGetInfos: a host asks who a connecting player
    /// is. Answered from the accounts; result 0 when the player is known.
    fn check_player(&mut self, msg: &Message, ticket: i64) {
        let user = msg.int("user_pid").unwrap_or(0) as u32;
        let nick = self.server.accounts.lock().unwrap().account(user).map(|a| a.nick.clone());
        let mut reply = self
            .message("CheckPlayerOnServerAndGetInfosReply")
            .with("perm_id", Value::Int(user as i64))
            .with("ticket_id", Value::Int(ticket));
        match nick {
            Some(nick) => {
                reply.set("username", Value::text(&nick));
                reply.set("result_id", Value::Int(0));
            }
            None => reply.set("result_id", Value::Int(1)),
        }
        self.send(reply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Accounts;
    use crate::village::Event;
    use crate::{Role, Server};
    use sadk_lobby_proto::frame::Frame;
    use std::sync::Arc;

    /// The lobby messages a match needs besides those in `games::tests::DEFS`
    /// (from the retail msgdefs.ini).
    const DEFS: &str = r#"# Result
[NETMSG_TYPE_42]
type="UNSHORT"
errorcode="UNBYTE"
errormsg="STRING 32"
ticket_id="UNLONG"
# AddResult
[NETMSG_TYPE_153]
type="UNSHORT"
errorcode="UNBYTE"
errormsg="STRING 32"
id="UNLONG"
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
# TANConnectionRequest
[NETMSG_TYPE_223]
type="UNSHORT"
ip="STRING 128"
nonce="MEMBLOCK 0"
char_id="UNLONG"
name="STRING 128"
owner_id="UNLONG"
owner_name="STRING 128"
guild_id="UNLONG"
guild_name="STRING 128"
guild_role="UNBYTE"
data="MEMBLOCK 0"
"#;

    fn server(name: &str) -> (Arc<Server>, [(u32, u32); 2]) {
        let path = std::env::temp_dir().join(format!("sadk-matches-{name}-{}", std::process::id())).join("accounts.json");
        let _ = std::fs::remove_file(&path);
        let mut accounts = Accounts::open(path).unwrap();
        let mut players = [(0, 0); 2];
        for (slot, (nick, character)) in players.iter_mut().zip([("siedler", "garibald"), ("besucher", "Besucher")]) {
            let perm = accounts.create(nick, b"x").unwrap().unwrap().id;
            let character = accounts.add_character(perm, character, vec![0; 68]).unwrap().unwrap().id;
            *slot = (perm, character);
        }
        (Server::test(&format!("{}{DEFS}", crate::games::tests::DEFS), accounts), players)
    }

    fn lobby(server: &Arc<Server>, id: u32, peer: &str, perm: u32, nick: &str) -> Session {
        let mut s = Session::new(server.clone(), id, Role::Lobby, peer.into());
        s.user = Some((perm, nick.into()));
        s
    }

    /// Hands pending events to the session, then decodes what it queued.
    fn sent(s: &mut Session) -> Vec<Message> {
        let events = s.events.as_mut().unwrap();
        let pending: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        for event in pending {
            s.handle_event(event);
        }
        let mut out = std::mem::take(&mut s.out);
        let mut msgs = Vec::new();
        while let Some((frame, used)) = Frame::decode(&out).unwrap() {
            msgs.push(Message::decode(&s.server.defs, &frame.payload).unwrap());
            out.drain(..used);
        }
        msgs
    }

    fn names(msgs: &[Message]) -> Vec<&str> {
        msgs.iter().map(|m| m.name.as_str()).collect()
    }

    fn request(s: &Session, name: &str, fields: &[(&str, i64)]) -> Message {
        fields.iter().fold(s.message(name), |m, &(f, v)| m.with(f, Value::Int(v)))
    }

    /// One player hosts, the other sees the match listed, joins it (host and
    /// joiner learn about each other) and sees it go when the host starts.
    #[test]
    fn host_list_join_and_start_a_match() {
        let (server, [(host_perm, _), (joiner_perm, joiner_char)]) = server("flow");
        let mut host = lobby(&server, 1, "10.0.0.5:50001", host_perm, "siedler");
        let mut joiner = lobby(&server, 2, "10.0.0.6:50002", joiner_perm, "besucher");

        joiner.handle_message(request(&joiner, "RegObserverServerList", &[("server_type", 5), ("ticket_id", 3)]));
        assert_eq!(names(&sent(&mut joiner)), ["Result"]);

        host.handle_message(crate::games::tests::add(&server.defs));
        let out = sent(&mut host);
        assert_eq!(names(&out), ["AddResult"]);
        assert_eq!((out[0].int("errorcode"), out[0].int("id"), out[0].int("ticket_id")), (Some(0), Some(1000), Some(20)));
        let out = sent(&mut joiner);
        assert_eq!(names(&out), ["GameServerData"]);
        assert_eq!((out[0].int("server_id"), out[0].int("owner_id"), out[0].int("port")), (Some(1000), Some(host_perm as i64), Some(5479)));
        assert_eq!(out[0].string("ip").unwrap(), "10.0.0.5", "the host's address as the lobby sees it");

        // The host renames the match with mask 0; the list follows.
        let change = host
            .message("ChangeGameServer")
            .with("name", Value::text("garibald will spielen"))
            .with("map", Value::text("MP_2P_fata_morgana"))
            .with("property_mask", Value::Int(0))
            .with("ticket_id", Value::Int(21));
        host.handle_message(change);
        assert_eq!(names(&sent(&mut host)), ["Result"]);
        let out = sent(&mut joiner);
        assert_eq!(names(&out), ["GameServerData"]);
        assert_eq!(out[0].string("name").unwrap(), "garibald will spielen");

        // A later observer gets the open match with its own ticket.
        let mut late = lobby(&server, 3, "10.0.0.7:50003", joiner_perm, "besucher");
        late.handle_message(request(&late, "RegObserverServerList", &[("server_type", 5), ("ticket_id", 9)]));
        let out = sent(&mut late);
        assert_eq!(names(&out), ["GameServerData", "Result"]);
        assert_eq!(out[0].int("ticket_id"), Some(9));

        joiner.handle_message(request(&joiner, "RequestConnectionData", &[("perm_id", joiner_perm as i64), ("server_id", 1000), ("ticket_id", 4)]));
        let out = sent(&mut joiner);
        assert_eq!(names(&out), ["ConnectionData"]);
        assert_eq!((out[0].string("ip").unwrap().as_str(), out[0].int("port"), out[0].int("errorcode")), ("10.0.0.5", Some(5479), Some(0)));
        let nonce = out[0].bytes("nonce").unwrap().to_vec();
        assert_eq!(nonce.len(), 16);
        let out = sent(&mut host);
        assert_eq!(names(&out), ["TANConnectionRequest"]);
        assert_eq!(out[0].string("ip").unwrap(), "10.0.0.6");
        assert_eq!(out[0].bytes("nonce").unwrap(), nonce);
        assert_eq!((out[0].int("char_id"), out[0].int("owner_id")), (Some(joiner_char as i64), Some(joiner_perm as i64)));
        assert_eq!((out[0].string("name").unwrap().as_str(), out[0].string("owner_name").unwrap().as_str()), ("Besucher", "besucher"));

        // Only the host changes its match: error 0x84 for anyone else.
        joiner.handle_message(request(&joiner, "ChangeGameServer", &[("ticket_id", 5)]));
        let out = sent(&mut joiner);
        assert_eq!((out[0].name.as_str(), out[0].int("errorcode")), ("Result", Some(UNKNOWN_SERVER)));

        // Starting the match takes it off the list.
        host.handle_message(request(&host, "RemoveServer", &[("server_id", 1000), ("ticket_id", 27)]));
        assert_eq!(names(&sent(&mut host)), ["Result"]);
        let out = sent(&mut joiner);
        assert_eq!(names(&out), ["RemoveServer"]);
        assert_eq!(out[0].int("server_id"), Some(1000));
        assert!(server.games.lock().unwrap().get(1000).is_none());
    }

    /// A host that drops its lobby connection takes its match along.
    #[test]
    fn host_leaving_closes_its_match() {
        let (server, [(host_perm, _), (joiner_perm, _)]) = server("leave");
        let mut host = lobby(&server, 1, "10.0.0.5:50001", host_perm, "siedler");
        let mut joiner = lobby(&server, 2, "10.0.0.6:50002", joiner_perm, "besucher");
        joiner.handle_message(request(&joiner, "RegObserverServerList", &[("server_type", 5), ("ticket_id", 3)]));
        host.handle_message(crate::games::tests::add(&server.defs));
        sent(&mut joiner);
        host.leave_matches();
        let out = sent(&mut joiner);
        assert_eq!(names(&out), ["RemoveServer"]);
        assert_eq!(server.games.lock().unwrap().list().count(), 0);
    }

    /// A host on the server's own machine is listed with the address each
    /// observer reaches the server on; one that set `!adresse` with what it set.
    #[test]
    fn matches_are_listed_with_an_address_that_works_for_the_observer() {
        let (server, [(host_perm, _), (joiner_perm, _)]) = server("address");
        *server.public_ip.write().unwrap() = Some("203.0.113.7".parse().unwrap());
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        let mut host = lobby(&server, 1, "127.0.0.1:50001", host_perm, "siedler").reached_at(ip("127.0.0.1"), ip("127.0.0.1"));
        host.account = Some(host_perm);
        let mut lan = lobby(&server, 2, "192.168.1.20:50002", joiner_perm, "besucher").reached_at(ip("192.168.1.20"), ip("192.168.1.10"));
        let mut far = lobby(&server, 3, "198.51.100.9:50003", joiner_perm, "besucher").reached_at(ip("198.51.100.9"), ip("192.168.1.10"));
        for s in [&mut host, &mut lan, &mut far] {
            s.handle_message(request(s, "RegObserverServerList", &[("server_type", 5), ("ticket_id", 3)]));
            sent(s);
        }
        let listed = |s: &mut Session| {
            let data = sent(s).into_iter().find(|m| m.name == "GameServerData").expect("listing");
            (data.string("ip").unwrap(), data.int("port").unwrap())
        };

        host.handle_message(crate::games::tests::add(&server.defs).with("ticket_id", Value::Int(4)));
        assert_eq!(listed(&mut host), ("127.0.0.1".to_string(), 5479));
        assert_eq!(listed(&mut lan), ("192.168.1.10".to_string(), 5479));
        assert_eq!(listed(&mut far), ("203.0.113.7".to_string(), 5479));

        server.accounts.lock().unwrap().set_match_address(host_perm, Some("100.101.1.2:24680".into())).unwrap();
        host.handle_message(crate::games::tests::add(&server.defs).with("ticket_id", Value::Int(5)));
        for s in [&mut host, &mut lan, &mut far] {
            let all: Vec<_> = sent(s).into_iter().filter(|m| m.name == "GameServerData").map(|m| (m.string("ip").unwrap(), m.int("port").unwrap())).collect();
            assert_eq!(all.last().unwrap(), &("100.101.1.2".to_string(), 24680));
        }
        let id = server.games.lock().unwrap().hosted_by(1).unwrap().id;
        lan.handle_message(request(&lan, "RequestConnectionData", &[("perm_id", joiner_perm as i64), ("server_id", id as i64), ("ticket_id", 6)]));
        let reply = sent(&mut lan).into_iter().find(|m| m.name == "ConnectionData").unwrap();
        assert_eq!((reply.string("ip").unwrap().as_str(), reply.int("port")), ("100.101.1.2", Some(24680)));
    }
}

//! The referee connection (see `crate::referee`): the AssignServer answer
//! that points the client at it, its login, and the match registrations.

use super::{code, Session};
use crate::referee::SERVER_ID;
use crate::village::Event;
use sadk_lobby_proto::lobby_message as lm;
use sadk_lobby_proto::referee::{self as rm, RegisterGame};
use sadk_lobby_proto::Value;
use tracing::{info, warn};

impl Session {
    /// AssignServer {4, 4} at login asks for the referee. tincat hands the
    /// client a server only as a `GameServerData` carrying the request's
    /// ticket (`GameServerAssigned`); the client keeps its id and connects
    /// to it (`RequestConnectionData`, then `SendToken`).
    pub(super) fn assign_referee(&mut self, ticket: i64) {
        let addr = std::net::SocketAddr::new(self.server_ip(), self.server.referee.port());
        let data = self
            .message("GameServerData")
            .with("server_id", Value::Int(SERVER_ID as i64))
            .with("name", Value::text("Schiedsrichter"))
            .with("ip", Value::text(&addr.ip().to_string()))
            .with("port", Value::Int(addr.port() as i64))
            .with("server_type", Value::Int(4))
            .with("server_subtype", Value::Int(4))
            .with("max_players", Value::Int(1000))
            .with("running", Value::Bool(true))
            .with("ticket_id", Value::Int(ticket));
        self.send(data);
    }

    /// After the token login the referee confirms with the player's perm id.
    pub(super) fn referee_login(&mut self, perm: u32) {
        self.send_village(rm::id::LOGIN_SUCCESS, lm::single_u32(perm));
    }

    /// A referee message from the client.
    pub(super) fn referee_message(&mut self, id: u16, data: &[u8]) {
        let game = rm::game_id(data).unwrap_or(0);
        match id {
            rm::id::REGISTER_GAME => match RegisterGame::parse(data) {
                Some(reg) => self.register_game(&reg),
                None => warn!(conn = self.id, "short RegisterGame: {}", super::hex_preview(data)),
            },
            rm::id::FINISH_GAME => {
                info!(conn = self.id, game, "match finished");
                self.send_village(rm::id::FINISH_GAME_ACK, rm::ack(game, code::OK as u32));
                self.send_village(rm::id::FINISH_GAME_RESULT, rm::finish_game_result(game));
            }
            rm::id::GIVE_UP_GAME => {
                info!(conn = self.id, game, "player gave up");
                self.send_village(rm::id::GIVE_UP_ACK, rm::ack(game, code::OK as u32));
            }
            rm::id::CLAIM_CHEST => self.send_village(rm::id::CLAIM_CHEST_ACK, rm::ack(game, code::OK as u32)),
            _ => warn!(conn = self.id, "unhandled referee message {id}: {}", super::hex_preview(data)),
        }
    }

    /// RegisterGame: acknowledged at once; when the last player of the match
    /// registers, everyone gets the result with one shared seed.
    fn register_game(&mut self, reg: &RegisterGame) {
        info!(
            conn = self.id,
            game = reg.game_id,
            map = %reg.map_name,
            avatars = ?reg.avatars,
            ai = reg.ai_players,
            "registers for the match"
        );
        self.send_village(rm::id::REGISTER_GAME_ACK, rm::ack(reg.game_id, code::OK as u32));
        let players = self.server.referee_games.lock().unwrap().register(self.id, reg, self.events_tx.clone());
        let Some(players) = players else {
            return;
        };
        let seed = u32::from_le_bytes(sadk_lobby_proto::auth::new_session_key().map(|k| [k[0], k[1], k[2], k[3]]).unwrap_or_default());
        info!(conn = self.id, game = reg.game_id, seed, "all {} players registered; starting", players.len());
        let result = self
            .message("SendGameData")
            .with("msg_type", Value::Int(lm::header(rm::id::REGISTER_GAME_RESULT) as i64))
            .with("data", Value::Bytes(Some(rm::register_game_result(reg.game_id, Ok(seed)))));
        for tx in players {
            let _ = tx.send(Event::Lobby(result.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::DEFS;
    use super::*;
    use crate::accounts::Accounts;
    use crate::{Role, Server};
    use sadk_lobby_proto::frame::Frame;
    use sadk_lobby_proto::Message;
    use std::sync::Arc;

    fn server(name: &str) -> Arc<Server> {
        let path = std::env::temp_dir().join(format!("sadk-referee-{name}-{}", std::process::id())).join("accounts.json");
        let _ = std::fs::remove_file(&path);
        let mut accounts = Accounts::open(path).unwrap();
        accounts.create("siedler", b"x").unwrap().unwrap();
        accounts.create("besucher", b"x").unwrap().unwrap();
        Server::test(DEFS, accounts)
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

    /// The referee messages (id, data) among the queued replies.
    fn referee_messages(s: &mut Session) -> Vec<(u16, Vec<u8>)> {
        sent(s)
            .into_iter()
            .filter(|m| m.name == "SendGameData")
            .map(|m| (lm::split_header(m.int("msg_type").unwrap() as u32).2, m.bytes("data").unwrap().to_vec()))
            .collect()
    }

    fn request(s: &Session, name: &str, fields: &[(&str, i64)]) -> Message {
        fields.iter().fold(s.message(name), |m, &(f, v)| m.with(f, Value::Int(v)))
    }

    #[test]
    fn assign_server_names_the_referee() {
        let server = server("assign");
        let mut s = Session::new(server.clone(), 1, Role::Lobby, "test".into());
        s.account = Some(1);
        s.handle_message(request(&s, "AssignServer", &[("server_type", 4), ("server_subtype", 4), ("ticket_id", 8)]));
        let out = sent(&mut s);
        let data = out.iter().find(|m| m.name == "GameServerData").expect("GameServerData");
        assert_eq!(data.int("server_id"), Some(SERVER_ID as i64));
        assert_eq!(data.int("port"), Some(17073));
        assert_eq!((data.int("server_type"), data.int("server_subtype"), data.int("ticket_id")), (Some(4), Some(4), Some(8)));

        s.handle_message(request(&s, "RequestConnectionData", &[("perm_id", 1), ("server_id", SERVER_ID as i64), ("ticket_id", 9)]));
        let out = sent(&mut s);
        assert_eq!((out[0].name.as_str(), out[0].int("port")), ("ConnectionData", Some(17073)));
    }

    /// Both players log in to the referee and register; the second
    /// registration starts the match for both with the same seed.
    #[test]
    fn registered_players_get_one_seed() {
        let server = server("register");
        let mut players: Vec<Session> = (1..=2).map(|i| Session::new(server.clone(), i, Role::Referee, "test".into())).collect();
        for (perm, s) in (1..).zip(players.iter_mut()) {
            s.token_login(perm, 3);
            let out = sent(s);
            assert_eq!((out[0].name.as_str(), out[0].int("id")), ("AddResult", Some(perm)));
            assert_eq!(out[1].int("msg_type"), Some(rm::id::LOGIN_SUCCESS as i64));
            assert_eq!(out[1].bytes("data"), Some(&lm::single_u32(perm as u32)[..]));
        }
        let mut reg = RegisterGame { game_id: 1000, map_name: "Karte".into(), ..Default::default() };
        reg.avatars[..2].copy_from_slice(&[11, 12]);
        let register = |s: &mut Session| {
            let msg = s.message("SendGameData").with("msg_type", Value::Int(0x3000 | rm::id::REGISTER_GAME as i64)).with("data", Value::Bytes(Some(reg.encode())));
            s.handle_message(msg);
        };
        register(&mut players[0]);
        assert_eq!(referee_messages(&mut players[0]), [(rm::id::REGISTER_GAME_ACK, rm::ack(1000, 0))]);
        register(&mut players[1]);
        let second = referee_messages(&mut players[1]);
        let first = referee_messages(&mut players[0]);
        assert_eq!(second[0], (rm::id::REGISTER_GAME_ACK, rm::ack(1000, 0)));
        assert_eq!(second[1].0, rm::id::REGISTER_GAME_RESULT);
        assert_eq!(first, [second[1].clone()]);
        let mut r = lm::BitReader::new(&first[0].1);
        assert_eq!((r.u32(), r.u32()), (Some(1000), Some(0)));
        assert!(r.u32().is_some());
    }
}

//! Lobby connections by account, for news a player gets without asking:
//! a friend coming online or going offline (`UserBuddyConn` with ticket 0).
//! The friend and ignore lists and the mail themselves live in `accounts`.

use crate::village::Event;
use sadk_lobby_proto::Message;
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Default)]
pub struct LobbyUsers {
    /// Connection id → (account perm id, sender).
    conns: HashMap<u32, (u32, UnboundedSender<Event>)>,
}

impl LobbyUsers {
    pub fn log_in(&mut self, conn: u32, account: u32, tx: UnboundedSender<Event>) {
        self.conns.insert(conn, (account, tx));
    }

    pub fn log_out(&mut self, conn: u32) {
        self.conns.remove(&conn);
    }

    /// Sends `msg` to every lobby connection of `account`; returns how many
    /// got it.
    pub fn send(&self, account: u32, msg: &Message) -> usize {
        self.conns.values().filter(|(a, _)| *a == account).filter(|(_, tx)| tx.send(Event::Lobby(msg.clone())).is_ok()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn reaches_every_connection_of_an_account() {
        let defs = sadk_lobby_proto::MsgDefs::parse("# Result\n[NETMSG_TYPE_42]\ntype=\"UNSHORT\"\nticket_id=\"UNLONG\"\n").unwrap();
        let msg = Message::new(&defs, "Result").unwrap();
        let mut users = LobbyUsers::default();
        let (a1, mut r1) = unbounded_channel();
        let (a2, mut r2) = unbounded_channel();
        let (b, mut rb) = unbounded_channel();
        users.log_in(1, 10, a1);
        users.log_in(2, 10, a2);
        users.log_in(3, 20, b);
        assert_eq!(users.send(10, &msg), 2);
        assert!(r1.try_recv().is_ok() && r2.try_recv().is_ok() && rb.try_recv().is_err());
        users.log_out(1);
        assert_eq!(users.send(10, &msg), 1);
        assert_eq!(users.send(30, &msg), 0);
    }
}

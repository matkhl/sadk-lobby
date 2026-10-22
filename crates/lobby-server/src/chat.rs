//! Chat channels ("cells") of the usercomm server: who is logged in to
//! usercomm, who is in which channel, and the senders to reach them. The
//! sessions build the cell messages; this only keeps membership and fans
//! messages out.

use crate::village::Event;
use sadk_lobby_proto::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use tokio::sync::mpsc::UnboundedSender;

struct Member {
    perm: u32,
    nick: String,
    tx: UnboundedSender<Event>,
}

#[derive(Default)]
pub struct Chat {
    /// Channel id → members keyed by connection id. Channels exist while
    /// someone is in them.
    channels: BTreeMap<u32, HashMap<u32, Member>>,
    /// Logged-in usercomm connections by connection id, for whispers.
    users: HashMap<u32, Member>,
}

impl Chat {
    /// A usercomm connection logged in as `perm`.
    pub fn log_in(&mut self, conn: u32, perm: u32, nick: &str, tx: UnboundedSender<Event>) {
        self.users.insert(conn, Member { perm, nick: nick.into(), tx });
    }

    /// Sends `msg` to every usercomm connection of `perm`; returns how many
    /// got it.
    pub fn send_to_user(&self, perm: u32, msg: &Cell) -> usize {
        self.users.values().filter(|m| m.perm == perm).filter(|m| m.tx.send(Event::Cell(msg.clone())).is_ok()).count()
    }

    /// Whether `perm` has a usercomm connection.
    pub fn is_online(&self, perm: u32) -> bool {
        self.users.values().any(|m| m.perm == perm)
    }

    /// The perm id a usercomm connection logged in as.
    pub fn user(&self, conn: u32) -> Option<u32> {
        self.users.get(&conn).map(|m| m.perm)
    }

    /// Puts a usercomm connection into a channel and returns the members
    /// that were there before it, as (perm id, nick).
    pub fn join(&mut self, cell: u32, conn: u32, perm: u32, nick: &str, tx: UnboundedSender<Event>) -> Vec<(u32, String)> {
        let members = self.channels.entry(cell).or_default();
        let before = members.iter().filter(|(&c, _)| c != conn).map(|(_, m)| (m.perm, m.nick.clone())).collect();
        members.insert(conn, Member { perm, nick: nick.into(), tx });
        before
    }

    /// Takes a connection out of a channel; returns its perm id if it was in.
    pub fn leave(&mut self, cell: u32, conn: u32) -> Option<u32> {
        let members = self.channels.get_mut(&cell)?;
        let member = members.remove(&conn)?;
        if members.is_empty() {
            self.channels.remove(&cell);
        }
        Some(member.perm)
    }

    /// Logs a connection out and takes it out of every channel; returns
    /// (channel, perm id) for each channel.
    pub fn leave_all(&mut self, conn: u32) -> Vec<(u32, u32)> {
        self.users.remove(&conn);
        let cells: Vec<u32> = self.channels.iter().filter(|(_, m)| m.contains_key(&conn)).map(|(&c, _)| c).collect();
        cells.into_iter().filter_map(|cell| self.leave(cell, conn).map(|perm| (cell, perm))).collect()
    }

    pub fn is_member(&self, cell: u32, conn: u32) -> bool {
        self.channels.get(&cell).is_some_and(|m| m.contains_key(&conn))
    }

    /// Sends `msg` to the members of `cell` that `to` accepts, given their
    /// connection id and perm id. Returns how many got it.
    pub fn send(&self, cell: u32, msg: &Cell, to: impl Fn(u32, u32) -> bool) -> usize {
        let Some(members) = self.channels.get(&cell) else { return 0 };
        members.iter().filter(|(&c, m)| to(c, m.perm)).filter(|(_, m)| m.tx.send(Event::Cell(msg.clone())).is_ok()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn drain(rx: &mut UnboundedReceiver<Event>) -> Vec<Event> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn tracks_members_per_channel() {
        let mut chat = Chat::default();
        let (a_tx, mut a) = unbounded_channel();
        let (b_tx, mut b) = unbounded_channel();
        assert!(chat.join(1, 10, 100, "a", a_tx.clone()).is_empty());
        chat.join(0, 10, 100, "a", a_tx.clone());
        assert_eq!(chat.join(1, 20, 200, "b", b_tx), [(100, "a".to_string())]);
        assert!(chat.is_member(1, 20) && !chat.is_member(0, 20));

        let msg = Cell::Status { cell: 1, ticket: 0, result: 0 };
        assert_eq!(chat.send(1, &msg, |conn, _| conn != 20), 1);
        assert_eq!(drain(&mut a), [Event::Cell(msg.clone())]);
        assert!(drain(&mut b).is_empty());

        // Users are found by perm id, in or out of channels.
        chat.log_in(10, 100, "a", a_tx);
        assert_eq!(chat.send_to_user(100, &msg), 1);
        assert_eq!(chat.send_to_user(200, &msg), 0);
        assert_eq!(drain(&mut a), [Event::Cell(msg.clone())]);

        let mut left = chat.leave_all(10);
        left.sort();
        assert_eq!(left, [(0, 100), (1, 100)]);
        assert_eq!(chat.send_to_user(100, &msg), 0);
        assert_eq!(chat.send(1, &msg, |_, _| true), 1);
        assert_eq!(chat.leave(1, 20), Some(200));
        assert_eq!(chat.leave(1, 20), None);
        assert!(chat.channels.is_empty());
    }
}

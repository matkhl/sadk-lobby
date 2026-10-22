//! Shared state of the lobby village: which avatars are in it, where they
//! stand, and the channels to tell each village session about the others.

use sadk_lobby_proto::cell::Cell;
use sadk_lobby_proto::lobby_message::{Location, Style};
use sadk_lobby_proto::Message;
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

/// What a session hears from the others: avatar news (village sessions)
/// and chat (usercomm sessions, see `chat`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// An avatar is in the village: sent once it reported its first position,
    /// and to a newcomer for everyone already there.
    Appeared { avatar: u32, style: Style, location: Location },
    Moved { avatar: u32, location: Location },
    Left { avatar: u32 },
    /// A chat channel message to pass on as it is.
    Cell(Cell),
    /// A lobby message to pass on (match list news, see `games`).
    Lobby(Message),
    /// A village message to send as it is (table games, see `tables`).
    Village { id: u16, data: Vec<u8> },
}

struct Member {
    avatar: u32,
    style: Style,
    /// None until the client sends its first message 2000; others only see
    /// the avatar from then on.
    location: Option<Location>,
    tx: UnboundedSender<Event>,
}

#[derive(Default)]
pub struct Village {
    /// Keyed by connection id.
    members: HashMap<u32, Member>,
}

impl Village {
    /// Adds a logged-in village connection and tells it about every avatar
    /// that already has a position.
    pub fn join(&mut self, conn: u32, avatar: u32, style: Style, tx: UnboundedSender<Event>) {
        for m in self.members.values() {
            if let Some(location) = m.location {
                let _ = tx.send(Event::Appeared { avatar: m.avatar, style: m.style.clone(), location });
            }
        }
        self.members.insert(conn, Member { avatar, style, location: None, tx });
    }

    /// Records a position from message 2000 and passes it on to the others.
    pub fn moved(&mut self, conn: u32, location: Location) {
        let Some(m) = self.members.get_mut(&conn) else { return };
        let event = match m.location.replace(location) {
            None => Event::Appeared { avatar: m.avatar, style: m.style.clone(), location },
            Some(_) => Event::Moved { avatar: m.avatar, location },
        };
        self.broadcast(conn, event);
    }

    pub fn leave(&mut self, conn: u32) {
        if let Some(m) = self.members.remove(&conn) {
            if m.location.is_some() {
                self.broadcast(conn, Event::Left { avatar: m.avatar });
            }
        }
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    fn broadcast(&self, from: u32, event: Event) {
        for (_, m) in self.members.iter().filter(|(&c, _)| c != from) {
            let _ = m.tx.send(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn drain(rx: &mut UnboundedReceiver<Event>) -> Vec<Event> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn style(name: &str) -> Style {
        Style { name: name.into(), ..Style::default() }
    }

    #[test]
    fn announces_after_first_position_and_on_leave() {
        let mut v = Village::default();
        let (a_tx, mut a) = unbounded_channel();
        let (b_tx, mut b) = unbounded_channel();
        v.join(1, 10, style("a"), a_tx);
        let here = Location { tick: 1, pos: [5, 6, 7], ..Location::default() };
        v.moved(1, here);
        v.join(2, 20, style("b"), b_tx);
        assert_eq!(drain(&mut b), [Event::Appeared { avatar: 10, style: style("a"), location: here }]);

        let there = Location { pos: [8, 6, 7], ..here };
        v.moved(2, here);
        v.moved(1, there);
        assert_eq!(drain(&mut a), [Event::Appeared { avatar: 20, style: style("b"), location: here }]);
        assert_eq!(drain(&mut b), [Event::Moved { avatar: 10, location: there }]);

        v.leave(1);
        assert_eq!(drain(&mut b), [Event::Left { avatar: 10 }]);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn silent_members_are_not_announced() {
        let mut v = Village::default();
        let (a_tx, mut a) = unbounded_channel();
        let (b_tx, _b) = unbounded_channel();
        v.join(1, 10, style("a"), a_tx);
        v.join(2, 20, style("b"), b_tx);
        v.leave(2);
        v.moved(3, Location::default());
        assert!(drain(&mut a).is_empty());
    }
}

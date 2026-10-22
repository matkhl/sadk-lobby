//! The referee: players about to start a match register with it (message
//! 3510, see `sadk_lobby_proto::referee`), and once everyone in the match
//! has, each gets the same random game seed, which makes the clients load
//! the match. The original server also checked results and handed out
//! rewards (chests, wagers); here every claim and result is accepted.

use crate::village::Event;
use sadk_lobby_proto::referee::RegisterGame;
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

/// Server id of the referee in `GameServerData` / `RequestConnectionData`
/// (the village is 1, matches start at 1000).
pub const SERVER_ID: u32 = 2;

#[derive(Default)]
pub struct Referee {
    games: HashMap<u32, Pending>,
}

/// A match some of whose players have registered.
struct Pending {
    /// Players the match was registered with (non-zero avatar slots).
    expected: usize,
    /// Referee connection and its channel, per registered player.
    players: Vec<(u32, UnboundedSender<Event>)>,
}

impl Referee {
    /// Registers connection `conn` for the match in `reg`. Once every player
    /// has registered, the match is dropped from the registry and the
    /// players' channels are returned, to send them the seed.
    pub fn register(&mut self, conn: u32, reg: &RegisterGame, tx: UnboundedSender<Event>) -> Option<Vec<UnboundedSender<Event>>> {
        let pending = self.games.entry(reg.game_id).or_insert_with(|| Pending { expected: 0, players: Vec::new() });
        pending.expected = pending.expected.max(reg.players()).max(1);
        pending.players.retain(|(c, _)| *c != conn);
        pending.players.push((conn, tx));
        if pending.players.len() < pending.expected {
            return None;
        }
        self.games.remove(&reg.game_id).map(|p| p.players.into_iter().map(|(_, tx)| tx).collect())
    }

    /// (registered, expected) players of a match still waiting.
    pub fn waiting(&self, game_id: u32) -> Option<(usize, usize)> {
        self.games.get(&game_id).map(|p| (p.players.len(), p.expected))
    }

    /// Drops a closed connection's registrations.
    pub fn leave(&mut self, conn: u32) {
        for pending in self.games.values_mut() {
            pending.players.retain(|(c, _)| *c != conn);
        }
        self.games.retain(|_, p| !p.players.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn reg(game_id: u32, avatars: &[u32]) -> RegisterGame {
        let mut r = RegisterGame { game_id, ..Default::default() };
        r.avatars[..avatars.len()].copy_from_slice(avatars);
        r
    }

    #[test]
    fn starts_once_every_player_registered() {
        let mut referee = Referee::default();
        let (tx, _rx) = unbounded_channel();
        let r = reg(1000, &[5, 9]);
        assert!(referee.register(1, &r, tx.clone()).is_none());
        // Registering twice does not count twice.
        assert!(referee.register(1, &r, tx.clone()).is_none());
        assert_eq!(referee.waiting(1000), Some((1, 2)));
        assert_eq!(referee.register(2, &r, tx.clone()).map(|p| p.len()), Some(2));
        assert_eq!(referee.waiting(1000), None);

        // A one-player match (against the AI) starts right away.
        assert_eq!(referee.register(3, &reg(1001, &[5]), tx).map(|p| p.len()), Some(1));
    }

    #[test]
    fn a_dropped_connection_leaves_its_match() {
        let mut referee = Referee::default();
        let (tx, _rx) = unbounded_channel();
        referee.register(1, &reg(1000, &[5, 9]), tx);
        referee.leave(1);
        assert_eq!(referee.waiting(1000), None);
    }
}

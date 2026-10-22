//! Referee server payloads (`LobbyRefereeServerConnection` in the client).
//!
//! The referee is a game server of type 4, subtype 4. The client is told
//! about it in reply to `AssignServer {4, 4}` at login and keeps a
//! connection to it. Before a match begins, every player sends
//! [`id::REGISTER_GAME`]. Once all have, the referee answers each with
//! [`id::REGISTER_GAME_RESULT`] and a shared random seed, and the match
//! loads. The messages travel as `SendGameData` with the same bit stream as
//! the village messages ([`crate::lobby_message`]); the client sends them on
//! channel 3.

use crate::lobby_message::{BitReader, BitWriter};

/// Message ids (senders `sub_478940`.., dispatcher `sub_47A280`).
pub mod id {
    /// Client → server: `{GameID, MapGUID, ActorID, ChestID}`.
    pub const CLAIM_CHEST: u16 = 3500;
    /// Server → client: `{GameID, Result}`.
    pub const CLAIM_CHEST_ACK: u16 = 3501;
    /// Client → server: [`super::RegisterGame`].
    pub const REGISTER_GAME: u16 = 3510;
    /// Server → client: `{GameID, Result}`; the client then waits for the
    /// other players.
    pub const REGISTER_GAME_ACK: u16 = 3511;
    /// Server → client: [`super::register_game_result`].
    pub const REGISTER_GAME_RESULT: u16 = 3512;
    /// Client → server at the end of a match:
    /// `{GameID, MapGUID, MapSettings, Winner}`.
    pub const FINISH_GAME: u16 = 3520;
    /// Server → client: `{GameID, Result}`.
    pub const FINISH_GAME_ACK: u16 = 3521;
    /// Server → client: `{GameID, Result}`, then `FailReason` if Result ≠ 0.
    pub const FINISH_GAME_RESULT: u16 = 3522;
    /// Server → client after the login: `{PermID}`, which must be the
    /// client's own.
    pub const LOGIN_SUCCESS: u16 = 3530;
    /// Server → client: `{PermID}`.
    pub const LOGIN_FAILED: u16 = 3531;
    /// Client → server: `{GameID, MapGUID}`.
    pub const GIVE_UP_GAME: u16 = 3540;
    /// Server → client: `{GameID, Result}`.
    pub const GIVE_UP_ACK: u16 = 3541;
}

/// Message 3510 (`sub_478B10`): a player registers for the match about to
/// start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegisterGame {
    pub game_id: u32,
    pub map_guid: [u8; 16],
    pub map_name: String,
    pub map_settings: String,
    pub ranked: bool,
    pub wager: u32,
    pub ai_players: u8,
    /// Up to six players; unused slots are 0.
    pub avatars: [u32; 6],
}

impl RegisterGame {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut r = BitReader::new(data);
        let game_id = r.u32()?;
        let mut map_guid = [0; 16];
        for b in &mut map_guid {
            *b = r.bits(8)? as u8;
        }
        let map_name = r.str()?;
        let map_settings = r.str()?;
        let ranked = r.bool()?;
        let wager = r.u32()?;
        let ai_players = r.bits(8)? as u8;
        let mut avatars = [0; 6];
        for a in &mut avatars {
            *a = r.u32()?;
        }
        Some(Self { game_id, map_guid, map_name, map_settings, ranked, wager, ai_players, avatars })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u32(self.game_id);
        for &b in &self.map_guid {
            w.bits(b as u32, 8);
        }
        w.str(&self.map_name).str(&self.map_settings).bool(self.ranked).u32(self.wager).bits(self.ai_players as u32, 8);
        for &a in &self.avatars {
            w.u32(a);
        }
        w.finish()
    }

    /// Human players in the match (the non-zero avatar slots).
    pub fn players(&self) -> usize {
        self.avatars.iter().filter(|&&a| a != 0).count()
    }
}

/// The game id every client→server referee message starts with.
pub fn game_id(data: &[u8]) -> Option<u32> {
    BitReader::new(data).u32()
}

/// `{GameID, Result}`: the acknowledgements 3501, 3511, 3521 and 3541.
pub fn ack(game_id: u32, result: u32) -> Vec<u8> {
    BitWriter::new().u32(game_id).u32(result).finish()
}

/// Message 3512: `{GameID, Result}`, then the game seed on success or a
/// failure text. On success the client starts loading the match.
pub fn register_game_result(game_id: u32, outcome: Result<u32, &str>) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u32(game_id);
    match outcome {
        Ok(seed) => w.u32(0).u32(seed),
        Err(reason) => w.u32(1).str(reason),
    };
    w.finish()
}

/// Message 3522: `{GameID, Result = 0}`.
pub fn finish_game_result(game_id: u32) -> Vec<u8> {
    ack(game_id, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_game_round_trips() {
        let reg = RegisterGame {
            game_id: 1000,
            map_guid: *b"0123456789abcdef",
            map_name: "Karte".into(),
            map_settings: "a=1".into(),
            ranked: false,
            wager: 0,
            ai_players: 2,
            avatars: [5, 9, 0, 0, 0, 0],
        };
        let data = reg.encode();
        assert_eq!(RegisterGame::parse(&data), Some(reg.clone()));
        assert_eq!(game_id(&data), Some(1000));
        assert_eq!(reg.players(), 2);
        assert_eq!(RegisterGame::parse(&data[..data.len() - 1]), None);
    }

    #[test]
    fn register_game_result_carries_the_seed() {
        let data = register_game_result(7, Ok(0xdeadbeef));
        let mut r = BitReader::new(&data);
        assert_eq!((r.u32(), r.u32(), r.u32()), (Some(7), Some(0), Some(0xdeadbeef)));

        let data = register_game_result(7, Err("nein"));
        let mut r = BitReader::new(&data);
        assert_eq!((r.u32(), r.u32(), r.str()), (Some(7), Some(1), Some("nein".into())));
    }
}

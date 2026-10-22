//! What token logins are checked against: the session key handed out at
//! each lobby login (`SessionKey`) and the one-time TANs from
//! `ConnectionData`.
//!
//! The client seals a `SendToken` token with its session key (see
//! `sadk_lobby_proto::auth::Token`) on every usercomm, village and referee
//! connection. A key stays valid while the lobby connection that got it is
//! open and for `grace` after it closes; the retail client keeps that
//! connection for the whole visit, matches included. A new login on the
//! same connection replaces the connection's key. One account may hold
//! several keys (logged in from two places).
//!
//! A TAN (`TANLogin {perm_id, nonce}`) is the nonce of a `ConnectionData`
//! the account asked for; it is good once, for `TAN_LIFETIME`, on the server
//! it was asked for.

use sadk_lobby_proto::auth::{self, KEY_LEN};
use std::time::{Duration, Instant};

/// How long a key outlives its lobby connection, so a client that lost the
/// lobby for a moment can still reach the village and the referee.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(10 * 60);
/// How long a TAN from ConnectionData can be used.
pub const TAN_LIFETIME: Duration = Duration::from_secs(2 * 60);

struct SessionKey {
    account: u32,
    /// Lobby connection that logged in.
    conn: u32,
    key: [u8; KEY_LEN],
    /// When that connection closed.
    closed: Option<Instant>,
}

struct Tan {
    account: u32,
    server: u32,
    nonce: Vec<u8>,
    issued: Instant,
}

pub struct Tokens {
    keys: Vec<SessionKey>,
    tans: Vec<Tan>,
    grace: Duration,
}

impl Default for Tokens {
    fn default() -> Self {
        Self::new(DEFAULT_GRACE)
    }
}

impl Tokens {
    pub fn new(grace: Duration) -> Self {
        Self { keys: Vec::new(), tans: Vec::new(), grace }
    }

    /// A fresh session key for `account`, logged in on lobby connection `conn`.
    pub fn issue(&mut self, account: u32, conn: u32) -> Result<[u8; KEY_LEN], auth::AuthError> {
        let key = auth::new_session_key()?;
        self.keys.retain(|k| k.conn != conn);
        self.keys.push(SessionKey { account, conn, key, closed: None });
        Ok(key)
    }

    /// The lobby connection `conn` closed: its key expires after the grace time.
    pub fn lobby_closed(&mut self, conn: u32, now: Instant) {
        for k in self.keys.iter_mut().filter(|k| k.conn == conn && k.closed.is_none()) {
            k.closed = Some(now);
        }
    }

    fn expire(&mut self, now: Instant) {
        let grace = self.grace;
        self.keys.retain(|k| k.closed.is_none_or(|t| now.duration_since(t) < grace));
        self.tans.retain(|t| now.duration_since(t.issued) < TAN_LIFETIME);
    }

    /// The account's keys that are still valid.
    pub fn keys(&mut self, account: u32, now: Instant) -> Vec<[u8; KEY_LEN]> {
        self.expire(now);
        self.keys.iter().filter(|k| k.account == account).map(|k| k.key).collect()
    }

    /// A one-time TAN for `account` to log in to `server` with.
    pub fn issue_tan(&mut self, account: u32, server: u32, now: Instant) -> Result<Vec<u8>, auth::AuthError> {
        self.expire(now);
        let nonce = auth::new_session_key()?[..16].to_vec();
        self.tans.push(Tan { account, server, nonce: nonce.clone(), issued: now });
        Ok(nonce)
    }

    /// Uses up a TAN; true when `account` was given it for `server` and it
    /// has not expired or been used.
    pub fn take_tan(&mut self, account: u32, server: u32, nonce: &[u8], now: Instant) -> bool {
        self.expire(now);
        let found = self.tans.iter().position(|t| t.account == account && t.server == server && t.nonce == nonce);
        found.map(|i| self.tans.swap_remove(i)).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_outlive_their_lobby_connection_by_the_grace_time() {
        let mut tokens = Tokens::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let first = tokens.issue(1, 10).unwrap();
        let second = tokens.issue(1, 11).unwrap();
        assert_eq!(tokens.keys(1, t0), [first, second]);
        assert!(tokens.keys(2, t0).is_empty());

        tokens.lobby_closed(10, t0);
        assert_eq!(tokens.keys(1, t0 + Duration::from_secs(59)), [first, second]);
        assert_eq!(tokens.keys(1, t0 + Duration::from_secs(61)), [second]);
    }

    #[test]
    fn a_new_login_on_the_same_connection_replaces_the_key() {
        let mut tokens = Tokens::default();
        let old = tokens.issue(1, 10).unwrap();
        let new = tokens.issue(1, 10).unwrap();
        assert_ne!(old, new);
        assert_eq!(tokens.keys(1, Instant::now()), [new]);
    }

    #[test]
    fn tans_work_once_for_their_server() {
        let mut tokens = Tokens::default();
        let t0 = Instant::now();
        let tan = tokens.issue_tan(1, 2, t0).unwrap();
        assert!(!tokens.take_tan(1, 1, &tan, t0), "another server");
        assert!(!tokens.take_tan(3, 2, &tan, t0), "another account");
        assert!(tokens.take_tan(1, 2, &tan, t0));
        assert!(!tokens.take_tan(1, 2, &tan, t0), "used up");

        let late = tokens.issue_tan(1, 2, t0).unwrap();
        assert!(!tokens.take_tan(1, 2, &late, t0 + TAN_LIFETIME));
    }
}

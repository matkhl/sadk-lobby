//! sadk-lobby: a replacement for the shut-down SADK lobby server.
//!
//! Point the game at it in `data/lobby/config/LobbySettings.ini`
//! (`Host = "127.0.0.1"`, `Port = 7070`) and run:
//!
//! ```text
//! sadk-lobby --msgdefs "<install>/bin/msgdefs.ini" [--listen 127.0.0.1:7070] [--data lobby-data]
//!     [--public-host lobby.example.org] [--token-grace 10]
//! ```

mod accounts;
mod address;
mod chat;
mod games;
mod referee;
mod session;
mod social;
mod tables;
mod tokens;
mod village;

use accounts::Accounts;
use sadk_lobby_proto::MsgDefs;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{info, warn};

pub struct Server {
    pub defs: MsgDefs,
    pub accounts: Mutex<Accounts>,
    pub motd: String,
    /// Lobby village server address, listed in GameServerData (lobby port + 1).
    pub village: SocketAddr,
    /// Usercomm (chat) server address, handed out in UsercommServerData (lobby port + 2).
    pub usercomm: SocketAddr,
    /// Referee server address, handed out for AssignServer {4, 4} (lobby port + 3).
    pub referee: SocketAddr,
    /// Avatars in the lobby village, shared by the village sessions.
    pub world: Mutex<village::Village>,
    /// Table games in the village taverns, shared by the village sessions.
    pub tables: Mutex<tables::Tables>,
    /// Chat channel members, shared by the usercomm sessions.
    pub chat: Mutex<chat::Chat>,
    /// Open matches hosted by players, shared by the lobby sessions.
    pub games: Mutex<games::Games>,
    /// Matches waiting for all their players to register at the referee.
    pub referee_games: Mutex<referee::Referee>,
    /// Session keys from the lobby logins and TANs from ConnectionData, which
    /// token logins must match.
    pub tokens: Mutex<tokens::Tokens>,
    /// Lobby connections by account, for friend status news.
    pub lobby_users: Mutex<social::LobbyUsers>,
    /// The server's address on the internet (`--public-host`, resolved),
    /// handed to players who connect from outside the server's own network.
    pub public_ip: RwLock<Option<IpAddr>>,
}

impl Server {
    /// A server whose lobby listens on `lobby`; the village, usercomm and the
    /// referee listen on the next three ports.
    pub fn new(defs: MsgDefs, accounts: Accounts, motd: String, lobby: SocketAddr, grace: Duration) -> Self {
        let port = |n: u16| SocketAddr::new(lobby.ip(), lobby.port() + n);
        Self {
            defs,
            accounts: Mutex::new(accounts),
            motd,
            village: port(1),
            usercomm: port(2),
            referee: port(3),
            world: Mutex::default(),
            tables: Mutex::default(),
            chat: Mutex::default(),
            games: Mutex::default(),
            referee_games: Mutex::default(),
            tokens: Mutex::new(tokens::Tokens::new(grace)),
            lobby_users: Mutex::default(),
            public_ip: RwLock::new(None),
        }
    }

    /// A server on 127.0.0.1:17070 for the unit tests.
    #[cfg(test)]
    pub fn test(defs: &str, accounts: Accounts) -> Arc<Self> {
        let defs = MsgDefs::parse(defs).unwrap();
        Arc::new(Self::new(defs, accounts, String::new(), "127.0.0.1:17070".parse().unwrap(), tokens::DEFAULT_GRACE))
    }
}

/// Which service a listener provides; all of them speak the same framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Lobby,
    Village,
    Usercomm,
    Referee,
}

struct Options {
    listen: String,
    msgdefs: PathBuf,
    data: PathBuf,
    /// Name or address of the server on the internet (see `Server::public_ip`).
    public_host: Option<String>,
    /// How long a session key stays valid after its lobby connection closes.
    token_grace: Duration,
}

const USAGE: &str = "usage: sadk-lobby --msgdefs <path to bin/msgdefs.ini> [--listen 127.0.0.1:7070] [--data lobby-data] [--public-host <name or ip>] [--token-grace <minutes>]";
/// How often `--public-host` is looked up again (for dynamic DNS names).
const RESOLVE_EVERY: Duration = Duration::from_secs(5 * 60);
const DEFAULT_MOTD: &str = "Willkommen, %name%! Dies ist ein lokaler Lobby-Server.";

fn options() -> Result<Options, String> {
    let mut opts = Options {
        listen: "127.0.0.1:7070".into(),
        msgdefs: PathBuf::new(),
        data: "lobby-data".into(),
        public_host: None,
        token_grace: tokens::DEFAULT_GRACE,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value\n{USAGE}"));
        match arg.as_str() {
            "--listen" => opts.listen = value()?,
            "--msgdefs" => opts.msgdefs = value()?.into(),
            "--data" => opts.data = value()?.into(),
            "--public-host" => opts.public_host = Some(value()?),
            "--token-grace" => {
                let minutes: u64 = value()?.parse().map_err(|e| format!("--token-grace: {e}"))?;
                opts.token_grace = Duration::from_secs(minutes * 60);
            }
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown argument {arg}\n{USAGE}")),
        }
    }
    if opts.msgdefs.as_os_str().is_empty() {
        return Err(USAGE.into());
    }
    Ok(opts)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let opts = options()?;
    let text = std::fs::read_to_string(&opts.msgdefs).map_err(|e| format!("{}: {e}", opts.msgdefs.display()))?;
    let defs = MsgDefs::parse(&text)?;
    for needed in [
        "Result",
        "RequestLogin",
        "RequestCreateAccount",
        "RequestMOTD",
        "MOTD",
        "CheckVersion",
        "StartAuthenticateSession",
        "AckAuthenticateSession",
        "AuthenticateUser",
        "SelfRegistration",
        "SessionKey",
        "PropertyData",
        "AddResult",
        "UserCharConn",
        "CharacterData",
        "UsercommServerData",
        "AckValidateTokenSession",
        "RequestJoinChannel",
        "ChatMessage",
        "UserJoinedChannel",
        "UserLeftChannel",
        "WhisperChatMessage",
        "PrivateChatMessage",
    ] {
        defs.by_name(needed).ok_or_else(|| format!("msgdefs has no {needed}"))?;
    }
    let motd = std::fs::read_to_string(opts.data.join("motd.txt")).unwrap_or_else(|_| DEFAULT_MOTD.into());
    let accounts = Accounts::open(opts.data.join("accounts.json")).map_err(|e| format!("accounts: {e}"))?;
    info!(messages = defs.len(), accounts = accounts.len(), "loaded");

    let lobby = TcpListener::bind(&opts.listen).await.map_err(|e| format!("bind {}: {e}", opts.listen))?;
    let base = lobby.local_addr().map_err(|e| format!("bind {}: {e}", opts.listen))?;
    let server = Arc::new(Server::new(defs, accounts, motd, base, opts.token_grace));
    let (village, usercomm, referee) = (server.village, server.usercomm, server.referee);
    let village_listener = TcpListener::bind(village).await.map_err(|e| format!("bind {village}: {e}"))?;
    let usercomm_listener = TcpListener::bind(usercomm).await.map_err(|e| format!("bind {usercomm}: {e}"))?;
    let referee_listener = TcpListener::bind(referee).await.map_err(|e| format!("bind {referee}: {e}"))?;
    info!("lobby on {base}, village on {village}, usercomm on {usercomm}, referee on {referee}");

    if let Some(host) = opts.public_host {
        let ip = resolve(&host).ok_or_else(|| format!("--public-host {host}: no IPv4 address"))?;
        info!("public address {ip} ({host})");
        *server.public_ip.write().unwrap() = Some(ip);
        tokio::spawn(follow_public_host(host, server.clone()));
    }
    // The timed parts of the table games (dice phases).
    let ticker = server.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            every.tick().await;
            ticker.tables.lock().unwrap().tick();
        }
    });
    let next_id = Arc::new(AtomicU32::new(1));
    tokio::spawn(accept(village_listener, Role::Village, server.clone(), next_id.clone()));
    tokio::spawn(accept(usercomm_listener, Role::Usercomm, server.clone(), next_id.clone()));
    tokio::spawn(accept(referee_listener, Role::Referee, server.clone(), next_id.clone()));
    accept(lobby, Role::Lobby, server, next_id).await;
    Ok(())
}

/// The first IPv4 address of `host` (a name or an address); the client
/// only takes IPv4 addresses.
fn resolve(host: &str) -> Option<IpAddr> {
    (host, 0).to_socket_addrs().ok()?.map(|a| a.ip()).find(IpAddr::is_ipv4)
}

/// Looks `--public-host` up again now and then, so a dynamic DNS name that
/// moves to a new address is followed.
async fn follow_public_host(host: String, server: Arc<Server>) {
    loop {
        tokio::time::sleep(RESOLVE_EVERY).await;
        let lookup = host.clone();
        let Ok(Some(ip)) = tokio::task::spawn_blocking(move || resolve(&lookup)).await else {
            warn!("--public-host {host} did not resolve; keeping the last address");
            continue;
        };
        let old = server.public_ip.write().unwrap().replace(ip);
        if old != Some(ip) {
            info!("public address now {ip} ({host})");
        }
    }
}

async fn accept(listener: TcpListener, role: Role, server: Arc<Server>, next_id: Arc<AtomicU32>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                warn!("accept failed: {e}");
                continue;
            }
        };
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let _ = stream.set_nodelay(true);
        info!(conn = id, ?role, %peer, "connected");
        let mut session = session::Session::new(server.clone(), id, role, peer.to_string());
        if let Ok(local) = stream.local_addr() {
            session = session.reached_at(peer.ip(), local.ip());
        }
        tokio::spawn(async move {
            match session.run(stream).await {
                Ok(()) => info!(conn = id, "disconnected"),
                Err(e) => info!(conn = id, "connection closed: {e}"),
            }
        });
    }
}

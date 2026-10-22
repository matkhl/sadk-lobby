# sadk-lobby

A replacement lobby server for **Die Siedler – Aufbruch der Kulturen**
(*The Settlers: Rise of Cultures*, 2008), written in Rust. The game's online
lobby went offline with the original servers; sadk-lobby speaks the game's
own protocol (tincat3), so the unmodified retail client can log in, walk
around the 3D lobby village, chat, and set up multiplayer matches again.

| Crate | What it is |
|---|---|
| `crates/lobby-proto` (`sadk-lobby-proto`) | the wire format: frames, CRC, the `msgdefs.ini`-driven message codec, the login key exchange (ECDH P-521, Twofish), village bit-stream messages, chat channel messages; no I/O |
| `crates/lobby-server` (`sadk-lobby`) | the server: tokio TCP lobby, village, usercomm (chat, friends) and referee services in one process, JSON account storage; `examples/village_bot.rs` is a scripted second player for testing |

## What works

Tested with the German retail client, build 34688:

- account creation on first login, login, message of the day
- avatar creation and selection
- the 3D lobby village: players see each other walk around
- chat (GLOBAL and local tabs), whispers (`/tell`), "[SYSTEM]" notices
- friend and ignore lists with online status, letters at the letterbox
- matches: host, list ("SPIEL SUCHEN"), join, ready, and the start through
  the referee; both players load the map with their avatars' tribes
- login from the internet to a server on Linux

Known gaps: avatar items and stats are not sent to other players; the MINIGAME chat tab is
not handled; a letter's sender shows "???" until the reader has seen that
player (a client limitation); RANDOM tribes are never resolved by the game,
so a character without a valid tribe crashes map loading. The rankings
(Hall of Fame) are not implemented.

## Build

With a current stable Rust toolchain:

```text
cargo build --release
cargo test
```

`target/release/sadk-lobby` is the server. It builds on Linux and Windows.
For a static Linux binary see [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

## Run

The server needs `msgdefs.ini` from the `bin` folder of a retail install
(the message schema; nothing from the game is included in this
repository):

```text
sadk-lobby --msgdefs "<install>/bin/msgdefs.ini" --listen 0.0.0.0:17070 --data lobby-data --public-host <your server's DNS name or IPv4>
```

- The lobby listens on the `--listen` port and the next three: village
  (+1), usercomm (+2) and referee (+3). Open all four (TCP).
- `--data` is where `accounts.json` and an optional `motd.txt` live.
- `--public-host` is the address handed to players from the internet; see
  [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for when it is needed.
- `RUST_LOG=sadk_lobby=debug` adds hex dumps to the log.

For a permanent server (systemd unit, firewall, backups, Windows service)
see [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md); `deploy/install-lobby.sh` sets
it all up on a systemd Linux.

## Pointing the game at a server

Each player edits `data\lobby\config\LobbySettings.ini` in the game's
install folder:

```ini
[LobbyServer]
Host = "lobby.example.org"
Port = 17070
```

`Host` may be a DNS name or an IPv4 address; `Port` is the server's
`--listen` port. Then, in the game:

- Log in with any new name and a password: the first login creates the
  account (the game's "Registrieren" button opened a web page of the old
  service and does nothing now). The serial number field must not be
  empty, but any text works.
- Create an avatar, pick it, and enter the village.

Matches are direct connections between the players' PCs: the host's game
must be reachable on TCP 5479. How to arrange that (port forwarding, a
virtual LAN, a tunnel through the server, and the `!adresse` chat command)
is in [docs/PLAYING-ONLINE.md](docs/PLAYING-ONLINE.md).

## Testing without a second player

`examples/village_bot.rs` logs in like the game (real key exchange and
tokens), walks an avatar in a circle, joins the chat channels and talks:

```text
cargo run --release --example village_bot -- --msgdefs "<install>/bin/msgdefs.ini" --lobby lobby.example.org:17070 --village lobby.example.org:17071 --usercomm lobby.example.org:17072 --login besucher:secret --perm <character id>
```

The account is created on its first login. `--perm` is a character id
from `accounts.json` (an account id also works, but then no avatar is shown
in the village). `--say`, `--every`, `--channels`, `--whisper-to`/`--whisper`
and `--mail-to`/`--mail` control what it says and sends; it prints what it
receives.

Two game instances on one PC: `SADK.exe` allows only one (it exits when the
named mutex `sadk` exists); closing that handle in the first instance (for
example with Process Explorer) lets a second one start.

The client writes protocol logs when started with
`SADK.exe -log_info -log_path <folder>` (`LobbyComm.log`, `comm.log`): they
name every message it sends, receives or drops.

## Documentation

- [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md): running a public server
- [docs/PLAYING-ONLINE.md](docs/PLAYING-ONLINE.md): matches over the internet
- [docs/PROTOCOL.md](docs/PROTOCOL.md): protocol notes (framing, login,
  tokens, village, chat, friends, letters, matches, referee)

## Credits

[S2-modders/Settlers-AdK-lobby-emulator](https://github.com/S2-modders/Settlers-AdK-lobby-emulator)
(public domain, C#) was the reference for the framing and the login
cryptography and provided the recorded packets used as test vectors.

*Die Siedler – Aufbruch der Kulturen* is a trademark of Ubisoft. This
project is not affiliated with Ubisoft or Funatics; it contains no game
files.

## License

Public domain ([Unlicense](UNLICENSE)).

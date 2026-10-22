# Running a public server

How to run sadk-lobby for a group or a community on a machine of your own:
a rented Linux server (VPS), or a PC at home behind a router.
`lobby.example.org` stands for your server's DNS name or IPv4 address
throughout.

## What the server needs

- The `sadk-lobby` binary. Build it on the server with Rust installed
  (`cargo build --release`, output in `target/release/sadk-lobby`). Or
  build a static Linux binary on any machine and copy it over:

  ```text
  rustup target add x86_64-unknown-linux-musl
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld cargo build --release --target x86_64-unknown-linux-musl
  ```

  (`aarch64-unknown-linux-musl` works the same way for ARM servers.) The
  result in `target/<target>/release/sadk-lobby` runs on any Linux of that
  architecture, with nothing else installed. It also builds for Windows.
- `bin/msgdefs.ini` from a retail install: the message schema. Nothing else
  of the game is used, and it is not part of this repository.
- A data folder (`lobby-data`) for `accounts.json` (accounts, password
  hashes, characters, friend and ignore lists, letters, match addresses)
  and an optional `motd.txt`. The lobby rewrites `accounts.json` on every
  change through a temporary file and a rename, so the file on disk is
  always complete.

Small machines are enough: the lobby is a single process that uses a few MB
of memory.

## Ports

| Port | Proto | Who connects | Open it on |
|---|---|---|---|
| 17070 | TCP | game: lobby (login, characters, match list, letters) | the server |
| 17071 | TCP | game: village (the 3D lobby) | the server |
| 17072 | TCP | game: usercomm (chat, whispers, friends) | the server |
| 17073 | TCP | game: referee (match start) | the server |
| 5479 | TCP | game: the match itself, to the **host's** PC | each host's router, see [PLAYING-ONLINE.md](PLAYING-ONLINE.md) |
| 5479 | TCP | game: a match tunnelled through the server | the server, only with `--match-forward` |

The four lobby ports are always `--listen` port + 0 to 3. The game's own
default was 7070-7073; any base port works, as long as the players' game
names the same one. 17070 avoids 7070, which other software (AnyDesk, for
one) often uses.

## Command line

```text
sadk-lobby --msgdefs /opt/sadk/msgdefs.ini --listen 0.0.0.0:17070 --data /var/lib/sadk/lobby-data --public-host lobby.example.org
```

- `--listen 0.0.0.0:17070` takes connections on every interface (the
  default, 127.0.0.1, serves only the machine itself).
- `--public-host` is the address the lobby hands to players from the
  internet for the village, usercomm and the referee. Set it whenever the
  server is reachable from the internet: on a rented server with its own
  public address it changes nothing, while behind a router or a cloud 1:1
  NAT (the machine only sees a private address) it is required. A DNS name
  is looked up again every five minutes, so a dynamic DNS name works.
  Players on the server's LAN or virtual LAN keep getting the LAN or
  virtual address.
- `--token-grace <minutes>` (10): how long a login's session key keeps
  working after the lobby connection that got it closed.
- `RUST_LOG=sadk_lobby=info` (default) logs connections, logins, chat
  channels, letters and matches to stdout; `RUST_LOG=sadk_lobby=debug` adds
  hex dumps.
- `lobby-data/motd.txt`, if present, replaces the message of the day
  (`%name%` becomes the account name). It is sent as 8-bit Latin-1, up to
  255 characters.

## Linux (systemd), with the install script

From a checkout of this repository on the server, after
`cargo build --release` (or with a copied binary and `--binary`):

```text
sudo sh deploy/install-lobby.sh --msgdefs /path/to/msgdefs.ini --public-host lobby.example.org
```

It creates the user `sadk`, puts the binary and `msgdefs.ini` in
`/opt/sadk`, keeps data in `/var/lib/sadk/lobby-data`, installs and starts
`sadk-lobby.service`, adds a backup job for `accounts.json` (every 30
minutes, kept 14 days, in `/var/lib/sadk/backup`), and opens TCP
17070-17073 in ufw or firewalld when one of them is active. A firewall at
the hosting provider must allow those ports as well. Run it again to update
the binary; the accounts stay.

`--match-forward` also installs `sadk-match-forward.service`, a socat relay
from the server's TCP 5479 to 127.0.0.1:15479, for a match host that
tunnels its game port to the server over SSH (see
[PLAYING-ONLINE.md](PLAYING-ONLINE.md)).

Daily use:

```text
systemctl status sadk-lobby
sudo journalctl -u sadk-lobby -f          # live log
sudo journalctl -u sadk-lobby --since today
sudo systemctl restart sadk-lobby         # everyone has to log in again
```

To see debug output for a while:

```text
sudo systemctl edit sadk-lobby            # add: [Service] / Environment=RUST_LOG=sadk_lobby=debug
sudo systemctl restart sadk-lobby
```

The unit the script writes, for reference or for a manual setup:

```ini
# /etc/systemd/system/sadk-lobby.service
[Unit]
Description=Die Siedler - Aufbruch der Kulturen lobby (sadk-lobby)
After=network-online.target
Wants=network-online.target

[Service]
User=sadk
Group=sadk
WorkingDirectory=/var/lib/sadk
Environment=RUST_LOG=sadk_lobby=info
ExecStart=/opt/sadk/sadk-lobby --msgdefs /opt/sadk/msgdefs.ini --listen 0.0.0.0:17070 --data /var/lib/sadk/lobby-data --public-host lobby.example.org
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/sadk
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

## Windows

The binary is not a native Windows service, so run it under a service
wrapper such as [NSSM](https://nssm.cc/) (or WinSW). From an elevated
PowerShell, with the files in `C:\sadk`:

```text
nssm install sadk-lobby C:\sadk\sadk-lobby.exe --msgdefs C:\sadk\msgdefs.ini --listen 0.0.0.0:17070 --data C:\sadk\lobby-data --public-host lobby.example.org
nssm set sadk-lobby AppDirectory C:\sadk
nssm set sadk-lobby AppEnvironmentExtra RUST_LOG=sadk_lobby=info
nssm set sadk-lobby AppStdout C:\sadk\logs\sadk-lobby.log
nssm set sadk-lobby AppStderr C:\sadk\logs\sadk-lobby.log
nssm set sadk-lobby AppRotateFiles 1
nssm set sadk-lobby AppRotateBytes 10485760
nssm start sadk-lobby

New-NetFirewallRule -DisplayName "SADK lobby" -Direction Inbound -Protocol TCP -LocalPort 17070-17073 -Action Allow
```

On a PC at home, the router must also forward TCP 17070-17073 to that PC,
and `--public-host` must name the router's public address or a dynamic DNS
name for it.

## Checking it from outside

From a PC elsewhere (PowerShell):

```text
Test-NetConnection lobby.example.org -Port 17070
```

must say `TcpTestSucceeded : True` (likewise 17071-17073). A full protocol
check without the game: the `village_bot` example logs in, enters the
village and talks in the chat (see the README).

## Backups

`lobby-data/accounts.json` is the only state; everything else (matches,
who is in the village, chat, session keys) lives in memory and is gone
after a restart (players just log in again). Because the lobby replaces
the file with a rename, a plain copy taken while it runs is always a
complete, consistent file. The install script copies it every 30 minutes;
keep a copy off the machine as well. To restore, stop sadk-lobby, put the
copy in place as `accounts.json`, and start it again. The file holds
salted SHA-256 password hashes, not passwords, but treat it as private: it
names every player.

## Security notes

- Accounts are created by the first login with a new name, so anyone who
  can reach the lobby port can make one. The password is the only
  protection of an account; there is no reset except editing
  `accounts.json` (remove the account and let the player log in again).
- Every village, usercomm and referee login is checked against the session
  key of a lobby login; a lobby connection is closed after five wrong
  passwords.
- The game sends the password inside the encrypted login, but the lobby
  protocol itself is the game's: anyone who can watch the traffic can read
  chat and letters.
- Run the lobby as an unprivileged user and keep `lobby-data` writable only
  by it (the install script does both).

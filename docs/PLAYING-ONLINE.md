# Playing over the internet

The lobby, the village, chat and the referee run on the server, so every
player only connects out to it. **The match itself does not**: the host's
game listens on a TCP port and every other player connects straight to it
(a star; tincat3's "Net Driver"). The game has no NAT traversal at all (no
UPnP, no hole punching, no relay; it never learns its public address), so
the host's PC must be reachable from the other players.

What each player needs:

| Who | Port | Proto | What for |
|---|---|---|---|
| everyone | the server's 17070-17073 | TCP, outgoing | lobby, village, chat, referee (nothing to open) |
| the host of a match | **5479** | TCP, **incoming** | the match: the others connect to it |
| everyone else in a match | 5479 | TCP, incoming | only if they may become host (see below) |

- 5479 is `gamePort` in `[Basics]` of `data\game\settings\network.ini`
  in the install. A host who changes it must forward the new port; the
  game announces its port to the lobby and the joiners use that.
- UDP is not used over the internet. The game's UDP ports 6582/6583 are
  only the LAN game browser (broadcasts), and the Hall of Fame (HTTP port
  7012) went with the old servers.
- **Host migration:** when the host quits or drops out, the game makes one
  of the other players the new host, and the rest reconnect to that
  player's address on gamePort. If that player's port is closed, everyone
  else sees "COULD NOT RECONNECT". For long matches, every player should
  have the port open.
- The game only takes IPv4 addresses (`inet_addr`): an IPv6-only
  connection cannot host.

## Option 1: forward the port on the router

1. Give the PC a fixed LAN address (a DHCP reservation in the router).
2. In the router, forward **TCP 5479** (external) to **TCP 5479** on that
   PC (FRITZ!Box: Internet → Freigaben → Portfreigaben; others call it
   "port forwarding" or "virtual server").
3. Allow it through the Windows firewall, in an elevated PowerShell:

   ```text
   New-NetFirewallRule -DisplayName "Siedler AdK match" -Direction Inbound -Protocol TCP -LocalPort 5479 -Action Allow
   ```

4. Host as usual ("Spiel hosten"). The lobby lists the match with the
   public address it sees the host on; nothing else to set.

To check, ask someone elsewhere to run
`Test-NetConnection <your public IP> -Port 5479` while you host (it must
say `TcpTestSucceeded : True`).

This does not work when the internet connection has no public IPv4
address of its own: carrier-grade NAT, DS-Lite (common on German cable
and fibre lines), most mobile connections. The router then shows a WAN
address in 100.64.0.0/10 or only an IPv6 address. Use option 2 then.

If the router can only forward a different external port (say 24680 →
5479), tell the lobby with `!adresse <public IP>:24680` (see below).

**Two players behind the same router** both get the public address for the
other's match, so the router must support hairpin NAT (NAT loopback; most
current routers do). If it does not, play over a virtual LAN.

## Option 2: a virtual LAN (Tailscale, ZeroTier)

A virtual LAN gives every player's PC an address that the others can reach
directly, with no router setup. Every player in the match installs the
same one and joins the same network (a Tailscale "tailnet" shared with the
others, or one ZeroTier network).

- **The lobby server is on the virtual LAN too** (simplest): players set
  `Host` in `LobbySettings.ini` to the server's virtual address (for
  Tailscale its 100.x.y.z address). The lobby then sees every host on its
  virtual address and lists matches with it; nothing else to set.
- **The lobby server is public, the players share a virtual LAN:** the
  lobby sees the host's public address, so the host tells it the virtual
  one by typing in the village chat (any tab):

  ```text
  !adresse 100.101.1.2
  ```

  Every match this player hosts from then on is listed with that address
  (the reply comes as a [SYSTEM] line only the player sees). `!adresse`
  alone shows the current setting, `!adresse aus` goes back to the
  address the server sees. The setting is saved with the account, and
  everyone in the lobby gets that address, so players who are not on the
  virtual LAN cannot join those matches.

`!adresse` takes an IPv4 address, optionally with a port
(`!adresse 203.0.113.7:24680`). It also works with a TCP tunnel service
(playit.gg, ngrok and the like): run its agent for TCP 5479 on the host's
PC and give `!adresse` the IPv4 address and port the service hands out
(resolve a host name it gives with `nslookup` first; the game cannot use
names).

Windows may treat the virtual network adapter as a public network; the
firewall rule above covers it.

## Option 3: tunnel the match port through the server

When the lobby runs on a Linux server you can log in to with SSH, the host
can carry its game port there and nobody touches a router:

1. On the server, install with `--match-forward` (see
   [DEPLOYMENT.md](DEPLOYMENT.md)): `sadk-match-forward` relays the
   server's public TCP 5479 to 127.0.0.1:15479 on the server.
2. On the host's PC, keep an SSH reverse tunnel open while hosting. It
   carries the server's 127.0.0.1:15479 to the PC's 127.0.0.1:5479, where
   the game listens:

   ```text
   ssh -N -R 127.0.0.1:15479:127.0.0.1:5479 user@lobby.example.org
   ```

   (PuTTY's `plink -N -R 127.0.0.1:15479:127.0.0.1:5479 ...` works the
   same.) The server's `sshd` needs `AllowTcpForwarding yes` (the default),
   not `GatewayPorts`.
3. In the village chat, the host types once
   `!adresse <server IPv4>:5479`. Matches this player hosts are then listed
   with the server's address.
4. Host as usual; joiners connect to the server, which relays to the host.

Only one match at a time can use this (one port). The host sees every
player as 127.0.0.1, so host migration cannot work: if the host leaves,
the match ends for everyone.

## A relay in the lobby server (plan, not built)

Analysis of tincat3 and the game shows a relay could work transparently:
the host accepts any TCP connection on gamePort (the logon carries the
fixed strings "user"/"password" and the game's address check always says
yes), and joiners connect to whatever address and port the lobby lists.
But the host's game only listens, so a relay needs a helper on the host's
PC that dials out:

1. `sadk-relay` on the server: a control port, plus a range of public TCP
   ports, one per relayed match.
2. A small agent on the host's PC (or a hook in the game's process,
   injected into the game) logs in to the relay with the player's
   session, gets port P, and keeps the control connection open.
3. The lobby lists the match with the server's address and P (the same
   override `!adresse` uses).
4. For each joiner that connects to P, the relay asks the agent for a new
   data connection; the agent connects to 127.0.0.1:5479 and the relay
   splices the two streams.

The costs: all match traffic goes through the server; the host sees every
player as coming from 127.0.0.1, so its host-migration list is useless,
and migration fails unless every player runs the agent and the relay
redirects P to the new host, which it would have to detect. That is more
than a small change, and tunnel services already do steps 1, 2 and 4 with
`!adresse` covering step 3, so it is not built.


# Tavern table games (dice, poker, pawn chess)

The two taverns in the lobby village have tables where players play dice,
poker and pawn chess ("Bauernschach") for funnies or gold. The client draws
the tables and checks most input. The server keeps the tables, seats the
players, and runs the games. Everything below comes from reading the demo
exe (build 34811; retail 34688 has the same code and strings). Notes on
retail are marked.

Code: `crates/lobby-proto/src/minigame.rs` (shared messages),
`crates/lobby-proto/src/pawnchess.rs`, `dice.rs` and `poker.rs` (the rules
and state of each game),
`crates/lobby-server/src/tables.rs` (the tables), and the village session
(`session.rs`, `create_table` / `table_message`).

Status: tables, seats and table chat for all three games, and all three
games are played; all three were tested with the retail client against
`village_bot`.

## Encoding

Village messages (header bit 15 debug, bits 12-14 channel, bits 0-11 id).
The client dispatches server messages on the id alone. The payload is an
MSB-first bit stream:

- `uN`: N plain bits; u8, u16 and u32 are 8, 16 and 32 bits.
- `str`: a u8 length, then the bytes (no NUL).
- `var`: 1 bit "present" (0 = the value 0); if 1, 3 bits `nibbles - 1`,
  then `4 * nibbles` bits of value. Example: 1000 = `1 010 0011 1110 1000`
  = `a3 e8`.
- `ActorID`: a u2 type (1 = avatar) and a `var` id (the character id).
- Password check: `crc32(password bytes)` (zlib CRC-32, no NUL), 0 without
  a password. The client compares it itself before it sends a join.
- Table key: `msgprt u16, rnid u8`, chosen by the server and opaque to the
  client (sadk-lobby: a counter 1..0xfffe, `rnid` 0).

## Messages

Client → server:

| id | channel | fields | meaning |
|---|---|---|---|
| 2001 | 2 | `type u8, plyMny u8, stck u32, gmnm str, lmtmn u32, lmtmx u32, mxplyr u8, tvrn u8, tblidx u8, psswd u8, psswdcrc u32` | open a table and sit down with `stck` chips |
| 209 | 5 | key, `stck u32, psswdcrc u32` | sit down at a table |
| 204 | 5 | key | stand up ("Verlassen") |
| 222 | 5 | key, `amnt u32` | add chips ("Account") |
| 220, 221 | 5 | key, … | dice, below |
| 300-302 | 5 | key, … | poker, below |
| 400-403 | 5 | key, … | pawn chess, below |

2001: `type` 1 dice, 2 poker, 3 pawn chess. `plyMny` 1 = funnies (the
default), 0 = gold. `lmtmn` is the minimum bet / big blind / stake per
chess game (dice 1; poker 2, 10, 50, 100, 250; chess 1, 10, 50, 100, 250).
`lmtmx` is always 0x7fffffff. `mxplyr`: dice 1-4, chess 2, poker 2-4, or
5-8 at tables 13 and 14. `tvrn` 0 or 1. `tblidx` is the table clicked
(0-14), or 0xff from the tavern menu ("the server picks").

Server → client:

| id | payload | meaning |
|---|---|---|
| 218 | header + settings | a table appeared (works in any client state) |
| 217 | header + sections | a table changed; ignored before the world welcome (1000) and when `chtid` is 0 |
| 216 | header | a table is gone |
| 219 | none | "Kein Tisch mehr frei": the answer to a 2001 that opened no table |
| 220 | none | "Tisch voll": the answer to a 209 that seated nobody |

After a create or join the client freezes the avatar until its own seat
shows up or a 219/220 arrives, so **every 2001 and 209 gets an answer**.

### Header (`sub_470930`)

```
mngt    u8   bits 0-3 game (1 dice, 2 poker, 3 chess; anything else crashes the client)
             0x10 settings follow, 0x20 funnies (else gold), 0x40 seats follow, 0x80 game state follows
scntbl  u8   low nibble scene (tavern + 2), high nibble table (0-14)
chtid   var  the table's chat channel
msgprt  u16, rnid u8   the key
```

Sections follow in the order settings, seats, game state:

- **Settings:** `psswd u8, psswdcrc u32, gmnm str, lmtmn var, lmtmx var,
  mxplyr u4`.
- **Seats:** `plyrcnt u4`, then for each taken seat `used u1 (always 1),
  sltidx u4, ActorID`. This is the complete list: seats it leaves out are
  emptied. `used = 0` makes the client write past its arrays. Every listed
  avatar must already be spawned on that client, or the client drops the
  rest of the message. The seat that holds the player's own avatar becomes
  "my seat": the client joins `chtid` over usercomm, opens the MINIGAME chat
  tab and the game's dialog, and leaves the channel when the seat goes.
- **Game state:** per game (pawn chess below).

A 218 must not carry seats: the client builds the 3D board on the first
217, and seats before that crash it. The server therefore announces a table
with 218 (settings) followed by 217 (seats and state). A player entering
the village gets that pair for every table, after the world welcome and
after the avatars (village `join` queues those first). Players leave their
table before their avatar leaves the village, so no client keeps a seat
for a despawned avatar.

The flow: 2001 → 218 + 217 to everyone (or 219). 209 → 217 to everyone
(or 220). 204 → 217, or 216 when the table is empty. Taverns are scenes 2
(`scene_taverne03.xml`, tavern 0) and 3 (`scene_taverne02.xml`, tavern 1),
each with tables 0-14; 13 and 14 are the eight-seat poker tables. One game
per table. sadk-lobby gives table chats channel 1000 + `msgprt`.

### Money

The client shows and checks the avatar's own gold and funnies (character
data block 4, default 1000 each; live updates by 3201 `{ownr, lvl, exp,
gold, glod}`). In a game state, `crdts` is a player's chips now and
`accnt` the chips brought in total (the rebuy cap is balance − `accnt`).
The client only reads block 4 when the data is in the full six-block
layout (`sub_480F10`); data with just the 44-byte creation block shows 50
gold and 50 funnies.

sadk-lobby keeps gold and funnies per character in `accounts.json`
(new characters, and those saved before balances were kept, start with
20 each) and treats the balance
the way the rebuy cap suggests: the chips at the table still count as the
player's own, so nothing is taken when they sit down.

- The character list (UserCharConn / CharacterData) carries the balance
  as block 4 (version 1: version, level 1, exp 0, gold, funnies); a
  creation-only character is sent split into the appearance and style
  blocks, so the look is the same.
- 2001 and 209 are refused (219 / 220) when the stake is more than the
  balance in the table's currency; a rebuy (222) is cut to balance −
  `accnt`.
- Standing up (204), leaving for a match or disconnecting books
  `crdts − accnt` on the character, saves it and sends 3201 to the
  player. No balance goes below 10: losses beyond that are forgiven, so
  everyone can always sit down again. Entering the village also sends
  3201. A server crash while seated changes nothing.

## Pawn chess

Two players, each with a king and seven pawns. A captured pawn is not
removed: it changes sides and the capturer puts it back on the board. A
pawn on the far baseline wins, and so does checkmate.

Board: a square is `x + 8y`. **x is the forward axis**: seat 0 starts on
x = 0 and moves +x, seat 1 starts on x = 7 and moves −x. A piece byte is
`x | y << 3 | 0x40 king | 0x80 owner (seat)`. Piece 0 must be seat 0's
king (in phase 3 only piece 0 is drawn); sadk-lobby uses 0 king, 1-7
pawns for seat 0 and 8 king, 9-15 pawns for seat 1. The 16 pieces always
stand on 16 different squares.

### State section (`sub_489040`)

```
phs      u8   1 waiting for a player, 2 seat 0 places its king, 3 seat 1 places its king,
              4 seat 0 to move, 5 seat 1 to move, 6 game over
trnid    u8   changes with every update; the client takes no board input after a move
              until it changes (never send 0xff, the client's "unlocked" value)
pwchkmt  u8   0xff, or the pawn (bits 0-3) of the side to move that may capture the single
              checking pawn, bit 0x10 = toward y − 1
plrstt   u8   seat 0 low nibble, seat 1 high nibble: 1 check, 2 checkmated, 4 won
pwnps    u8 × 16
plyrcnt  u8, then per player: sltidx u8, crdts var, accnt var
```

### Actions (channel 5, after the key)

| id | fields | when |
|---|---|---|
| 400 | `pwn u8, trgt u8` | move a piece to an empty square |
| 401 | `pwn u8, trgt u8, cppwn u8, cptrgt u8` | capture the enemy pawn on `trgt` and put it on `cptrgt` |
| 402 | `posy u8` | place the own king at y on the own baseline (phases 2 and 3) |
| 403 | `plridx u8` | NEUES SPIEL (phase 6) |

The client offers, and sadk-lobby accepts, exactly:

- **King:** the 8 neighbouring squares that hold no own piece and are not
  attacked by the enemy (pawns attack diagonally forward; kings attack all
  8 neighbours; occupied squares count).
- **Pawn:** one square forward if it is empty; diagonally forward onto an
  enemy piece.
- **In check:** only the king and the `pwchkmt` pawn (whose only move is
  that capture).
- **Capture placement:** the empty squares in the captured pawn's column on
  the board before the move. If there are none, the next column toward the
  capturer's own baseline is used.

The client checks no result. After every action the server works out the
rest. If a pawn of the mover stands on the far baseline, the mover wins. If
the other side cannot move, the mover wins (checkmate if in check,
otherwise stalemate; the client has no text for stalemate). Otherwise it is
the other side's turn, with its check bit and `pwchkmt`.

Every action is answered with a 217 carrying the state. A refused one gets
the unchanged state with a new `trnid`, or the client would stay locked.

### Table rules (sadk-lobby's choice where the client says nothing)

- The game starts (phase 2) when both seats are taken and both players have
  at least `lmtmn` chips.
- Each player pays `lmtmn` into the pot when placing its king. The winner
  gets both stakes in the phase-6 update.
- Seat 0 moves first.
- NEUES SPIEL from both players starts the next game.
- A player who leaves during a game loses it; the table then waits for a
  new opponent (phase 1).

## Dice

One to four players bet chips on the sums 2-12 of two dice; the dealer
throws and the house pays. The client works out every payout itself when
phase 3 starts (`MiniGameDiceProxy`, `sub_4871B0`), so the server pays by
the same formula:

- the exact sum pays 5× the chips on it;
- the other fields of the sum's colour return their chips (colour A = 2,
  4, 6, 9, 11; colour B = 3, 5, 8, 10, 12);
- a 7 pays only field 7, 3×; every other bet is lost.

### State section (`sub_487740`)

```
dice     u8   die 1 low nibble, die 2 high nibble; 0 until phase 3
dlr      u8   the seat that throws
plyrcnt  u8
phase    u8   1 bets, 2 the dealer throws, 3 the dice are shown, 4 payout
time     u16  ms / 10 already gone in this phase (the client's countdown)
per player: sltidx u8, crdts var, accnt var, bt00..bt10 var (chips on 2..12)
```

`crdts` still includes the chips on the table; the client shows `crdts`
minus the bets. The client ignores the own bets while its stored phase is
1, since it keeps its own clicks.

### Actions

| id | fields | when |
|---|---|---|
| 220 | `crdts var, bt00..bt10 var` | phase 1: the sender's complete bets (totals) after every click |
| 221 | — | phase 2: the dealer throws |

### Table rules (sadk-lobby)

- Betting starts as soon as one player sits down. Phase 1 lasts 15 s (the
  client's countdown), phase 2 until the dealer throws or 10 s, phase 3
  2.5 s and phase 4 4 s. The 4 → 1 update settles the chips.
- The dice are drawn when betting closes (OS random, no bias).
- The dealer moves one seat on every round.
- Bets beyond a player's chips, bets outside phase 1 and throws by anyone
  but the dealer are refused (the state goes out unchanged).
- A player who leaves in phase 1 takes their bets back; later, their bets
  are settled against the roll. A leaving dealer hands over at once.

## Poker

No-limit Texas Hold'em for two to four players, or two to eight at tables
13 and 14. The big blind is `lmtmn` (2, 10, 50, 100 or 250), the small
blind half of it.

The client only animates. It deals when `phase` turns 5 and flies the bets
into the pots in phase 8. In phase 9 it picks every pot's winners itself:
the seats in the pot's `plrmsk` with the highest `hndval` (ties split
`pot / n`). So the server pays by the same rule and leaves folded players
out of `plrmsk`.

### Cards and hand values

A card is `suit * 13 + rank`: rank 0 = '2' … 12 = 'A', suit 0 d, 1 h, 2 s,
3 c. 52 is a face-down card, 0xff none.

`hndval` (u32): the category in bits 24-31 (0 high card, 1 pair, 2 two
pair, 3 trips, 4 straight, 5 flush, 6 full house, 7 quads, 8 straight
flush), then the deciding ranks in 4-bit steps from bit 16 down (kickers
after the made ranks; a straight gives only its top card, 3 for A-5).
Preflop it is `hi, lo` or `1, pair rank`.

### State section (`sub_487DE0`)

```
dlr      u8   the button, 0xff none
plyrcnt  u8
crntplyr u8   the seat to act, 0xff outside betting
actns    u16  actions offered to it: at most three bits (more overflow the client's buttons)
phase    u8   1 waiting, 5 deal `round`, 6 betting, 8 collect the bets, 9 showdown and payout
round    u8   0 none, 1 preflop, 2 flop, 3 turn, 4 river
egr      u8   run-outs (phase 5, round 4): the street shown before (1 deal flop to river, 2 turn and river)
shwcrds  u8   phase 9: 0xff showdown, 0xfe won by folds
time     u16  ms / 10 gone in this turn
minwgr   u32  the highest bet this street, or the minimum bet (the big blind) without one
lstply   u8   the seat of the last action, 0xff none (then lstvl 0, lstactn 0)
lstvl    var  the chips shown with it
mnrs     var  the minimum raise step
lstactn  u16  the last action bit
board    u8 × 3/4/5 for round 2/3/4
wgrmsk   u8   seats with a bet in front; then one var per set bit, seat 0 first
per player: sltidx u8, trsfcrds u1 [crd0 u8, crd1 u8, hndval u32], actn u16, crdts var, accnt var
ptncnt   u8   pot count; bit 0x80: phase 8, each pot lists the chips moving into it
per pot: pot u32, plrmsk u8 [wgrplrmsk u8, one var per set bit]
```

- Action bits: 0x2 CALL, 0x4 FOLD, 0x8 RAISE TO, 0x10 CHECK, 0x20 BET, 0x40
  small blind, 0x80 big blind, 0x100 ALL IN.
- `actn`: the player's last action this street, plus 0x8000 folded, 0x2000
  not dealt in, 0x1000 sitting out, 0x800 all in (the client shows "Auf
  nächste Runde warten ..." for the first three, and never gives the turn
  to 0x1000 or 0x800; the names are ours).
- `crdts` does not count the bet in front of the player.
- `trsfcrds` 1 shows a player's cards to everyone. sadk-lobby sends it in
  a showdown for every seat in a pot. Otherwise other players' cards are
  drawn face down and the own ones come from 303.

**303** (server → one player): key, `crd0 u8, crd1 u8, hndval u32`, the
own hole cards and current hand value. It is sent before every street's
phase-5 update: the client reads the own cards on the first frame of the
deal.

### Actions

| id | fields | when |
|---|---|---|
| 300 | `actn u16` | FOLD, CALL, CHECK or ALL IN |
| 301 | `actn u16, wgr var` | BET or RAISE TO `wgr`, the total for this street |
| 302 | `enbl u8` | SIT OUT on (1) or off (0) |

The client locks its buttons after sending until `crntplyr`, `phase` or
`round` changes. A refused action would lock it for good, so sadk-lobby
takes every action from the player whose turn it is. It clamps a wager
into the allowed range and turns an action that was not offered into the
nearest one that was (a check that faces a bet folds).

### Table rules (sadk-lobby)

- A hand starts 2 s after two players can play (seated, not sitting out,
  at least the big blind in chips), and again right after the previous
  hand's payout.
- The button moves one seat each hand. Heads-up the button posts the small
  blind and acts first preflop; otherwise the seat after the big blind
  does. After the flop the first seat after the button acts.
- A bet is at least the big blind, a raise at least the last bet or raise
  step. An all-in below a full raise does not reopen the betting.
- Offered: facing a bet, RAISE/FOLD/CALL, or FOLD/CALL/ALL IN when the
  stack cannot reach a full raise, or FOLD/ALL IN when it cannot cover the
  call; with nothing to call, BET (or RAISE for the big blind's option),
  FOLD, CHECK.
- Side pots: one per all-in level of the players still in. An uncalled bet
  forms a pot only its owner can win.
- When at most one player can still bet, the rest of the board is dealt at
  once (`egr`), then the showdown.
- A turn lasts 15 s (the client warns at 10 s left); then the server checks
  if that is free and folds otherwise. Two timeouts in a row sit the player
  out.
- SIT OUT: the player checks or folds at once for the rest of the hand and
  is not dealt in until they turn it off.
- A player who leaves mid-hand folds; their chips stay in the pots.
- Pacing: the preflop deal waits 700 × (seats + 1) + 500 ms, the other
  streets 1.5 s (+0.5 s per extra street in a run-out), the collection
  1 s, and the payout 1 s + 2 s per pot.
- The odd chips of a split pot go to the first winners after the button.

## Testing

`cargo test -p sadk-lobby-proto -p sadk-lobby` covers the encodings
(including the example bytes worked out from the client for each game),
the rules (pawn chess: check, mate, stalemate, baseline, captures, 300
random games that all end; dice: payouts, dealer hand-over, leavers;
poker: every hand category, a hand byte for byte, side pots, run-outs,
timeouts, sitting out, leavers), and the table flow.
`village_bot --chess|--dice|--poker create|join` plays as a second player
(see the README).

Tested live on 22 October 2026 with the retail client against `village_bot`
standing in the tavern (`--zone 3`, next to the player). The player opened a
table, and the client joined the table chat. The bot sat down, and both
placed their kings. The game ran 30 moves, two of them captures with
placement, and no action was refused. It ended with a pawn on the far
baseline: "Du hast gewonnen!", the pot went to the winner, and NEUES SPIEL
appeared. The board, the dialog and the coins all looked right. Dice and
poker were tested the same way the same day (the player opened the table,
`village_bot --dice join` / `--poker join` sat down), and both played
through correctly.

The client sends a chat `Status { cell, ticket 0, result 2 }` for the table
channel right after joining it; the server ignores it, with no visible
effect.

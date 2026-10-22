# Protocol notes

* **Frame**: `u32 0xDABAFBEF | from | to | kind | flags | size | crc`, then the
  payload. `kind` is 3 for connect, 5 for connected, 2 for application and 11
  for ping. The CRC is reflected CRC-32 with initial value 0 and **no** final
  XOR, so it is not zlib's.
* **Handshake**: the client sends 0x34 bytes as `0xEFFFFFEE`. The server echoes
  them with the assigned connection id and `2d 00…` in the password slot.
* **Messages**: `u16 magic | u16 id`, followed by the fields of
  `[NETMSG_TYPE_<id>]` in `bin\msgdefs.ini` order. Retail build 34688 uses
  magic **0x26B6**. An older recorded client used 0x27D8, and the server echoes
  whichever magic the client sends.
  * `STRING n`: `u32` length including the NUL, then the bytes.
  * `MEMBLOCK`: `u32` length, then the bytes.
  * `LBOOL`: 1 byte.
  * The retail file's `BrokerSoldNotification` (46) is the only entry
    without a `type` field.
* **Login**:
  1. `CheckVersion` (version 27) gets `Result`.
  2. `StartAuthenticateSession` sends a DER secp521r1 key. The server answers
     `AckAuthenticateSession` with
     `SEQUENCE{OID sha512, OCTET STRING(server key), OCTET STRING(K ⊕ SHA-512(ecdh_x)[..32])}`.
  3. `AuthenticateUser` / `SelfRegistration` carry `iv ‖ Twofish-256-CTR_K(len,name,len,password[,cd key])`.
     The CTR counter is little-endian.
  4. The server answers `SessionKey { perm_id, iv ‖ CTR_K(session key) }`.
* **Server list**: `RegObserverServerList { server_type: 4 }` gets one
  `GameServerData` for the village with `server_subtype` 2 (1 is a game
  server), `running` true, and a data block holding, as a bit stream, `u32`
  village protocol (1000 for retail, 999 for the demo) and a 1-bit flag. The
  client drops villages with another protocol or with load ≥ 90 %.
* **Token login** (village and usercomm connections): `CheckVersion` →
  `Result`, `StartValidateTokenSession` → `AckValidateTokenSession { nonce }`,
  `SendToken` (or `TANLogin`) → `AddResult { errorcode 0, id: perm_id }`.
  tincat ignores a plain `Result` here. The retail client's `perm_id` is the
  **character id** of the avatar picked in "Charakterauswahl", so this is
  where the server learns which avatar plays; from here on chat, whispers,
  the village and the lists name characters. The token itself (tincat3
  `Authenticator.GenerateToken`; `auth::Token`) is
  `iv ‖ Twofish-256-CTR(session key)` of `u32 id, u8 len + account name,
  u8 len + cd-key hash, u32 value, u8 len + server password, u8 len + nonce`
  and 128 bytes of salt. It names the account, not the character. Accounts
  and characters share one id space, so a bare id is never ambiguous (a
  file written before that may still hold an account and a character with
  the same id; the character wins).
  * The server checks every token (`tokens.rs`, `Session::check_token`).
    The lobby login issues a session key per lobby connection; the token
    must open with a live key of the account the perm id belongs to. CTR
    has no MAC, so the opened fields are the proof: the name must be the
    account's (any case), `id` the account's id or one of its characters'
    (tincat seals `SessionKey.perm_id` until an `AddResult` of a token
    login hands it the character id, which later tokens carry), and
    `nonce` the one this connection got in `AckValidateTokenSession`.
    `value` is the connection's server id (1 for the village and usercomm,
    2 for the referee) and the cd-key hash and server password are not
    checked. A failure gets `AddResult {AUTH_FAILED, errormsg}` ("Invalid
    token", "Stale token", "Session expired", "Unknown user").
  * A key lives as long as its lobby connection plus `--token-grace`
    minutes (10), so a village or referee login shortly after the lobby
    connection dropped still works. A new lobby login replaces nothing:
    each connection has its own key.
  * Before a token login a village, usercomm or referee connection may send
    only `CheckVersion`, `StartValidateTokenSession`, `SendToken` and
    `TANLogin`; anything else that carries a ticket gets `AUTH_FAILED`. On
    the lobby port, five wrong passwords close the connection.
  * `TANLogin {perm_id, nonce}` carries no cipher. The retail client never
    sends it (tincat only does for a "login with TAN" call the game does
    not make), but the server accepts one: `ConnectionData` hands a
    logged-in account a one-time nonce for that server, good for two
    minutes.
* **Usercomm**: `AssignServer { server_type: 4 }` gets
  `UsercommServerData { ip, port }` and `Result`.
* **Village connect**: after "Spiel starten" the client asks
  `RequestConnectionData` on the lobby connection and gets
  `ConnectionData { perm_id, server_id, ip, port, nonce, errorcode 0 }`, then
  connects to the village and logs in with the token (tincat sends this
  request itself for a connection made by server id; it ignores the nonce
  on the token path).
* **Addresses** (`address.rs`): every address the lobby hands out (village,
  usercomm and referee in `ConnectionData`, `UsercommServerData` and
  `GameServerData`) is one that works from where that client is. A client
  on loopback, a private LAN or a virtual LAN (100.64.0.0/10, e.g.
  Tailscale) gets the address it reached the lobby on; one from the
  internet gets `--public-host` (or, without it, that same address).
* **Village messages** travel in `SendGameData { msg_type, data }` (or
  `SendGameDataBundle`). `msg_type` bit 15 marks a typed/debug encoding, bits
  12-14 a channel and bits 0-11 the id. `data` is an MSB-first bit stream;
  strings are a `u8` length plus bytes. After the token login the server sends
  message 1000 (world welcome: world name, server perm id, chat channel per
  zone; the client joins every listed channel), and the avatar is placed in the
  world. The client labels chat lines from the server perm id "[SYSTEM]"
  (demo exe `sub_436310`), so it must not be a player's: we send the server's
  frame id `0xEFFFFFCC`. See `lobby_message.rs`.
* **Avatars in the village** (handlers in the demo exe's dispatcher
  `sub_470260`, encoder `sub_46BF40`):
  * The client sends **2000** on channel 2 about three times a second, 9 bytes:
    `u16 tick` (its millisecond clock, always odd), x, y, z as 11-bit
    fractions of the world box, the heading in 7 bits (1/128 turn), zone and
    "ghost zone" (4 bits each), a running flag and one more flag.
  * **1002** `{u32 avatar}` announces an avatar, **1003** `{u32 avatar}`
    removes it.
  * **1001** carries avatar data: `u32 avatar`, a 4-bit block mask (1
    location, 2 style, 4 active items, 8 stats), then the blocks. The location
    block is the 66 bits of message 2000, so the server relays them as they
    are. The style block is the name (`u8` length) plus 9 bytes: tribe << 4 |
    gender, hair, skin, shirt and trouser colour, and 4 extra colours. The
    server takes them from the character's `data`, read the way the client
    reads it (`lobby_message::Style::from_character`).
  * A character's `data` (AddCharacter, UserCharConn, CharacterData) is six
    `u32` block sizes followed by the six blocks (tincat3.dll splits it that
    way). Character creation sends only block 0, a 44-byte creation block:
    `u32` version (1), tribe (0 Bavarians, 1 Scots, 2 Egyptians), gender,
    hair, skin, shirt, trousers and 4 extra colours. The client builds its
    own avatar's look from it (demo exe `sub_480F10`), and that tribe is the
    one it brings into a match. Data stored in the layout this server once
    assumed (`u32 size` before each block) is rewritten when the accounts
    are loaded.
  * The avatar id is the **character id**. The client knows its own avatar by
    that id and ignores the location of its own avatar in 1001.
  * The client sends **3798** `{u32 code}` every 30 s, and the server echoes
    it back in **3799**.
* **Chat channels ("cells")** run on the usercomm connection. A cell frame's
  payload is `u16 0x0062 | u16 0 | u16 id`, then the fields (see
  `lobby-proto/src/cell.rs`; the flow follows the Sacred 2 recordings in the
  emulator repository):
  * **0** ChannelInfo `{ChannelData, u32 cell, u32 ticket}` announces a
    channel; ChannelData is five strings (publish `SC`, name, subject,
    creator, password), four bools (protected, persistent, autodelete,
    hidden) and `u32` creator perm id. The server sends one per channel when
    usercomm connects.
  * The client asks `RequestJoinChannel {cell_id, ticket_id}` (an ordinary
    msgdefs message) and gets **9** Joined `{cell, ticket 0, u16 option}`,
    **11** Status `{cell, ticket, u16 result 0}`, then a `UserJoinedChannel
    {perm_id, cell_id, nick}` for each member including itself. Those are
    delivered as **3** Deliver `{u16 message, block data, u32 cell 0, u32 from
    = SERVER, u8 propset 1}`; the members already there get the newcomer's.
    Leaving sends `UserLeftChannel {perm_id, cell_id}`.
  * A chat line is **2** Route `{u16 module, u16 message 2, u32 except
    (0xFFFFFFFF = nobody), block ChatMessage, u32 cell, u8 to_self, u8
    propset}`. The server passes it on as a Deliver with the channel as cell
    and the sender's perm id as `from`, to every member (the sender too when
    `to_self` is set).
  * `data` is a property set: the msgdefs message without magic and id,
    starting at its `type` field. The client's property reader drops the
    last byte of each string, so strings must end in a NUL (the Sacred 2
    recording had none; we send it and accept both).
  * The chat window's tabs (demo exe `sub_436310` / `sub_46C670`) come from
    the world welcome's zone → channel list: zone 255 → GLOBAL, 254 →
    MINIGAME, zones 0-13 → LOKAL (the tab open at start), others are dropped.
    The server lists channel 1 for zone 255 and channel 10 + zone for zones
    0-13; avatars in the village are in zone 0, so they talk in channel 10.
  * **Whispers** (`/tell <name> <text>`, also started by clicking a name in
    the chat) are routed like chat lines but as `WhisperChatMessage {mode 3,
    txt, cell_id, from_id, perm_id = receiver}`. tincat3 has no case for a
    delivered WhisperChatMessage (its cell dispatcher drops it); it shows a
    delivered `PrivateChatMessage {mode, txt, cell_id, from_id, to_id,
    from_name}` through the client's `PrivateChatReceived` (demo exe
    `sub_4802C0`). The server therefore delivers a PrivateChatMessage to the
    receiver's usercomm connection and, as the sender asks for its own copy,
    to the sender. The receiver shows "[name] flüstert : …", the sender
    (seeing its own `from_id`) "An [name] : …".
  * A whisper to a player who is not on usercomm gets the sender a
    `ChatMessage` from the server's id in the whisper's channel (GLOBAL):
    "[SYSTEM] Besucher ist nicht online." (or "Diesen Spieler gibt es
    nicht." for an unknown id). A `/tell` to a name the client does not
    know is resolved first with `RequestCharacters {name}`; an unknown name
    fails there and is never sent. A whisper to a player who ignores the
    sender is dropped and the sender is not told; the client would hide it
    too (`PrivateChatReceived` skips senders it has as ignored).
* **Friends and ignore lists** ("FREUNDE" button; `session/social.rs`).
  Results go by request type in tincat3 (dispatcher `sub_10019AD0`).
  * At login the client asks `RequestUserBuddyList {user_id}` (56) and
    `RequestUserIgnoreList` (157). The server sends one `UserBuddyConn`
    (61) / `UserIgnoreConn` (160) `{user_id, buddy_id | ignore_id, name,
    perm_id_type 2, status, server_id, server_name}` per entry with the
    request's ticket, then `Result`. The client drops entries whose
    `perm_id_type` is not 2 (character; demo exe `sub_46AAE0`).
  * A friend shows online (light text instead of grey) only with `status`
    3 and a `server_id` above 1 (demo exe `sub_57A1D0`); the server sends
    3, 3 and "Siedlerdorf" while the character is on usercomm, else 0, 0, "".
  * A `UserBuddyConn` with ticket 0 is a status update (tincat
    `BuddyUpdateReceived`). The server sends one to every lobby connection
    whose account lists the character when it logs in to usercomm and when
    its last usercomm connection closes.
  * "Hinzufügen" sends `AddUserBuddy {user_id: account, buddy_id}` (98) /
    `AddUserIgnore {ignore_id}` (154) for the avatar selected in the
    village, "Entfernen" `RemoveUserBuddy` (99) / `RemoveUserIgnore` (155).
    On an OK `Result` the client lists the new friend as offline, so the
    server sends the status of an online friend right after the Result.
    The lists live in `accounts.json` (`buddies`, `ignored`: character ids).
  * `RegObserverBuddylist` / `DeregObserverBuddylist` (175/176) get an
    `AddResult` OK; status news goes to every lobby connection anyway.
* **Mailbox** ("Briefkasten", the letterbox by the townhall: `Letterbox` at
  (36.05, 2.48, 33.32) in `lobby/scene/scene1.xml`; tabs "Posteingang" and
  "Post versenden"). There is no usercomm `UCPrivate` path in the client;
  letters are lobby messages.
  * The client asks `RequestPrivateMessageList {delivery_target: character
    id}` (147) at login and every 60 s. tincat clears its copy on each
    request, stores each `PrivateMessage {delivery_target, message_id,
    creator, creation_time, title, status, message_text, data}` (149) that
    carries the ticket, and hands them over on the `Result`. Opening a
    letter reads that copy (tincat `sub_10028A30`), so every letter goes out
    with every list.
  * Sending resolves the receiver's name with `RequestCharacters`, then
    sends `AddPrivateMessage {delivery_target, creator, creation_time 0,
    title, message_text, data}` (150), answered by `AddResult` with the
    letter's id. Opening a letter sends `ChangePrivateMessage {message_id,
    status 1}` (148), "LÖSCHEN" `RemovePrivateMessage {message_id}` (151);
    both get a `Result`. Letters live in `accounts.json` (`mails`); one to a
    player who ignores the sender is accepted but not stored.
  * The sender's name is the client's business (demo exe
    `LobbyComm::PostOffice`, `sub_48B680`): it looks the creator id up in
    its own id → name cache, which only the village avatars, chat lines,
    whispers and the friend list fill (`sub_477360`). On a miss it shows
    "???" and sends `RequestCharacters {char_id}`; the answer reaches the
    letter (`ResolveNameCallback`) but repaints nothing, and the next
    60-second list resets the name to "???" again. A letter from a player
    the reader saw in the village, the chat or the friend list shows the
    name; one from anyone else (or to oneself) shows "Von ???". The server
    cannot change that.
  * After `ChangePrivateMessage` the client logs "Got MarkReadResult for
    unknown MailID 0x0": tincat passes 0 instead of the letter's id to the
    result callback (`sub_10028F60`), whatever the server sends. The
    server keeps status 1, so the next list shows the letter as read.
* **Matches** (`games.rs`, `session/matches.rs`). The game itself runs
  peer to peer: the host's client starts a tincat "Net Driver" game server on
  port 5479, and the others connect to it directly. The lobby only keeps the
  list.
  * "Spiel hosten" sends `AddGameServer` (server_type 5, subtype 1, empty
    `ip`, port 5479, map, `description` "build|max|…|host name") and waits
    for `AddResult {errorcode 0, id}`. The server fills in the host's address
    as it sees it. Later `ChangeGameServer` messages carry `property_mask 0`
    and every field, so 0 means "all fields". `cipher` is 145 bytes with or
    without a password, so it does not mark a match as locked.
  * The match list ("Spiel beitreten" → "SPIEL SUCHEN") comes from
    `RegObserverServerList {server_type 5}`, sent at login. The server answers
    with a `GameServerData` per open match plus `Result`. Every later change
    goes to all observers as `GameServerData`, and a closed match as
    `RemoveServer` (ticket 0).
  * "Beitreten" opens a plain TCP connection straight to the listed
    ip:port; the joiner asks the lobby nothing more (static analysis of
    tincat3 and the game confirms it: no TAN, nonce or `ConnectionData` is
    used for a match). The `ip` goes through `inet_addr`, so it must be an
    IPv4 literal. The host's client sends no `ip` at all, so the lobby
    lists the address it sees the host on, rewritten per observer: a host
    on the server's machine or LAN is listed with the server's address
    (`--public-host` for players on the internet), others keep theirs. A
    player can override it with `!adresse` (see [Playing over the
    internet](PLAYING-ONLINE.md)). `RequestConnectionData` for a
    match id is still answered (ConnectionData to the joiner and a
    `TANConnectionRequest` with the same nonce to the host; the game
    ignores both), and `CheckPlayerOnServerAndGetInfos` is answered from
    the accounts.
  * When everyone is "Bereit" the host sends `RemoveServer` and broadcasts
    `NE_StartLoading` to the others (peer to peer), and every player sends
    village message **2002** `{u32 0xAFFEDEAD}` (leaving the village). The
    server takes the avatar out of the village and answers with village
    message **1006** `{u32 0xDEADBEEF}`. The client then logs off usercomm
    and closes its village connection itself (demo exe `sub_46E350`). Only
    that counts as a clean log-out in tincat (connection state 9 →
    `LoggedOut`). A close from the server's side, at any time, is
    `ConnectionLost`, "Verbindung zum Server verloren", and the character
    list. A frame of kind 6 (`LOGOFFACCEPTED`) does nothing.
  * The village log-out is what starts the match (`sub_4317C0`): the client
    shows "Verbindung zu Spieleserver wird hergestellt" and logs in to the
    **referee** (`referee.rs`, `session/referee.rs`). The client learns
    about the referee at login: `AssignServer {4, 4}` gets `UsercommServerData`
    plus a `GameServerData` carrying the request's ticket (server id 2,
    type 4/4, port lobby + 3). tincat only delivers an assigned server that
    way (`GameServerAssigned`); a plain `Result` leaves the client without
    a referee. The client connects straight to that ip:port and logs in with
    `StartValidateTokenSession` / `SendToken` (→ `AddResult` with the perm
    id).
  * Referee messages travel like village messages (`SendGameData`, bit
    stream; the client sends on channel 3; see
    `lobby-proto/src/referee.rs`). After the login the server sends **3530**
    `{u32 perm id}`. Each player then sends **3510** RegisterGame {game id
    (the match's server id), map GUID, map name, settings, ranked, wager, AI
    players, 6 avatar ids} and gets **3511** `{game id, 0}` ("waiting for
    other players"). Once every avatar listed has registered, everyone gets
    **3512** `{game id, 0, u32 seed}` and the clients load the map. 3520
    FinishGame, 3540 GiveUpGame and 3500 ClaimChest are acknowledged with
    result 0.


//! Friend ("Freunde") and ignore ("Ignorieren") lists and the mailbox
//! ("Briefkasten") on the lobby connection; the lists name characters.

use super::{code, Session, VILLAGE_NAME};
use crate::accounts::{List, Mail};
use sadk_lobby_proto::{Message, Value};
use tracing::{info, warn};

/// `perm_id_type` of a list entry; the client drops entries of any other
/// type (demo exe `sub_46AAE0`).
const CHARACTER_TYPE: i64 = 2;
/// `status` of a friend who is online. The client shows a friend as online
/// (light text instead of grey) only with this status and a `server_id`
/// above 1, which it keeps with `server_name`.
const ONLINE: i64 = 3;
const ONLINE_SERVER_ID: i64 = 3;

impl Session {
    /// A UserBuddyConn / UserIgnoreConn entry of `owner`'s list.
    fn list_entry(&self, list: List, owner: u32, char_id: u32, ticket: i64) -> Message {
        let name = self.server.accounts.lock().unwrap().character(char_id, "").map(|(_, c)| c.name.clone()).unwrap_or_default();
        let online = list == List::Buddies && self.server.chat.lock().unwrap().is_online(char_id);
        let (kind, field) = match list {
            List::Buddies => ("UserBuddyConn", "buddy_id"),
            List::Ignored => ("UserIgnoreConn", "ignore_id"),
        };
        self.message(kind)
            .with("user_id", Value::Int(owner as i64))
            .with(field, Value::Int(char_id as i64))
            .with("name", Value::text(&name))
            .with("perm_id_type", Value::Int(CHARACTER_TYPE))
            .with("status", Value::Int(if online { ONLINE } else { 0 }))
            .with("server_id", Value::Int(if online { ONLINE_SERVER_ID } else { 0 }))
            .with("server_name", Value::text(if online { VILLAGE_NAME } else { "" }))
            .with("ticket_id", Value::Int(ticket))
    }

    /// RequestUserBuddyList / RequestUserIgnoreList (at login): one entry per
    /// character with the request's ticket, then the Result that hands the
    /// list to the game.
    pub(super) fn send_list(&mut self, list: List, ticket: i64) {
        let Some(account) = self.account else {
            return self.result(ticket, code::AUTH_FAILED, Some("Not logged in"));
        };
        let ids = self.server.accounts.lock().unwrap().list(account, list).to_vec();
        for id in ids {
            let entry = self.list_entry(list, account, id, ticket);
            self.send(entry);
        }
        self.result(ticket, code::OK, None);
    }

    /// AddUserBuddy / RemoveUserBuddy / AddUserIgnore / RemoveUserIgnore
    /// {user_id, buddy_id | ignore_id}: "Hinzufügen" / "Entfernen" in the
    /// friend dialog, for the avatar selected in the village. On an OK the
    /// client lists a new friend as offline, so an online friend's status
    /// follows the Result.
    pub(super) fn edit_list(&mut self, msg: &Message, ticket: i64) {
        let Some(account) = self.account else {
            return self.result(ticket, code::AUTH_FAILED, Some("Not logged in"));
        };
        let list = if msg.name.ends_with("Buddy") { List::Buddies } else { List::Ignored };
        let add = msg.name.starts_with("Add");
        let id = msg.int("buddy_id").or(msg.int("ignore_id")).unwrap_or(0) as u32;
        let own = self.server.accounts.lock().unwrap().owner(id).is_some_and(|a| a.id == account);
        if add && own {
            return self.result(ticket, code::AUTH_FAILED, Some("Own character"));
        }
        let done = self.server.accounts.lock().unwrap().set_listed(account, list, id, add);
        match done {
            Ok(true) => {
                info!(conn = self.id, account, id, "{}", msg.name);
                self.result(ticket, code::OK, None);
                if add && list == List::Buddies && self.server.chat.lock().unwrap().is_online(id) {
                    let entry = self.list_entry(list, account, id, 0);
                    self.send(entry);
                }
            }
            Ok(false) => self.result(ticket, code::AUTH_FAILED, Some("Unknown character")),
            Err(e) => {
                warn!(conn = self.id, "saving accounts failed: {e}");
                self.result(ticket, code::AUTH_FAILED, Some("Server error"));
            }
        }
    }

    /// The lobby connection of `account` logged in: it hears about friends.
    pub(super) fn lobby_login(&mut self, account: u32) {
        self.server.lobby_users.lock().unwrap().log_in(self.id, account, self.events_tx.clone());
    }

    /// A character came online or went offline on usercomm: everyone with it
    /// on the friend list gets its entry with ticket 0, which the client
    /// takes as a status update (BuddyUpdateReceived).
    pub(super) fn buddy_status_changed(&self, char_id: u32) {
        let friends = self.server.accounts.lock().unwrap().befriended(char_id);
        for owner in friends {
            let entry = self.list_entry(List::Buddies, owner, char_id, 0);
            self.server.lobby_users.lock().unwrap().send(owner, &entry);
        }
    }

    /// RequestPrivateMessageList {delivery_target}: the client asks for its
    /// mail at login and every minute. Every letter goes out each time as a
    /// PrivateMessage with the request's ticket (tincat clears its copy on
    /// each request, and opening a letter reads that copy), then the Result.
    /// Unread letters light the mailbox in the village.
    pub(super) fn mail_list(&mut self, msg: &Message, ticket: i64) {
        let to = msg.int("delivery_target").unwrap_or(0) as u32;
        let letters: Vec<Mail> = {
            let accounts = self.server.accounts.lock().unwrap();
            let mine = self.account.is_some_and(|a| accounts.character_ids(a).contains(&to));
            if !mine {
                drop(accounts);
                warn!(conn = self.id, to, "mail list for someone else");
                return self.result(ticket, code::AUTH_FAILED, Some("Not yours"));
            }
            accounts.mails_to(to).cloned().collect()
        };
        for letter in letters {
            let entry = self
                .message("PrivateMessage")
                .with("delivery_target", Value::Int(letter.to as i64))
                .with("message_id", Value::Int(letter.id as i64))
                .with("creator", Value::Int(letter.from as i64))
                .with("creation_time", Value::Int(letter.created_unix as i64))
                .with("title", Value::text(&letter.title))
                .with("status", Value::Int(letter.status as i64))
                .with("message_text", Value::Bytes(Some(letter.text)))
                .with("data", Value::Bytes(Some(letter.data)))
                .with("ticket_id", Value::Int(ticket));
            self.send(entry);
        }
        self.result(ticket, code::OK, None);
    }

    /// AddPrivateMessage {delivery_target, creator, title, message_text,
    /// data}: a letter from "Post versenden", from one of the sender's
    /// characters to another character. The client looked the receiver's
    /// name up first (RequestCharacters), so it reaches that player even when
    /// offline. It is answered with an
    /// AddResult carrying the letter's id. A letter from someone the
    /// receiver ignores is not stored, but the sender is not told.
    pub(super) fn send_mail(&mut self, msg: &Message, ticket: i64) {
        let to = msg.int("delivery_target").unwrap_or(0) as u32;
        let from = msg.int("creator").unwrap_or(0) as u32;
        let mut reply = self.message("AddResult").with("ticket_id", Value::Int(ticket));
        let outcome = {
            let mut accounts = self.server.accounts.lock().unwrap();
            let sender_ok = self.account.is_some_and(|a| accounts.character_ids(a).contains(&from));
            let receiver = accounts.owner(to).map(|a| a.id);
            match (sender_ok, receiver) {
                (false, _) => Err("Not your character"),
                (_, None) => Err("Unknown receiver"),
                (true, Some(_)) if accounts.ignores(to, from) => Ok(None),
                (true, Some(_)) => {
                    let letter = Mail {
                        id: 0,
                        to,
                        from,
                        created_unix: now(),
                        title: msg.string("title").unwrap_or_default(),
                        text: msg.bytes("message_text").unwrap_or_default().to_vec(),
                        data: msg.bytes("data").unwrap_or_default().to_vec(),
                        status: 0,
                    };
                    accounts.add_mail(letter).map(Some).map_err(|e| {
                        warn!(conn = self.id, "saving mail failed: {e}");
                        "Server error"
                    })
                }
            }
        };
        match outcome {
            Ok(letter) => {
                match &letter {
                    Some(l) => info!(conn = self.id, id = l.id, from, to, "letter: {}", l.title),
                    None => info!(conn = self.id, from, to, "letter dropped: the receiver ignores the sender"),
                }
                reply.set("errorcode", Value::Int(code::OK));
                reply.set("id", Value::Int(letter.map_or(0, |l| l.id) as i64));
            }
            Err(text) => {
                warn!(conn = self.id, from, to, "letter refused: {text}");
                reply.set("errorcode", Value::Int(code::AUTH_FAILED));
                reply.set("errormsg", Value::text(text));
            }
        }
        self.send(reply);
    }

    /// ChangePrivateMessage {message_id, status 1} (a letter was opened) and
    /// RemovePrivateMessage {message_id} ("LÖSCHEN"), for the player's own
    /// letters.
    pub(super) fn edit_mail(&mut self, msg: &Message, ticket: i64) {
        let id = msg.int("message_id").unwrap_or(0) as u32;
        let done = {
            let mut accounts = self.server.accounts.lock().unwrap();
            let mine = match (self.account, accounts.mail(id)) {
                (Some(a), Some(letter)) => accounts.character_ids(a).contains(&letter.to),
                _ => false,
            };
            if !mine {
                Ok(false)
            } else if msg.name == "RemovePrivateMessage" {
                accounts.remove_mail(id)
            } else {
                accounts.set_mail_status(id, msg.int("status").unwrap_or(1) as u8)
            }
        };
        match done {
            Ok(true) => self.result(ticket, code::OK, None),
            Ok(false) => self.result(ticket, code::AUTH_FAILED, Some("Unknown letter")),
            Err(e) => {
                warn!(conn = self.id, "saving mail failed: {e}");
                self.result(ticket, code::AUTH_FAILED, Some("Server error"));
            }
        }
    }

}

fn now() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32)
}

#[cfg(test)]
mod tests {
    use super::super::tests::DEFS;
    use super::*;
    use crate::accounts::Accounts;
    use crate::village::Event;
    use crate::{Role, Server};
    use sadk_lobby_proto::frame::Frame;
    use std::sync::Arc;

    /// The list and mailbox messages, from the retail msgdefs.ini.
    const SOCIAL_DEFS: &str = r#"# RequestUserBuddyList
[NETMSG_TYPE_56]
type="UNSHORT"
user_id="UNLONG"
ticket_id="UNLONG"
# UserBuddyConn
[NETMSG_TYPE_61]
type="UNSHORT"
user_id="UNLONG"
buddy_id="UNLONG"
name="STRING 128"
perm_id_type="UNSHORT"
status="UNBYTE"
server_id="UNLONG"
server_name="STRING 128"
ticket_id="UNLONG"
# AddUserBuddy
[NETMSG_TYPE_98]
type="UNSHORT"
user_id="UNLONG"
buddy_id="UNLONG"
ticket_id="UNLONG"
# RemoveUserBuddy
[NETMSG_TYPE_99]
type="UNSHORT"
user_id="UNLONG"
buddy_id="UNLONG"
ticket_id="UNLONG"
# RequestPrivateMessageList
[NETMSG_TYPE_147]
type="UNSHORT"
delivery_target="UNLONG"
selection="UNLONG"
ticket_id="UNLONG"
# ChangePrivateMessage
[NETMSG_TYPE_148]
type="UNSHORT"
message_id="UNLONG"
status="UNBYTE"
ticket_id="UNLONG"
# PrivateMessage
[NETMSG_TYPE_149]
type="UNSHORT"
delivery_target="UNLONG"
message_id="UNLONG"
creator="UNLONG"
creation_time="UNLONG"
title="STRING 128"
status="UNBYTE"
message_text="MEMBLOCK 0"
data="MEMBLOCK 0"
ticket_id="UNLONG"
# AddPrivateMessage
[NETMSG_TYPE_150]
type="UNSHORT"
delivery_target="UNLONG"
creator="UNLONG"
creation_time="UNLONG"
title="STRING 128"
message_text="MEMBLOCK 0"
data="MEMBLOCK 0"
ticket_id="UNLONG"
# RemovePrivateMessage
[NETMSG_TYPE_151]
type="UNSHORT"
message_id="UNLONG"
ticket_id="UNLONG"
# AddUserIgnore
[NETMSG_TYPE_154]
type="UNSHORT"
user_id="UNLONG"
ignore_id="UNLONG"
ticket_id="UNLONG"
# RemoveUserIgnore
[NETMSG_TYPE_155]
type="UNSHORT"
user_id="UNLONG"
ignore_id="UNLONG"
ticket_id="UNLONG"
# RequestUserIgnoreList
[NETMSG_TYPE_157]
type="UNSHORT"
user_id="UNLONG"
ticket_id="UNLONG"
# UserIgnoreConn
[NETMSG_TYPE_160]
type="UNSHORT"
user_id="UNLONG"
ignore_id="UNLONG"
name="STRING 128"
perm_id_type="UNSHORT"
status="UNBYTE"
server_id="UNLONG"
server_name="STRING 128"
ticket_id="UNLONG"
"#;

    /// Account and character of "siedler"/garibald and "besucher"/Besucher.
    struct Player {
        account: u32,
        character: u32,
    }

    fn server(name: &str) -> (Arc<Server>, [Player; 2]) {
        let path = std::env::temp_dir().join(format!("sadk-social-{name}-{}", std::process::id())).join("accounts.json");
        let _ = std::fs::remove_file(&path);
        let mut accounts = Accounts::open(path).unwrap();
        let players = [("siedler", "garibald"), ("besucher", "Besucher")].map(|(nick, character)| {
            let account = accounts.create(nick, b"x").unwrap().unwrap().id;
            let character = accounts.add_character(account, character, vec![]).unwrap().unwrap().id;
            Player { account, character }
        });
        (Server::test(&format!("{DEFS}{SOCIAL_DEFS}"), accounts), players)
    }

    /// A lobby connection after the login (`authenticate`).
    fn lobby(server: &Arc<Server>, conn: u32, player: &Player) -> Session {
        let mut s = Session::new(server.clone(), conn, Role::Lobby, "lobby".into());
        s.account = Some(player.account);
        s.lobby_login(player.account);
        s
    }

    /// A usercomm connection logged in with the character's id, as the game does.
    fn usercomm(server: &Arc<Server>, conn: u32, player: &Player) -> Session {
        let mut s = Session::new(server.clone(), conn, Role::Usercomm, "uc".into());
        s.token_login(player.character as i64, 1);
        s.out.clear();
        s
    }

    /// Hands pending events to the session, then decodes the lobby messages it queued.
    fn sent(s: &mut Session) -> Vec<Message> {
        let events = s.events.as_mut().unwrap();
        let pending: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        for event in pending {
            s.handle_event(event);
        }
        let mut out = std::mem::take(&mut s.out);
        let mut msgs = Vec::new();
        while let Some((frame, used)) = Frame::decode(&out).unwrap() {
            msgs.push(Message::decode(&s.server.defs, &frame.payload).unwrap());
            out.drain(..used);
        }
        msgs
    }

    fn names(msgs: &[Message]) -> Vec<&str> {
        msgs.iter().map(|m| m.name.as_str()).collect()
    }

    fn request(s: &Session, name: &str, fields: &[(&str, i64)]) -> Message {
        fields.iter().fold(s.message(name), |m, &(f, v)| m.with(f, Value::Int(v)))
    }

    /// (status, server_id, server_name, ticket) of a list entry.
    fn status(entry: &Message) -> (Option<i64>, Option<i64>, String, Option<i64>) {
        (entry.int("status"), entry.int("server_id"), entry.string("server_name").unwrap_or_default(), entry.int("ticket_id"))
    }

    /// A friend is listed with its name and character type; its entry turns
    /// online when it logs in to usercomm and offline when it leaves.
    #[test]
    fn friends_are_listed_and_followed_online() {
        let (server, [me, friend]) = server("friends");
        let mut s = lobby(&server, 1, &me);
        s.handle_message(request(&s, "AddUserBuddy", &[("user_id", me.account as i64), ("buddy_id", friend.character as i64), ("ticket_id", 4)]));
        let out = sent(&mut s);
        assert_eq!(names(&out), ["Result"]);
        assert_eq!(out[0].int("errorcode"), Some(code::OK));

        s.handle_message(request(&s, "RequestUserBuddyList", &[("user_id", me.account as i64), ("ticket_id", 5)]));
        let out = sent(&mut s);
        assert_eq!(names(&out), ["UserBuddyConn", "Result"]);
        let entry = &out[0];
        assert_eq!((entry.int("user_id"), entry.int("buddy_id")), (Some(me.account as i64), Some(friend.character as i64)));
        assert_eq!((entry.string("name").unwrap().as_str(), entry.int("perm_id_type")), ("Besucher", Some(CHARACTER_TYPE)));
        assert_eq!(status(entry), (Some(0), Some(0), String::new(), Some(5)));

        let mut chat = usercomm(&server, 2, &friend);
        let out = sent(&mut s);
        assert_eq!(names(&out), ["UserBuddyConn"]);
        assert_eq!(status(&out[0]), (Some(ONLINE), Some(ONLINE_SERVER_ID), VILLAGE_NAME.to_string(), Some(0)));

        chat.leave_all_channels();
        let out = sent(&mut s);
        assert_eq!(names(&out), ["UserBuddyConn"]);
        assert_eq!(status(&out[0]), (Some(0), Some(0), String::new(), Some(0)));

        s.handle_message(request(&s, "RemoveUserBuddy", &[("user_id", me.account as i64), ("buddy_id", friend.character as i64), ("ticket_id", 6)]));
        assert_eq!(sent(&mut s)[0].int("errorcode"), Some(code::OK));
        s.handle_message(request(&s, "RequestUserBuddyList", &[("user_id", me.account as i64), ("ticket_id", 7)]));
        assert_eq!(names(&sent(&mut s)), ["Result"]);
    }

    /// The client lists a friend it just added as offline, so the status of
    /// one who is online follows the Result.
    #[test]
    fn adding_an_online_friend_shows_it_online() {
        let (server, [me, friend]) = server("online-friend");
        let _chat = usercomm(&server, 2, &friend);
        let mut s = lobby(&server, 1, &me);
        s.handle_message(request(&s, "AddUserBuddy", &[("user_id", me.account as i64), ("buddy_id", friend.character as i64), ("ticket_id", 4)]));
        let out = sent(&mut s);
        assert_eq!(names(&out), ["Result", "UserBuddyConn"]);
        assert_eq!(status(&out[1]), (Some(ONLINE), Some(ONLINE_SERVER_ID), VILLAGE_NAME.to_string(), Some(0)));
    }

    #[test]
    fn ignore_list_names_characters() {
        let (server, [me, other]) = server("ignore");
        let mut s = lobby(&server, 1, &me);
        for (id, code) in [(other.character, code::OK), (me.character, code::AUTH_FAILED), (999, code::AUTH_FAILED)] {
            s.handle_message(request(&s, "AddUserIgnore", &[("user_id", me.account as i64), ("ignore_id", id as i64), ("ticket_id", 4)]));
            assert_eq!(sent(&mut s)[0].int("errorcode"), Some(code), "ignoring {id}");
        }
        assert!(server.accounts.lock().unwrap().ignores(me.character, other.character));
        s.handle_message(request(&s, "RequestUserIgnoreList", &[("user_id", me.account as i64), ("ticket_id", 5)]));
        let out = sent(&mut s);
        assert_eq!(names(&out), ["UserIgnoreConn", "Result"]);
        assert_eq!((out[0].int("ignore_id"), out[0].string("name").unwrap().as_str()), (Some(other.character as i64), "Besucher"));
        s.handle_message(request(&s, "RemoveUserIgnore", &[("user_id", me.account as i64), ("ignore_id", other.character as i64), ("ticket_id", 6)]));
        assert_eq!(sent(&mut s)[0].int("errorcode"), Some(code::OK));
        assert!(!server.accounts.lock().unwrap().ignores(me.character, other.character));
    }

    /// A letter goes from one character to another, waits until the
    /// receiver's mailbox asks, is marked read and deleted; nobody else may
    /// read or touch it.
    #[test]
    fn letters_reach_the_receivers_mailbox() {
        let (server, [me, friend]) = server("mail");
        let (mut a, mut b) = (lobby(&server, 1, &me), lobby(&server, 2, &friend));
        let letter = |s: &Session, from: u32, to: u32| {
            request(s, "AddPrivateMessage", &[("delivery_target", to as i64), ("creator", from as i64), ("creation_time", 0), ("ticket_id", 9)])
                .with("title", Value::text("Hallo"))
                .with("message_text", Value::Bytes(Some(b"Wie geht's?\0".to_vec())))
                .with("data", Value::Bytes(Some(vec![])))
        };
        a.handle_message(letter(&a, me.character, friend.character));
        let out = sent(&mut a);
        assert_eq!(names(&out), ["AddResult"]);
        assert_eq!((out[0].int("errorcode"), out[0].int("id"), out[0].int("ticket_id")), (Some(code::OK), Some(1), Some(9)));
        // Not from someone else's character, not to an unknown one.
        for (from, to) in [(friend.character, friend.character), (me.character, 999)] {
            a.handle_message(letter(&a, from, to));
            assert_eq!(sent(&mut a)[0].int("errorcode"), Some(code::AUTH_FAILED));
        }

        let list = |s: &mut Session, to: u32| {
            s.handle_message(request(s, "RequestPrivateMessageList", &[("delivery_target", to as i64), ("selection", 0), ("ticket_id", 5)]));
            sent(s)
        };
        assert_eq!(list(&mut a, friend.character).last().unwrap().int("errorcode"), Some(code::AUTH_FAILED));
        assert_eq!(names(&list(&mut a, me.character)), ["Result"]);
        let out = list(&mut b, friend.character);
        assert_eq!(names(&out), ["PrivateMessage", "Result"]);
        let mail = &out[0];
        assert_eq!((mail.int("message_id"), mail.int("creator"), mail.int("delivery_target")), (Some(1), Some(me.character as i64), Some(friend.character as i64)));
        assert_eq!((mail.string("title").unwrap().as_str(), mail.bytes("message_text").unwrap(), mail.int("status")), ("Hallo", &b"Wie geht's?\0"[..], Some(0)));
        assert_eq!(mail.int("ticket_id"), Some(5));
        assert!(mail.int("creation_time").unwrap() > 0);

        for s in [&mut a, &mut b] {
            s.handle_message(request(s, "ChangePrivateMessage", &[("message_id", 1), ("status", 1), ("ticket_id", 6)]));
        }
        assert_eq!(sent(&mut a)[0].int("errorcode"), Some(code::AUTH_FAILED));
        assert_eq!(sent(&mut b)[0].int("errorcode"), Some(code::OK));
        assert_eq!(list(&mut b, friend.character)[0].int("status"), Some(1));

        for s in [&mut a, &mut b] {
            s.handle_message(request(s, "RemovePrivateMessage", &[("message_id", 1), ("ticket_id", 7)]));
        }
        assert_eq!(sent(&mut a)[0].int("errorcode"), Some(code::AUTH_FAILED));
        assert_eq!(sent(&mut b)[0].int("errorcode"), Some(code::OK));
        assert_eq!(names(&list(&mut b, friend.character)), ["Result"]);
    }

    /// A letter to someone who ignores the sender is accepted but never arrives.
    #[test]
    fn letters_from_ignored_players_are_dropped() {
        let (server, [me, other]) = server("mail-ignored");
        server.accounts.lock().unwrap().set_listed(other.account, List::Ignored, me.character, true).unwrap();
        let mut a = lobby(&server, 1, &me);
        let msg = request(&a, "AddPrivateMessage", &[("delivery_target", other.character as i64), ("creator", me.character as i64), ("ticket_id", 3)])
            .with("title", Value::text("Hallo"));
        a.handle_message(msg);
        let out = sent(&mut a);
        assert_eq!((out[0].int("errorcode"), out[0].int("id")), (Some(code::OK), Some(0)));
        assert_eq!(server.accounts.lock().unwrap().mails_to(other.character).count(), 0);
    }
}

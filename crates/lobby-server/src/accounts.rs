//! Lobby accounts, kept in a small JSON file next to the server.
//!
//! The retail client sends the password as typed (8-bit, Windows-1252),
//! inside the Twofish-encrypted `AuthenticateUser` block; tincat3's
//! `TinCat_Scramble` is used only by the old `RequestLogin` message. We
//! store `sha256(salt || password bytes)`, so the file holds no password.

use sadk_lobby_proto::lobby_message::{repair_character_data, Stats};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: u32,
    pub nick: String,
    salt: String,
    password_hash: String,
    pub created_unix: u64,
    /// Lobby "properties" (PropertyGet/PropertySet*), keyed "kategory:index".
    #[serde(default)]
    properties: BTreeMap<String, i32>,
    #[serde(default)]
    pub characters: Vec<Character>,
    /// Friend list ("Freunde"): character ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buddies: Vec<u32>,
    /// Ignore list ("Ignorieren"): character ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<u32>,
    /// Where others reach the matches this player hosts ("ip" or
    /// "ip:port"), set with the chat command `!adresse`; none means the
    /// address the lobby sees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_address: Option<String>,
}

/// Which of an account's two lists of characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum List {
    Buddies,
    Ignored,
}

/// A letter in the in-game mailbox ("Briefkasten"): `PrivateMessage`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mail {
    pub id: u32,
    /// Receiver (`delivery_target`), the id the client asks its mail for.
    pub to: u32,
    /// Sender (`creator`), a character id.
    pub from: u32,
    pub created_unix: u32,
    pub title: String,
    #[serde(with = "hex_bytes")]
    pub text: Vec<u8>,
    #[serde(with = "hex_bytes", default)]
    pub data: Vec<u8>,
    /// 0 unread, 1 read (`ChangePrivateMessage`).
    pub status: u8,
}

/// A lobby avatar. `data` is the client's look of it: six `u32` block sizes,
/// then the blocks (`lobby_message::character_blocks`). The balance is the
/// server's: block 4 of `data` is replaced by it on the way out
/// (`lobby_message::with_stats`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Character {
    pub id: u32,
    pub name: String,
    #[serde(with = "hex_bytes")]
    pub data: Vec<u8>,
    #[serde(default = "start_gold")]
    pub gold: u32,
    #[serde(default = "start_funnies")]
    pub funnies: u32,
}

/// What a new character starts with, in gold and in funnies. Characters
/// saved before balances were kept get it too.
pub const START_BALANCE: u32 = 20;

/// No balance goes below this: losses beyond it are forgiven, so a player
/// can always sit down at a table again.
pub const MIN_BALANCE: u32 = 10;

fn start_gold() -> u32 {
    START_BALANCE
}

fn start_funnies() -> u32 {
    START_BALANCE
}

impl Character {
    pub fn new(id: u32, name: &str, data: Vec<u8>) -> Self {
        Self { id, name: name.to_string(), data, gold: start_gold(), funnies: start_funnies() }
    }

    /// Level and experience are not kept: every character is level 1.
    pub fn stats(&self) -> Stats {
        Stats { gold: self.gold, funnies: self.funnies, ..Stats::default() }
    }

    /// The balance in funnies or gold.
    pub fn balance(&self, funnies: bool) -> u32 {
        if funnies {
            self.funnies
        } else {
            self.gold
        }
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        (0..text.len())
            .step_by(2)
            .map(|i| text.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .collect::<Option<Vec<u8>>>()
            .ok_or_else(|| serde::de::Error::custom("bad hex"))
    }
}

/// Accounts and characters share one id space, as perm ids did on the
/// original server: the client names both by bare id (a character in chat,
/// the village and the lists, an account at login). New ids come from the
/// larger of the two counters; a file written before they were shared may
/// still hold a character and an account with the same id.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    next_id: u32,
    #[serde(default)]
    next_character_id: u32,
    accounts: BTreeMap<String, Account>,
    #[serde(default)]
    next_mail_id: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mails: Vec<Mail>,
}

pub struct Accounts {
    path: PathBuf,
    store: Store,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoginError {
    UnknownUser,
    WrongPassword,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash(salt: &str, password: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(password);
    hex(&h.finalize())
}

/// Nicks compare case-insensitively, like most lobby systems of the time.
fn key(nick: &str) -> String {
    nick.to_lowercase()
}

impl Accounts {
    /// Loads the accounts. Character data in the layout this server once
    /// assumed is rewritten into the client's layout and saved.
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let mut store: Store = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(std::io::Error::other)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store { next_id: 1, ..Store::default() },
            Err(e) => return Err(e),
        };
        let mut repaired = 0;
        for c in store.accounts.values_mut().flat_map(|a| a.characters.iter_mut()) {
            if let Some(data) = repair_character_data(&c.data) {
                tracing::warn!(char_id = c.id, "rewrote the data of character {} into the client's block layout", c.name);
                c.data = data;
                repaired += 1;
            }
        }
        let accounts = Self { path, store };
        if repaired > 0 {
            accounts.save()?;
        }
        Ok(accounts)
    }

    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.store).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, &self.path)
    }

    pub fn len(&self) -> usize {
        self.store.accounts.len()
    }

    /// Returns `Ok(None)` when the nick is taken.
    pub fn create(&mut self, nick: &str, password: &[u8]) -> std::io::Result<Option<Account>> {
        if self.store.accounts.contains_key(&key(nick)) {
            return Ok(None);
        }
        let mut salt = [0u8; 16];
        getrandom::getrandom(&mut salt).map_err(|e| std::io::Error::other(e.to_string()))?;
        let salt = hex(&salt);
        let account = Account {
            id: self.fresh_id(),
            nick: nick.to_string(),
            password_hash: hash(&salt, password),
            salt,
            created_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            properties: BTreeMap::new(),
            characters: Vec::new(),
            buddies: Vec::new(),
            ignored: Vec::new(),
            match_address: None,
        };
        self.store.accounts.insert(key(nick), account.clone());
        self.save()?;
        Ok(Some(account))
    }

    fn fresh_id(&mut self) -> u32 {
        let id = self.store.next_id.max(self.store.next_character_id).max(1);
        self.store.next_id = id + 1;
        self.store.next_character_id = id + 1;
        id
    }

    fn by_id(&mut self, id: u32) -> Option<&mut Account> {
        self.store.accounts.values_mut().find(|a| a.id == id)
    }

    /// Unset properties read as 0, like on the original server.
    pub fn property(&self, id: u32, kategory: i32, index: i32) -> i32 {
        self.store
            .accounts
            .values()
            .find(|a| a.id == id)
            .and_then(|a| a.properties.get(&format!("{kategory}:{index}")).copied())
            .unwrap_or(0)
    }

    /// Sets (or with `relative`, adds to) a property; returns the new value.
    pub fn set_property(&mut self, id: u32, kategory: i32, index: i32, value: i32, relative: bool) -> std::io::Result<i32> {
        let Some(account) = self.by_id(id) else { return Ok(0) };
        let slot = account.properties.entry(format!("{kategory}:{index}")).or_insert(0);
        *slot = if relative { slot.saturating_add(value) } else { value };
        let new = *slot;
        self.save()?;
        Ok(new)
    }

    pub fn account(&self, id: u32) -> Option<&Account> {
        self.store.accounts.values().find(|a| a.id == id)
    }

    /// Character names are unique across all accounts. `Ok(None)` = taken.
    pub fn add_character(&mut self, owner: u32, name: &str, data: Vec<u8>) -> std::io::Result<Option<Character>> {
        let taken = self.store.accounts.values().flat_map(|a| &a.characters).any(|c| key(&c.name) == key(name));
        if taken || self.account(owner).is_none() {
            return Ok(None);
        }
        let id = self.fresh_id();
        let character = Character::new(id, name, data);
        self.by_id(owner).unwrap().characters.push(character.clone());
        self.save()?;
        Ok(Some(character))
    }

    /// Finds a character by id (or by name when `id` is 0), with its owner.
    pub fn character(&self, id: u32, name: &str) -> Option<(&Account, &Character)> {
        self.store.accounts.values().find_map(|a| {
            a.characters
                .iter()
                .find(|c| if id != 0 { c.id == id } else { key(&c.name) == key(name) })
                .map(|c| (a, c))
        })
    }

    /// Updates a character owned by `owner`; returns false if it is not theirs.
    pub fn change_character(&mut self, owner: u32, id: u32, name: Option<&str>, data: Option<Vec<u8>>) -> std::io::Result<bool> {
        let Some(c) = self.by_id(owner).and_then(|a| a.characters.iter_mut().find(|c| c.id == id)) else {
            return Ok(false);
        };
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            c.name = name.to_string();
        }
        if let Some(data) = data {
            c.data = data;
        }
        self.save()?;
        Ok(true)
    }

    /// Adds `delta` to a character's funnies or gold (never below
    /// [`MIN_BALANCE`]) and saves; returns the new stats, or `None` for an
    /// unknown character.
    pub fn add_balance(&mut self, char_id: u32, funnies: bool, delta: i64) -> std::io::Result<Option<Stats>> {
        let Some(c) = self.store.accounts.values_mut().flat_map(|a| a.characters.iter_mut()).find(|c| c.id == char_id) else {
            return Ok(None);
        };
        let balance = if funnies { &mut c.funnies } else { &mut c.gold };
        *balance = (*balance as i64 + delta).clamp(MIN_BALANCE as i64, u32::MAX as i64) as u32;
        let stats = c.stats();
        self.save()?;
        Ok(Some(stats))
    }

    pub fn remove_character(&mut self, owner: u32, id: u32) -> std::io::Result<bool> {
        let Some(account) = self.by_id(owner) else { return Ok(false) };
        let before = account.characters.len();
        account.characters.retain(|c| c.id != id);
        let removed = account.characters.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// The account owning a character.
    pub fn owner(&self, char_id: u32) -> Option<&Account> {
        self.store.accounts.values().find(|a| a.characters.iter().any(|c| c.id == char_id))
    }

    pub fn list(&self, account: u32, list: List) -> &[u32] {
        match (self.account(account), list) {
            (Some(a), List::Buddies) => &a.buddies,
            (Some(a), List::Ignored) => &a.ignored,
            (None, _) => &[],
        }
    }

    /// Adds a character to (or with `add` false, takes it off) one of an
    /// account's lists. `Ok(false)` when the account or character does not
    /// exist; adding twice keeps one entry.
    pub fn set_listed(&mut self, account: u32, list: List, char_id: u32, add: bool) -> std::io::Result<bool> {
        if add && self.character(char_id, "").filter(|_| char_id != 0).is_none() {
            return Ok(false);
        }
        let Some(a) = self.by_id(account) else { return Ok(false) };
        let ids = match list {
            List::Buddies => &mut a.buddies,
            List::Ignored => &mut a.ignored,
        };
        let had = ids.contains(&char_id);
        if add && !had {
            ids.push(char_id);
        } else if !add && had {
            ids.retain(|&c| c != char_id);
        } else {
            return Ok(true);
        }
        self.save()?;
        Ok(true)
    }

    /// Sets (or with `None` clears) the account's match address.
    pub fn set_match_address(&mut self, account: u32, address: Option<String>) -> std::io::Result<bool> {
        let Some(a) = self.by_id(account) else { return Ok(false) };
        a.match_address = address;
        self.save()?;
        Ok(true)
    }

    /// Accounts with `char_id` on their friend list.
    pub fn befriended(&self, char_id: u32) -> Vec<u32> {
        self.store.accounts.values().filter(|a| a.buddies.contains(&char_id)).map(|a| a.id).collect()
    }

    /// Whether the account behind `receiver` (an account or character id)
    /// has character `sender` on its ignore list.
    pub fn ignores(&self, receiver: u32, sender: u32) -> bool {
        let account = self.owner(receiver).or_else(|| self.account(receiver));
        account.is_some_and(|a| a.ignored.contains(&sender))
    }

    /// An account's character ids: whom its letters may come from and go to.
    pub fn character_ids(&self, account: u32) -> Vec<u32> {
        self.account(account).into_iter().flat_map(|a| a.characters.iter().map(|c| c.id)).collect()
    }

    pub fn mails_to(&self, to: u32) -> impl Iterator<Item = &Mail> {
        self.store.mails.iter().filter(move |m| m.to == to)
    }

    pub fn mail(&self, id: u32) -> Option<&Mail> {
        self.store.mails.iter().find(|m| m.id == id)
    }

    /// Stores a new letter; its id and status are set here.
    pub fn add_mail(&mut self, mut mail: Mail) -> std::io::Result<Mail> {
        mail.id = self.store.next_mail_id.max(1);
        mail.status = 0;
        self.store.next_mail_id = mail.id + 1;
        self.store.mails.push(mail.clone());
        self.save()?;
        Ok(mail)
    }

    /// Sets a letter's status; `Ok(false)` if there is no such letter.
    pub fn set_mail_status(&mut self, id: u32, status: u8) -> std::io::Result<bool> {
        let Some(mail) = self.store.mails.iter_mut().find(|m| m.id == id) else { return Ok(false) };
        if mail.status != status {
            mail.status = status;
            self.save()?;
        }
        Ok(true)
    }

    pub fn remove_mail(&mut self, id: u32) -> std::io::Result<bool> {
        let before = self.store.mails.len();
        self.store.mails.retain(|m| m.id != id);
        let removed = self.store.mails.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn login(&self, nick: &str, password: &[u8]) -> Result<&Account, LoginError> {
        let account = self.store.accounts.get(&key(nick)).ok_or(LoginError::UnknownUser)?;
        if hash(&account.salt, password) == account.password_hash {
            Ok(account)
        } else {
            Err(LoginError::WrongPassword)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_login_and_reload() {
        let dir = std::env::temp_dir().join(format!("sadk-lobby-test-{}", std::process::id()));
        let path = dir.join("accounts.json");
        let _ = std::fs::remove_file(&path);
        let mut accounts = Accounts::open(path.clone()).unwrap();
        let a = accounts.create("Siedler", b"test123").unwrap().unwrap();
        assert!(accounts.create("siedler", b"x").unwrap().is_none());
        let b = accounts.create("Other", b"x").unwrap().unwrap();
        assert_ne!(a.id, b.id);

        let reloaded = Accounts::open(path.clone()).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.login("SIEDLER", b"test123").unwrap().id, a.id);
        assert_eq!(reloaded.login("Siedler", b"test124").unwrap_err(), LoginError::WrongPassword);
        assert_eq!(reloaded.login("nobody", b"").unwrap_err(), LoginError::UnknownUser);
        let mut reloaded = reloaded;
        assert_eq!(reloaded.property(a.id, 1, 1), 0);
        assert_eq!(reloaded.set_property(a.id, 1, 1, 5, false).unwrap(), 5);
        assert_eq!(reloaded.set_property(a.id, 1, 1, -2, true).unwrap(), 3);
        assert_eq!(Accounts::open(path.clone()).unwrap().property(a.id, 1, 1), 3);
        assert_eq!(reloaded.property(b.id, 1, 1), 0);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("test123"));

        let c = reloaded.add_character(a.id, "Garibald", vec![1, 0xab]).unwrap().unwrap();
        assert!(reloaded.add_character(b.id, "garibald", vec![]).unwrap().is_none());
        assert!(!reloaded.change_character(b.id, c.id, None, Some(vec![9])).unwrap());
        assert!(reloaded.change_character(a.id, c.id, None, Some(vec![9])).unwrap());
        let again = Accounts::open(path.clone()).unwrap();
        let (owner, found) = again.character(0, "GARIBALD").unwrap();
        assert_eq!((owner.id, found.id, found.data.clone()), (a.id, c.id, vec![9]));
        assert!(reloaded.remove_character(a.id, c.id).unwrap());
        assert!(reloaded.character(c.id, "").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lists_and_mail_survive_a_reload() {
        let dir = std::env::temp_dir().join(format!("sadk-lobby-test-social-{}", std::process::id()));
        let path = dir.join("accounts.json");
        let _ = std::fs::remove_file(&path);
        let mut accounts = Accounts::open(path.clone()).unwrap();
        let a = accounts.create("siedler", b"x").unwrap().unwrap().id;
        let b = accounts.create("besucher", b"x").unwrap().unwrap().id;
        let garibald = accounts.add_character(a, "garibald", vec![]).unwrap().unwrap().id;
        let gast = accounts.add_character(b, "Besucher", vec![]).unwrap().unwrap().id;
        let c = accounts.create("dritter", b"x").unwrap().unwrap().id;
        assert_eq!([a, b, garibald, gast, c], [1, 2, 3, 4, 5], "accounts and characters share the ids");

        assert!(accounts.set_listed(a, List::Buddies, gast, true).unwrap());
        assert!(accounts.set_listed(a, List::Buddies, gast, true).unwrap());
        assert!(!accounts.set_listed(a, List::Buddies, 999, true).unwrap());
        assert!(accounts.set_listed(b, List::Ignored, garibald, true).unwrap());
        let letter = Mail { id: 0, to: gast, from: garibald, created_unix: 5, title: "Hallo".into(), text: b"Wie geht's?\0".to_vec(), data: vec![], status: 7 };
        let sent = accounts.add_mail(letter).unwrap();
        assert_eq!((sent.id, sent.status), (1, 0));

        let mut again = Accounts::open(path.clone()).unwrap();
        assert_eq!(again.list(a, List::Buddies), [gast]);
        assert_eq!(again.befriended(gast), [a]);
        assert!(again.ignores(gast, garibald) && again.ignores(b, garibald) && !again.ignores(garibald, gast));
        assert_eq!(again.character_ids(b), [gast]);
        assert_eq!(again.mails_to(gast).cloned().collect::<Vec<_>>(), [sent.clone()]);
        assert!(again.set_mail_status(sent.id, 1).unwrap());
        assert_eq!(Accounts::open(path.clone()).unwrap().mail(sent.id).unwrap().status, 1);
        assert!(again.remove_mail(sent.id).unwrap() && !again.remove_mail(sent.id).unwrap());
        assert!(again.set_listed(a, List::Buddies, gast, false).unwrap());
        assert!(Accounts::open(path.clone()).unwrap().list(a, List::Buddies).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn repairs_old_layout_character_data_on_open() {
        let dir = std::env::temp_dir().join(format!("sadk-lobby-test-repair-{}", std::process::id()));
        let path = dir.join("accounts.json");
        let _ = std::fs::remove_file(&path);
        let mut accounts = Accounts::open(path.clone()).unwrap();
        let a = accounts.create("besucher", b"x").unwrap().unwrap();
        // "Besucher" in the old "size, block" layout: a 44-byte creation
        // block with tribe 1, then five empty blocks.
        let mut old = 44u32.to_le_bytes().to_vec();
        for v in [1u32, 1, 1, 2, 1, 5, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0] {
            old.extend(v.to_le_bytes());
        }
        let c = accounts.add_character(a.id, "Besucher", old.clone()).unwrap().unwrap();
        // A character in the client's layout is left alone.
        let mut good = vec![0u8; 68];
        good[0] = 44;
        good[24] = 1;
        accounts.add_character(a.id, "Garibald", good.clone()).unwrap().unwrap();

        let reopened = Accounts::open(path.clone()).unwrap();
        let fixed = reopened.character(c.id, "").unwrap().1.data.clone();
        assert_eq!(&fixed[..24], [44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&fixed[24..], &old[4..48]);
        assert_eq!(reopened.character(0, "garibald").unwrap().1.data, good);
        let style = sadk_lobby_proto::lobby_message::Style::from_character("Besucher", &fixed);
        assert_eq!(style.tribe_gender >> 4, 1);
        // The repair was saved.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&super::hex(&fixed)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn balances_start_at_20_and_never_go_below_10() {
        let dir = std::env::temp_dir().join(format!("sadk-lobby-test-balance-{}", std::process::id()));
        let path = dir.join("accounts.json");
        let _ = std::fs::remove_file(&path);
        let mut accounts = Accounts::open(path.clone()).unwrap();
        let a = accounts.create("spieler", b"x").unwrap().unwrap();
        let c = accounts.add_character(a.id, "Spieler", vec![]).unwrap().unwrap();
        assert_eq!((c.gold, c.funnies), (20, 20));
        let s = accounts.add_balance(c.id, false, 7).unwrap().unwrap();
        assert_eq!((s.gold, s.funnies), (27, 20));
        let s = accounts.add_balance(c.id, true, -15).unwrap().unwrap();
        assert_eq!((s.gold, s.funnies), (27, 10), "losses below 10 are forgiven");
        assert_eq!(accounts.add_balance(999, true, 1).unwrap(), None);
        // Saved, and characters saved without a balance get the start one.
        let reopened = Accounts::open(path.clone()).unwrap();
        assert_eq!(reopened.character(c.id, "").unwrap().1.funnies, 10);
        let text = std::fs::read_to_string(&path).unwrap().replace("\"gold\": 27,", "").replace("\"funnies\": 10", "\"x\": 0");
        std::fs::write(&path, text).unwrap();
        let old = Accounts::open(path).unwrap();
        assert_eq!((old.character(c.id, "").unwrap().1.gold, old.character(c.id, "").unwrap().1.funnies), (20, 20));
        let _ = std::fs::remove_dir_all(dir);
    }
}

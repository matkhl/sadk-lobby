//! Parser for the game's `msgdefs.ini`, the schema of every lobby message:
//!
//! ```ini
//! # RequestLogin
//! [NETMSG_TYPE_4]
//! type="UNSHORT"
//! nick="STRING 256"
//! ```
//!
//! The section number is the wire id, the comment above it the message name,
//! and the keys are the fields in wire order (normally starting with `type`).

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldType {
    UnByte,
    SiByte,
    UnShort,
    SiShort,
    UnLong,
    SiLong,
    /// One byte, 0 or 1.
    LBool,
    /// u32 length including the terminating NUL, then the bytes. Length 0
    /// means "no string". The number is the declared maximum.
    String(u32),
    /// u32 length, then raw bytes. Declared maximum, 0 = unbounded.
    MemBlock(u32),
}

impl FieldType {
    fn parse(spec: &str) -> Option<Self> {
        let mut parts = spec.split_whitespace();
        let name = parts.next()?;
        let limit = parts.next().map(str::parse::<u32>);
        Some(match (name, limit) {
            ("UNBYTE", None) => Self::UnByte,
            ("SIBYTE", None) => Self::SiByte,
            ("UNSHORT", None) => Self::UnShort,
            ("SISHORT", None) => Self::SiShort,
            ("UNLONG", None) => Self::UnLong,
            ("SILONG", None) => Self::SiLong,
            ("LBOOL", None) => Self::LBool,
            ("STRING", Some(Ok(n))) => Self::String(n),
            ("MEMBLOCK", Some(Ok(n))) => Self::MemBlock(n),
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct FieldDef {
    pub name: String,
    pub ty: FieldType,
}

#[derive(Clone, Debug)]
pub struct MessageDef {
    pub id: u16,
    pub name: String,
    pub fields: Vec<FieldDef>,
}

#[derive(Clone, Debug, Default)]
pub struct MsgDefs {
    by_id: HashMap<u16, MessageDef>,
    by_name: HashMap<String, u16>,
}

impl MsgDefs {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut defs = MsgDefs::default();
        let mut pending_name: Option<String> = None;
        let mut current: Option<MessageDef> = None;
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            let at = || format!("msgdefs line {}", index + 1);
            if line.is_empty() {
                continue;
            }
            if let Some(comment) = line.strip_prefix('#') {
                pending_name = Some(comment.trim().to_string());
            } else if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                defs.finish(current.take())?;
                let id = section
                    .strip_prefix("NETMSG_TYPE_")
                    .and_then(|n| n.parse::<u16>().ok())
                    .ok_or_else(|| format!("{}: unexpected section [{section}]", at()))?;
                let name = pending_name.take().unwrap_or_else(|| format!("Type{id}"));
                current = Some(MessageDef { id, name, fields: Vec::new() });
            } else if let Some((key, value)) = line.split_once('=') {
                let def = current.as_mut().ok_or_else(|| format!("{}: field outside a section", at()))?;
                let spec = value.trim().trim_matches('"');
                let ty = FieldType::parse(spec).ok_or_else(|| format!("{}: unknown field type {spec:?}", at()))?;
                def.fields.push(FieldDef { name: key.trim().to_string(), ty });
            } else {
                return Err(format!("{}: cannot parse {line:?}", at()));
            }
        }
        defs.finish(current)?;
        Ok(defs)
    }

    fn finish(&mut self, def: Option<MessageDef>) -> Result<(), String> {
        let Some(def) = def else { return Ok(()) };
        // Every entry starts with type=UNSHORT except the retail file's
        // BrokerSoldNotification (46); fields are coded as listed either way.
        if let Some(f) = def.fields.iter().position(|f| f.name == "type") {
            if f != 0 || def.fields[f].ty != FieldType::UnShort {
                return Err(format!("NETMSG_TYPE_{} ({}): type must be the first field, UNSHORT", def.id, def.name));
            }
        }
        if self.by_id.contains_key(&def.id) {
            return Err(format!("NETMSG_TYPE_{} defined twice", def.id));
        }
        self.by_name.insert(def.name.clone(), def.id);
        self.by_id.insert(def.id, def);
        Ok(())
    }

    pub fn get(&self, id: u16) -> Option<&MessageDef> {
        self.by_id.get(&id)
    }

    pub fn by_name(&self, name: &str) -> Option<&MessageDef> {
        self.by_name.get(name).and_then(|id| self.by_id.get(id))
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A few definitions in the game's format, enough for codec tests.
    pub(crate) const SAMPLE: &str = r#"
# RequestLogin
[NETMSG_TYPE_4]
type="UNSHORT"
nick="STRING 256"
password="STRING 32"
cd_key="STRING 128"
keypool="UNSHORT"
patchlevel="UNLONG"
ticket_id="UNLONG"

# Result
[NETMSG_TYPE_42]
type="UNSHORT"
errorcode="UNBYTE"
errormsg="STRING 32"
ticket_id="UNLONG"

# Mixed
[NETMSG_TYPE_300]
type="UNSHORT"
flag="LBOOL"
delta="SILONG"
small="SIBYTE"
data="MEMBLOCK 0"
"#;

    #[test]
    fn parses_sample() {
        let defs = MsgDefs::parse(SAMPLE).unwrap();
        assert_eq!(defs.len(), 3);
        let login = defs.by_name("RequestLogin").unwrap();
        assert_eq!(login.id, 4);
        assert_eq!(login.fields.len(), 7);
        assert_eq!(login.fields[1].ty, FieldType::String(256));
        assert_eq!(defs.get(300).unwrap().fields[4].ty, FieldType::MemBlock(0));
    }

    #[test]
    fn rejects_bad_types() {
        assert!(MsgDefs::parse("[NETMSG_TYPE_1]\ntype=\"UNSHORT\"\nx=\"FLOAT\"").is_err());
        assert!(MsgDefs::parse("[NETMSG_TYPE_1]\nx=\"UNLONG\"\ntype=\"UNSHORT\"").is_err());
        assert!(MsgDefs::parse("[NETMSG_TYPE_1]\nx=\"UNLONG\"").is_ok());
    }

    /// Full check against a retail msgdefs.ini, when SADK_MSGDEFS names one
    /// (e.g. SADK_MSGDEFS="C:/Games/Die Siedler - Aufbruch der Kulturen/bin/msgdefs.ini").
    #[test]
    fn parses_retail_msgdefs_if_installed() {
        let Some(path) = std::env::var_os("SADK_MSGDEFS") else { return };
        let text = std::fs::read_to_string(path).expect("SADK_MSGDEFS is not readable");
        let defs = MsgDefs::parse(&text).unwrap();
        assert_eq!(defs.len(), 221);
        assert_eq!(defs.by_name("RequestCreateAccount").unwrap().id, 71);
        assert_eq!(defs.by_name("MOTD").unwrap().id, 106);
        assert_eq!(defs.by_name("CheckVersion").unwrap().id, 188);
    }
}

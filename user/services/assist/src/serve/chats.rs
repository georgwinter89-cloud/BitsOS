//! The chats on disk (plan/spec/assistent-eins K1): one file `$XDG_DATA_HOME/bitsos/chats/<id>.jsonl` per chat,
//! mode 0600 in a folder 0700, one JSON object per line, only ever appended to. The first line says what the chat
//! is; then Georg's questions, the assistant's answers, the tool lines and how a question ended. Pictures are not
//! stored: a question that carried one says so. A file that cannot be read is skipped (with an event), a line
//! that cannot be read is skipped, and nothing of either ends the service.

use crate::conv::{Conversation, Message, Role};
use crate::json::{self, Json};
use crate::prompt::Origin;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The words a stored question carries for a picture that was sent with it.
pub const PICTURE_SENT: &str = "[Ein Bild des Fensters wurde mitgeschickt; es ist nicht gespeichert.]";
/// The longest title.
const TITLE_MAX: usize = 80;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The folder of the chats: `$XDG_DATA_HOME/bitsos/chats`, else `.local/share/bitsos/chats` in the owner's home
/// (`HOME`, or `/data/home/<uid>` where a start that dropped root left HOME at `/`).
pub fn default_dir() -> PathBuf {
    let xdg = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute());
    xdg.or_else(|| crate::provider::program::pictures::owner_home().map(|h| h.join(".local/share"))).unwrap_or_else(|| PathBuf::from("/tmp")).join("bitsos/chats")
}

/// A chat id names a file: lower-case letters, digits and `-`, 1 to 40 of them.
pub fn valid_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

static NEXT: AtomicU64 = AtomicU64::new(0);

fn new_id() -> String {
    format!("c{:x}-{:x}", now_ms(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// One line as Georg reads a title: no control characters, bounded.
pub fn title_of(raw: &str) -> String {
    let line: String = raw.trim().lines().next().unwrap_or("").chars().filter(|c| !c.is_control()).collect();
    let t: String = line.trim().chars().take(TITLE_MAX).collect();
    if line.trim().chars().count() > TITLE_MAX { format!("{}…", t.trim_end()) } else { t }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    pub id: String,
    pub origin: Origin,
    /// The program the chat was started over (the popup's window), when there was one.
    pub app: Option<String>,
    pub title: String,
    /// Whether the title was given (by Georg or the view) rather than taken from the first question.
    pub named: bool,
    pub created: u64,
    pub updated: u64,
    /// Georg's questions so far.
    pub turns: usize,
}

impl Meta {
    pub fn json(&self) -> Json {
        Json::obj()
            .with("chat", self.id.as_str())
            .with("title", self.title.as_str())
            .with("origin", self.origin.name())
            .with("app", self.app.clone())
            .with("updated", i64::try_from(self.updated).unwrap_or(i64::MAX))
            .with("turns", i64::try_from(self.turns).unwrap_or(i64::MAX))
    }
}

/// What a stored line is.
#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    Georg { text: String, context: String, picture: bool, by: String },
    Assistant(String),
    Tool { name: String, state: String, line: String },
    /// A question that ended without an answer that is kept (an error, Esc): its words.
    Failed(String),
    Title(String),
}

impl Record {
    pub fn json(&self) -> Json {
        let j = Json::obj().with("at", i64::try_from(now_ms()).unwrap_or(0));
        match self {
            Record::Georg { text, context, picture, by } => {
                let mut j = j.with("t", "georg").with("text", text.as_str()).with("by", by.as_str());
                if !context.is_empty() {
                    j = j.with("context", context.as_str());
                }
                if *picture { j.with("picture", true) } else { j }
            }
            Record::Assistant(t) => j.with("t", "assistant").with("text", t.as_str()),
            Record::Tool { name, state, line } => j.with("t", "tool").with("name", name.as_str()).with("state", state.as_str()).with("line", line.as_str()),
            Record::Failed(t) => j.with("t", "failed").with("text", t.as_str()),
            Record::Title(t) => j.with("t", "title").with("title", t.as_str()),
        }
    }

    fn parse(j: &Json) -> Option<(Record, u64)> {
        let at = j.get("at").and_then(Json::as_i64).and_then(|a| u64::try_from(a).ok()).unwrap_or(0);
        let s = |k: &str| j.str(k).map(str::to_string);
        let r = match j.str("t")? {
            "georg" => Record::Georg { text: s("text")?, context: s("context").unwrap_or_default(), picture: j.get("picture").and_then(Json::as_bool) == Some(true), by: s("by").unwrap_or_else(|| "hand".into()) },
            "assistant" => Record::Assistant(s("text")?),
            "tool" => Record::Tool { name: s("name")?, state: s("state")?, line: s("line").unwrap_or_default() },
            "failed" => Record::Failed(s("text").unwrap_or_default()),
            "title" => Record::Title(s("title")?),
            _ => return None,
        };
        Some((r, at))
    }
}

/// A chat as it was read: what it is, and what happened in it, oldest first.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub meta: Meta,
    pub records: Vec<(Record, u64)>,
}

impl Loaded {
    /// The conversation as a provider gets it again after a restart: each question that was answered (with the
    /// window it carried, without its picture) and the answer's words. Tool calls stay out: the answers say
    /// what they did.
    pub fn conversation(&self) -> Conversation {
        let mut conv = Conversation::default();
        let mut open: Option<String> = None;
        for (r, _) in &self.records {
            match r {
                Record::Georg { text, context, picture, .. } => {
                    let mut t = if context.is_empty() { text.clone() } else { format!("{context}\n\nGeorg's question: {text}") };
                    if *picture {
                        t = format!("{t}\n{PICTURE_SENT}");
                    }
                    open = Some(t);
                }
                Record::Assistant(a) => {
                    if let Some(q) = open.take() {
                        conv.messages.push(Message::user(&q));
                    }
                    match conv.messages.last_mut() {
                        Some(m) if m.role == Role::Assistant => *m = Message { role: Role::Assistant, parts: vec![crate::conv::Part::Text(format!("{}\n\n{a}", m.text()))], raw: None },
                        _ => conv.messages.push(Message { role: Role::Assistant, parts: vec![crate::conv::Part::Text(a.clone())], raw: None }),
                    }
                }
                Record::Failed(_) => open = None,
                Record::Tool { .. } | Record::Title(_) => {}
            }
        }
        conv
    }
}

pub struct Store {
    pub dir: PathBuf,
}

/// Why a file was not taken.
#[derive(Debug)]
pub struct Skipped {
    pub file: String,
    pub why: String,
}

impl Store {
    pub fn at(dir: PathBuf) -> Store {
        Store { dir }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    fn ensure_dir(&self) -> Result<(), String> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        Ok(())
    }

    /// `new`: the chat's head line, which alone makes the file. Any other line goes only into a file that is
    /// there: a question still at work in a chat that was removed must not leave a file without its head.
    fn append(&self, id: &str, line: &Json, new: bool) -> Result<(), String> {
        let p = self.path(id);
        let mut f = std::fs::OpenOptions::new().read(true).append(true).create_new(new).mode(0o600).open(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        // A line cut short (the machine went off while it was written) is ended first, so it spoils only itself.
        let len = f.metadata().map_or(0, |m| m.len());
        let mut last = [0u8; 1];
        let cut = len > 0 && std::os::unix::fs::FileExt::read_at(&f, &mut last, len - 1).is_ok_and(|n| n == 1 && last[0] != b'\n');
        let text = format!("{}{}\n", if cut { "\n" } else { "" }, line.to_text());
        f.write_all(text.as_bytes()).map_err(|e| format!("{}: {e}", p.display()))
    }

    /// A new chat, written at once (it is in the list after a restart even before its first question).
    pub fn create(&self, origin: Origin, app: Option<&str>, title: Option<&str>) -> Result<Meta, String> {
        self.ensure_dir()?;
        let now = now_ms();
        let named = title.map(title_of).filter(|t| !t.is_empty());
        let meta = Meta { id: new_id(), origin, app: app.map(title_of).filter(|a| !a.is_empty()), title: named.clone().unwrap_or_else(|| "Neuer Chat".into()), named: named.is_some(), created: now, updated: now, turns: 0 };
        let mut head = Json::obj().with("t", "chat").with("chat", meta.id.as_str()).with("origin", origin.name()).with("at", i64::try_from(now).unwrap_or(0)).with("app", meta.app.clone());
        if meta.named {
            head = head.with("title", meta.title.as_str());
        }
        self.append(&meta.id, &head, true)?;
        Ok(meta)
    }

    pub fn add(&self, id: &str, r: &Record) -> Result<(), String> {
        self.append(id, &r.json(), false)
    }

    /// Every chat in the folder; what cannot be read is listed apart.
    pub fn load_all(&self) -> (Vec<Loaded>, Vec<Skipped>) {
        let (mut ok, mut bad) = (Vec::new(), Vec::new());
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return (ok, bad) };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".jsonl") else { continue };
            match self.load(id) {
                Ok(l) => ok.push(l),
                Err(why) => bad.push(Skipped { file: name.clone(), why }),
            }
        }
        (ok, bad)
    }

    pub fn load(&self, id: &str) -> Result<Loaded, String> {
        if !valid_id(id) {
            return Err("not a chat's name".into());
        }
        let p = self.path(id);
        let meta = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
        if !meta.file_type().is_file() {
            return Err("not a plain file".into());
        }
        let text = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
        let mut lines = text.lines();
        let head = lines.next().and_then(|l| json::parse(l).ok()).filter(|h| h.str("t") == Some("chat") && h.str("chat") == Some(id)).ok_or("its first line does not say which chat it is")?;
        let created = head.get("at").and_then(Json::as_i64).and_then(|a| u64::try_from(a).ok()).unwrap_or(0);
        let title = head.str("title").map(title_of);
        let mut m = Meta {
            id: id.to_string(),
            origin: head.str("origin").and_then(Origin::parse).unwrap_or_default(),
            app: head.str("app").map(title_of),
            named: title.is_some(),
            title: title.unwrap_or_else(|| "Neuer Chat".into()),
            created,
            updated: created,
            turns: 0,
        };
        let mut records = Vec::new();
        for line in lines {
            let Some((r, at)) = json::parse(line).ok().as_ref().and_then(Record::parse) else { continue };
            m.updated = m.updated.max(at);
            match &r {
                Record::Georg { text, .. } => {
                    m.turns += 1;
                    if !m.named && m.turns == 1 {
                        m.title = title_of(text);
                    }
                }
                Record::Title(t) => {
                    m.title = title_of(t);
                    m.named = true;
                }
                _ => {}
            }
            records.push((r, at));
        }
        Ok(Loaded { meta: m, records })
    }

    /// Takes the chat's file out of the folder, into the trash of its file system (K6).
    pub fn remove(&self, id: &str) -> Result<String, String> {
        if !valid_id(id) {
            return Err("not a chat's name".into());
        }
        crate::tools::native::trash::put(&self.path(id), id, "remove").map(|p| p.said())
    }

    pub fn exists(&self, id: &str) -> bool {
        valid_id(id) && self.path(id).is_file()
    }

    pub fn file(&self, id: &str) -> PathBuf {
        self.path(id)
    }
}

pub fn check_mode(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o077 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Store {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../../target/assist-chats-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Store::at(d)
    }

    #[test]
    fn a_chat_is_written_read_and_its_conversation_rebuilt() {
        let s = store("rw");
        let m = s.create(Origin::Popup, Some("brave"), None).unwrap();
        assert!(valid_id(&m.id) && check_mode(&s.file(&m.id)) && check_mode(&s.dir));
        s.add(&m.id, &Record::Georg { text: "Was ist das?".into(), context: "Fenster: brave".into(), picture: true, by: "hand".into() }).unwrap();
        s.add(&m.id, &Record::Tool { name: "sehen".into(), state: "done".into(), line: "Ein Fenster ansehen".into() }).unwrap();
        s.add(&m.id, &Record::Assistant("Eine Suchseite.".into())).unwrap();
        s.add(&m.id, &Record::Georg { text: "Und?".into(), context: String::new(), picture: false, by: "hand".into() }).unwrap();
        s.add(&m.id, &Record::Failed("abgebrochen".into())).unwrap();
        // A line half written when the machine went off.
        std::fs::OpenOptions::new().append(true).open(s.file(&m.id)).unwrap().write_all(b"{\"t\":\"assis").unwrap();
        let l = s.load(&m.id).unwrap();
        assert_eq!((l.meta.title.as_str(), l.meta.turns, l.meta.origin, l.meta.app.as_deref()), ("Was ist das?", 2, Origin::Popup, Some("brave")));
        let c = l.conversation();
        assert_eq!(c.messages.len(), 2, "the stopped question is not part of what the provider gets");
        assert!(c.messages[0].text().contains("Fenster: brave") && c.messages[0].text().contains(PICTURE_SENT) && c.messages[0].text().ends_with(PICTURE_SENT));
        assert_eq!(c.messages[1].text(), "Eine Suchseite.");
        s.add(&m.id, &Record::Title("Suchseite".into())).unwrap();
        assert_eq!(s.load(&m.id).unwrap().meta.title, "Suchseite");
        std::fs::write(s.dir.join("kaputt.jsonl"), "kein json\n").unwrap();
        std::fs::write(s.dir.join("NICHT-GUELTIG.jsonl"), "{}\n").unwrap();
        let (ok, bad) = s.load_all();
        assert_eq!((ok.len(), bad.len()), (1, 2), "{bad:?}");
        assert!(title_of(&"x".repeat(200)).ends_with('…') && title_of("a\nb") == "a");
        assert!(!valid_id("../x") && !valid_id("") && valid_id(&m.id));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[test]
    fn a_line_for_a_chat_whose_file_is_gone_makes_no_file() {
        let s = store("gone");
        let m = s.create(Origin::Sidebar, None, None).unwrap();
        std::fs::remove_file(s.file(&m.id)).unwrap();
        assert!(s.add(&m.id, &Record::Failed("abgebrochen".into())).is_err());
        assert!(!s.file(&m.id).exists(), "a file without its head would be skipped with a notice at every start");
        let (ok, bad) = s.load_all();
        assert_eq!((ok.len(), bad.len()), (0, 0), "{bad:?}");
        let _ = std::fs::remove_dir_all(&s.dir);
    }
}

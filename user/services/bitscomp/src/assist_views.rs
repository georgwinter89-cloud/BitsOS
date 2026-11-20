//! EINS-B: what both views of the assistant share in the App: the level pill (PLAN K2: tightening is anyone's,
//! loosening only Georg's own click, confirmed to settingsd), the sidebar's window in front as context
//! (`assistant.sidebar_sees`, PLAN K4), its own questions (a step, removing a chat), and the trash's listing after
//! a delete (PLAN K6: `bits papierkorb liste`, shown in the chat).

use mcpreg::Json;

use crate::assist::CardFor;
use crate::assist_level::{self as lv, Level};
use crate::db_question as q;
use crate::input::Origin;
use crate::state::App;
use crate::ui_reg::Full;

/// What the trash's listing is asked from (wave D's command, PLAN K6).
pub const BITS: &str = "/bin/bits";
pub const LOOSEN_REFUSED: &str = "Mehr Freiheit gibt nur deine eigene Hand: klick selbst auf die Stufe.";
pub const SEES_REFUSED: &str = "Mitsehen schaltet nur deine eigene Hand ein: klick selbst auf den Schalter.";
pub const SETTINGS_DOWN: &str = "Der Einstellungsdienst antwortet nicht: nicht geändert.";
/// Under the level's rows on the page.
pub const LOOSEN_LINE: &str = "Mehr Freiheit gibt nur deine eigene Hand; weniger geht jederzeit.";

impl App {
    /// A setting of the views (`confirmed`: Georg's own yes, which settingsd takes from the compositor only).
    /// Less freedom holds here at once; more only once it went to settingsd (false: it did not, nothing changed).
    pub(crate) fn assist_set(&mut self, key: &str, value: Json, confirmed: bool) -> bool {
        let text = match &value {
            Json::Bool(b) => b.to_string(),
            Json::Str(s) => s.clone(),
            other => other.to_string(),
        };
        let old = self.settings.text(key).unwrap_or("").to_string();
        let loosens = crate::page_join::loosens(key, &old, &text);
        let sent = self.settings_live && self.settings_writer.as_ref().is_some_and(|w| w.send(crate::desk::DESK_PID, crate::settings_write::Req::Set { key: key.into(), value, confirmed }));
        if loosens && !sent {
            eprintln!("bitscomp: assist: {key} not loosened: settingsd not there");
            self.db.note = Some(SETTINGS_DOWN.into());
            self.assist.note = Some((SETTINGS_DOWN.into(), true));
            self.chrome_dirty();
            return false;
        }
        if loosens {
            self.assist_route.loosened.retain(|(k, _)| k != key);
            self.assist_route.loosened.push((key.to_string(), old));
        }
        self.settings.set(key, &text);
        self.needs_frame = true;
        self.chrome_dirty();
        true
    }

    /// settingsd answered a setting of the views: a loosening it refused goes back (true: it was one of those).
    pub(crate) fn assist_set_done(&mut self, d: &crate::settings_write::Done) -> bool {
        let Some(key) = d.key.as_deref() else {
            // settingsd did not answer at all (no key said): every loosening on its way goes back.
            if !d.ok {
                for (key, old) in std::mem::take(&mut self.assist_route.loosened) {
                    eprintln!("bitscomp: assist: {key} back to {old:?}: settingsd did not answer");
                    self.settings.set(&key, &old);
                }
                self.chrome_dirty();
            }
            return false;
        };
        let Some(i) = self.assist_route.loosened.iter().position(|(k, _)| k == key) else { return false };
        let (key, old) = self.assist_route.loosened.remove(i);
        if !d.ok {
            eprintln!("bitscomp: assist: settingsd refused {key}: back to {old:?} ({})", d.text);
            self.settings.set(&key, &old);
            let note = format!("Nicht geändert: {}", d.text);
            self.db.note = Some(note.clone());
            self.assist.note = Some((note, true));
            self.needs_frame = true;
            self.chrome_dirty();
        }
        true
    }

    /// A level from a pill. Less freedom goes at once; more only from Georg's own hand, and not from a click meant
    /// for what was there before (`armed`: the pill's view has been there long enough).
    pub(crate) fn assist_level_pick(&mut self, l: Level, origin: Origin, armed: bool) {
        let now = Level::of(&self.settings);
        if l == now {
            return;
        }
        let loosens = l > now;
        if loosens && (origin != Origin::Physical || !armed) {
            eprintln!("bitscomp: assist: the level {} from {} input refused (only Georg's own click gives more freedom)", l.word(), origin.name());
            self.db.note = Some(LOOSEN_REFUSED.into());
            self.assist.note = Some((LOOSEN_REFUSED.into(), true));
            return;
        }
        crate::db_app::journal(&format!("bitscomp assist.level from={} to={} by={}", now.word(), l.word(), origin.name()));
        if !self.assist_set(lv::LEVEL, Json::str(l.word()), loosens) {
            return;
        }
        // Less freedom holds at once: a step that waits is refused under Nur antworten.
        self.db_act_answer_only();
        let lines = self.assist.refresh(crate::pacer::now());
        self.assist_sync(lines);
    }

    /// The sidebar's question carries the window in front when Georg switched that on (never a never-see one).
    pub(crate) fn assist_context(&self) -> Option<Json> {
        if !lv::sidebar_sees(&self.settings) {
            return None;
        }
        let front = self.ui_focused_id();
        let w = self.ui_windows(&Full::One(front.clone())).into_iter().find(|w| w.window == front)?;
        if lv::never_sees(&self.settings, &w.app) {
            crate::db_app::journal("bitscomp assist.sent view=sidebar kinds=- (a program the assistant never sees)");
            return None;
        }
        let question = q::build(&q::Inputs { window: &w, pointed: None, access: None, picture: false });
        crate::db_app::journal(&format!("bitscomp assist.sent view=sidebar window={} kinds={} bytes={}", w.window, question.words(), question.list.len()));
        Some(question.context(&w, None))
    }

    /// "×" on a chat of the list: asked first (it goes to the trash, PLAN K1).
    pub(crate) fn assist_ask_remove(&mut self, chat: &str, now: std::time::Duration) {
        let title = self.assist.chat_title(chat);
        self.assist.card = Some(crate::assist::Card {
            ask_id: 0,
            of: CardFor::Remove(chat.to_string()),
            title: None,
            text: format!("Den Chat „{title}“ entfernen? Er kommt in den Papierkorb."),
            yes_picked: false,
            note: None,
            remote: false,
            since: now,
            armed: false,
        });
        self.assist_sync(Vec::new());
    }

    /// The sidebar's own question was answered (its rules: `assist_card`).
    pub(crate) fn assist_decided(&mut self, of: CardFor, yes: bool) {
        match of {
            CardFor::Step if yes => self.db_act_confirm(),
            CardFor::Step => self.db_act_stop(crate::db_act_app::STOP_NOTE),
            CardFor::Remove(chat) if yes => {
                eprintln!("bitscomp: assist: chat {chat} removed (to the trash)");
                claim_chat_file(&chat);
                let mut lines = vec![crate::assist::remove_line(&chat), crate::assist::list_line()];
                if self.assist.chat.as_deref() == Some(chat.as_str()) {
                    lines.extend(self.assist.new_conversation(crate::pacer::now()));
                }
                self.assist_out(lines, crate::assist_route::NewFor::Sidebar);
            }
            CardFor::Bits(n) => self.bits_card_decided(n, yes),
            CardFor::Remove(_) | CardFor::Tool => {}
        }
        // A question of `bits` that waited for the card comes now.
        self.bits_card_show();
        self.chrome_dirty();
    }

    /// "Papierkorb zeigen": `bits papierkorb liste` on a thread; its lines come back into the chat.
    pub(crate) fn assist_trash_list(&mut self) {
        let Some(tx) = self.db.tx.clone() else { return };
        let _ = std::thread::Builder::new().name("bitscomp-trash".into()).spawn(move || {
            let text = match std::process::Command::new(BITS).args(["papierkorb", "liste"]).stdin(std::process::Stdio::null()).output() {
                Ok(o) => {
                    let out = String::from_utf8_lossy(&o.stdout).chars().take(8000).collect::<String>();
                    if out.trim().is_empty() { "Der Papierkorb ist leer.".to_string() } else { format!("Im Papierkorb:\n{}", out.trim_end()) }
                }
                Err(_) => "Den Papierkorb kann BitsOS gerade nicht zeigen (der Befehl „bits“ fehlt). Im Terminal: bits papierkorb liste".to_string(),
            };
            let _ = tx.send((crate::db_app::ACT_GEN, crate::db_worker::Msg::Trash(text)));
        });
    }

    pub(crate) fn assist_trash_shown(&mut self, text: &str) {
        self.assist.note_item(text, false);
        self.assist.trash = None;
        self.chrome_dirty();
    }
}

/// Georg's yes makes the chat's file his before the service removes it: the kernel keeps a file in the trash only
/// when a process outside the assistant's tree opened it (one the tree made and only it saw is removed for real,
/// eins-c.md §2), and the service runs inside the tree. The folder as `assist` finds it (eins-a.md §1).
fn claim_chat_file(chat: &str) {
    if chat.is_empty() || !chat.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return;
    }
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(std::path::PathBuf::from).filter(|p| p.is_absolute());
    let uid = std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(u32::MAX);
    let homes = [env("HOME").filter(|h| h != std::path::Path::new("/")).map(|h| h.join(".local/share")), Some(std::path::PathBuf::from(format!("/data/home/{uid}/.local/share")))];
    for data in std::iter::once(env("XDG_DATA_HOME")).chain(homes).flatten() {
        if std::fs::File::open(data.join("bitsos/chats").join(format!("{chat}.jsonl"))).is_ok() {
            return;
        }
    }
}


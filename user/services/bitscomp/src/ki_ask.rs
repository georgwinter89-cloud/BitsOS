//! CL-5: the questions of the page "KI" (remove a provider, delete a key, type a key) and who may say yes.
//! The page's dialog answers them; a yes counts only as Georg's own, by the sidebar's rule (`assist_card`):
//! - a click only when press, release and the pointer's last move all came from a real device (`Hand`), a key
//!   only from a real keyboard;
//! - never on a question that injected input opened or touched (it can only be cancelled; he asks again himself);
//! - only `ARM` after the question appeared, so a click or an Enter already on its way confirms nothing.
//! Each page has its own question, and a removal goes to settingsd only while the list of providers is still
//! the one the dialog was built from.

use std::time::Duration;

use mcpreg::Json;

use crate::assist::{ARM, Hand};
use crate::ki;
use crate::ki_guard::Typing;
use crate::page::Part;
use crate::settings_write::Req;
use crate::state::App;
use crate::text::Weight;

/// What injected input gets instead of a key, a deleted key or a removed provider.
pub const REAL_INPUT: &str = "Das geht nur mit deiner echten Maus oder Tastatur: nichts geändert.";
/// A real yes on a question that was not (only) Georg's.
pub const NOT_YOURS: &str = "Diese Frage kam nicht nur von deiner Maus oder Tastatur: nichts geändert. Bitte selbst noch einmal anklicken.";
/// Under the open key field.
pub const TYPE_NOW: &str = "Jetzt den Schlüssel tippen (es erscheint nichts), dann Enter. Esc bricht ab.";

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum What {
    /// `list`: the list of providers the dialog was built from.
    Remove { id: String, list: String },
    Clear(String),
    /// `lines`: where the key goes, as the dialog shows it.
    Key { id: String, lines: Vec<String> },
    /// The kind or address of the chat's provider (`ki_change`).
    Change { key: String, from: String, to: String },
    /// Helper `name`'s provider `id` takes the chat over from `before` (`ki_helpers`).
    Chat { id: String, before: String, name: String },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Asking {
    pub what: What,
    /// When it appeared or injected input last touched it (`pacer` time): a yes counts `ARM` later.
    pub since: Duration,
    /// Injected input opened or touched it.
    pub tainted: bool,
}

impl App {
    /// The click being handled is Georg's own in every part.
    pub(crate) fn hand_real(&self) -> bool {
        Hand { press: self.desk.press_origin, release: self.input_origin, moved: self.pointer_origin }.real()
    }

    /// False, and the page says why, when the click or key being handled is not Georg's own.
    pub(crate) fn ki_real(&mut self, pid: u32, what: &str) -> bool {
        if self.pages.real {
            return true;
        }
        eprintln!("bitscomp: ki: {what} by injected input refused");
        self.page_say(pid, REAL_INPUT, true);
        false
    }

    /// The content of page `pid`'s window (host tests have none: room for the widest dialog).
    fn ki_content(&self, pid: u32) -> crate::layout::Rect {
        let placed = self.placements().into_iter().find(|p| p.visible && self.page_pid(&p.win) == Some(pid));
        placed.map_or(crate::layout::Rect { x: 0, y: 0, w: 592, h: 4096 }, |p| self.content_rect(p.frame))
    }

    /// Where the key of `id` goes, in lines that fit page `pid`'s dialog as it would be drawn now, with what
    /// settingsd must still hold when the key arrives. Err: the German reason no key is typed here.
    fn ki_key_lines(&self, pid: u32, id: &str) -> Result<(Vec<String>, Vec<(String, Json)>), String> {
        let f = ki::facts(&self.settings);
        let p = f.provider(id).ok_or_else(|| format!("Einen Anbieter „{id}“ gibt es nicht mehr."))?;
        let c = self.ki_content(pid);
        // The widest font the dialog draws a line with; its lines are 48 narrower than its frame.
        let font = crate::chrome_draw::font(&self.theme, 15.0, Weight::Medium);
        let max = crate::header::px(crate::page_confirm::boxes(c, 0).0.w - 48, self.theme.scale) - 2;
        let lines = crate::ki_key::key_lines(p, max, |s| font.width(s))?;
        if crate::page_confirm::boxes(c, lines.len()).0.h > c.h {
            return Err("Das Fenster ist zu niedrig, um ganz zu zeigen, wohin der Schlüssel geht: bitte größer machen.".into());
        }
        let whilst = [("address", &p.address), ("kind", &p.kind), ("place", &p.place)].iter().map(|(field, v)| (ki::key_of(id, field), Json::str(v))).collect();
        Ok((lines, whilst))
    }

    /// A click into a provider's key field: the page first shows in full where the key will go.
    pub(crate) fn ki_key_begin(&mut self, pid: u32, key: &str) {
        let Some((id, _)) = ki::provider_of(key) else { return };
        if !self.ki_real(pid, "typing a key") {
            return;
        }
        match self.ki_key_lines(pid, id) {
            Err(why) => self.page_say(pid, why, true),
            Ok((lines, _)) => self.ki_ask(pid, What::Key { id: id.to_string(), lines }),
        }
    }

    pub(crate) fn ki_ask(&mut self, pid: u32, what: What) {
        let f = ki::facts(&self.settings);
        let (title, lines, ok) = match &what {
            What::Remove { id, .. } => (format!("Anbieter „{id}“ wirklich entfernen?"), ki::remove_lines(&f, id), "Entfernen"),
            What::Clear(id) => (
                format!("Schlüssel von „{id}“ wirklich löschen?"),
                vec![
                    "Der gespeicherte Schlüssel wird gelöscht; er lässt sich nicht zurückholen.".into(),
                    "Danach lassen sich Adresse, Art und Ort ändern, und du gibst den Schlüssel neu ein.".into(),
                ],
                "Löschen",
            ),
            What::Key { id, lines } => (format!("Schlüssel für „{id}“ eintippen?"), lines.clone(), "Eintippen"),
            What::Change { key, from, to } => crate::ki_change::lines(&f, key, from, to),
            What::Chat { id, before, name } => (
                "Im Gespräch wechseln?".into(),
                vec![
                    format!("Bisher antwortet „{before}“, wenn du mit dem Assistenten sprichst."),
                    format!("Danach antwortet „{name}“ (Anbieter „{id}“)."),
                    "„Rückgängig“ oben rechts auf dieser Seite holt den bisherigen zurück.".into(),
                ],
                "Wechseln",
            ),
        };
        self.address_end();
        // A key's field Georg clicked again is ended by him, not lost.
        self.ki_key_own_end();
        self.page_edit_end();
        let tainted = !self.pages.real;
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.confirm = Some(crate::page_confirm::plain(title, lines, ok));
            s.msg = None;
            self.ki.asking.insert(pid, Asking { what, since: crate::pacer::now(), tainted });
            self.needs_frame = true;
        }
    }

    /// A row's name in the page's answer line.
    pub(crate) fn ki_short(&self, key: &str) -> String {
        self.ki.built.model.rows.iter().find(|r| r.key == key).map_or(key.to_string(), |r| r.short.clone())
    }

    /// The dialog that closes on `pid` is this page's own question (not one of settingsd's).
    pub(crate) fn ki_asks(&self, pid: u32) -> bool {
        self.ki.asking.contains_key(&pid)
    }

    /// Another dialog took the place of the page's own question: it is forgotten, nothing is sent.
    pub(crate) fn ki_ask_dropped(&mut self, pid: u32) {
        self.ki.asking.remove(&pid);
    }

    /// A key for the page's own question came from injected input: it may cancel, nothing else; the pick is
    /// on "Abbrechen" again and the question is no longer Georg's alone.
    pub(crate) fn ki_ask_touched(&mut self, pid: u32) {
        if let Some(a) = self.ki.asking.get_mut(&pid) {
            (a.tainted, a.since) = (true, crate::pacer::now());
            eprintln!("bitscomp: ki: injected input touched the question of page {pid}: it can only be cancelled now");
        }
        if let Some(d) = self.pages.by_pid.get_mut(&pid).and_then(|s| s.confirm.as_mut()) {
            self.needs_frame |= d.on_ok;
            d.on_ok = false;
        }
    }

    /// A yes on the page's own question comes too early to be meant for it: the dialog stays.
    pub(crate) fn ki_yes_early(&self, pid: u32) -> bool {
        self.ki.asking.get(&pid).is_some_and(|a| crate::pacer::now() < a.since + ARM)
    }

    /// The yes button of the page's own dialog, or "Abbrechen".
    pub(crate) fn ki_confirmed(&mut self, pid: u32, yes: bool) {
        let Some(a) = self.ki.asking.remove(&pid) else { return };
        if !yes {
            let text = match &a.what {
                What::Remove { id, .. } => format!("Anbieter „{id}“ bleibt (abgebrochen)."),
                What::Clear(id) => format!("Der Schlüssel von „{id}“ bleibt (abgebrochen)."),
                What::Key { id, .. } => format!("Kein Schlüssel für „{id}“ eingetippt (abgebrochen)."),
                What::Change { key, .. } => format!("{}: nicht geändert (abgebrochen).", self.ki_short(key)),
                What::Chat { before, .. } => format!("Im Gespräch antwortet weiter „{before}“ (abgebrochen)."),
            };
            return self.page_say(pid, text, false);
        }
        let what = match &a.what {
            What::Remove { .. } => "the removal of a provider",
            What::Clear(_) => "the deletion of a key",
            What::Key { .. } => "typing a key",
            What::Change { .. } => "a change of the chat provider",
            What::Chat { .. } => "a new chat provider",
        };
        if !self.ki_real(pid, what) {
            return;
        }
        if a.tainted {
            eprintln!("bitscomp: ki: {what}: Georg's yes on a question injected input opened or touched does not count");
            return self.page_say(pid, NOT_YOURS, true);
        }
        match a.what {
            What::Remove { id, list } => {
                if !ki::ids(self.settings.text(ki::LIST).unwrap_or("")).contains(&id) {
                    return self.page_say(pid, format!("Einen Anbieter „{id}“ gibt es nicht mehr."), true);
                }
                eprintln!("bitscomp: ki: Georg confirmed the removal of provider {id}");
                // Only while the list is the one his dialog was built from: his yes removes nothing else.
                let r = Req::Guarded { key: ki::LIST.into(), value: Json::str(&ki::list_without(&list, &id)), whilst: vec![(ki::LIST.into(), Json::str(&list))] };
                if self.ki_send(pid, r) {
                    self.ki.removing = Some((pid, id));
                }
            }
            What::Clear(id) => {
                if !ki::ids(self.settings.text(ki::LIST).unwrap_or("")).contains(&id) {
                    return self.page_say(pid, format!("Einen Anbieter „{id}“ gibt es nicht mehr."), true);
                }
                eprintln!("bitscomp: ki: Georg confirmed the deletion of the key of provider {id}");
                self.page_send(pid, Req::Reset { key: ki::key_of(&id, "key"), confirmed: true });
            }
            What::Key { id, lines } => self.ki_key_field(pid, id, lines),
            What::Change { key, from, to } => self.ki_change_confirmed(pid, key, from, to),
            What::Chat { id, before, .. } => {
                let key = ki::role_key("chat", false);
                let r = Req::Guarded { key: key.clone(), value: Json::str(&id), whilst: vec![(key, Json::str(&before))] };
                self.page_send(pid, r);
            }
        }
    }

    /// Georg read where the key goes and said "Eintippen": the field takes his keys, if what he read is still
    /// true and still shown whole (the settings or the window may have changed under the dialog).
    fn ki_key_field(&mut self, pid: u32, id: String, shown: Vec<String>) {
        let (lines, whilst) = match self.ki_key_lines(pid, &id) {
            Ok(v) => v,
            Err(why) => return self.page_say(pid, why, true),
        };
        if lines != shown {
            eprintln!("bitscomp: ki: where the key of {id} goes changed under the dialog: asked again");
            return self.ki_ask(pid, What::Key { id, lines });
        }
        let Some(r) = self.ki.built.row_of(&ki::key_of(&id, "key")) else { return };
        self.page_edit_begin(pid, Part::Field(r), String::new());
        self.ki.typing = Some(Typing { pid, id, whilst, last: crate::pacer::now(), noted: false });
        self.ki.swallow = false;
        self.page_say(pid, TYPE_NOW, false);
    }
}

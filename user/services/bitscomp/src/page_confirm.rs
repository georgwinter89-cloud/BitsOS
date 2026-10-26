//! WY-10: the settings page's own confirmation. A key of settingsd's safety class `confirm` changes only after
//! Georg chose "Ändern" in a small dialog on the page; then the change goes to settingsd with `"confirmed": true`,
//! the same field Claude's settings_set sends after asking (settingsd change.rs: no other way past the check).
//! Undo of such a key is a change too: settingsd answers `needs_confirm`, and the page asks the same way.
//! The dialog keeps the key and the value, not a row number: a new registry list may move the rows under it.
//! Focus starts on "Abbrechen", so an Enter meant for something else never confirms.

use mcpreg::Json;
use smithay::input::keyboard::Keysym;

use crate::canvas::Canvas;
use crate::chrome_draw::{Px, font};
use crate::layout::Rect;
use crate::page::{Kind, Part, Row};
use crate::settings_write::{Ask, Req};
use crate::state::App;
use crate::text::Weight;
use crate::theme::{Theme, alpha};

#[derive(Clone, Debug, PartialEq)]
pub struct Dialog {
    /// What goes to settingsd on "Ändern" (already confirmed).
    pub ask: Ask,
    pub title: String,
    pub lines: Vec<String>,
    pub ok: String,
    /// The name in the page's answer line.
    pub short: String,
    pub on_ok: bool,
}

/// What a key or a click does to the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Stay,
    Cancel,
    Confirm,
}

impl Dialog {
    pub fn key(&mut self, sym: Keysym) -> Step {
        match sym {
            Keysym::Escape => Step::Cancel,
            Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                if self.on_ok {
                    Step::Confirm
                } else {
                    Step::Cancel
                }
            }
            Keysym::Tab | Keysym::ISO_Left_Tab | Keysym::Left | Keysym::Right | Keysym::KP_Left | Keysym::KP_Right => {
                self.on_ok = !self.on_ok;
                Step::Stay
            }
            _ => Step::Stay,
        }
    }
}

/// `v` as the row shows it: a choice's German name, Ein/Aus, a number with its unit; `Json::Null` is the default.
pub fn shown(row: Option<&Row>, v: &Json) -> String {
    let text = match v {
        Json::Null => return row.map_or("Standard".into(), |r| format!("{} (Standard)", shown(Some(r), &Json::str(&r.default)))),
        Json::Str(s) => s.clone(),
        Json::Bool(b) => b.to_string(),
        Json::Num(_) => v.as_i64().map_or_else(|| v.to_string(), |n| n.to_string()),
        _ => v.to_string(),
    };
    let named = match row.map(|r| &r.kind) {
        Some(Kind::Choice(opts)) => opts.iter().find(|(o, _)| *o == text).map(|(_, n)| n.clone()),
        Some(Kind::Switch) => match text.as_str() {
            "true" => Some("Ein".into()),
            "false" => Some("Aus".into()),
            _ => None,
        },
        Some(Kind::Number { word: Some((w, name)), .. }) if *w == text => Some(name.clone()),
        Some(Kind::Number { unit, .. }) if !unit.is_empty() => Some(format!("{text} {unit}")),
        _ => None,
    };
    match named {
        Some(n) => n,
        None if text.is_empty() => "–".into(),
        None => text,
    }
}

/// The dialog for `ask` on `row` (None: a key the page has no row for), whose value is `now` today.
pub fn dialog(row: Option<&Row>, now: &str, ask: Ask) -> Dialog {
    let undo = matches!(ask.req, Req::Undo { .. });
    let label = row.map_or(ask.key.clone(), |r| r.label.clone());
    let to = shown(row, &ask.to);
    let mut lines = vec![label, format!("Jetzt: {} · {}: {to}", shown(row, &Json::str(now)), if undo { "Zurück auf" } else { "Neu" })];
    lines.push("Diese Einstellung ist geschützt: sie ändert sich nur, wenn du hier zustimmst.".into());
    if let Some(n) = row.map(|r| r.note.as_str()).filter(|n| !n.is_empty()) {
        let mut c = n.chars();
        let first = c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default();
        lines.push(format!("{first}{}.", c.as_str()));
    }
    Dialog {
        title: if undo { "Wirklich rückgängig machen?" } else { "Wirklich ändern?" }.into(),
        ok: if undo { "Rückgängig" } else { "Ändern" }.into(),
        short: row.map_or(ask.key.clone(), |r| r.short.clone()),
        lines,
        ask,
        on_ok: false,
    }
}

/// Where the dialog lies in content `c` with `lines` lines of text: (frame, Abbrechen, Ändern).
pub fn boxes(c: Rect, lines: usize) -> (Rect, Rect, Rect) {
    let w = (c.w - 32).clamp(1, 560);
    let h = 24 + 34 + lines.min(16) as i32 * 24 + 20 + 36 + 24;
    let f = Rect { x: c.x + (c.w - w) / 2, y: c.y + ((c.h - h) / 2).max(0), w, h };
    let bw = 140.min((w - 48 - 12) / 2).max(1);
    let by = f.y + h - 24 - 36;
    let ok = Rect { x: f.x + w - 24 - bw, y: by, w: bw, h: 36 };
    let cancel = Rect { x: ok.x - 12 - bw, ..ok };
    (f, cancel, ok)
}

/// The dialog's hit parts (it is modal: anything else in the content is the page's body, which does nothing).
pub fn parts(c: Rect, d: &Dialog) -> Vec<(Part, Rect)> {
    let (_, cancel, ok) = boxes(c, d.lines.len());
    vec![(Part::DialogCancel, cancel), (Part::DialogOk, ok)]
}

pub fn draw(c: &mut Canvas, p: &Px, t: &Theme, content: Rect, d: &Dialog, hover: Option<Part>) {
    let all = p.r(content);
    c.fill(all.x, all.y, all.w, all.h, alpha(t.paper, 0.72));
    let (f, cancel, ok) = boxes(content, d.lines.len());
    c.glow(p.b(f), p.v(14.0), if t.effects_gpu { 6 } else { 2 }, t.glow_soft);
    c.rrect(p.b(f), p.v(14.0), t.panel);
    c.rrect_line(p.b(f), p.v(14.0), p.line(), t.accent);
    let band = |y: i32, h: i32| Rect { x: f.x + 24, y, w: f.w - 48, h };
    c.text_fit(&font(t, 20.0, Weight::SemiBold), p.r(band(f.y + 24, 28)), &d.title, t.text);
    let (body, small) = (font(t, 15.0, Weight::Medium), font(t, 13.0, Weight::Regular));
    for (i, line) in d.lines.iter().enumerate() {
        let (fnt, col) = if i == 0 { (&body, t.text) } else if i == 1 { (&body, t.accent) } else { (&small, t.muted) };
        c.text_fit(fnt, p.r(band(f.y + 24 + 34 + i as i32 * 24, 20)), line, col);
    }
    let ctl = font(t, 14.0, Weight::Medium);
    for (part, b, text, focus) in [(Part::DialogCancel, cancel, "Abbrechen", !d.on_ok), (Part::DialogOk, ok, d.ok.as_str(), d.on_ok)] {
        crate::page_draw::button(c, p, t, b, text, hover == Some(part) || focus, true, &ctl);
        if focus {
            c.rrect_line(p.b(b).inset(-p.v(3.0)), p.v(10.0), p.v(1.5), t.accent);
        }
    }
}

impl App {
    /// Opens the dialog on page `pid` for `ask` (a change of a confirm key, or settingsd's `needs_confirm`).
    pub fn page_ask(&mut self, pid: u32, ask: Ask) {
        if !self.pages.by_pid.contains_key(&pid) {
            return;
        }
        self.address_end();
        self.page_edit_end();
        let row = self.page_model.rows.iter().find(|r| r.key == ask.key).cloned();
        let now = match &row {
            Some(r) => crate::page_app::value_of(self, r),
            None => self.settings.text(&ask.key).unwrap_or("").to_string(),
        };
        eprintln!("bitscomp: settings page {pid} asks before {}: {}", ask.key, crate::settings_write::request_line(&ask.req));
        let d = dialog(row.as_ref(), &now, ask);
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.confirm = Some(d);
            s.msg = None;
        }
        self.needs_frame = true;
    }

    /// The focused page with its dialog open: it takes the keys.
    pub fn page_dialog_pid(&self) -> Option<u32> {
        let pid = self.page_pid(self.ws.focused()?)?;
        self.pages.by_pid.get(&pid)?.confirm.is_some().then_some(pid)
    }

    pub fn page_dialog_key(&mut self, pid: u32, sym: Keysym) {
        let Some(d) = self.pages.by_pid.get_mut(&pid).and_then(|s| s.confirm.as_mut()) else { return };
        let before = d.on_ok;
        match d.key(sym) {
            Step::Stay => self.needs_frame |= before != d.on_ok,
            Step::Cancel => self.page_dialog_close(pid, false),
            Step::Confirm => self.page_dialog_close(pid, true),
        }
    }

    /// "Ändern": the confirmed change goes to settingsd; "Abbrechen": nothing is sent.
    pub fn page_dialog_close(&mut self, pid: u32, confirm: bool) {
        let Some(d) = self.pages.by_pid.get_mut(&pid).and_then(|s| s.confirm.take()) else { return };
        self.needs_frame = true;
        if confirm {
            eprintln!("bitscomp: settings page {pid}: Georg confirmed {}", d.ask.key);
            self.page_send(pid, d.ask.req);
        } else {
            eprintln!("bitscomp: settings page {pid}: change of {} cancelled, nothing sent", d.ask.key);
            self.page_say(pid, format!("{}: nicht geändert (abgebrochen).", d.short), false);
        }
    }

    /// Closes every dialog without sending (settingsd gone, `ctl page clear`), or only those whose key is no
    /// longer in the rows (`keys`: the new rows' keys).
    pub fn page_dialogs_drop(&mut self, keys: Option<&[String]>, why: &str) {
        self.needs_frame |= self.pages.drop_dialogs(keys, why);
    }
}

impl crate::page_app::Pages {
    /// `App::page_dialogs_drop` without the frame: whether a dialog closed. Nothing is sent, `busy` stays.
    pub fn drop_dialogs(&mut self, keys: Option<&[String]>, why: &str) -> bool {
        let mut closed = false;
        for (pid, s) in self.by_pid.iter_mut() {
            let gone = s.confirm.as_ref().is_some_and(|d| keys.is_none_or(|k| !k.contains(&d.ask.key)));
            if gone && let Some(d) = s.confirm.take() {
                eprintln!("bitscomp: settings page {pid}: dialog for {} closed ({why}), nothing sent", d.ask.key);
                if !why.is_empty() {
                    s.msg = Some((format!("{}: nicht geändert ({why}).", d.short), true));
                }
                closed = true;
            }
        }
        closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_schema::{Spec, Ty, builtin, model};

    fn risky() -> Row {
        let s = Spec { key: "test.risky".into(), ty: Ty::Int { min: 0, max: 9, unit: String::new(), word: None }, safety: "confirm".into(), restart: "reboot".into(), de: "Riskanter Testwert".into(), default: Some("1".into()), ..builtin()[0].clone() };
        model(&[s]).rows.remove(0)
    }

    fn ask(v: i64) -> Ask {
        Ask { req: Req::Set { key: "test.risky".into(), value: Json::Num(v as f64), confirmed: true }, key: "test.risky".into(), to: Json::Num(v as f64) }
    }

    #[test]
    fn focus_starts_on_cancel_and_only_change_confirms() {
        let r = risky();
        assert!(r.confirm && matches!(r.kind, Kind::Number { .. }));
        let mut d = dialog(Some(&r), "1", ask(3));
        assert!(!d.on_ok);
        assert_eq!(d.clone().key(Keysym::Return), Step::Cancel, "Enter at the start cancels");
        assert_eq!(d.clone().key(Keysym::KP_Enter), Step::Cancel);
        assert_eq!(d.clone().key(Keysym::Escape), Step::Cancel);
        assert_eq!(d.key(Keysym::a), Step::Stay);
        assert_eq!(d.key(Keysym::Tab), Step::Stay);
        assert!(d.on_ok);
        assert_eq!(d.clone().key(Keysym::Escape), Step::Cancel, "Esc cancels with the focus on Ändern too");
        assert_eq!(d.clone().key(Keysym::Return), Step::Confirm);
        assert_eq!(d.key(Keysym::Left), Step::Stay);
        assert!(!d.on_ok);
        assert!(matches!(d.ask.req, Req::Set { confirmed: true, .. }));
    }

    #[test]
    fn the_text_names_the_key_both_values_and_the_restart() {
        let r = risky();
        let d = dialog(Some(&r), "1", ask(3));
        assert_eq!((d.title.as_str(), d.ok.as_str()), ("Wirklich ändern?", "Ändern"));
        assert_eq!(d.lines[0], "Riskanter Testwert");
        assert_eq!(d.lines[1], "Jetzt: 1 · Neu: 3");
        assert_eq!(d.lines.last().unwrap(), "Wirkt nach dem nächsten Start.");
        let u = Ask { req: Req::Undo { confirmed: Some(crate::settings_write::Sure { key: "test.risky".into(), seq: Some(4) }) }, key: "test.risky".into(), to: Json::Null };
        let d = dialog(Some(&r), "3", u);
        assert_eq!((d.title.as_str(), d.ok.as_str()), ("Wirklich rückgängig machen?", "Rückgängig"));
        assert_eq!(d.lines[1], "Jetzt: 3 · Zurück auf: 1 (Standard)");
        let mut sw = r.clone();
        sw.kind = Kind::Switch;
        assert_eq!(shown(Some(&sw), &Json::Bool(true)), "Ein");
        assert_eq!(shown(None, &Json::str("")), "–");
        // Every character has a glyph in the page's font (an arrow had none).
        for t in d.lines.iter().chain([&d.title, &d.ok, &"Abbrechen".to_string()]) {
            assert!(t.chars().all(crate::text::Font::has), "{t}");
        }
        let d = dialog(None, "x", Ask { key: "zz.gone".into(), ..ask(2) });
        assert_eq!((d.lines[0].as_str(), d.short.as_str()), ("zz.gone", "zz.gone"));
    }

    #[test]
    fn the_buttons_fit_any_content() {
        for c in [Rect { x: 0, y: 0, w: 1200, h: 800 }, Rect { x: 50, y: 40, w: 300, h: 200 }] {
            let (f, cancel, ok) = boxes(c, 4);
            assert!(cancel.x + cancel.w < ok.x && cancel.w > 0 && ok.w > 0, "{c:?}");
            assert!(ok.x + ok.w <= f.x + f.w && cancel.x >= f.x && f.x >= c.x && f.x + f.w <= c.x + c.w, "{c:?}");
        }
        // A tiny window: no overflow, no panic.
        let _ = boxes(Rect { x: 0, y: 0, w: 10, h: 10 }, usize::MAX / 64);
    }

    #[test]
    fn dialogs_close_without_sending_when_their_key_or_settingsd_goes() {
        use crate::page_app::{PageState, Pages};
        let open = |p: &mut Pages| {
            for pid in [3, 4] {
                p.by_pid.get_mut(&pid).unwrap().confirm = Some(dialog(Some(&risky()), "1", ask(3)));
            }
        };
        let mut p = Pages::default();
        p.by_pid.insert(3, PageState::new("einstellungen/test/risky"));
        p.by_pid.insert(4, PageState::new("einstellungen"));
        p.busy = 2;
        open(&mut p);
        // A new settingsd list that still has the key keeps the dialogs.
        assert!(!p.drop_dialogs(Some(&["theme.mode".into(), "test.risky".into()]), "weg"));
        assert!(p.by_pid.values().all(|s| s.confirm.is_some()));
        // The key left the list: closed with a line, nothing sent.
        assert!(p.drop_dialogs(Some(&["theme.mode".into()]), "die Einstellung gibt es nicht mehr"));
        assert!(p.by_pid.values().all(|s| s.confirm.is_none()));
        let m = p.by_pid[&3].msg.clone().unwrap();
        assert_eq!(m, ("Riskanter Testwert: nicht geändert (die Einstellung gibt es nicht mehr).".to_string(), true));
        // settingsd lost (apply.rs) and `ctl page clear` (no line).
        open(&mut p);
        assert!(p.drop_dialogs(None, "der Einstellungsdienst antwortet nicht"));
        assert!(p.by_pid[&4].msg.as_ref().is_some_and(|m| m.0.contains("antwortet nicht")));
        open(&mut p);
        p.by_pid.get_mut(&4).unwrap().msg = None;
        assert!(p.drop_dialogs(None, ""));
        assert!(p.by_pid.values().all(|s| s.confirm.is_none()) && p.by_pid[&4].msg.is_none());
        assert!(!p.drop_dialogs(None, ""), "nothing left to close");
        assert_eq!(p.busy, 2, "closing a dialog sends nothing");
    }
}

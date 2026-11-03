//! MC-9b: the installer overlay in the compositor: bitscomp is bitsinstall's confirmer (`install_link`). An
//! `ask` opens the question on top of everything (`install_draw`, paint layer `System`): no window can cover,
//! move or answer it, every key and click goes to it while it is shown, and only input from a real device
//! (`Origin::Physical`) can say yes. Strg+Alt+G at the real keyboard opens the grant dialog; the grants running
//! show as a marker in the status bar and leave it when their time is up.

use std::collections::HashSet;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use smithay::input::keyboard::Keysym;

use crate::input::Origin;
use crate::install_dialog::{AskDialog, Btn, DURS, Dur, GrantDialog, GrantStep, Outcome, Stop, until_of};
use crate::install_link::{self as link, AccessShown, ClientShown, FromInstaller, Msg, Shown};
use crate::pointer::BTN_LEFT;
use crate::state::App;

/// How many lines `ctl install log` keeps (R1 does not apply to a test log; this bounds its memory).
const LOG_KEEP: usize = 64;

#[derive(Default)]
pub struct Install {
    pub ask: Option<AskDialog>,
    pub grant: Option<GrantDialog>,
    /// The grants the installer reported (hello/grants), dropped here when their time is up.
    pub grants: Vec<Shown>,
    link: Option<(u64, UnixStream)>,
    /// Buttons pressed while the overlay was shown: their releases stay with it (`pointer::button`).
    pub buttons: HashSet<u32>,
    /// Headless test aid (`ctl install test-seconds N`, nested build only): a grant of N seconds is offered.
    pub test_secs: Option<i64>,
    log: Vec<String>,
    /// What the overlay draws, cached (`install_draw`).
    pub images: crate::install_draw::Images,
    /// The look last drawn of the ask (countdown, armed): a change asks for a frame.
    shown_key: Option<(u64, bool)>,
    /// MC-10: the full-access dialog, the grants running and the paired clients (the installer's lines).
    pub access: Option<crate::access_dialog::AccessDialog>,
    pub access_live: Vec<AccessShown>,
    pub clients: Vec<ClientShown>,
}

impl Install {
    pub fn shown(&self) -> bool {
        self.ask.is_some() || self.grant.is_some() || self.access.is_some()
    }

    pub fn connected(&self) -> bool {
        self.link.is_some()
    }
}

fn now() -> Duration {
    crate::pacer::now()
}

impl App {
    pub(crate) fn install_note(&mut self, line: String) {
        eprintln!("bitscomp: installer: {line}");
        let l = &mut self.install.log;
        if l.len() >= LOG_KEEP {
            l.remove(0);
        }
        l.push(line);
    }

    pub(crate) fn install_send(&mut self, line: &str) -> bool {
        let Some((_, w)) = self.install.link.as_mut() else { return false };
        if link::send(w, line) {
            return true;
        }
        self.install.link = None;
        self.install_note("the installer did not take a line within 200 ms: link dropped".into());
        false
    }

    pub fn install_takes_keys(&self) -> bool {
        self.install.shown()
    }

    /// The overlay opened or closed: windows lose (or get back) the pointer, modes of the pointer end.
    pub(crate) fn install_changed(&mut self) {
        self.needs_frame = true;
        let t = self.now_ms();
        self.motion(t);
        self.cursor_sync();
    }

    pub fn install_msg(&mut self, m: Msg) {
        match m {
            Msg::Up(n, w, how) => {
                self.install.link = Some((n, w));
                self.install_note(format!("connected (link {n}; {how})"));
            }
            Msg::Down(n, why) => {
                self.install_note(format!("link {n} ended: {why}"));
                match self.install.link.as_ref() {
                    // A newer link is up: what is shown belongs to it (the thread sends in order, so this is only
                    // a guard).
                    Some((k, _)) if *k != n => return,
                    _ => self.install.link = None,
                }
                // Nobody waits for an answer any more; the grants are the installer's, shown again on the next hello.
                let had = self.install.shown() || !self.install.grants.is_empty() || !self.install.access_live.is_empty();
                if let Some(a) = self.install.ask.take() {
                    self.install_note(format!("dialog {} closed: the installer is gone (nothing answered)", a.id));
                }
                self.install.grant = None;
                self.install.grants.clear();
                self.install.access = None;
                self.install.access_live.clear();
                self.install.clients.clear();
                if had {
                    self.install_changed();
                    self.chrome_dirty();
                }
            }
            Msg::Line(n, msg, len) => {
                if !self.install.link.as_ref().is_some_and(|(k, _)| *k == n) {
                    return;
                }
                match msg {
                    Some(m) => self.install_line(m),
                    None => self.install_note(format!("a line of {len} bytes not understood, ignored")),
                }
            }
        }
    }

    fn install_line(&mut self, m: FromInstaller) {
        match m {
            FromInstaller::Hello { version, grants, access, clients } => {
                self.install_note(format!("hello version {version}, {} grant(s), {} full access, {} paired client(s)", grants.len(), access.len(), clients.len()));
                self.install_grants(grants);
                self.access_update(access, clients);
            }
            FromInstaller::Access { access, clients } => {
                self.install_note(format!("full access now: {}", access.iter().map(|a| format!("{}@{}{}", &a.client[..8], a.until, if a.nvme { "+nvme" } else { "" })).collect::<Vec<_>>().join(",")));
                self.access_update(access, clients);
            }
            FromInstaller::AskAccess { id, title: _, text, timeout_ms, client, name, hours } => self.access_ask(id, text, timeout_ms, client, name, hours),
            FromInstaller::Grants(g) => {
                self.install_note(format!("grants now: {}", grants_text(&g)));
                self.install_grants(g);
            }
            FromInstaller::Refused { what, reason } => {
                self.install_note(format!("refused {what}: {reason}"));
                self.notice(format!("Nicht übernommen: {reason}"));
            }
            FromInstaller::Cancel { id, reason } => {
                if self.install.access.as_ref().is_some_and(|a| a.id == Some(id)) {
                    self.install.access = None;
                    self.install_note(format!("access dialog {id} cancelled by the installer: {reason}"));
                    self.install_changed();
                }
                if self.install.ask.as_ref().is_some_and(|a| a.id == id) {
                    self.install.ask = None;
                    self.install_note(format!("dialog {id} cancelled by the installer: {reason}"));
                    self.install_changed();
                }
            }
            FromInstaller::Ask { id, kind, title, text, timeout_ms } => {
                let shown = self.install.ask.as_ref().map(|a| a.id).or_else(|| self.install.access.as_ref().and_then(|a| a.id));
                if let Some(shown) = shown {
                    // One question at a time: a second one is refused, the shown one never swapped under a click.
                    self.install_note(format!("ask {id} while {shown} is shown: answered no"));
                    self.install_send(&link::answer(id, false, "none"));
                    return;
                }
                self.install_note(format!("asks {id} ({kind}, {} s): {title}", timeout_ms.div_ceil(1000)));
                self.cancel_pointer_modes();
                self.address_end();
                self.page_edit_end();
                if self.install.grant.take().is_some() || self.install.access.take().is_some() {
                    self.install_note("own dialog closed for a question".into());
                }
                self.install.ask = Some(AskDialog::new(id, kind, title, text, now(), Duration::from_millis(timeout_ms)));
                self.install.shown_key = None;
                self.install_fit();
                self.install_changed();
            }
        }
    }

    fn install_grants(&mut self, g: Vec<Shown>) {
        let now = crate::status::epoch_secs();
        self.install.grants = g.into_iter().filter(|s| s.until > now).collect();
        self.chrome_dirty();
    }

    /// Sends the answer and closes the dialog; `input` None: nobody answered (the dialog time ran out).
    fn install_answer(&mut self, yes: bool, input: Option<Origin>, how: &str) {
        let Some(a) = self.install.ask.take() else { return };
        let sent = self.install_send(&link::answer(a.id, yes, input.map_or("none", Origin::name)));
        let word = if yes { "Erlauben" } else { "Ablehnen" };
        let tail = if sent { "" } else { " (not delivered: no installer)" };
        match input {
            Some(Origin::Injected) => self.install_note(format!("answer {} {word} from injected input ({how}): sent as injected, so it is refused{tail}", a.id)),
            Some(i) => self.install_note(format!("answer {} {word} ({how}, {}){tail}", a.id, i.name())),
            None => self.install_note(format!("answer {} {word} ({how}, nobody answered){tail}", a.id)),
        }
        self.install_changed();
    }

    fn install_outcome(&mut self, o: Outcome, how: &str) {
        match o {
            Outcome::Stay(changed) => self.needs_frame |= changed,
            Outcome::Answer { yes, input } => self.install_answer(yes, Some(input), how),
            Outcome::Ignored => {
                let id = self.install.ask.as_ref().map_or(0, |a| a.id);
                self.install_note(format!("injected {how} on dialog {id} ignored"));
            }
        }
    }

    pub fn install_key(&mut self, sym: Keysym) {
        let origin = self.input_origin;
        if let Some(a) = self.install.ask.as_mut() {
            let o = a.key(sym, origin, now());
            self.install_outcome(o, &format!("key {sym:?}"));
            return;
        }
        if self.install.access.is_some() {
            self.access_key(sym);
            return;
        }
        if let Some(g) = self.install.grant.as_mut() {
            let s = g.key(sym, origin);
            self.install_grant_step(s, &format!("key {sym:?}"));
        }
    }

    pub fn install_press(&mut self, code: u32) {
        if code != BTN_LEFT {
            return;
        }
        let origin = self.input_origin;
        self.install_fit();
        let full = self.area_full();
        let (x, y) = (self.pointer.x, self.pointer.y);
        if let Some(a) = self.install.ask.as_mut() {
            let hit = crate::install_draw::ask_hit(full, a, x, y);
            let o = a.click(hit, origin, now());
            self.install_outcome(o, "click");
            return;
        }
        if self.install.access.is_some() {
            self.access_press();
            return;
        }
        if let Some(g) = self.install.grant.as_mut() {
            let hit = crate::install_draw::grant_hit(full, g, x, y);
            let s = g.click(hit, origin);
            self.install_grant_step(s, "click");
        }
    }

    pub fn install_motion(&mut self) {
        let full = self.area_full();
        let (x, y) = (self.pointer.x, self.pointer.y);
        let changed = if let Some(a) = self.install.ask.as_mut() {
            let h = crate::install_draw::ask_hit(full, a, x, y);
            std::mem::replace(&mut a.hover, h) != h
        } else if self.install.access.is_some() {
            self.access_motion()
        } else if let Some(g) = self.install.grant.as_mut() {
            let h = crate::install_draw::grant_hit(full, g, x, y);
            std::mem::replace(&mut g.hover, h) != h
        } else {
            false
        };
        self.needs_frame |= changed;
    }

    pub fn install_wheel(&mut self, steps: f64) {
        let origin = self.input_origin;
        if let Some(a) = self.install.ask.as_mut() {
            let o = a.wheel(steps, origin);
            self.install_outcome(o, "wheel");
        }
    }

    /// Strg+Alt+G: the grant dialog, from the real keyboard only.
    pub fn install_grant_open(&mut self) {
        if self.input_origin == Origin::Injected {
            self.install_note("injected Strg+Alt+G ignored: grants are set at the real keyboard only".into());
            return;
        }
        if self.install.shown() {
            return;
        }
        if !self.install.connected() {
            self.install_note("Strg+Alt+G: no installer connected".into());
            self.notice("Kein Installer verbunden: es gibt nichts zu erlauben.");
            return;
        }
        let mut durs = DURS.to_vec();
        if let Some(s) = self.install.test_secs {
            durs.insert(0, Dur::Secs(s));
        }
        self.cancel_pointer_modes();
        self.address_end();
        self.page_edit_end();
        self.install.grant = Some(GrantDialog::new(durs, !self.install.grants.is_empty()));
        self.install_note("grant dialog opened".into());
        self.install_changed();
    }

    fn install_grant_step(&mut self, s: GrantStep, how: &str) {
        match s {
            GrantStep::Stay(changed) => self.needs_frame |= changed,
            GrantStep::Ignored => self.install_note(format!("injected {how} on the grant dialog ignored")),
            GrantStep::Close => {
                self.install.grant = None;
                self.install_note("grant dialog closed, nothing granted".into());
                self.install_changed();
            }
            GrantStep::RevokeAll => {
                self.install.grant = None;
                let sent = self.install_send(&link::revoke("all"));
                self.install_note(format!("revoke all{}", if sent { "" } else { " (not delivered)" }));
                self.install_changed();
            }
            GrantStep::Grant => {
                let Some(g) = self.install.grant.take() else { return };
                let (kind, until) = (g.kind_name(), self.install_until(g.durs[g.dur.min(g.durs.len() - 1)]));
                let sent = self.install_send(&link::grant(kind, until));
                self.install_note(format!("grant {kind} until {until} ({}){}", self.install_hm(until), if sent { "" } else { " (not delivered)" }));
                self.install_changed();
            }
        }
    }

    /// Unix end time of `d` from now (the grant's `until`, always wall-clock).
    pub fn install_until(&self, d: Dur) -> i64 {
        let now = crate::status::epoch_secs();
        let tm = crate::status::tm_of(now, self.status.fixed);
        until_of(d, now, tm.tm_hour as i64 * 3600 + tm.tm_min as i64 * 60 + tm.tm_sec as i64)
    }

    /// "08:00" for Unix time `t`, as the clock shows it.
    pub fn install_hm(&self, t: i64) -> String {
        let tm = crate::status::tm_of(t, self.status.fixed);
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }

    /// The bar's marker while grants run.
    pub fn install_grant_marker(&self) -> Option<String> {
        let g: Vec<(String, String)> = self.install.grants.iter().map(|s| (s.kind.clone(), self.install_hm(s.until))).collect();
        crate::install_dialog::marker(&g)
    }

    /// The bar's chip: MC-10's "Fernzugriff offen bis HH:MM" first, then the standing grants.
    pub fn install_marker(&self) -> Option<String> {
        match (self.access_chip(), self.install_grant_marker()) {
            (Some(a), Some(g)) => Some(format!("{a} · {g}")),
            (a, g) => a.or(g),
        }
    }

    /// When the overlay or the marker changes by itself next (countdown, arming, a grant's end).
    pub fn install_due(&self) -> Option<Duration> {
        if self.install.ask.is_some() || self.install.access.is_some() {
            return Some(now() + Duration::from_millis(250));
        }
        (!self.install.grants.is_empty() || !self.install.access_live.is_empty()).then(|| now() + Duration::from_secs(1))
    }

    pub fn install_tick(&mut self, mono: Duration) {
        if self.access_tick(mono) {
            self.install_note("full access ran out".into());
            self.chrome_dirty();
        }
        if let Some(a) = self.install.access.as_ref() {
            // Erlauben arms after a second: one frame then.
            let key = (a.id.unwrap_or(0), a.armed(mono));
            if self.install.shown_key != Some(key) {
                self.install.shown_key = Some(key);
                self.needs_frame = true;
            }
        }
        if let Some(a) = self.install.ask.as_ref() {
            if mono >= a.deadline {
                let id = a.id;
                self.install_note(format!("dialog {id}: time is up, that means no"));
                self.install_answer(false, None, "time up");
                return;
            }
            let key = (a.secs_left(mono), a.armed(mono));
            if self.install.shown_key != Some(key) {
                self.install.shown_key = Some(key);
                self.needs_frame = true;
            }
        }
        let now = crate::status::epoch_secs();
        let before = self.install.grants.len();
        self.install.grants.retain(|g| g.until > now);
        if self.install.grants.len() != before {
            self.install_note(format!("a grant ran out; now: {}", grants_text(&self.install.grants)));
            self.chrome_dirty();
        }
    }

    /// `ctl install`: the state, `log`, the button centres (physical pixels), test aids.
    pub fn install_ctl(&mut self, words: &[&str]) -> Result<String, String> {
        match words.get(1).copied() {
            None => Ok(self.install_report()),
            Some("log") => Ok(self.install.log.join("\n")),
            Some("test-seconds") if cfg!(feature = "nested") && self.headless => {
                let n: i64 = words.get(2).and_then(|w| w.parse().ok()).filter(|n| (1..=600).contains(n)).ok_or("usage: install test-seconds 1..600")?;
                self.install.test_secs = Some(n);
                Ok(format!("the grant dialog offers {n} s"))
            }
            Some("test-seconds") => Err("install test-seconds: only in the headless test build".into()),
            Some(o) => Err(format!("unknown install command {o}")),
        }
    }

    fn install_report(&self) -> String {
        let i = &self.install;
        let s = self.theme.scale;
        let full = self.area_full();
        let at = |r: crate::layout::Rect| format!("{:.0},{:.0}", (r.x as f64 + r.w as f64 / 2.0) * s, (r.y as f64 + r.h as f64 / 2.0) * s);
        let mut out = format!("link={} grants={}", if i.connected() { "up" } else { "down" }, grants_text(&i.grants));
        if let Some(m) = self.install_marker() {
            out.push_str(&format!(" marker=\"{m}\""));
        }
        let t = now();
        if let Some(a) = &i.ask {
            let (no, yes) = crate::install_draw::ask_buttons(full, a);
            out.push_str(&format!(
                " ask={} kind={} focus={} armed={} left={} lines={}/{} scroll={} title=\"{}\" no={} yes={}",
                a.id,
                a.kind,
                if a.on_yes { "yes" } else { "no" },
                a.armed(t),
                a.secs_left(t),
                a.visible.min(a.lines),
                a.lines,
                a.scroll,
                a.title,
                at(no),
                at(yes)
            ));
        }
        if let Some(a) = &i.access {
            out.push_str(&format!(
                " access-dialog id={} client={} hours={} choice={} nvme={} focus={:?} armed={} title=\"{}\"",
                a.id.map_or_else(|| String::from("own"), |x| x.to_string()),
                &a.client[..16.min(a.client.len())],
                a.hours,
                a.hours_chosen(),
                a.nvme,
                a.focus,
                a.armed(t),
                self.access_title(a)
            ));
            let lines = self.access_lines().len();
            for (stop, r) in crate::access_dialog::parts(full, a, lines) {
                out.push_str(&format!(" {}={}", format!("{stop:?}").to_lowercase().replace(['(', ')'], ""), at(r)));
            }
        }
        if !i.access_live.is_empty() {
            out.push_str(&format!(" access={}", i.access_live.iter().map(|a| format!("{}@{}{}", &a.client[..16.min(a.client.len())], a.until, if a.nvme { "+nvme" } else { "" })).collect::<Vec<_>>().join(",")));
        }
        if let Some(g) = &i.grant {
            let d = g.durs[g.dur.min(g.durs.len() - 1)];
            out.push_str(&format!(
                " grant-dialog kind={} dur=\"{}\" focus={:?} sentence=\"{}\"",
                g.kind_name(),
                crate::install_dialog::dur_label(d),
                g.focus,
                g.sentence(&self.install_hm(self.install_until(d)))
            ));
            for (stop, r) in crate::install_draw::grant_parts(full, g) {
                out.push_str(&format!(" {}={}", format!("{stop:?}").to_lowercase(), at(r)));
            }
        }
        out
    }
}

fn grants_text(g: &[Shown]) -> String {
    if g.is_empty() {
        return "none".into();
    }
    g.iter().map(|s| format!("{}@{}", s.kind, s.until)).collect::<Vec<_>>().join(",")
}

impl Stop {
    pub fn label(self) -> &'static str {
        match self {
            Stop::Kind => "Was",
            Stop::Until => "Wie lange",
            Stop::Cancel => "Abbrechen",
            Stop::Revoke => "Alle zurücknehmen",
            Stop::Ok => "Erlauben",
        }
    }
}

impl Btn {
    pub fn label(self) -> &'static str {
        match self {
            Btn::No => "Ablehnen",
            Btn::Yes => "Erlauben",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use smithay::reexports::wayland_server::Display;

    /// A bitscomp state without a back end, linked to the installer's end of a socket pair.
    fn linked() -> (Display<App>, App, UnixStream) {
        let d = Display::<App>::new().expect("display");
        let mut app = App::new(d.handle(), "wy-test".into(), (1280, 720));
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs.set_nonblocking(true).unwrap();
        app.install_msg(Msg::Up(1, ours, "test".into()));
        (d, app, theirs)
    }

    fn sent(p: &mut UnixStream) -> String {
        let mut b = vec![0u8; 4096];
        match p.read(&mut b) {
            Ok(n) => String::from_utf8_lossy(&b[..n]).into_owned(),
            Err(_) => String::new(),
        }
    }

    fn ask(id: u64) -> FromInstaller {
        FromInstaller::Ask { id, kind: "reboot".into(), title: "Neu starten".into(), text: "Werkzeug: reboot".into(), timeout_ms: 60_000 }
    }

    fn line(app: &mut App, link: u64, m: FromInstaller) {
        app.install_msg(Msg::Line(link, Some(m), 0));
    }

    #[test]
    fn one_question_at_a_time_and_only_its_own_cancel_closes_it() {
        let (_d, mut app, mut p) = linked();
        line(&mut app, 1, ask(1));
        assert_eq!(app.install.ask.as_ref().map(|a| a.id), Some(1));
        // A second ask is answered no at once (nobody saw it); the shown one stays as it is.
        line(&mut app, 1, ask(2));
        assert_eq!(sent(&mut p), link::answer(2, false, "none"));
        assert_eq!(app.install.ask.as_ref().map(|a| a.id), Some(1));
        // A cancel of another id, a line of a stale link and a grants line leave the question alone.
        line(&mut app, 1, FromInstaller::Cancel { id: 9, reason: "x".into() });
        line(&mut app, 7, FromInstaller::Cancel { id: 1, reason: "stale".into() });
        let until = crate::status::epoch_secs() + 600;
        line(&mut app, 1, FromInstaller::Grants(vec![Shown { kind: "install".into(), until }]));
        assert_eq!(app.install.ask.as_ref().map(|a| a.id), Some(1));
        assert_eq!(app.install.grants, vec![Shown { kind: "install".into(), until }]);
        assert_eq!(sent(&mut p), "", "nothing answered for them");
        // Its own cancel closes it without an answer.
        line(&mut app, 1, FromInstaller::Cancel { id: 1, reason: "timeout".into() });
        assert!(app.install.ask.is_none());
        assert_eq!(sent(&mut p), "");
    }

    #[test]
    fn a_lost_link_closes_the_question_unanswered_and_forgets_the_grants() {
        let (_d, mut app, mut p) = linked();
        line(&mut app, 1, FromInstaller::Hello { version: 1, grants: vec![Shown { kind: "pin".into(), until: crate::status::epoch_secs() + 60 }], access: vec![], clients: vec![] });
        line(&mut app, 1, ask(3));
        // An older link's end changes nothing.
        app.install_msg(Msg::Down(0, "old".into()));
        assert!(app.install.ask.is_some() && app.install.connected());
        app.install_msg(Msg::Down(1, "closed the connection".into()));
        assert!(app.install.ask.is_none() && app.install.grants.is_empty() && !app.install.connected());
        assert_eq!(sent(&mut p), "");
        // A grant already over when reported is not shown.
        let (_d2, mut app, _p) = linked();
        line(&mut app, 1, FromInstaller::Grants(vec![Shown { kind: "pin".into(), until: crate::status::epoch_secs() - 1 }]));
        assert!(app.install.grants.is_empty());
    }

    #[test]
    fn the_dialog_time_is_no_from_nobody_and_revoke_all_is_sent() {
        let (_d, mut app, mut p) = linked();
        line(&mut app, 1, ask(4));
        let deadline = app.install.ask.as_ref().unwrap().deadline;
        app.install_tick(deadline);
        assert!(app.install.ask.is_none());
        assert_eq!(sent(&mut p), link::answer(4, false, "none"));
        app.install.grant = Some(GrantDialog::new(DURS.to_vec(), true));
        app.install_grant_step(GrantStep::RevokeAll, "test");
        assert!(app.install.grant.is_none());
        assert_eq!(sent(&mut p), link::revoke("all"));
    }

    /// MC-10: the access question: a real Enter (armed) grants the preselected 24 h for that client and id, an
    /// injected one is sent as injected; the installer's access line shows the chip; Strg+Alt+V from injected
    /// input opens nothing; a revoke at the keyboard goes out.
    #[test]
    fn the_access_question_and_the_chip() {
        let (_d, mut app, mut p) = linked();
        let c = "cd".repeat(32);
        let q = |id| FromInstaller::AskAccess { id, title: String::new(), text: "Der Gaming-PC bittet um Vollzugriff für 24 h.".into(), timeout_ms: 240_000, client: "cd".repeat(32), name: "bitsos-desk".into(), hours: 24 };
        line(&mut app, 1, q(5));
        assert!(app.install.shown() && app.install.access.as_ref().is_some_and(|a| a.id == Some(5) && a.focus == crate::access_dialog::AStop::Yes));
        assert!(app.install_report().contains("access-dialog id=5") && app.install_report().contains("Vollzugriff für den Gaming-PC (bitsos-desk, cdcd cdcd cdcd cdcd) bis"));
        // A second question while it is shown: no.
        line(&mut app, 1, ask(6));
        assert_eq!(sent(&mut p), link::answer(6, false, "none"));
        app.install.access.as_mut().unwrap().opened = Duration::ZERO;
        app.input_origin = Origin::Injected;
        app.install_key(Keysym::Return);
        assert_eq!(sent(&mut p), link::access(Some(5), &c, 24, false, "injected"));
        assert!(!app.install.shown());
        line(&mut app, 1, q(7));
        app.install.access.as_mut().unwrap().opened = Duration::ZERO;
        app.input_origin = Origin::Physical;
        app.install_key(Keysym::Return);
        assert_eq!(sent(&mut p), link::access(Some(7), &c, 24, false, "physical"));
        // The installer's line: the chip; then Strg+Alt+V injected (ignored) and real (the own dialog), revoke.
        let until = crate::status::epoch_secs() + 3600;
        line(&mut app, 1, FromInstaller::Access { access: vec![AccessShown { client: c.clone(), name: "bitsos-desk".into(), until, nvme: false }], clients: vec![ClientShown { client: c.clone(), name: "bitsos-desk".into() }] });
        assert!(app.install_marker().is_some_and(|m| m.starts_with("Fernzugriff offen bis ")), "{:?}", app.install_marker());
        app.input_origin = Origin::Injected;
        app.access_open();
        assert!(!app.install.shown(), "injected Strg+Alt+V opens nothing");
        app.input_origin = Origin::Physical;
        app.access_open();
        assert!(app.install.access.as_ref().is_some_and(|a| a.id.is_none() && a.revoke));
        app.install.access.as_mut().unwrap().focus = crate::access_dialog::AStop::Revoke;
        app.install_key(Keysym::Return);
        assert_eq!(sent(&mut p), link::revoke("access"));
        line(&mut app, 1, FromInstaller::Access { access: vec![], clients: vec![] });
        assert_eq!(app.install_marker(), None);
    }
}

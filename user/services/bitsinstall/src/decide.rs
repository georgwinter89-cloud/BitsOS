//! The installer's rule for one request, and its grants:
//! 1. no confirmer connected (no desktop session) and no text console that can ask (MC-11, `console`): no
//!    (`no_session`), grants or not (MC-9a brief: nothing on the screen would show what runs);
//! 2. a standing grant of the request's kind that holds now: yes (`by` = `grant`), no dialog;
//! 3. else the dialog: yes only when the person answered yes with real input (`dialog`); a yes from injected
//!    input is refused (`refused`), no answer in time is no (`timeout`), a confirmer that went away is no.
//!
//! MC-10: a `request_access` (kind `access`) is never covered by a grant: the access dialog asks, and only a
//! choice made with real input sets full access (`crate::access`), which the main loop hands the kernel before
//! the yes; a `pair_client` (kind `pair`) is an ordinary yes/no question.
//!
//! Every decision and every grant change is journaled (kernel log lines, which bitsjournal keeps; the kernel
//! adds its own `mcp svc.install_decided` event with the same id). Grants are kept in a file on the stick
//! (`store`), so a Reload in a night round keeps them.

use crate::access::{Access, Client};
use crate::confirm::Input;
use crate::grant::{Ended, Grant, Grants};
use crate::log;
use mcp_wire::installer::{Asked, Decision, Kind};
use std::sync::Mutex;
use std::time::Duration;

/// What became of a dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Answer { yes: bool, input: Input },
    Timeout,
    /// No confirmer is connected.
    NoSession,
    /// The confirmer went away while the dialog was open.
    Ended,
    /// MC-10: the access dialog's choice.
    Access { hours: i64, nvme: bool, input: Input },
}

/// Shows a dialog and waits for its answer (bounded by `timeout`).
pub trait Confirmer {
    fn ask(&self, id: u64, kind: Kind, title: &str, text: &str, timeout: Duration) -> Outcome;
    fn connected(&self) -> bool;
    /// MC-10: the access dialog for `client` (bitscomp: the choices, the NVMe box).
    fn ask_access(&self, _id: u64, _client: &Client, _hours: i64, _title: &str, _text: &str, _timeout: Duration) -> Outcome {
        Outcome::NoSession
    }
    /// MC-10: full access changed (the confirmer's chip).
    fn access_changed(&self) {}
}

/// Where grants are kept between starts.
pub trait Keep: Send + Sync {
    fn save(&self, text: &str) -> Result<(), String>;
}

pub struct State {
    grants: Mutex<Grants>,
    keep: Option<Box<dyn Keep>>,
    pub dialog: Duration,
    /// MC-10: full access (None: tests of the older kinds).
    access: Option<std::sync::Arc<Access>>,
    /// MC-10: clients whose last `request_access` Georg refused (or let pass), and until when (Unix s) a new one
    /// is refused without a dialog: a client cannot keep a dialog in front of him.
    cool: Mutex<Vec<(String, i64)>>,
}

/// How long a client's `request_access` is refused without a dialog after one was refused or not answered.
pub const ACCESS_COOL_S: i64 = 60;

fn when(t: i64) -> String {
    format!("{t} (unix)")
}

impl State {
    pub fn new(grants: Grants, keep: Option<Box<dyn Keep>>, dialog: Duration) -> State {
        State { grants: Mutex::new(grants), keep, dialog, access: None, cool: Mutex::new(Vec::new()) }
    }

    /// MC-10: with full access.
    pub fn with_access(mut self, a: std::sync::Arc<Access>) -> State {
        self.access = Some(a);
        self
    }

    pub fn access(&self) -> Option<&std::sync::Arc<Access>> {
        self.access.as_ref()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Grants> {
        self.grants.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, g: &Grants) {
        if let Some(k) = &self.keep
            && let Err(e) = k.save(&g.to_text())
        {
            log::warn(&format!("grants not kept on the stick ({e}); they hold until this service ends"));
        }
    }

    pub fn grants(&self) -> Vec<Grant> {
        self.lock().all().to_vec()
    }

    /// Ends what ended by `now` (journaled); true when something did.
    pub fn sweep(&self, now: i64) -> bool {
        let mut g = self.lock();
        let ended = g.sweep(now);
        for (e, why) in &ended {
            log::info(&format!("grant {} until {} {}", e.kind.name(), when(e.until), why.text()));
        }
        if !ended.is_empty() {
            self.save(&g);
        }
        !ended.is_empty()
    }

    /// A grant set at the keyboard; only real input sets one.
    pub fn set_grant(&self, kind: Option<Kind>, kind_text: &str, until: i64, input: Input, now: i64) -> Result<Grant, String> {
        let refuse = |why: String| {
            log::warn(&format!("grant {kind_text} refused: {why}"));
            Err(why)
        };
        if input != Input::Physical {
            return refuse(format!("set with {} input; only a real keyboard or mouse sets a grant", input.name()));
        }
        let Some(kind) = kind else { return refuse(String::from("no such kind of action")) };
        let mut g = self.lock();
        match g.set(kind, until, now) {
            Ok((grant, old)) => {
                if let Some(o) = old {
                    log::info(&format!("grant {} until {} {}", o.kind.name(), when(o.until), Ended::Replaced.text()));
                }
                log::info(&format!("grant {} set at the keyboard until {} ({} s from now)", kind.name(), when(until), until - now));
                self.save(&g);
                Ok(grant)
            }
            Err(e) => {
                drop(g);
                refuse(e)
            }
        }
    }

    pub fn revoke(&self, kind: Option<Kind>) -> Vec<Grant> {
        let mut g = self.lock();
        let gone = g.revoke(kind);
        for e in &gone {
            log::info(&format!("grant {} until {} {}", e.kind.name(), when(e.until), Ended::Revoked.text()));
        }
        if !gone.is_empty() {
            self.save(&g);
        }
        gone
    }

    /// The decision for `a` at `now`.
    pub fn decide(&self, c: &dyn Confirmer, a: &Asked, now: i64) -> Decision {
        self.sweep(now);
        let d = |yes: bool, by: &str, reason: String| Decision { id: a.id, yes, by: String::from(by), reason };
        let what = format!("request {} {} ({}) from {}", a.id, a.tool, a.kind.name(), a.peer);
        if !c.connected() {
            log::info(&format!("{what}: no, no desktop session to ask"));
            return d(false, "no_session", String::new());
        }
        if a.kind == Kind::Access {
            return self.decide_access(c, a, &what);
        }
        let covering = self.lock().covering(a.kind, now);
        if let Some(g) = covering {
            log::info(&format!("{what}: yes by the grant until {}", when(g.until)));
            return d(true, "grant", format!("standing grant {} until {}", g.kind.name(), when(g.until)));
        }
        log::info(&format!("{what}: asking at the screen ({} s)", self.dialog.as_secs()));
        let out = c.ask(a.id, a.kind, &crate::text::title(a), &crate::text::text(a), self.dialog);
        let dec = match out {
            Outcome::Answer { yes: true, input: Input::Physical } => d(true, "dialog", String::from("confirmed at the machine")),
            Outcome::Answer { yes: true, input } => d(false, "refused", format!("the yes came from {} input, which never confirms", input.name())),
            Outcome::Answer { yes: false, .. } => d(false, "dialog", String::new()),
            Outcome::Timeout => d(false, "timeout", format!("no answer within {} s", self.dialog.as_secs())),
            Outcome::NoSession => d(false, "no_session", String::new()),
            Outcome::Ended => d(false, "no_session", String::from("the desktop session ended while asking")),
            Outcome::Access { .. } => d(false, "refused", String::from("an access choice is no answer to this question")),
        };
        log::info(&format!("{what}: {} by {}{}", if dec.yes { "yes" } else { "no" }, dec.by, if dec.reason.is_empty() { String::new() } else { format!(" ({})", dec.reason) }));
        // MC-10: a pairing Georg confirmed at the keyboard is what lets that key ask for full access.
        if a.kind == Kind::Pair
            && dec.yes
            && let Some(access) = &self.access
        {
            access.confirm_pairing(a.args.get("fingerprint").and_then(mcp_wire::Json::as_str).unwrap_or(""));
        }
        dec
    }
}

impl State {
    /// MC-10: `request_access`: the access dialog; yes only when the choice came with real input and full access
    /// was set (the kernel gets the table before the yes: `Access` is dirty).
    fn decide_access(&self, c: &dyn Confirmer, a: &Asked, what: &str) -> Decision {
        let d = |yes: bool, by: &str, reason: String| {
            log::info(&format!("{what}: {} by {by}{}", if yes { "yes" } else { "no" }, if reason.is_empty() { String::new() } else { format!(" ({reason})") }));
            Decision { id: a.id, yes, by: String::from(by), reason }
        };
        let Some(access) = &self.access else { return d(false, "refused", String::from("full access is not kept by this installer")) };
        let Some(client) = access.client(&a.client) else {
            return d(false, "refused", String::from("the asking key is not a client Georg paired at the keyboard (pair again under mcp=service: host/mcp/pair.mjs)"));
        };
        {
            let now = access.wall_now();
            let mut cool = self.cool.lock().unwrap_or_else(|e| e.into_inner());
            cool.retain(|(_, until)| *until > now);
            if let Some((_, until)) = cool.iter().find(|(k, _)| *k == client.fp) {
                return d(false, "refused", format!("asked again within {ACCESS_COOL_S} s of a refusal; no dialog until {until} (unix)"));
            }
        }
        let cool_down = || self.cool.lock().unwrap_or_else(|e| e.into_inner()).push((client.fp.clone(), access.wall_now().saturating_add(ACCESS_COOL_S)));
        let hours = a.args.get("hours").and_then(mcp_wire::Json::as_i64).unwrap_or(mcp_wire::access::DEFAULT_H);
        let reason = a.args.get("reason").and_then(mcp_wire::Json::as_str).unwrap_or("");
        // The person has to walk to the keyboard: the longest dialog time.
        let wait = Duration::from_millis(mcp_wire::installer::DIALOG_MAX_MS).max(self.dialog);
        log::info(&format!("{what}: asking at the screen for full access ({} s)", wait.as_secs()));
        let out = c.ask_access(a.id, &client, hours, &crate::text::access_title(&client), &crate::text::access_text(hours, reason, &a.peer), wait);
        match out {
            Outcome::Access { hours, nvme, input: Input::Physical } => match access.grant(&client.fp, hours, nvme, Input::Physical) {
                Ok(g) => {
                    c.access_changed();
                    d(true, "dialog", format!("full access until {} (unix){}", g.until, if nvme { ", NVMe too" } else { "" }))
                }
                Err(why) => d(false, "refused", why),
            },
            Outcome::Access { input, .. } | Outcome::Answer { yes: true, input } => {
                cool_down();
                d(false, "refused", format!("the choice came from {} input, which never grants", input.name()))
            }
            Outcome::Answer { yes: false, .. } => {
                cool_down();
                d(false, "dialog", String::new())
            }
            Outcome::Timeout => {
                cool_down();
                d(false, "timeout", format!("no answer within {} s", wait.as_secs()))
            }
            Outcome::NoSession => d(false, "no_session", String::new()),
            Outcome::Ended => d(false, "no_session", String::from("the desktop session ended while asking")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_wire::Json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const T: i64 = 1_800_000_000;

    struct Fake {
        out: Outcome,
        on: bool,
        asked: AtomicUsize,
    }
    impl Confirmer for Fake {
        fn ask(&self, _: u64, _: Kind, title: &str, text: &str, _: Duration) -> Outcome {
            assert!(!title.is_empty() && text.contains("Werkzeug:"));
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.out
        }
        fn connected(&self) -> bool {
            self.on
        }
    }
    fn fake(out: Outcome, on: bool) -> Fake {
        Fake { out, on, asked: AtomicUsize::new(0) }
    }

    struct Mem(Arc<Mutex<String>>);
    impl Keep for Mem {
        fn save(&self, t: &str) -> Result<(), String> {
            *self.0.lock().unwrap() = t.into();
            Ok(())
        }
    }

    fn asked(tool: &str) -> Asked {
        Asked { id: 4, tool: tool.into(), kind: Kind::of(tool), args: Json::Obj(Vec::new()), timeout_ms: 1000, client: String::new(), peer: "10.0.2.2:4000".into() }
    }

    fn state() -> (State, Arc<Mutex<String>>) {
        let file = Arc::new(Mutex::new(String::new()));
        (State::new(Grants::default(), Some(Box::new(Mem(file.clone()))), Duration::from_secs(60)), file)
    }

    #[test]
    fn the_dialog_decides_only_with_real_input() {
        let (s, _) = state();
        let yes = s.decide(&fake(Outcome::Answer { yes: true, input: Input::Physical }, true), &asked("install_kernel"), T);
        assert_eq!((yes.yes, yes.by.as_str(), yes.id), (true, "dialog", 4));
        let inj = s.decide(&fake(Outcome::Answer { yes: true, input: Input::Injected }, true), &asked("install_kernel"), T);
        assert_eq!((inj.yes, inj.by.as_str()), (false, "refused"));
        let none = s.decide(&fake(Outcome::Answer { yes: true, input: Input::None }, true), &asked("reload"), T);
        assert!(!none.yes);
        let no = s.decide(&fake(Outcome::Answer { yes: false, input: Input::Physical }, true), &asked("reload"), T);
        assert_eq!((no.yes, no.by.as_str()), (false, "dialog"));
        let late = s.decide(&fake(Outcome::Timeout, true), &asked("reload"), T);
        assert_eq!((late.yes, late.by.as_str()), (false, "timeout"));
        let gone = s.decide(&fake(Outcome::Ended, true), &asked("reload"), T);
        assert_eq!((gone.yes, gone.by.as_str()), (false, "no_session"));
    }

    #[test]
    fn without_a_session_everything_is_refused() {
        let (s, _) = state();
        let f = fake(Outcome::Answer { yes: true, input: Input::Physical }, false);
        for t in ["install_kernel", "reload", "system_pin", "reboot", "settings_set", "nvme_gate", "frob"] {
            let d = s.decide(&f, &asked(t), T);
            assert_eq!((d.yes, d.by.as_str()), (false, "no_session"), "{t}");
        }
        assert_eq!(f.asked.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_grant_passes_its_kind_until_it_expires_by_itself() {
        let (s, file) = state();
        let f = fake(Outcome::Answer { yes: false, input: Input::Physical }, true);
        assert!(s.set_grant(Some(Kind::Install), "install", T + 600, Input::Injected, T).is_err(), "injected input sets no grant");
        assert!(s.set_grant(None, "everything", T + 600, Input::Physical, T).is_err());
        assert!(s.set_grant(Some(Kind::Nvme), "nvme", T + 600, Input::Physical, T).is_err());
        s.set_grant(Some(Kind::Install), "install", T + 600, Input::Physical, T).unwrap();
        assert_eq!(*file.lock().unwrap(), format!("install {} {T}\n", T + 600), "kept on the stick");
        let d = s.decide(&f, &asked("install_kernel"), T + 10);
        assert_eq!((d.yes, d.by.as_str()), (true, "grant"));
        assert_eq!(s.decide(&f, &asked("install_system"), T + 599).by, "grant");
        assert_eq!(f.asked.load(Ordering::SeqCst), 0, "no dialog while the grant holds");
        let other = s.decide(&f, &asked("reload"), T + 10);
        assert_eq!((other.yes, other.by.as_str()), (false, "dialog"), "another kind is asked");
        let after = s.decide(&f, &asked("install_kernel"), T + 600);
        assert_eq!((after.yes, after.by.as_str()), (false, "dialog"), "expired: asked again");
        assert!(s.grants().is_empty() && file.lock().unwrap().is_empty(), "the expiry is kept too");
        s.set_grant(Some(Kind::Reload), "reload", T + 900, Input::Physical, T).unwrap();
        assert_eq!(s.revoke(None).len(), 1);
        assert_eq!(s.decide(&f, &asked("reload"), T + 1).by, "dialog");
    }

    #[test]
    fn no_session_refuses_even_with_a_grant_and_nvme_is_always_asked() {
        let (s, _) = state();
        s.set_grant(Some(Kind::Reload), "reload", T + 600, Input::Physical, T).unwrap();
        assert_eq!(s.decide(&fake(Outcome::Timeout, false), &asked("reload"), T + 1).by, "no_session");
        let on = fake(Outcome::Timeout, true);
        assert_eq!(s.decide(&on, &asked("reload"), T + 1).by, "grant");
        assert_eq!(s.decide(&on, &asked("nvme_gate"), T + 1).by, "timeout", "never granted, always asked");
    }
}

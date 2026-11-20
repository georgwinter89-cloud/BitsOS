//! SYS-A: what the program "System" does on a click: tabs, ranges, the task manager's search, filters and sort,
//! choosing a row or device, "Fenster zeigen", "Beenden" (politely first: the windows' own close, else SIGTERM; after
//! `WAIT` a question offers "Sofort beenden"), Vorrang (setpriority for every thread of the row's processes), the
//! link to "Wer hat was gemacht", "Bericht für Claude kopieren". sys_rules decides what is free and what asks; an
//! asked step runs only on a yes by Georg's own input (`Pages::real`, the same proof the page "KI" uses).

use crate::page_confirm::plain;
use crate::state::App;
use crate::sys_app::{Asked, Pending, WAIT};
use crate::sys_group::Row;
use crate::sys_rules::{self as rules, Act, Gate, Vorrang};
use crate::sys_view::SysPart;

/// Whether a yes at `now` comes too soon after a question that appeared at `at`.
pub fn early(at: std::time::Duration, now: std::time::Duration) -> bool {
    now < at + crate::assist::ARM
}

/// The start ticks of `pid` now (None: gone, or ended and not yet reaped).
fn start_of(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    if matches!(crate::ending::parse_state(&stat), Some('Z' | 'X')) {
        return None;
    }
    crate::ending::parse_start_time(&stat)
}

fn signal(pid: u32, sig: libc::c_int) -> std::io::Result<()> {
    // SAFETY: kill only sends a signal; `may_signal` keeps pid 0, 1 and bitscomp itself out.
    if crate::ending::may_signal(pid as i32, std::process::id() as i32) && unsafe { libc::kill(pid as libc::pid_t, sig) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// setpriority(2) for a process group (`group`) or one thread.
fn setprio(group: bool, who: u32, nice: i32) -> std::io::Result<()> {
    // SAFETY: setpriority only changes a scheduling value; the kernel checks ownership and RLIMIT_NICE.
    let r = unsafe { if group { libc::setpriority(libc::PRIO_PGRP, who as libc::id_t, nice) } else { libc::setpriority(libc::PRIO_PROCESS, who as libc::id_t, nice) } };
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Every thread of `pid` (its /proc/<pid>/task), the main thread when the list is unreadable.
fn threads(pid: u32) -> Vec<u32> {
    let mut v: Vec<u32> = std::fs::read_dir(format!("/proc/{pid}/task")).map(|d| d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect()).unwrap_or_default();
    if v.is_empty() {
        v.push(pid);
    }
    v
}

/// Every thread of `pid` to `v`'s nice value (PRIO_PROCESS acts on one thread). The caller keeps the rules (never
/// BitsOS's services; Hoch only after Georg's yes by real input).
pub fn renice(pid: u32, v: Vorrang) -> Result<(), String> {
    if !crate::ending::may_signal(pid as i32, std::process::id() as i32) {
        return Err(format!("pid {pid} is not a program's"));
    }
    for tid in threads(pid) {
        setprio(false, tid, v.nice()).map_err(|e| errno_words(&e))?;
    }
    Ok(())
}

fn errno_words(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EACCES) => "BitsOS erlaubt dem Desktop keinen höheren Vorrang".into(),
        Some(libc::EPERM) => "das Programm gehört dir nicht".into(),
        Some(libc::ESRCH) => "das Programm läuft nicht mehr".into(),
        _ => e.to_string(),
    }
}

impl App {
    fn sys_row(&self, pid: u32) -> Option<Row> {
        self.sys.rows.iter().find(|r| r.main() == pid).cloned()
    }

    /// The row the buttons act on: the page's explicit choice while it still runs, never a stand-in.
    pub(crate) fn sys_detail_row(&self, page: u32) -> Option<Row> {
        let v = &self.pages.by_pid.get(&page)?.sys;
        crate::sys_model3::selected(&self.sys.rows, v.selected).cloned()
    }

    /// A detail shown without a choice becomes the choice the moment it is drawn, so what Georg sees is what a
    /// button acts on, even when the table regroups before his click.
    pub(crate) fn sys_pin(&mut self, page: u32) {
        let Some(s) = self.pages.by_pid.get(&page) else { return };
        let v = &s.sys;
        // Pinned only once the rows carry their CPU shares: before that the order by CPU still settles.
        if v.selected.is_some() || v.tab != crate::sys_view::Tab::Procs || !self.sys.rows.iter().any(|r| r.cpu.is_some()) {
            return;
        }
        let shown = crate::sys_group::shown(&self.sys.rows, v.filter, &v.query, v.sort, v.up, crate::sys_model3::SYSTEM_TOP);
        if let Some(p) = crate::sys_model3::pin(&self.sys.rows, &shown, v.selected)
            && let Some(s) = self.pages.by_pid.get_mut(&page)
        {
            s.sys.selected = Some(p);
        }
    }

    pub(crate) fn sys_act(&mut self, page: u32, p: SysPart) {
        match p {
            SysPart::Tab(t) => return self.sys_set_tab(page, t),
            SysPart::Search => {
                let q = self.pages.by_pid.get(&page).map(|s| s.sys.query.clone()).unwrap_or_default();
                return self.page_edit_begin(page, crate::page::Part::Sys(SysPart::Search), q);
            }
            SysPart::Show => self.sys_show(page),
            SysPart::End => self.sys_end(page, false),
            SysPart::Prio(x) => self.sys_prio(page, x),
            SysPart::WhoDid => self.sys_who(page),
            SysPart::Copy => self.sys_copy(page),
            SysPart::Link(i) => {
                let links = self.pages.by_pid.get(&page).map(|s| self.sys_model(&s.sys).links).unwrap_or_default();
                if let Some(a) = links.get(i).cloned() {
                    eprintln!("bitscomp: system: link opens {a}");
                    self.open_address(&a, crate::address::Where::Tab, None);
                }
            }
            _ => {
                let start = if let SysPart::Row(pid) = p { self.sys_row(pid).map_or(0, |r| r.start) } else { 0 };
                let Some(s) = self.pages.by_pid.get_mut(&page) else { return };
                let v = &mut s.sys;
                match p {
                    SysPart::Range(r) => v.range = r,
                    SysPart::Filter(f) => {
                        v.filter = f;
                        s.scroll = 0;
                    }
                    SysPart::Sort(c) if v.sort == c => v.up = !v.up,
                    SysPart::Sort(c) => {
                        v.sort = c;
                        v.up = matches!(c, crate::sys_group::Column::Name | crate::sys_group::Column::State);
                    }
                    SysPart::Row(pid) => v.selected = Some((pid, start)),
                    SysPart::More => {
                        v.filter = crate::sys_group::Filter::System;
                        s.scroll = 0;
                    }
                    SysPart::Device(i) => v.device = i,
                    _ => {}
                }
            }
        }
        self.needs_frame = true;
        self.sys_want();
    }

    /// Typing in the search field filters at once.
    pub(crate) fn sys_typed(&mut self, page: u32) {
        if let Some(s) = self.pages.by_pid.get_mut(&page)
            && let Some((_, e)) = s.edit.as_ref()
        {
            s.sys.query = e.text.clone();
            s.scroll = 0;
        }
    }

    fn sys_show(&mut self, page: u32) {
        let Some(r) = self.sys_detail_row(page) else { return };
        let ids: Vec<u64> = r.wins.iter().map(|w| w.id).collect();
        if let Some(w) = self.all_windows().into_iter().find(|w| ids.contains(&crate::ctl::win_id(w))) {
            eprintln!("bitscomp: system: shows the window of {}", r.name);
            self.sys_show_window(&w);
        }
    }

    fn sys_gate(&self, r: &Row, a: Act) -> Gate {
        rules::gate(r, a, self.sys.own_uid, self.sys.root.is_real())
    }

    /// Opens the page's own question for `asked`.
    fn sys_ask(&mut self, page: u32, title: String, lines: Vec<String>, ok: &str, asked: Asked) {
        self.address_end();
        self.page_edit_end();
        if let Some(s) = self.pages.by_pid.get_mut(&page) {
            s.confirm = Some(plain(title, lines, ok));
            s.msg = None;
        }
        eprintln!("bitscomp: system page {page} asks before {asked:?}");
        self.sys.asking.insert(page, asked);
        self.sys.asked_at.insert(page, crate::pacer::now());
        self.needs_frame = true;
    }

    pub(crate) fn sys_asks(&self, page: u32) -> bool {
        self.sys.asking.contains_key(&page)
    }

    /// A yes right after the page's question appeared was not meant for it: the question stays.
    pub(crate) fn sys_yes_early(&self, page: u32) -> bool {
        self.sys.asking.contains_key(&page) && self.sys.asked_at.get(&page).is_some_and(|t| early(*t, crate::pacer::now()))
    }

    fn sys_end(&mut self, page: u32, kill: bool) {
        let Some(r) = self.sys_detail_row(page) else { return };
        match self.sys_gate(&r, if kill { Act::Kill } else { Act::End }) {
            Gate::No(why) => self.page_say(page, why, true),
            Gate::Ask => {
                let lines = vec![r.name.clone(), "Ein Dienst von BitsOS: ohne ihn fehlt vielleicht etwas, bis BitsOS ihn neu startet.".into(), rules::REAL_INPUT.into()];
                self.sys_ask(page, format!("„{}“ beenden?", r.name), lines, "Beenden", Asked::End { pid: r.main(), start: r.start, kill });
            }
            Gate::Free => self.sys_end_now(page, &r, kill),
        }
    }

    fn sys_end_now(&mut self, page: u32, r: &Row, kill: bool) {
        if start_of(r.main()) != Some(r.start) {
            return self.page_say(page, format!("„{}“ läuft nicht mehr.", r.name), false);
        }
        let ids: Vec<u64> = r.wins.iter().map(|w| w.id).collect();
        let wins: Vec<crate::win::Window> = self.all_windows().into_iter().filter(|w| ids.contains(&crate::ctl::win_id(w))).collect();
        if kill {
            eprintln!("bitscomp: system: Sofort beenden {} (pid {})", r.name, r.main());
            if wins.is_empty() {
                if let Err(e) = signal(r.main(), libc::SIGKILL) {
                    return self.page_say(page, format!("„{}“ nicht beendet: {}", r.name, errno_words(&e)), true);
                }
            }
            for w in &wins {
                let _ = self.end_window(w, "Sofort beenden im Taskmanager");
            }
            self.sys.pending.retain(|p| p.pid != r.main());
            return self.page_say(page, format!("„{}“ wird sofort beendet.", r.name), false);
        }
        eprintln!("bitscomp: system: Beenden {} (pid {}), politely", r.name, r.main());
        if wins.is_empty() {
            if let Err(e) = signal(r.main(), libc::SIGTERM) {
                return self.page_say(page, format!("„{}“ nicht beendet: {}", r.name, errno_words(&e)), true);
            }
            let _ = signal(r.main(), libc::SIGCONT);
        }
        for w in &wins {
            crate::xwm::close(w);
        }
        self.sys.pending.retain(|p| p.pid != r.main());
        self.sys.pending.push(Pending { page, pid: r.main(), start: r.start, name: r.name.clone(), at: crate::sys_sample::now_s(), offered: false });
        self.page_say(page, format!("„{}“ wurde gebeten, sich zu beenden …", r.name), false);
    }

    /// Each snapshot: ended programs are said, those still there after `WAIT` get the offer.
    pub(crate) fn sys_pending_tick(&mut self) {
        let now = crate::sys_sample::now_s();
        let mut said = Vec::new();
        let mut offer = Vec::new();
        self.sys.pending.retain_mut(|p| {
            if start_of(p.pid) != Some(p.start) {
                said.push((p.page, format!("„{}“ ist beendet.", p.name)));
                return false;
            }
            if !p.offered && now - p.at >= WAIT {
                p.offered = true;
                offer.push(p.clone());
            }
            true
        });
        for (page, text) in said {
            self.page_say(page, text, false);
        }
        for p in offer {
            if self.pages.by_pid.get(&p.page).is_some_and(|s| s.confirm.is_none()) {
                let lines = vec![p.name.clone(), "Es hat sich nach der höflichen Bitte nicht beendet.".into(), "„Sofort beenden“ schließt es ohne Rückfrage: nicht Gespeichertes geht verloren.".into()];
                self.sys_ask(p.page, format!("„{}“ reagiert nicht", p.name), lines, "Sofort beenden", Asked::End { pid: p.pid, start: p.start, kill: true });
            }
        }
    }

    fn sys_prio(&mut self, page: u32, v: Vorrang) {
        let Some(r) = self.sys_detail_row(page) else { return };
        let real = self.pages.real;
        match self.sys_gate(&r, Act::Prio(v)) {
            Gate::No(why) => self.page_say(page, why, true),
            Gate::Ask => {
                let mut lines = vec![r.name.clone()];
                if r.service {
                    lines.push("Ein Dienst von BitsOS: sein Vorrang ändert, wie schnell der Rest reagiert.".into());
                } else {
                    lines.push("Hoher Vorrang: das Programm kommt vor allen anderen dran; der Rest kann langsamer werden.".into());
                }
                lines.push(rules::REAL_INPUT.into());
                let title = if v == Vorrang::Hoch { "Hohen Vorrang geben?".to_string() } else { format!("Vorrang „{}“ für einen Dienst?", v.name()) };
                self.sys_ask(page, title, lines, v.name(), Asked::Prio { pid: r.main(), start: r.start, v });
            }
            Gate::Free => self.sys_prio_now(page, &r, v, real),
        }
    }

    /// Sets the nice value of every thread of the row: the process group in one call when it is the row's alone.
    pub(crate) fn sys_prio_now(&mut self, page: u32, r: &Row, v: Vorrang, real: bool) {
        if start_of(r.main()) != Some(r.start) {
            return self.page_say(page, format!("„{}“ läuft nicht mehr.", r.name), false);
        }
        let own_group = crate::ending::own_group();
        let group_alone = r.pgrp > 1 && r.pgrp != own_group && self.sys.snap.procs.iter().filter(|p| p.pgrp == r.pgrp).all(|p| r.pids.contains(&p.pid));
        let res = if group_alone {
            setprio(true, r.pgrp, v.nice()).map_err(|e| errno_words(&e))
        } else {
            r.pids.iter().map(|pid| renice(*pid, v)).fold(Ok(()), |a, x| a.and(x))
        };
        match res {
            Ok(()) => {
                eprintln!("bitscomp: system: Vorrang {} for {} (pid {}, {}){}", v.word(), r.name, r.main(), if group_alone { "group" } else { "threads" }, if real { ", Georg's own input" } else { "" });
                for p in self.sys.rows.iter_mut().filter(|x| x.main() == r.main()) {
                    p.nice = v.nice();
                }
                if rules::learns(r, real) {
                    let app = if r.app.is_empty() { r.comm.clone() } else { r.app.clone() };
                    // K2 as lane CX's ctxd reads it: `by` georg, since only Georg's own input reaches here.
                    self.habit_event("vorrang", vec![("app", mcpreg::Json::str(&app)), ("value", mcpreg::Json::str(v.word())), ("name", mcpreg::Json::str(&r.name)), ("by", mcpreg::Json::str("georg"))]);
                }
                self.page_say(page, format!("„{}“ läuft jetzt mit Vorrang {}.", r.name, v.name()), false);
            }
            Err(e) => self.page_say(page, format!("Vorrang von „{}“ nicht geändert: {e}.", r.name), true),
        }
    }

    /// The yes or no of a System page's question; an asked step needs Georg's own yes.
    pub(crate) fn sys_confirmed(&mut self, page: u32, yes: bool) {
        let Some(a) = self.sys.asking.remove(&page) else { return };
        self.sys.asked_at.remove(&page);
        let (pid, start) = match &a {
            Asked::End { pid, start, .. } | Asked::Prio { pid, start, .. } => (*pid, *start),
        };
        let Some(r) = self.sys_row(pid).filter(|r| r.start == start) else {
            return self.page_say(page, "Das Programm läuft nicht mehr: nichts geändert.", false);
        };
        if !yes {
            return self.page_say(page, format!("„{}“: nichts geändert (abgebrochen).", r.name), false);
        }
        let act = match &a {
            Asked::End { kill, .. } => if *kill { Act::Kill } else { Act::End },
            Asked::Prio { v, .. } => Act::Prio(*v),
        };
        let real = self.pages.real;
        if !rules::may(&self.sys_gate(&r, act), real) {
            eprintln!("bitscomp: system page {page}: the yes for {a:?} came by injected input: refused");
            return self.page_say(page, rules::REAL_INPUT, true);
        }
        match a {
            Asked::End { kill, .. } => self.sys_end_now(page, &r, kill),
            Asked::Prio { v, .. } => self.sys_prio_now(page, &r, v, real),
        }
    }

    fn sys_who(&mut self, page: u32) {
        let Some(r) = self.sys_detail_row(page) else { return };
        let name = r.argv.first().and_then(|a| a.rsplit('/').next()).filter(|n| !n.is_empty()).unwrap_or(&r.comm).to_string();
        let a = format!("{}/{name}", crate::werhatwas::ADDRESS);
        eprintln!("bitscomp: system: Wer hat was gemacht for {name}");
        self.open_address(&a, crate::address::Where::Tab, None);
    }

    fn sys_copy(&mut self, page: u32) {
        let text = crate::sys_report::text(&self.sys.snap, &self.sys.desk, crate::werhatwas_app::clock(self.status.fixed).1);
        self.sys_clip_set(text);
        self.page_say(page, "Der Bericht liegt in der Zwischenablage: in Claudes Eingabe einfügen (Strg+V).", false);
    }

    /// For `ctl system tab`: the page a command means (the focused System window, else the newest).
    pub(crate) fn sys_page_for_ctl(&self) -> Option<u32> {
        let pages = self.sys_pages();
        self.ws.focused().and_then(|w| self.page_pid(w)).filter(|p| pages.contains(p)).or_else(|| pages.last().copied())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn a_yes_counts_only_after_the_arm_time() {
        let at = Duration::from_secs(50);
        assert!(super::early(at, at), "the click that was already on its way");
        assert!(super::early(at, at + Duration::from_millis(900)));
        assert!(!super::early(at, at + crate::assist::ARM));
        assert!(!super::early(at, at + Duration::from_secs(3)));
    }
}

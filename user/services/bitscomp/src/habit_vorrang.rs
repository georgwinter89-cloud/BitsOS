//! VORRANG (K2) in bitscomp: a program starts with the priority Georg gave it before. ctxd learns it from the
//! task manager's `habit_event {kind:"vorrang"}`; at the program's first window bitscomp sets it on the program's
//! process group (all its threads): niedrig/normal imitated with the one-time hint and "Rückgängig" (back to
//! normal, ctxd told: not imitated again until deleted), hoch only asked (`habit_ask`) and set after Georg's own
//! "Ja". Never for BitsOS's own services, never for another user's process, never for bitscomp's own group.
//! Raising a priority (hoch, or back to normal) needs the session's RLIMIT_NICE (lane GM gives nice down to -5);
//! without it the kernel says EACCES and Georg reads why in a notice, once per program and run.

use mcpreg::Json;

use crate::state::App;
use crate::win::Window;

pub const NIEDRIG: i32 = 10;
pub const NORMAL: i32 = 0;
/// As far as the session's RLIMIT_NICE (25: nice -5) lets Georg's programs go.
pub const HOCH: i32 = -5;
/// BitsOS's own services by their program file (the programs init starts): never given a Vorrang here.
const OWN: [&str; 13] =
    ["init", "bitscomp", "ctxd", "settingsd", "bitsmcp", "bitsinstall", "bitsjournal", "bitspkgd", "bitspkg", "bitsaccess", "atspid", "bitssnap", "assist"];

pub fn nice_of(value: &str) -> Option<i32> {
    match value {
        "niedrig" => Some(NIEDRIG),
        "normal" => Some(NORMAL),
        "hoch" => Some(HOCH),
        _ => None,
    }
}

/// One learned Vorrang of ctxd's `habit_rules` (`ask`: hoch, not yet said yes to).
#[derive(Clone, Debug, PartialEq)]
pub struct VRule {
    pub id: String,
    pub app: String,
    pub value: String,
    pub ask: bool,
    pub hinted: bool,
    pub title: String,
    pub reason: String,
}

pub fn rules_of(r: &Json) -> Vec<VRule> {
    let s = |j: &Json, k: &str| j.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    r.get("rules")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter(|j| j.get("vorrang").and_then(Json::as_str).and_then(nice_of).is_some())
        .map(|j| VRule {
            id: s(j, "id"),
            app: s(j, "app"),
            value: s(j, "vorrang"),
            ask: j.get("ask").and_then(Json::as_bool).unwrap_or(true),
            hinted: j.get("hinted").and_then(Json::as_bool).unwrap_or(false),
            title: s(j, "title"),
            reason: s(j, "reason"),
        })
        .collect()
}

/// Where a priority goes: the program's process group (every thread of it), or only its own threads when the
/// group is not the program's alone (bitscomp's, another window's program's, Steam's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Group(u32),
    Process(u32),
}

impl Target {
    pub fn pid(self) -> u32 {
        match self {
            Target::Group(g) => g,
            Target::Process(p) => p,
        }
    }
}

/// The target for the program `pid` (None: never, e.g. init or bitscomp itself).
pub fn target(pid: u32, own_pid: u32, own_group: u32, pgid: Option<u32>, others: &[u32], leader: Option<&str>) -> Option<Target> {
    if pid <= 1 || pid == own_pid {
        return None;
    }
    Some(crate::ending::group_target(pgid, own_group, others, leader).map_or(Target::Process(pid), Target::Group))
}

/// Whether the program is BitsOS's own or not Georg's: a page of bitscomp, a service's program file, another uid.
pub fn not_georgs(app: &str, exe: Option<&str>, uid: Option<u32>, my_uid: u32) -> bool {
    let base = exe.map(|e| e.rsplit('/').next().unwrap_or(e));
    app.starts_with("bitscomp.") || base.is_some_and(|b| OWN.contains(&b)) || uid != Some(my_uid)
}

/// The real uid in a /proc/PID/status text.
pub fn status_uid(status: &str) -> Option<u32> {
    status.lines().find_map(|l| l.strip_prefix("Uid:"))?.split_whitespace().next()?.parse().ok()
}

/// The nice value (field 19) of a /proc/PID/stat line.
pub fn stat_nice(stat: &str) -> Option<i32> {
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(16)?.parse().ok()
}

fn nice_now(pid: u32) -> Option<i32> {
    stat_nice(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn prio(which: libc::c_int, who: u32, nice: i32) -> std::io::Result<()> {
    // SAFETY: setpriority only names a pid or group (> 1, checked by `target`); no memory is touched.
    if unsafe { libc::setpriority(which as _, who as _, nice) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Sets `nice` on every thread of the target (PRIO_PROCESS names one thread on Linux, so a process's threads
/// one by one; threads made later inherit it from their maker).
pub fn set(t: Target, nice: i32) -> std::io::Result<()> {
    match t {
        Target::Group(g) => prio(libc::PRIO_PGRP as _, g, nice),
        Target::Process(p) => {
            let tids: Vec<u32> = std::fs::read_dir(format!("/proc/{p}/task")).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect();
            if tids.is_empty() {
                return prio(libc::PRIO_PROCESS as _, p, nice);
            }
            let mut last = Ok(());
            for tid in tids {
                if let Err(e) = prio(libc::PRIO_PROCESS as _, tid, nice)
                    && e.raw_os_error() != Some(libc::ESRCH)
                {
                    last = Err(e);
                }
            }
            last
        }
    }
}

/// What Georg reads when the kernel refused (EACCES: raising needs the session's right to it).
pub fn refused(name: &str, nice: i32, e: &std::io::Error) -> String {
    match (e.raw_os_error() == Some(libc::EACCES), nice) {
        (true, HOCH) => format!("{name} läuft mit normalem Vorrang – hohen Vorrang darf BitsOS Programmen noch nicht geben."),
        (true, _) => format!("{name} bleibt bis zum nächsten Start bei niedrigem Vorrang – BitsOS darf den Vorrang noch nicht wieder auf normal setzen."),
        _ => format!("Den Vorrang von {name} konnte BitsOS nicht ändern."),
    }
}

impl App {
    /// The program `w` belongs to as a priority target (None: BitsOS's own, not Georg's, or unknown).
    pub fn vorrang_target(&self, w: &Window, app: &str) -> Option<Target> {
        let pid = u32::try_from(self.pid_of(w)).ok().filter(|p| *p > 1)?;
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.to_string_lossy().into_owned());
        let uid = std::fs::read_to_string(format!("/proc/{pid}/status")).ok().as_deref().and_then(status_uid);
        // SAFETY: getuid has no arguments and cannot fail.
        if not_georgs(app, exe.as_deref(), uid, unsafe { libc::getuid() }) {
            eprintln!("bitscomp: habits: no Vorrang for {app} (pid {pid}): a service of BitsOS or not Georg's");
            return None;
        }
        let pgid = crate::ending::group_read(pid);
        let others: Vec<u32> = self
            .all_windows()
            .iter()
            .filter(|o| *o != w && crate::ctl::names(o).0 != app)
            .filter_map(|o| u32::try_from(self.pid_of(o)).ok().filter(|p| *p > 1).and_then(crate::ending::group_read))
            .collect();
        let leader = pgid.and_then(crate::ending::leader_comm);
        target(pid, std::process::id(), crate::ending::own_group(), pgid, &others, leader.as_deref())
    }

    /// Sets `value` on the target; Ok(Some(old nice)) when it changed something, Ok(None) when it already was so.
    /// A refusal is told to Georg once per program and run.
    pub fn vorrang_set(&mut self, t: Target, value: &str, name: &str) -> Result<Option<i32>, ()> {
        let nice = nice_of(value).ok_or(())?;
        let before = nice_now(t.pid());
        if before == Some(nice) {
            return Ok(None);
        }
        match set(t, nice) {
            Ok(()) => {
                eprintln!("bitscomp: habits: {name} ({t:?}) runs with Vorrang {value} (nice {nice}, was {before:?})");
                Ok(Some(before.unwrap_or(NORMAL)))
            }
            Err(e) => {
                eprintln!("bitscomp: habits: Vorrang {value} for {name} ({t:?}) refused: {e}");
                if self.habit.vorrang_said.insert(format!("{name}:{value}")) {
                    self.notice(refused(name, nice, &e));
                }
                Err(())
            }
        }
    }

    /// The program's first window (`habit_initial`): its learned Vorrang, set or asked.
    pub fn vorrang_start(&mut self, w: &Window, app: &str, name: &str) {
        let Some(rule) = self.habit.vorrang.iter().find(|r| r.app == app).cloned() else { return };
        let Some(t) = self.vorrang_target(w, app) else { return };
        if rule.ask {
            return self.vorrang_ask(w, &rule, t, name);
        }
        if let Ok(Some(_)) = self.vorrang_set(t, &rule.value, name)
            && !rule.hinted
            && rule.value != "hoch"
        {
            let r = crate::habit_app::Rule { id: rule.id.clone(), app: rule.app.clone(), title: rule.title.clone(), reason: rule.reason.clone(), ..Default::default() };
            self.habit_hint_offer(w, r, crate::habit_hint::Undo::Vorrang(t));
        }
    }

    /// "Rückgängig" on a Vorrang hint: back to normal (ctxd is told by the hint, the rule dropped there).
    pub fn vorrang_undo(&mut self, t: Target, id: &str) {
        let app = id.split_once(':').map_or(id, |(_, a)| a).to_string();
        let name = self.habit_name(&app);
        let _ = self.vorrang_set(t, "normal", &name);
    }

    /// `ctl habits vorrang APP VALUE`: the task manager's event until it is merged. From the control socket it is
    /// marked `by:"ctl"` and teaches nothing.
    pub fn vorrang_ctl(&mut self, app: &str, value: &str) -> Result<String, String> {
        if nice_of(value).is_none() {
            return Err("habits vorrang: niedrig, normal or hoch".into());
        }
        let by = if self.ctl_origin == crate::input::Origin::Physical { "georg" } else { "ctl" };
        let name = self.habit_name(app);
        self.habit_event("vorrang", vec![("app", Json::str(app)), ("value", Json::str(value)), ("name", Json::str(&name)), ("by", Json::str(by))]);
        Ok(format!("ok by={by}"))
    }

    pub fn vorrang_report(&self) -> String {
        // The notice line shows what Georg reads after a refused Vorrang (or another notice now on screen).
        let mut out = self.desk.notice.as_ref().map_or_else(String::new, |(n, _)| format!("notice {n:?}\n"));
        for r in &self.habit.vorrang {
            out.push_str(&format!("vorrang id={} app={} value={} ask={} hinted={}\n", r.id, r.app, r.value, r.ask, r.hinted));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, app: &str) -> VRule {
        VRule { id: id.into(), app: app.into(), value: "niedrig".into(), ask: false, hinted: false, title: String::new(), reason: String::new() }
    }

    #[test]
    fn rueckgaengig_drops_the_vorrang_at_once_and_a_shown_hint_counts_at_once() {
        let mut h = crate::habit_app::Habit::default();
        h.vorrang = vec![rule("vorrang:habtest", "habtest"), rule("vorrang:gimp", "gimp")];
        h.hinted("vorrang:habtest");
        assert!(h.vorrang[0].hinted && !h.vorrang[1].hinted, "a restart before ctxd's next answer shows no second hint");
        h.undone("vorrang:habtest");
        assert!(h.vorrang.iter().all(|r| r.app != "habtest"), "a restart before ctxd's next answer imitates nothing");
        assert_eq!(h.vorrang.len(), 1);
    }

    #[test]
    fn values_rules_and_what_proc_says() {
        assert_eq!((nice_of("niedrig"), nice_of("normal"), nice_of("hoch"), nice_of("Hoch")), (Some(10), Some(0), Some(-5), None));
        let r = Json::parse(r#"{"rules":[{"id":"fenster:a","app":"a","state":"maximised"},{"id":"vorrang:b","app":"b","vorrang":"hoch","ask":true,"title":"T","reason":"R"},{"id":"vorrang:c","app":"c","vorrang":"sehr"}]}"#).unwrap();
        let v = rules_of(&r);
        assert_eq!(v.len(), 1, "only Vorrang rules with a known value");
        assert_eq!((v[0].app.as_str(), v[0].value.as_str(), v[0].ask), ("b", "hoch", true));
        assert_eq!(status_uid("Name:\tx\nUid:\t1000\t1000\t1000\t1000\n"), Some(1000));
        let stat = "42 (a prog) S 1 42 42 0 -1 4194304 1 0 0 0 0 0 0 0 20 10 1 0 100 0 0";
        assert_eq!(stat_nice(stat), Some(10));
    }

    #[test]
    fn never_bitsos_own_never_another_user_never_bitscomp_s_group() {
        assert!(not_georgs("bitscomp.einstellungen", Some("/boot/BITSOS2/bin/bitscomp"), Some(1000), 1000));
        assert!(not_georgs("x", Some("/bin/ctxd"), Some(1000), 1000), "a service's program file");
        assert!(not_georgs("blender", Some("/usr/bin/blender"), Some(0), 1000), "another user's process");
        assert!(not_georgs("blender", Some("/usr/bin/blender"), None, 1000), "unknown owner: never");
        assert!(!not_georgs("blender", Some("/usr/bin/blender"), Some(1000), 1000));
        assert_eq!(target(1, 50, 50, Some(1), &[], Some("init")), None);
        assert_eq!(target(50, 50, 50, Some(50), &[], Some("bitscomp")), None, "bitscomp itself");
        assert_eq!(target(70, 50, 50, Some(70), &[], Some("blender")), Some(Target::Group(70)));
        assert_eq!(target(70, 50, 50, Some(50), &[], Some("bitscomp")), Some(Target::Process(70)), "in bitscomp's group: only its own threads");
        assert_eq!(target(71, 50, 50, Some(60), &[60], Some("steam")), Some(Target::Process(71)), "a group another program shares");
        assert_eq!(target(72, 50, 50, None, &[], None), Some(Target::Process(72)));
    }

    #[test]
    fn a_refusal_says_why_in_plain_german() {
        let acc = std::io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(refused("Blender", HOCH, &acc), "Blender läuft mit normalem Vorrang – hohen Vorrang darf BitsOS Programmen noch nicht geben.");
        assert_eq!(refused("Blender", NORMAL, &acc), "Blender bleibt bis zum nächsten Start bei niedrigem Vorrang – BitsOS darf den Vorrang noch nicht wieder auf normal setzen.");
        assert!(refused("Blender", NIEDRIG, &std::io::Error::from_raw_os_error(libc::EPERM)).starts_with("Den Vorrang von Blender"));
    }

    #[test]
    fn a_child_of_the_test_gets_its_vorrang_on_every_thread() {
        let mut c = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = c.id();
        set(Target::Process(pid), NIEDRIG).unwrap();
        assert_eq!(nice_now(pid), Some(NIEDRIG));
        let _ = c.kill();
        let _ = c.wait();
    }
}

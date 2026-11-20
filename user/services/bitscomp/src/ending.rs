//! Ending a window's program (WY-4): the "Beenden" button of a hung window, "Programm beenden" in the circle menu,
//! `ctl kill` and the MCP tool `window_kill`. SIGTERM and SIGCONT to the client's pid (SO_PEERCRED at accept),
//! SIGKILL 2 s later if the same process is still there, and then the Wayland client is dropped, so the window
//! goes even if the process lingers in the kernel. "The same process": its start time in /proc/PID/stat, read at
//! accept, must still match before every signal, so a pid that was reused since the client connected is never
//! hit. SIGCONT because a stopped process keeps SIGTERM pending until it runs again (linux-7.3.1/kernel/signal.c:877
//! prepare_signal: only SIGKILL and SIGCONT act on a stopped task). Only that one pid: like other Linux
//! desktops, a program's own children are its business. Never waits (B4): the SIGKILL is a deadline the main
//! loop checks.

use std::time::Duration;

use crate::win::Window;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::backend::DisconnectReason;

use crate::ClientState;
use crate::state::App;

pub const KILL_AFTER: Duration = Duration::from_secs(2);

pub struct Ending {
    pub pid: i32,
    /// Field 22 of /proc/PID/stat at accept (`None` without procfs).
    start: Option<u64>,
    pub due: Duration,
    client: Client,
    what: String,
}

/// Never pid 0 (unknown), 1 (init) or bitscomp itself (kill(2) with pid <= 0 means groups,
/// linux-7.3.1/kernel/signal.c:1615 kill_something_info).
pub fn may_signal(pid: i32, own: i32) -> bool {
    pid > 1 && pid != own
}

/// The start time (clock ticks since boot) from a /proc/PID/stat line; the command may contain spaces and ')'.
pub fn parse_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After the command: state is field 3, so starttime (field 22) is the 20th word.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The state letter (field 3) from a /proc/PID/stat line ('Z': a zombie, already ended).
pub fn parse_state(stat: &str) -> Option<char> {
    stat[stat.rfind(')')? + 1..].split_whitespace().next()?.chars().next()
}

fn stat_of(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()
}

/// WY-5 (session): the argv of `pid` from /proc/PID/cmdline, `None` without one.
pub fn cmdline(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    split_cmdline(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?)
}

/// Every argument ends with a NUL (linux-7.3.1/fs/proc/base.c:283 get_mm_cmdline): only that last terminator is
/// dropped, so an empty argument inside (`foot -T ""`) stays.
pub fn split_cmdline(raw: &[u8]) -> Option<Vec<String>> {
    let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
    if raw.is_empty() {
        return None;
    }
    Some(raw.split(|&b| b == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
}

pub fn start_time(pid: i32) -> Option<u64> {
    parse_start_time(&stat_of(pid)?)
}

/// Whether `pid` is still the process that connected: same start time, or no procfs to tell (then the open
/// socket is the only hint and the caller decides).
fn still_same(pid: i32, at_accept: Option<u64>) -> Option<bool> {
    at_accept.map(|s| start_time(pid) == Some(s))
}

fn signal(pid: i32, sig: libc::c_int) -> std::io::Result<()> {
    // SAFETY: kill(2) on a single positive pid checked by `may_signal`; no memory is touched.
    if unsafe { libc::kill(pid, sig) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// GM-6c: bitscomp's own process group (a group to signal must never be it).
pub fn own_group() -> u32 {
    // SAFETY: getpgrp has no arguments and cannot fail.
    unsafe { libc::getpgrp() as u32 }
}

/// GM-6c: whether Spiel beenden may SIGTERM the window's process group `pgid`: never 0, 1 or bitscomp's own,
/// never a group another window's program is in (`others`), never the Steam client's (`leader` is the group
/// leader's /proc comm; a game Steam started may share Steam's group, and Steam must live on).
pub fn group_ok(pgid: u32, own: u32, others: &[u32], leader: &str) -> bool {
    pgid > 1 && pgid != own && !others.contains(&pgid) && !matches!(leader.trim(), "steam" | "steam.sh" | "steamwebhelper")
}

/// The process group of `pid`, None when it cannot be read (ended, reaped, foreign): never a guess.
pub fn group_read(pid: u32) -> Option<u32> {
    let p = libc::pid_t::try_from(pid).ok().filter(|p| *p > 0)?;
    // SAFETY: getpgid only reads the pid; an ended or foreign pid answers -1 (ESRCH).
    u32::try_from(unsafe { libc::getpgid(p) }).ok()
}

/// The group leader's /proc comm, None when unreadable or empty (the Steam check must not pass on nothing).
pub fn leader_comm(pgid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pgid}/comm")).ok().map(|c| c.trim().to_string()).filter(|c| !c.is_empty())
}

/// Spiel beenden's group to SIGTERM, read before the window's program is signalled: none when the group or its
/// leader's name cannot be read (fails closed), else as `group_ok` says.
pub fn group_target(pgid: Option<u32>, own: u32, others: &[u32], leader: Option<&str>) -> Option<u32> {
    let (g, l) = (pgid?, leader?);
    group_ok(g, own, others, l).then_some(g)
}

/// SIGTERM (or `sig`) to process group `pgid` (kill(2) with -pgid), after `group_ok`.
pub fn signal_group(pgid: u32, sig: libc::c_int) -> std::io::Result<()> {
    let g = libc::pid_t::try_from(pgid).ok().filter(|g| *g > 1).ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: kill(2) on the negative of a group id > 1 checked above (never 0 or -1, which mean "all"); no
    // memory is touched.
    if unsafe { libc::kill(-g, sig) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

impl App {
    pub fn client_of(&self, w: &Window) -> Option<Client> {
        smithay::reexports::wayland_server::Resource::client(w.toplevel()?.wl_surface())
    }

    /// The pid of the window's program (0 when unknown).
    pub fn pid_of(&self, w: &Window) -> i32 {
        // WY-3: Xwayland is one client for every X11 program; the program's own pid comes from the X server.
        if let Some(x) = w.x11() {
            return x.pid.get() as i32;
        }
        self.client_of(w).and_then(|c| c.get_data::<ClientState>().map(|d| d.pid)).unwrap_or(0)
    }

    /// LO-1: the process group of `pid`: a program a launcher started (LibreOffice's soffice.bin under its sh
    /// launcher) is in the group `spawn` made for the launcher. `pid` itself when it has none to read.
    pub fn group_of(pid: u32) -> u32 {
        // SAFETY: getpgid only reads the pid; an ended or foreign pid answers -1 (ESRCH).
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        u32::try_from(pgid).unwrap_or(pid)
    }

    /// "window N (app_id) of pid P", for log lines and answers.
    pub fn describe(&self, w: &Window) -> String {
        format!("window {} ({}) of pid {}", crate::ctl::win_id(w), crate::ctl::names(w).0, self.pid_of(w))
    }

    /// Ends the program of `w`; the answer says what was done.
    pub fn end_window(&mut self, w: &Window, why: &str) -> Result<String, String> {
        // WY-3: never Xwayland itself: the X server drops the program's connection (KillClient).
        if let Some(x) = w.x11() {
            x.want_kill.set(true);
            let what = self.describe(w);
            eprintln!("bitscomp: {what}: its X connection is closed (KillClient, {why})");
            return Ok(format!("{what}: X client killed"));
        }
        let client = self.client_of(w).ok_or("the window has no client any more")?;
        let pid = self.pid_of(w);
        let what = self.describe(w);
        if self.endings.iter().any(|e| e.client.id() == client.id()) {
            return Ok(format!("{what}: already ending"));
        }
        if !may_signal(pid, std::process::id() as i32) {
            // No pid to signal: dropping the client is all bitscomp can do.
            self.drop_client(&client);
            eprintln!("bitscomp: {what}: no pid to signal, client dropped ({why})");
            return Ok(format!("{what}: no pid to signal; client dropped"));
        }
        let start = client.get_data::<ClientState>().and_then(|d| d.start);
        if still_same(pid, start) == Some(false) {
            // The process that connected is gone and its pid is free or someone else's: signal nothing.
            self.drop_client(&client);
            eprintln!("bitscomp: {what}: pid {pid} is no longer the program that connected; not signalled, client dropped ({why})");
            return Ok(format!("{what}: the program is gone (pid {pid} not signalled); client dropped"));
        }
        if stat_of(pid).as_deref().and_then(parse_state) == Some('Z') {
            self.drop_client(&client);
            eprintln!("bitscomp: {what}: already ended (zombie), client dropped ({why})");
            return Ok(format!("{what}: the program has already ended; client dropped"));
        }
        // PE-1: an end the desktop asked for is no program error.
        self.prog_asked(pid as u32);
        if let Err(e) = signal(pid, libc::SIGTERM) {
            // ESRCH: the process is gone already; its socket closes by itself.
            self.drop_client(&client);
            eprintln!("bitscomp: {what}: SIGTERM failed ({e}), client dropped ({why})");
            return Ok(format!("{what}: SIGTERM failed ({e}); client dropped"));
        }
        // A stopped program acts on the SIGTERM only once it runs; for a running one SIGCONT changes nothing.
        let _ = signal(pid, libc::SIGCONT);
        eprintln!("bitscomp: {what}: SIGTERM ({why}), SIGKILL in {} s if still there", KILL_AFTER.as_secs());
        let due = crate::pacer::now() + KILL_AFTER;
        self.endings.push(Ending { pid, start, due, client, what: what.clone() });
        Ok(format!("{what}: SIGTERM sent; SIGKILL in {} s if it is still there", KILL_AFTER.as_secs()))
    }

    pub fn drop_client(&mut self, client: &Client) {
        if let Some(cs) = client.get_data::<ClientState>() {
            cs.ended_by_us.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.dh.backend_handle().kill_client(client.id(), DisconnectReason::ConnectionClosed);
        self.layout_dirty = true;
    }

    /// The SIGKILL deadlines that passed by `now`.
    pub fn ending_tick(&mut self, now: Duration) {
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.endings).into_iter().partition(|e| now >= e.due);
        self.endings = later;
        for e in due {
            let connected = self.dh.backend_handle().get_client_data(e.client.id()).is_ok();
            let same = still_same(e.pid, e.start).unwrap_or(connected);
            if same {
                match signal(e.pid, libc::SIGKILL) {
                    Ok(()) => eprintln!("bitscomp: {}: still there {} s after SIGTERM: SIGKILL", e.what, KILL_AFTER.as_secs()),
                    Err(err) => eprintln!("bitscomp: {}: SIGKILL failed: {err}", e.what),
                }
            }
            if connected {
                self.drop_client(&e.client);
            }
        }
    }

    pub fn ending_due(&self) -> Option<Duration> {
        self.endings.iter().map(|e| e.due).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdline_keeps_empty_arguments() {
        let v = |x: &[&str]| Some(x.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(split_cmdline(b"foot\0-T\0\0x\0"), v(&["foot", "-T", "", "x"]));
        assert_eq!(split_cmdline(b"foot\0"), v(&["foot"]));
        assert_eq!(split_cmdline(b"rewritten title"), v(&["rewritten title"]));
        assert_eq!(split_cmdline(b""), None);
        assert_eq!(split_cmdline(b"\0"), None);
    }

    #[test]
    fn spiel_beenden_never_signals_a_shared_or_steams_group() {
        assert!(group_ok(4242, 100, &[], "wyclient"));
        assert!(!group_ok(0, 100, &[], "x"));
        assert!(!group_ok(1, 100, &[], "init"));
        assert!(!group_ok(100, 100, &[], "bitscomp"));
        assert!(!group_ok(4242, 100, &[7, 4242], "sh"), "another window's program is in it");
        assert!(!group_ok(4242, 100, &[], "steam\n"), "Steam's group");
        assert!(signal_group(1, 0).is_err() && signal_group(0, 0).is_err());
        assert_eq!(group_target(Some(4242), 100, &[], Some("wyclient")), Some(4242));
        assert_eq!(group_target(None, 100, &[], Some("wyclient")), None, "getpgid failed: no group");
        assert_eq!(group_target(Some(4242), 100, &[], None), None, "unreadable leader: not signalled");
        assert_eq!(group_target(Some(4242), 100, &[4242], Some("wyclient")), None);
        assert_eq!(group_read(0), None);
    }

    #[test]
    fn spiel_beenden_terms_a_programs_own_group() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        // A program in a group of its own (as `spawn` starts launchers): read, chosen, SIGTERMed as a group.
        let mut c = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let pid = c.id();
        let g = group_read(pid);
        assert_eq!(g, Some(pid));
        // Between fork and exec the child still carries this test binary's name: wait for the exec.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut comm = leader_comm(pid);
        while comm.as_deref() != Some("sleep") && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            comm = leader_comm(pid);
        }
        assert_eq!(comm.as_deref(), Some("sleep"));
        let t = group_target(g, own_group(), &[], comm.as_deref());
        assert_eq!(t, Some(pid));
        signal_group(pid, libc::SIGTERM).unwrap();
        assert_eq!(c.wait().unwrap().signal(), Some(libc::SIGTERM));
        assert_eq!(group_read(pid), None, "reaped: no group to read");
        assert_eq!(group_target(Some(own_group()), own_group(), &[], Some("bitscomp")), None, "never bitscomp's own");
    }

    #[test]
    fn never_signals_unknown_init_or_itself() {
        assert!(!may_signal(0, 500));
        assert!(!may_signal(1, 500));
        assert!(!may_signal(-7, 500));
        assert!(!may_signal(500, 500));
        assert!(may_signal(501, 500));
    }

    #[test]
    fn start_time_survives_spaces_and_parens_in_the_name() {
        let stat = "4242 (my (odd) prog) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 3 0 987654 1000 10";
        assert_eq!(parse_start_time(stat), Some(987654));
        assert_eq!(parse_start_time("garbage"), None);
        assert_eq!(parse_start_time("1 (x) S 1 2"), None);
    }

    #[test]
    fn state_letter_after_the_name() {
        assert_eq!(parse_state("77 (a) b) Z 1 77"), Some('Z'));
        assert_eq!(parse_state("77 (sh) S 1"), Some('S'));
        assert_eq!(parse_state("77 (sh)"), None);
        assert_eq!(parse_state(""), None);
    }

    #[test]
    fn own_start_time_is_readable_and_stable() {
        let me = std::process::id() as i32;
        let s = start_time(me);
        assert!(s.is_some());
        assert_eq!(still_same(me, s), Some(true));
        assert_eq!(still_same(me, s.map(|v| v + 1)), Some(false));
        assert_eq!(still_same(me, None), None);
    }
}

//! Ending a window's program (WY-4): the "Beenden" button of a hung window, "Programm beenden" in the circle menu,
//! `ctl kill` and the MCP tool `window_kill`. SIGTERM and SIGCONT to the client's pid (SO_PEERCRED at accept),
//! SIGKILL 2 s later if the same process is still there, and then the Wayland client is dropped, so the window
//! goes even if the process lingers in the kernel. "The same process": its start time in /proc/PID/stat, read at
//! accept, must still match before every signal, so a pid that was reused since the client connected is never
//! hit. SIGCONT because a stopped process keeps SIGTERM pending until it runs again (linux-6.6/kernel/signal.c
//! prepare_signal: only SIGKILL and SIGCONT act on a stopped task). Only that one pid: like other Linux
//! desktops, a program's own children are its business. Never waits (B4): the SIGKILL is a deadline the main
//! loop checks.

use std::time::Duration;

use smithay::desktop::Window;
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

/// Never pid 0 (unknown), 1 (init) or bitscomp itself (kill(2) with pid <= 0 means groups, linux-6.6/kernel/signal.c
/// kill_something_info).
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

/// Every argument ends with a NUL (linux-6.6/fs/proc/base.c:256 get_mm_cmdline): only that last terminator is dropped,
/// so an empty argument inside (`foot -T ""`) stays.
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

impl App {
    pub fn client_of(&self, w: &Window) -> Option<Client> {
        smithay::reexports::wayland_server::Resource::client(w.toplevel()?.wl_surface())
    }

    /// The pid of the window's program (0 when unknown).
    pub fn pid_of(&self, w: &Window) -> i32 {
        self.client_of(w).and_then(|c| c.get_data::<ClientState>().map(|d| d.pid)).unwrap_or(0)
    }

    /// "window N (app_id) of pid P", for log lines and answers.
    pub fn describe(&self, w: &Window) -> String {
        format!("window {} ({}) of pid {}", crate::ctl::win_id(w), crate::ctl::names(w).0, self.pid_of(w))
    }

    /// Ends the program of `w`; the answer says what was done.
    pub fn end_window(&mut self, w: &Window, why: &str) -> Result<String, String> {
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

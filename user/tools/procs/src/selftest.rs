//! `procs --selftest`: /proc against what this process knows, and a child started and killed. The printed
//! lines are the same on Linux and V2 (no pids, sizes or times), so CachyOS's transcript is the expected one.

use crate::proc::{key_numbers, list_pids, meminfo, parse_stat, read_proc};
use std::time::{Duration, Instant};

struct Checks {
    ok: u32,
    failed: u32,
}

impl Checks {
    fn check(&mut self, what: &str, r: Result<(), String>) {
        match r {
            Ok(()) => {
                self.ok += 1;
                println!("ok   {what}");
            }
            Err(e) => {
                self.failed += 1;
                println!("FAIL {what}: {e}");
            }
        }
    }
}

fn want<T: PartialEq + std::fmt::Debug>(what: &str, got: T, expected: T) -> Result<(), String> {
    if got == expected { Ok(()) } else { Err(format!("{what} {got:?}, want {expected:?}")) }
}

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

fn self_checks(c: &mut Checks) {
    let me = std::process::id();
    c.check(
        "/proc/self names this process",
        std::fs::read_link("/proc/self").map_err(|e| format!("readlink /proc/self: {e}")).and_then(|l| want("link", l.to_string_lossy().into_owned(), me.to_string())),
    );
    c.check(
        "stat: pid, comm, state R, ppid",
        read("/proc/self/stat").and_then(|s| {
            let pid: u32 = s.split_whitespace().next().and_then(|p| p.parse().ok()).ok_or("no pid")?;
            want("pid", pid, me)?;
            let (comm, f) = parse_stat(&s).ok_or("no (comm)")?;
            want("comm", comm.as_str(), "procs")?;
            want("state", f.first().copied(), Some("R"))?;
            want("ppid", f.get(1).and_then(|p| p.parse::<usize>().ok()), usys::getppid().ok())
        }),
    );
    c.check(
        "status: Name, Pid, PPid, Uid",
        read("/proc/self/status").and_then(|s| {
            want("Name", s.lines().find_map(|l| l.strip_prefix("Name:")).map(str::trim), Some("procs"))?;
            let m = key_numbers(&s);
            want("Pid", m.get("Pid").copied(), Some(me as u64))?;
            want("PPid", m.get("PPid").map(|v| *v as usize), usys::getppid().ok())?;
            want("Uid", m.get("Uid").map(|v| *v as usize), usys::getuid().ok())
        }),
    );
    c.check(
        "cmdline: argv",
        std::fs::read("/proc/self/cmdline").map_err(|e| e.to_string()).and_then(|b| {
            let got: Vec<String> = b.split(|&c| c == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
            want("argv", got, std::env::args().collect())
        }),
    );
    c.check("meminfo: MemTotal >= MemAvailable > 0", {
        let m = meminfo();
        match (m.get("MemTotal"), m.get("MemAvailable")) {
            (Some(t), Some(a)) if *t >= *a && *a > 0 => Ok(()),
            other => Err(format!("MemTotal/MemAvailable {other:?}")),
        }
    });
    c.check(
        "uptime > 0",
        read("/proc/uptime").and_then(|s| match s.split_whitespace().next().and_then(|u| u.parse::<f64>().ok()) {
            Some(u) if u > 0.0 => Ok(()),
            other => Err(format!("uptime {other:?}")),
        }),
    );
    c.check(
        "listing has this process",
        list_pids().map_err(|e| e.to_string()).and_then(|p| if p.contains(&me) { Ok(()) } else { Err(format!("{me} not in {p:?}")) }),
    );
}

fn child_checks(c: &mut Checks) {
    let me = std::process::id();
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return c.check("child", Err(format!("current_exe: {e}"))),
    };
    let mut child = match std::process::Command::new(&exe).args(["--idle", "30"]).spawn() {
        Ok(ch) => ch,
        Err(e) => return c.check("child", Err(format!("spawn: {e}"))),
    };
    let pid = child.id();
    let want_argv = vec![exe.to_string_lossy().into_owned(), "--idle".into(), "30".into()];
    // The child shows the parent's argv until its exec is done.
    let end = Instant::now() + Duration::from_secs(10);
    let mut seen = None;
    while Instant::now() < end {
        if let Some(p) = read_proc(pid) {
            if p.argv == want_argv {
                seen = Some(p);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    c.check(
        "child listed: ppid, comm, cmdline",
        match &seen {
            Some(p) => want("ppid", p.ppid, Some(me)).and_then(|_| want("comm", p.comm.as_str(), "procs")),
            None => Err(format!("pid {pid} never showed argv {want_argv:?}")),
        },
    );
    let _ = child.kill();
    let st = child.wait();
    c.check(
        "child gone after kill and wait",
        match st {
            Ok(_) if std::fs::metadata(format!("/proc/{pid}")).is_err() && !list_pids().unwrap_or_default().contains(&pid) => Ok(()),
            Ok(_) => Err(format!("/proc/{pid} still there")),
            Err(e) => Err(format!("wait: {e}")),
        },
    );
}

pub fn run() -> i32 {
    let mut c = Checks { ok: 0, failed: 0 };
    self_checks(&mut c);
    child_checks(&mut c);
    println!("selftest: {} ok, {} failed", c.ok, c.failed);
    if c.failed == 0 { 0 } else { 1 }
}

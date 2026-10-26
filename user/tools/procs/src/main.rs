//! procs: a ps/top-like process listing from /proc (item UL-4b). Named `procs` because `/bin/ps` and
//! `/bin/top` are busybox applets (UL-3a) whose transcripts the busybox tests compare.
//!
//! - `procs [-p PID]...`: one line per process: PID PPID UID S THR RSS VSZ TIME CMD.
//! - `procs --top [-n N] [-d SECS]`: N frames (default 3), each after SECS (default 1): uptime, load, memory,
//!   then the processes sorted by CPU use over the frame.
//! - `procs --probe`: the /proc and /sys files libcobalt reads (Y-NUMBERS 1.4), each `ok` or its errno.
//! - `procs --selftest`: checks /proc against what this process knows (pid, ppid, uid, argv) and a child it
//!   starts and kills; prints the same lines on Linux and V2 (`ok ...`, the summary), exit 0 when all pass.
//!
//! Every file may be missing (V2's procfs is partial until AB-7): a missing value prints `-`, a process that
//! ends between the listing and the read is skipped. Field layout: linux-6.6/fs/proc/array.c `do_task_stat`
//! (:467; comm in parentheses, may hold spaces and ')': parsed up to the last ')'), `proc_pid_status`; TIME is
//! utime+stime in USER_HZ (100) ticks.
//! Exit codes: 0 ok, 1 a selftest check failed or /proc unreadable, 2 usage.
#![forbid(unsafe_code)]

mod proc;
mod selftest;

use proc::{Proc, list_pids, read_proc};
use std::process::exit;

const USAGE: &str = "usage: procs [-p PID]... | --top [-n N] [-d SECS] | --probe | --selftest\n";

fn usage(msg: &str) -> ! {
    eprint!("procs: {msg}\n{USAGE}");
    exit(2)
}

fn help() -> ! {
    print!(
        "{USAGE}\n  (no option)  one line per process: PID PPID UID S THR RSS(KiB) VSZ(KiB) TIME CMD\n\
         \x20 -p PID       only these processes\n\
         \x20 --top        N frames (-n, default 3) every SECS (-d, default 1), sorted by CPU use\n\
         \x20 --probe      the /proc and /sys files libcobalt reads: ok or errno\n\
         \x20 --selftest   check /proc against this process and a child; exit 0 when all pass\n\
         Missing /proc files print '-'. exit codes: 0 ok, 1 failed check or no /proc, 2 usage\n"
    );
    exit(0)
}

fn time_text(ticks: Option<u64>) -> String {
    match ticks {
        Some(t) => {
            let s = t / 100;
            format!("{}:{:02}", s / 60, s % 60)
        }
        None => "-".into(),
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

fn row(p: &Proc, cpu: Option<f64>) -> String {
    let cpu = match cpu {
        Some(c) => format!("{c:5.1} "),
        None => String::new(),
    };
    format!(
        "{:>7} {:>7} {:>5} {} {:>3} {:>8} {:>9} {}{:>7} {}",
        p.pid,
        opt(p.ppid),
        opt(p.uid),
        p.state,
        opt(p.threads),
        opt(p.rss_kb),
        opt(p.vsz_kb),
        cpu,
        time_text(p.ticks),
        p.cmd()
    )
}

fn header(cpu: bool) -> String {
    format!(
        "{:>7} {:>7} {:>5} S {:>3} {:>8} {:>9} {}{:>7} CMD",
        "PID",
        "PPID",
        "UID",
        "THR",
        "RSS",
        "VSZ",
        if cpu { " %CPU " } else { "" },
        "TIME"
    )
}

fn snapshot(only: &[u32]) -> Vec<Proc> {
    let pids = match list_pids() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("procs: /proc: {e}");
            exit(1)
        }
    };
    pids.into_iter().filter(|p| only.is_empty() || only.contains(p)).filter_map(read_proc).collect()
}

/// Like procps' ps: with `-p`, exit 1 when none of the named processes exists.
fn listing(only: &[u32]) -> i32 {
    println!("{}", header(false));
    let procs = snapshot(only);
    for p in &procs {
        println!("{}", row(p, None));
    }
    if !only.is_empty() && procs.is_empty() { 1 } else { 0 }
}

fn first_field(path: &str) -> Option<f64> {
    std::fs::read_to_string(path).ok()?.split_whitespace().next()?.parse().ok()
}

fn top(frames: u32, delay: std::time::Duration) {
    let hz = 100.0;
    // Lines cut at the terminal width, 120 columns without a terminal (a short frame reads better in MCP's answer).
    let width = usys::ioctl_winsize(1).map(|w| w[1] as usize).ok().filter(|w| *w > 0).unwrap_or(120);
    let mut before: Vec<Proc> = snapshot(&[]);
    let mut t0 = usys::mono_ns();
    for f in 0..frames {
        std::thread::sleep(delay);
        let now = snapshot(&[]);
        let t1 = usys::mono_ns();
        let dt = (t1.saturating_sub(t0)) as f64 / 1e9;
        let si = usys::sysinfo().ok();
        let up = first_field("/proc/uptime").or(si.as_ref().map(|s| s.uptime as f64));
        let load = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
            .or(si.as_ref().map(|s| s.loads.iter().map(|l| format!("{:.2}", *l as f64 / 65536.0)).collect::<Vec<_>>().join(" ")));
        let mem = proc::meminfo();
        let mib = |k: &str| mem.get(k).map(|v| format!("{} MiB", v / 1024)).unwrap_or_else(|| "-".into());
        if f > 0 {
            println!();
        }
        let upt = up.map(|u| {
            let u = u as u64;
            format!("{}:{:02}:{:02}", u / 3600, u / 60 % 60, u % 60)
        });
        let running = now.iter().filter(|p| p.state == 'R').count();
        println!("procs - up {}, load {}, {} processes ({} running)", opt(upt), opt(load), now.len(), running);
        println!("Mem: {} total, {} available, {} shmem", mib("MemTotal"), mib("MemAvailable"), mib("Shmem"));
        let mut rows: Vec<(f64, &Proc)> = now
            .iter()
            .map(|p| {
                let prev = before.iter().find(|b| b.pid == p.pid).and_then(|b| b.ticks).unwrap_or(0);
                let d = p.ticks.unwrap_or(0).saturating_sub(prev) as f64;
                (if dt > 0.0 { d / hz / dt * 100.0 } else { 0.0 }, p)
            })
            .collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.pid.cmp(&b.1.pid)));
        println!("{}", header(true));
        for (c, p) in rows {
            println!("{}", row(p, Some(c)).chars().take(width).collect::<String>());
        }
        before = now;
        t0 = t1;
    }
}

/// libcobalt's reads (Y-NUMBERS 1.4): each file ok or the errno; 2147483647 is its "no such pid" probe.
fn probe() {
    let me = std::process::id();
    let files = [
        "/proc/self/stat".to_string(),
        "/proc/self/cmdline".into(),
        "/proc/self/status".into(),
        "/proc/self/statm".into(),
        "/proc/self/smaps_rollup".into(),
        "/proc/self/maps".into(),
        format!("/proc/{me}/status"),
        format!("/proc/{me}/stat"),
        "/proc/2147483647/status".into(),
        "/proc/2147483647/statm".into(),
        "/proc/meminfo".into(),
        "/proc/uptime".into(),
        "/proc/loadavg".into(),
        "/proc/stat".into(),
        "/proc/cpuinfo".into(),
        "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq".into(),
    ];
    for f in files {
        let shown = f.replace(&format!("/proc/{me}/"), "/proc/<pid>/");
        match std::fs::read(&f) {
            Ok(_) => println!("{shown}: ok"),
            Err(e) => println!("{shown}: {}", e.raw_os_error().map(|n| usys::Errno(n).name()).unwrap_or_else(|| e.to_string())),
        }
    }
}

fn main() {
    // Like ps: a closed pipe (`procs | head`) ends the program quietly (Rust's runtime ignores SIGPIPE).
    let _ = usys::rt_sigaction_simple(13, false);
    let mut it = std::env::args().skip(1);
    let mut only = Vec::new();
    let (mut top_mode, mut frames, mut delay) = (false, 3u32, std::time::Duration::from_secs(1));
    while let Some(a) = it.next() {
        let mut val = |what: &str| it.next().unwrap_or_else(|| usage(&format!("{what} needs a value")));
        match a.as_str() {
            "--help" | "-h" => help(),
            "-p" => only.push(val("-p").parse().unwrap_or_else(|_| usage("-p wants a pid"))),
            "--top" => top_mode = true,
            "-n" => frames = val("-n").parse().unwrap_or_else(|_| usage("-n wants a number")),
            "-d" => {
                // try_from refuses negative, NaN, infinite and too large values (from_secs_f64 would panic).
                let secs: f64 = val("-d").parse().unwrap_or_else(|_| usage("-d wants seconds"));
                delay = std::time::Duration::try_from_secs_f64(secs).unwrap_or_else(|_| usage("-d wants seconds"));
            }
            "--probe" => {
                probe();
                exit(0)
            }
            "--selftest" => exit(selftest::run()),
            "--idle" => {
                let s: u64 = val("--idle").parse().unwrap_or_else(|_| usage("--idle wants seconds"));
                std::thread::sleep(std::time::Duration::from_secs(s));
                exit(0)
            }
            _ => usage(&format!("unknown argument {a}")),
        }
    }
    if top_mode {
        top(frames, delay)
    } else {
        exit(listing(&only))
    }
}

//! FS-14 inotify ktests:
//! - `fs.inotify.programs`: `/bin/fs-inotify` (host/tests/fs/progs/fs-inotify.c) in each mode through the real
//!   syscall, wait, fork and epoll paths, compared line by line with CachyOS's transcript
//!   (host/tests/fs/inotify-linux.txt);
//! - `fs.inotify.devfs`: device nodes the kernel makes and removes under /dev (what evdev hot plug does for
//!   /dev/input/eventN) reach a watch on their directory as IN_CREATE and IN_DELETE with their names;
//! - `fs.inotify.limits`: /proc/sys/fs/inotify answers Linux's defaults, max_user_instances is enforced
//!   (EMFILE), and an instance's last close ends its watches.
//! Safe and rerunnable: scratch names are fresh per run and removed at the end.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use vfs::inotify::{Group, IN_CREATE, IN_DELETE, IN_IGNORED};
use vfs::node::File;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const PROG: &str = "/bin/fs-inotify";
const LINUX: &str = include_str!("../../../host/tests/fs/inotify-linux.txt");
static RUN: AtomicU64 = AtomicU64::new(0);

/// The block of `mode`: (end line, stdout lines).
fn block(mode: &str) -> Option<(&'static str, Vec<&'static str>)> {
    let mut inside = false;
    let mut end = None;
    let mut v = Vec::new();
    for l in LINUX.lines().filter(|l| !l.starts_with('#')) {
        if let Some(m) = l.strip_prefix("=== ") {
            if inside {
                break;
            }
            inside = m == mode;
        } else if inside && end.is_none() {
            end = Some(l);
        } else if inside {
            v.push(l);
        }
    }
    end.map(|e| (e, v))
}

fn one_mode(mode: &str) -> TestResult {
    let (end, lines) = block(mode).ok_or_else(|| format!("inotify-linux.txt has no {mode} block"))?;
    let argv = ["fs-inotify", mode];
    let env = ["HOME=/", "PATH=/bin"];
    let out = proc::stdio::Capture::new(1 << 16);
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn File>);
    let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {mode}: {e}"))?;
    p.wait(Some(120 * SEC));
    let got_end = match p.status() {
        Some(proc::ExitStatus::Exited(c)) => format!("end: exit {c}"),
        Some(proc::ExitStatus::Signaled(s)) => format!("end: signal {s}"),
        None => {
            p.kill(9);
            p.wait(Some(5 * SEC));
            return Err(format!("{mode}: still running after 120 s"));
        }
    };
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    let got: Vec<&str> = text.lines().collect();
    if got_end != end || got != lines {
        let first = got.iter().zip(lines.iter()).position(|(a, b)| a != b).unwrap_or(got.len().min(lines.len()));
        klog::event!(Warn, "fs", "inotify.diff", mode = mode, line = first, v2 = got.get(first).copied().unwrap_or("<none>"),
            linux = lines.get(first).copied().unwrap_or("<none>"));
        return Err(format!("{mode}: {got_end} vs Linux {end}; first different line {first}: {:?} vs {:?}", got.get(first), lines.get(first)));
    }
    Ok(())
}

fn programs() -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::skip("/bin/fs-inotify is not in this image (xtask builds host/tests/fs/progs into every image)");
    }
    for mode in ["checks", "dir", "self", "wait"] {
        one_mode(mode)?;
    }
    Ok(())
}
ktest!("fs.inotify.programs", Safe, programs);

/// (wd, mask, name) of every queued event.
fn drain(g: &Group) -> Vec<(i32, u32, Vec<u8>)> {
    let mut b = alloc::vec![0u8; 4096];
    let n = g.read_stream(&mut b, true).unwrap_or(0);
    let mut out = Vec::new();
    let mut at = 0;
    while at + 16 <= n {
        let w = |o: usize| u32::from_le_bytes([b[at + o], b[at + o + 1], b[at + o + 2], b[at + o + 3]]);
        let len = w(12) as usize;
        let name = &b[at + 16..at + 16 + len];
        let end = name.iter().position(|&c| c == 0).unwrap_or(len);
        out.push((w(0) as i32, w(4), name[..end].to_vec()));
        at += 16 + len;
    }
    out
}

fn devfs() -> TestResult {
    let ns = vfs::root_ns();
    let dir = format!("/dev/fs14-input-{}-{}", time::now_ns() / MS, RUN.fetch_add(1, Ordering::Relaxed));
    let d = ns.mkdir_p(&dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
    let g = vfs::inotify::sys::new_group_for_kernel(0).map_err(|e| format!("group: {e}"))?;
    let r = (|| -> TestResult {
        kassert!(g.update_watch(&d.inode, IN_CREATE | IN_DELETE) == Ok(1), "watch");
        let node = format!("{dir}/event7");
        ns.create_special(node.as_bytes(), uapi::S_IFCHR | 0o660, (13 << 8) | 71).map_err(|e| format!("node: {e}"))?;
        let got = drain(&g);
        kassert!(got == [(1, IN_CREATE, b"event7".to_vec())], "after the node came: {got:?}");
        ns.remove(&node).map_err(|e| format!("remove: {e}"))?;
        let got = drain(&g);
        kassert!(got == [(1, IN_DELETE, b"event7".to_vec())], "after the node went: {got:?}");
        Ok(())
    })();
    // Nobody else holds the directory's name: its removal ends the watch at once (`d_delete`).
    drop(d);
    let _ = ns.remove(&dir);
    let got = drain(&g);
    kassert!(r.is_err() || got.iter().any(|e| e.1 == IN_IGNORED), "removing the directory ends the watch: {got:?}");
    r
}
ktest!("fs.inotify.devfs", Safe, devfs);

fn limits() -> TestResult {
    let ns = vfs::root_ns();
    let q = ns.read_file("/proc/sys/fs/inotify/max_queued_events", 64).map_err(|e| format!("max_queued_events: {e}"))?;
    kassert!(q == b"16384\n", "max_queued_events {:?}", String::from_utf8_lossy(&q));
    let i = ns.read_file("/proc/sys/fs/inotify/max_user_instances", 64).map_err(|e| format!("max_user_instances: {e}"))?;
    let w = ns.read_file("/proc/sys/fs/inotify/max_user_watches", 64).map_err(|e| format!("max_user_watches: {e}"))?;
    let parse = |b: &[u8]| core::str::from_utf8(b).ok().and_then(|s| s.trim().parse::<u64>().ok());
    let max = parse(&i).ok_or("max_user_instances is no number")?;
    let watches = parse(&w).ok_or("max_user_watches is no number")?;
    kassert!((8192..=1_048_576).contains(&watches), "max_user_watches {watches}");
    // A uid nobody else uses: exactly `max` instances, then EMFILE; the last close gives them back.
    let uid = 0x7fee_0000 + (RUN.fetch_add(1, Ordering::Relaxed) as u32 & 0xffff);
    let mut held = Vec::new();
    for n in 0..max {
        held.push(vfs::inotify::sys::new_group_for_kernel(uid).map_err(|e| format!("instance {n}: {e}"))?);
    }
    let over = vfs::inotify::sys::new_group_for_kernel(uid).err();
    drop(held);
    kassert!(over == Some(uapi::errno::EMFILE), "instance {max}: {over:?}");
    kassert!(vfs::inotify::sys::new_group_for_kernel(uid).is_ok(), "the instances came back");
    Ok(())
}
ktest!("fs.inotify.limits", Safe, limits);

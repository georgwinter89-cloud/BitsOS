//! FS-10 ktests that need processes (`/bin/fs-memfd`, host/tests/fs/progs/fs-memfd.c):
//! - `fs.memfd.programs`: mode `basic` through the real syscall, fault and /proc paths, compared line by line with
//!   what CachyOS printed (host/tests/fs/memfd-linux.txt);
//! - `fs.memfd.wl_shm`: the Wayland wl_shm path with two programs. The client makes a memfd, maps it MAP_SHARED,
//!   draws frame 1 and seals it; the kernel takes the client's open file out of its fd table and installs that
//!   same open file description as the compositor's fd 0 (what SCM_RIGHTS does: NW-5 is not in this branch, nor
//!   AB-4's fork), the compositor maps it read-only and sees frame 1, then frame 2 through the shared pages.
//!   Each half's lines must equal the client:/compositor: lines of the wl-fork run on CachyOS.
//! - `fs.memfd.wl_fork`, `fs.memfd.wl_socket`: the same path in one program (fork-inherited fd; socketpair with
//!   SCM_RIGHTS), compared with their CachyOS blocks; they skip while fork / AF_UNIX answer ENOSYS.
//! - `fs.memfd.seal_race` (fixer): F_ADD_SEALS(F_SEAL_WRITE) against writable shared mappings and writes made
//!   on other CPUs at the same time (`-- -smp 16`): granted only with no counted mapping, no write after it.
//! Safe and rerunnable: programs still running when a test ends are killed; the memfds die with them.

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use vfs::node::File;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const PROG: &str = "/bin/fs-memfd";
const LINUX: &str = include_str!("../../../host/tests/fs/memfd-linux.txt");

fn have_prog() -> Result<(), TestResult> {
    match vfs::root_ns().lookup(PROG) {
        Ok(_) => Ok(()),
        Err(_) => Err(ktest::skip("/bin/fs-memfd is not in this image (xtask builds host/tests/fs/progs into every image)")),
    }
}

/// The block of `mode` in memfd-linux.txt: (end line, stdout lines).
fn block(mode: &str) -> Option<(&'static str, Vec<&'static str>)> {
    let mut cur: Option<(&str, Vec<&str>)> = None;
    let mut inside = false;
    for l in LINUX.lines().filter(|l| !l.starts_with('#')) {
        if let Some(m) = l.strip_prefix("=== ") {
            if inside {
                break;
            }
            inside = m == mode;
            continue;
        }
        if !inside {
            continue;
        }
        match &mut cur {
            None => cur = Some((l, Vec::new())),
            Some((_, v)) => v.push(l),
        }
    }
    cur
}

/// Kills what still runs when the test ends (also on a failed kassert).
#[derive(Default)]
struct Cleanup {
    progs: Vec<Weak<proc::Process>>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for w in &self.progs {
            if let Some(p) = w.upgrade()
                && !p.is_finished()
            {
                klog::event!(Warn, "fs", "ktest.cleanup_kill", pid = p.pid; "a test program still ran when its test ended");
                p.kill(9);
                p.wait(Some(5 * SEC));
            }
        }
    }
}

fn spawn(c: &mut Cleanup, mode: &str, stdin: Option<Arc<dyn File>>, out: &Arc<proc::stdio::Capture>, held: bool) -> Result<Arc<proc::Process>, String> {
    let argv = ["fs-memfd", mode];
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdin = stdin;
    o.stdout = Some(out.clone() as Arc<dyn File>);
    o.held = held;
    let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {mode}: {e}"))?;
    c.progs.push(Arc::downgrade(&p));
    Ok(p)
}

fn end_line(p: &proc::Process, limit_ns: u64, what: &str) -> Result<String, String> {
    p.wait(Some(limit_ns));
    match p.status() {
        Some(proc::ExitStatus::Exited(c)) => Ok(format!("end: exit {c}")),
        Some(proc::ExitStatus::Signaled(s)) => Ok(format!("end: signal {s}")),
        None => Err(format!("{what}: still running after {} ms", limit_ns / MS)),
    }
}

/// Waits (10 ms slices, bounded) until `out` holds a line equal to `line`; Err when the program ended first.
fn wait_line(p: &proc::Process, out: &proc::stdio::Capture, line: &str, limit_ns: u64) -> Result<(), String> {
    let t0 = time::now_ns();
    loop {
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        if text.lines().any(|l| l == line) {
            return Ok(());
        }
        if p.is_finished() {
            return Err(format!("ended ({:?}) before {line:?}; it said {text:?}", p.status()));
        }
        if time::now_ns() - t0 > limit_ns {
            return Err(format!("no {line:?} within {} ms; it said {text:?}", limit_ns / MS));
        }
        sched::sleep_ns(10 * MS);
    }
}

fn compare(what: &str, got_end: &str, text: &str, end: &str, lines: &[&str]) -> TestResult {
    let got: Vec<&str> = text.lines().collect();
    if got_end != end || got != lines {
        let first = got.iter().zip(lines.iter()).position(|(a, b)| a != b).unwrap_or(got.len().min(lines.len()));
        klog::event!(Warn, "fs", "memfd.diff", what = what, line = first, v2 = got.get(first).copied().unwrap_or("<none>"),
            linux = lines.get(first).copied().unwrap_or("<none>"));
        return Err(format!("{what}: {got_end} vs Linux {end}; first different line {first}: {:?} vs {:?}", got.get(first), lines.get(first)));
    }
    Ok(())
}

/// One program run of `mode` compared with its Linux block; a skip when the program says fork or socketpair
/// are missing (`skip_on`: the line prefixes that mean so).
fn one_mode(mode: &str, skip_on: &[&str]) -> TestResult {
    if let Err(skip) = have_prog() {
        return skip;
    }
    let (end, lines) = block(mode).ok_or_else(|| format!("memfd-linux.txt has no {mode} block"))?;
    let mut c = Cleanup::default();
    let out = proc::stdio::Capture::new(1 << 16);
    let p = spawn(&mut c, mode, None, &out, false)?;
    let got_end = end_line(&p, 60 * SEC, mode)?;
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    if let Some(l) = text.lines().find(|l| skip_on.iter().any(|s| l.starts_with(s))) {
        return ktest::skip(&format!("{mode}: {l} (needs AB-4's fork and NW-5's AF_UNIX; fs.memfd.wl_shm covers the path meanwhile)"));
    }
    compare(mode, &got_end, &text, end, &lines)
}

fn programs() -> TestResult {
    one_mode("basic", &[])
}
ktest!("fs.memfd.programs", Safe, programs);

/// The wl_shm path in one program with the fd inherited by fork (runs once AB-4 is in).
fn wl_fork() -> TestResult {
    one_mode("wl-fork", &["fork:"])
}
ktest!("fs.memfd.wl_fork", Safe, wl_fork);

/// The wl_shm path in one program with the fd passed over a socketpair by SCM_RIGHTS (runs once NW-5 and AB-4
/// are in).
fn wl_socket() -> TestResult {
    one_mode("wl-socket", &["socketpair:", "fork:", "client: sendmsg", "client: compositor ended early"])
}
ktest!("fs.memfd.wl_socket", Safe, wl_socket);

fn wl_shm() -> TestResult {
    if let Err(skip) = have_prog() {
        return skip;
    }
    let (end, lines) = block("wl-fork").ok_or("memfd-linux.txt has no wl-fork block")?;
    let client_lines: Vec<&str> = lines.iter().copied().filter(|l| l.starts_with("client:")).collect();
    let comp_lines: Vec<&str> = lines.iter().copied().filter(|l| l.starts_with("compositor:")).collect();
    let mut c = Cleanup::default();
    let t0 = time::now_ns();
    let (r, w) = vfs::pipe::kernel_pipe();
    let cout = proc::stdio::Capture::new(1 << 14);
    let client = spawn(&mut c, "wl-client", Some(r as Arc<dyn File>), &cout, false)?;
    wait_line(&client, &cout, "client: seal shrink|grow: 0", 30 * SEC).map_err(|e| format!("client: {e}"))?;
    // The hand-over: the client's memfd (found by its name), the same open file description.
    let (fd, memfd) = client
        .fds
        .open_fds()
        .into_iter()
        .find(|(_, f)| f.path.as_ref().is_some_and(|p| p.dentry.name() == b"memfd:wl_shm"))
        .ok_or("the client has no memfd:wl_shm open")?;
    // /proc/<pid>/fd shows it as Linux names it.
    let ns = vfs::root_ns();
    let link = vfs::Ctx::kernel(&ns).readlink(None, format!("/proc/{}/fd/{fd}", client.pid).as_bytes()).map_err(|e| format!("readlink /proc/{}/fd/{fd}: errno {e}", client.pid))?;
    kassert!(link == b"/memfd:wl_shm (deleted)", "the fd link reads {:?}", String::from_utf8_lossy(&link));
    let sout = proc::stdio::Capture::new(1 << 14);
    let comp = spawn(&mut c, "wl-compositor", None, &sout, true)?;
    comp.fds.install_at(0, memfd).map_err(|e| format!("install the memfd as the compositor's fd 0: errno {e}"))?;
    comp.release();
    wait_line(&comp, &sout, "compositor: seen 1", 30 * SEC).map_err(|e| format!("compositor: {e}"))?;
    w.write_stream(b"next\n", false).map_err(|e| format!("next to the client: errno {e}"))?;
    let comp_end = end_line(&comp, 30 * SEC, "compositor")?;
    // The client ends on its stdin's end (after the compositor, as wl-fork's parent waits for its child).
    drop(w);
    let client_end = end_line(&client, 30 * SEC, "client")?;
    let ctext = String::from_utf8_lossy(&cout.contents()).into_owned();
    let stext = String::from_utf8_lossy(&sout.contents()).into_owned();
    compare("compositor", &comp_end, &stext, end, &comp_lines)?;
    compare("client", &client_end, &ctext, end, &client_lines)?;
    klog::event!(Info, "fs", "memfd.wl_shm", client = client.pid, compositor = comp.pid, ms = (time::now_ns() - t0) / MS;
        "a memfd buffer went from a client to a compositor and both frames arrived (spawns included)");
    Ok(())
}
ktest!("fs.memfd.wl_shm", Safe, wl_shm);

/// Fixer (finding 4): F_ADD_SEALS(F_SEAL_WRITE) races two threads that keep making and dropping writable shared
/// mappings and one that keeps writing, on other CPUs (run with `-- -smp 16`). A counted writable mapping and
/// F_SEAL_WRITE never exist together (each mapper looks at the seals while it holds its mapping), the seal comes
/// once no mapping is held (EBUSY before), and no write lands after it: what the file holds right after the
/// grant is what it holds at the end. Kernel memfds only (nothing outlives a round): Safe and rerunnable.
fn seal_race() -> TestResult {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use vfs::memfd::{F_ADD_SEALS, F_GET_SEALS, F_SEAL_WRITE, MFD_ALLOW_SEALING};
    const ROUNDS: u64 = 60;
    const MAPPERS: u64 = 2;
    let (mut busy, mut maps) = (0u64, 0u64);
    for round in 0..ROUNDS {
        let f = vfs::memfd::create(b"seal-race", MFD_ALLOW_SEALING, vfs::Cred::ROOT).map_err(|e| format!("memfd: {e}"))?;
        f.inode().ok_or("no inode")?.truncate(4096).map_err(|e| format!("truncate: {e}"))?;
        let ready = Arc::new(AtomicU64::new(0));
        let go = Arc::new(AtomicBool::new(false));
        let counted = Arc::new(AtomicU64::new(0));
        let bad = Arc::new(AtomicU64::new(0));
        let start = move |ready: &AtomicU64, go: &AtomicBool| {
            ready.fetch_add(1, Ordering::SeqCst);
            let deadline = time::now_ns() + 2 * SEC;
            while !go.load(Ordering::Acquire) && time::now_ns() < deadline {
                core::hint::spin_loop();
            }
        };
        let mut hs = Vec::new();
        for _ in 0..MAPPERS {
            let (f, ready, go, counted, bad) = (f.clone(), ready.clone(), go.clone(), counted.clone(), bad.clone());
            hs.push(sched::spawn("fs-ktest-sealmap", sched::Class::Normal, move || {
                start(&ready, &go);
                for _ in 0..20_000 {
                    match vfs::mmap::check(&f, 3, vfs::mmap::MAP_SHARED, 0, 4096) {
                        Ok(m) if m.may_write => {
                            counted.fetch_add(1, Ordering::SeqCst);
                            let seals = vfs::memfd::seal_fcntl(&f, F_GET_SEALS, 0).unwrap_or(0) as u32;
                            if seals & F_SEAL_WRITE != 0 {
                                bad.fetch_add(1, Ordering::SeqCst);
                            }
                            drop(m);
                        }
                        Err(uapi::errno::EPERM) => return,
                        _ => {
                            bad.fetch_add(1000, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            }).map_err(|e| format!("spawn: {e:?}"))?);
        }
        let (fw, readyw, gow, badw) = (f.clone(), ready.clone(), go.clone(), bad.clone());
        let writer = sched::spawn("fs-ktest-sealwrite", sched::Class::Normal, move || {
            start(&readyw, &gow);
            for i in 1u64..=200_000 {
                match fw.seal_clip(0, 8).and_then(|_| fw.pwrite(0, &i.to_le_bytes())) {
                    Ok(8) => {}
                    Err(uapi::errno::EPERM) => return,
                    _ => {
                        badw.fetch_add(1000, Ordering::SeqCst);
                        return;
                    }
                }
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        let deadline = time::now_ns() + 2 * SEC;
        while ready.load(Ordering::SeqCst) < MAPPERS + 1 && time::now_ns() < deadline {
            core::hint::spin_loop();
        }
        go.store(true, Ordering::Release);
        // Seal at a different moment each round, while the others are in their loops.
        let at = time::now_ns() + (round % 10) * 20_000;
        while time::now_ns() < at {
            core::hint::spin_loop();
        }
        let deadline = time::now_ns() + 2 * SEC;
        let sealed = loop {
            match vfs::memfd::seal_fcntl(&f, F_ADD_SEALS, F_SEAL_WRITE as u64) {
                Ok(_) => break true,
                Err(uapi::errno::EBUSY) if time::now_ns() < deadline => busy += 1,
                Err(_) => break false,
            }
        };
        kassert!(sealed, "round {}: F_SEAL_WRITE never came", round);
        let mut at_seal = [0u8; 8];
        f.pread(0, &mut at_seal).map_err(|e| format!("pread: {e}"))?;
        let mut joined = writer.join_timeout(5 * SEC);
        for h in hs {
            joined &= h.join_timeout(5 * SEC);
        }
        kassert!(joined, "round {}: a helper did not finish", round);
        let mut end = [0u8; 8];
        f.pread(0, &mut end).map_err(|e| format!("pread: {e}"))?;
        kassert!(at_seal == end, "round {}: a write landed after F_SEAL_WRITE was granted", round);
        kassert!(bad.load(Ordering::SeqCst) == 0, "round {}: a writable mapping under F_SEAL_WRITE or an odd answer ({})", round, bad.load(Ordering::SeqCst));
        kassert!(f.inode().and_then(|i| i.page_cache()).map(|c| c.writable_maps()) == Some(0), "round {}: writable mappings left counted", round);
        maps += counted.load(Ordering::SeqCst);
    }
    klog::event!(Info, "fs", "memfd.seal_race", rounds = ROUNDS, busy = busy, maps = maps;
        "F_SEAL_WRITE against writable shared mappings and writes on other CPUs");
    Ok(())
}
ktest!("fs.memfd.seal_race", Safe, seal_race);

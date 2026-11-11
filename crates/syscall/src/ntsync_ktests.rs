//! SC-8 ktests: /dev/ntsync on a real process's fds and memory.
//!
//! As SC-5's futex ktests: a held process gives the fd table and user memory; helper kernel threads that load
//! its address space play its threads and make the ioctls (`ntsync::ioctl_with`, the dispatcher's path with an
//! explicit context, since a kernel thread has no fd table of its own). Real programs: `sc.ntsync.abi` and
//! `sc.ntsync.abi_threads` (abi-ntsync against CachyOS's transcripts).
//!
//! All `Safe` and rerunnable: each test makes its own process, devices and objects and kills the process at the
//! end; every wait a helper makes has a timeout or is ended by the test, and the test gives up after a limit.

use crate::ntsync::*;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, Ordering};
use ktest::{TestResult, kassert, ktest};
use proc::Process;
use uapi::errno::*;
use vfs::sys::UserMem;

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const PENDING: i64 = i64::MIN;
/// `mov eax, 231; xor edi, edi; syscall` (never runs: the process stays held).
const EXIT0: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
const PAGES: u64 = 4;
const EOWNERDEAD: i64 = sched::ntsync::EOWNERDEAD;

/// The process's memory for the ioctls.
struct PMem<'a>(&'a Process);

impl UserMem for PMem<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        self.0.mm.copy_from_user(buf, addr).is_ok()
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.0.mm.copy_to_user(addr, data).is_ok()
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
}

/// One ioctl on fd `fd` of `p` as the dispatcher makes it; a wait ends when `p` is killed.
fn call(p: &Process, fd: i64, cmd: u32, arg: u64) -> i64 {
    let Ok(f) = p.fds.get(fd as usize) else { return -EBADF };
    let intr = || p.has_exited().then_some(EINTR);
    let c = Ctx { mem: &PMem(p), fds: Some(&p.fds), intr: &intr };
    match ioctl_with(&f.file, cmd, arg, &c) {
        Some(r) => r.unwrap_or_else(|e| -e),
        None => -ENOTTY,
    }
}

/// A held process with `PAGES` pages of memory and one open of /dev/ntsync (`dev`).
struct Nx {
    p: Arc<Process>,
    mem: u64,
    dev: i64,
}

struct Helper {
    h: sched::JoinHandle,
    r: Arc<AtomicI64>,
}

impl Helper {
    fn result(&self, ns: u64) -> Option<i64> {
        self.h.join_timeout(ns).then(|| self.r.load(Ordering::Acquire))
    }
    fn running(&self) -> bool {
        self.r.load(Ordering::Acquire) == PENDING
    }
}

impl Nx {
    fn new(name: &str) -> Result<Nx, String> {
        let image = crate::ktests::elf(&EXIT0);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, PAGES * 4096, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        p.mm.copy_to_user(mem, &[0u8; (PAGES * 4096) as usize]).map_err(|e| format!("fill: {e:?}"))?;
        let mut n = Nx { p, mem, dev: -1 };
        n.dev = n.open()?;
        Ok(n)
    }
    /// Another open of /dev/ntsync (a device of its own), as an fd.
    fn open(&self) -> Result<i64, String> {
        let f = vfs::root_ns().open_flags(b"/dev/ntsync", vfs::abi::O_RDWR, 0).map_err(|e| format!("open /dev/ntsync: {e}"))?;
        self.p.fds.install_flags(f, 0, true).map(|n| n as i64).map_err(|e| format!("install: {e}"))
    }
    fn put(&self, off: u64, b: &[u8]) {
        let _ = self.p.mm.copy_to_user(self.mem + off, b);
    }
    fn get32(&self, off: u64) -> u32 {
        let mut b = [0u8; 4];
        let _ = self.p.mm.copy_from_user(&mut b, self.mem + off);
        u32::from_le_bytes(b)
    }
    /// Two u32 at `off`; their address.
    fn pair(&self, off: u64, a: u32, b: u32) -> u64 {
        let mut o = [0u8; 8];
        o[..4].copy_from_slice(&a.to_le_bytes());
        o[4..].copy_from_slice(&b.to_le_bytes());
        self.put(off, &o);
        self.mem + off
    }
    fn now(&self, fd: i64, cmd: u32, arg: u64) -> i64 {
        call(&self.p, fd, cmd, arg)
    }
    fn create(&self, cmd: u32, a: u32, b: u32) -> Result<i64, String> {
        let r = self.now(self.dev, cmd, self.pair(0, a, b));
        kassert!(r >= 0, "create {cmd:#x}({a}, {b}): {r}");
        Ok(r)
    }
    /// A `struct ntsync_wait_args` at `off` with the object fds right after it; its address.
    fn wait_args(&self, off: u64, fds: &[i64], alert: i64, owner: u32, timeout: u64, flags: u32) -> u64 {
        let list = off + WAIT_ARGS as u64;
        let raw: Vec<u8> = fds.iter().flat_map(|f| (*f as u32).to_le_bytes()).collect();
        self.put(list, &raw);
        let mut a = [0u8; WAIT_ARGS];
        a[0..8].copy_from_slice(&timeout.to_le_bytes());
        a[8..16].copy_from_slice(&(self.mem + list).to_le_bytes());
        a[16..20].copy_from_slice(&(fds.len() as u32).to_le_bytes());
        a[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        a[24..28].copy_from_slice(&flags.to_le_bytes());
        a[28..32].copy_from_slice(&owner.to_le_bytes());
        a[32..36].copy_from_slice(&(alert as u32).to_le_bytes());
        self.put(off, &a);
        self.mem + off
    }
    /// The index a wait at `off` wrote back.
    fn index(&self, off: u64) -> u32 {
        self.get32(off + 20)
    }
    /// A "thread" of the process running `f`.
    fn spawn(&self, name: &str, f: impl FnOnce(&Process) -> i64 + Send + 'static) -> Result<Helper, String> {
        let r = Arc::new(AtomicI64::new(PENDING));
        let (p, r2) = (self.p.clone(), r.clone());
        let h = sched::spawn(name, sched::Class::Normal, move || {
            p.mm.activate();
            r2.store(f(&p), Ordering::Release);
        })
        .map_err(|e| format!("spawn {name}: {e:?}"))?;
        Ok(Helper { h, r })
    }
    /// A "thread" making one ioctl.
    fn thread(&self, name: &str, fd: i64, cmd: u32, arg: u64) -> Result<Helper, String> {
        self.spawn(name, move |p| call(p, fd, cmd, arg))
    }
    fn objects(&self) -> Option<usize> {
        objects_of(&self.p.fds.get(self.dev as usize).ok()?.file)
    }
    /// The waits under way on this test's device (fd `dev`) only.
    fn waiters(&self) -> usize {
        self.p.fds.get(self.dev as usize).ok().and_then(|f| waiters_of(&f.file)).unwrap_or(0)
    }
    /// Waits until `n` waits are under way on this test's device.
    fn queued(&self, n: usize) -> TestResult {
        let end = time::now_ns() + 10 * SEC;
        while self.waiters() < n {
            kassert!(time::now_ns() < end, "only {} of {n} waits queued after 10 s", self.waiters());
            sched::sleep_ns(MS);
        }
        Ok(())
    }
}

impl Drop for Nx {
    fn drop(&mut self) {
        self.p.kill(9);
        let _ = self.p.wait(Some(10 * SEC));
    }
}

fn expect(what: &str, got: i64, want: i64) -> TestResult {
    kassert!(got == want, "{what}: got {got}, want {want}");
    Ok(())
}

fn done(h: &Helper, what: &str, want: i64) -> TestResult {
    match h.result(10 * SEC) {
        Some(v) => expect(what, v, want),
        None => Err(format!("{what}: still waiting after 10 s")),
    }
}

// ---------------------------------------------------------------------------------------------------------

// The node, a device per open, object fds (close-on-exec, anon_inode:ntsync), objects gone on close.
ktest!("sc.ntsync.node", Safe, node);
fn node() -> TestResult {
    let d = vfs::root_ns().lookup("/dev/ntsync").map_err(|e| format!("/dev/ntsync: {e}"))?;
    let f = vfs::node::file_of(&d);
    expect("node mode", f.mode() as i64, 0o020666)?;
    expect("node rdev", f.rdev() as i64, vfs::abi::encode_dev(10, 261) as i64)?;
    let t = Nx::new("sc-nt-node")?;
    let s = t.create(NTSYNC_IOC_CREATE_SEM, 0, 1)?;
    kassert!(t.p.fds.cloexec(s as usize) == Ok(true), "object fd not close-on-exec");
    let of = t.p.fds.get(s as usize).map_err(|e| format!("get: {e}"))?;
    kassert!(of.file.link_name().as_deref() == Some("anon_inode:ntsync"), "link name {:?}", of.file.link_name());
    expect("F_GETFL", of.status_flags() as i64 & 3, 2)?;
    expect("objects", t.objects().unwrap_or(99) as i64, 1)?;
    // A second open is a device of its own: its waits refuse the first one's objects.
    let d2 = t.open()?;
    expect("other device's object", t.now(d2, NTSYNC_IOC_WAIT_ANY, t.wait_args(64, &[s], 0, 1, 0, 0)), -EINVAL)?;
    expect("own device's object", t.now(t.dev, NTSYNC_IOC_WAIT_ANY, t.wait_args(64, &[s], 0, 1, 0, 0)), -ETIMEDOUT)?;
    // Object commands on the device, device commands on an object, a wrong size field: ENOTTY.
    expect("SEM_READ on device", t.now(t.dev, NTSYNC_IOC_SEM_READ, t.mem), -ENOTTY)?;
    expect("CREATE_SEM on object", t.now(s, NTSYNC_IOC_CREATE_SEM, t.pair(0, 0, 1)), -ENOTTY)?;
    expect("size field counts", t.now(t.dev, NTSYNC_IOC_CREATE_SEM ^ (1 << 16), t.pair(0, 0, 1)), -ENOTTY)?;
    drop(of);
    t.p.fds.close(s as usize).map_err(|e| format!("close: {e}"))?;
    expect("objects after close", t.objects().unwrap_or(99) as i64, 0)?;
    // A kernel thread without fds gets EMFILE and leaves no object behind.
    let dev = t.p.fds.get(t.dev as usize).map_err(|e| format!("get: {e}"))?;
    let intr = || None;
    let c = Ctx { mem: &PMem(&t.p), fds: None, intr: &intr };
    let r = ioctl_with(&dev.file, NTSYNC_IOC_CREATE_EVENT, t.pair(0, 0, 0), &c).map(|r| r.unwrap_or_else(|e| -e));
    expect("create without fd table", r.unwrap_or(0), -EMFILE)?;
    expect("objects after EMFILE", t.objects().unwrap_or(99) as i64, 0)?;
    Ok(())
}

// A wait-any sleeps until another thread releases the semaphore; the index is the semaphore's place. Each set
// of an auto event wakes exactly one of three waiters.
ktest!("sc.ntsync.wait_any", Safe, wait_any);
fn wait_any() -> TestResult {
    let t = Nx::new("sc-nt-any")?;
    let e = t.create(NTSYNC_IOC_CREATE_EVENT, 0, 0)?;
    let s = t.create(NTSYNC_IOC_CREATE_SEM, 0, 5)?;
    let w = t.thread("sc-nt-w", t.dev, NTSYNC_IOC_WAIT_ANY, t.wait_args(64, &[e, s], 0, 1, u64::MAX, 0))?;
    t.queued(1)?;
    t.put(0, &2u32.to_le_bytes());
    expect("release", t.now(s, NTSYNC_IOC_SEM_RELEASE, t.mem), 0)?;
    done(&w, "wait-any woken by release", 0)?;
    expect("index", t.index(64) as i64, 1)?;
    expect("sem read", t.now(s, NTSYNC_IOC_SEM_READ, t.mem), 0)?;
    expect("sem left", t.get32(0) as i64, 1)?;
    let ws: Vec<Helper> = (0..3u64).map(|i| t.thread(&format!("sc-nt-w{i}"), t.dev, NTSYNC_IOC_WAIT_ANY, t.wait_args(256 + i * 128, &[e], 0, 1, u64::MAX, 0))).collect::<Result<_, _>>()?;
    t.queued(3)?;
    for round in 1..=3 {
        expect("set", t.now(e, NTSYNC_IOC_EVENT_SET, t.mem), 0)?;
        let end = time::now_ns() + 10 * SEC;
        while ws.iter().filter(|w| !w.running()).count() < round {
            kassert!(time::now_ns() < end, "set {round} woke nobody");
            sched::sleep_ns(MS);
        }
        kassert!(ws.iter().filter(|w| !w.running()).count() == round, "one set woke more than one waiter");
    }
    for w in &ws {
        done(w, "auto event waiter", 0)?;
    }
    expect("waits left", t.waiters() as i64, 0)?;
    Ok(())
}

// Wait-all takes nothing until every object is there, then all at once; a wait-any meanwhile gets the
// semaphore it does not hold. The alert ends a wait-all with the index after the objects.
ktest!("sc.ntsync.wait_all", Safe, wait_all);
fn wait_all() -> TestResult {
    let t = Nx::new("sc-nt-all")?;
    let s = t.create(NTSYNC_IOC_CREATE_SEM, 0, 5)?;
    let m = t.create(NTSYNC_IOC_CREATE_MUTEX, 9, 1)?;
    let e = t.create(NTSYNC_IOC_CREATE_EVENT, 0, 0)?;
    let w = t.thread("sc-nt-wall", t.dev, NTSYNC_IOC_WAIT_ALL, t.wait_args(64, &[s, m, e], 0, 4, u64::MAX, 0))?;
    t.queued(1)?;
    t.put(0, &1u32.to_le_bytes());
    expect("release", t.now(s, NTSYNC_IOC_SEM_RELEASE, t.mem), 0)?;
    expect("unlock", t.now(m, NTSYNC_IOC_MUTEX_UNLOCK, t.pair(8, 9, 0)), 0)?;
    sched::sleep_ns(20 * MS);
    kassert!(w.running(), "wait-all ended with the event missing");
    expect("wait-any meanwhile", t.now(t.dev, NTSYNC_IOC_WAIT_ANY, t.wait_args(512, &[s], 0, 1, 0, 0)), 0)?;
    t.put(0, &1u32.to_le_bytes());
    expect("release again", t.now(s, NTSYNC_IOC_SEM_RELEASE, t.mem), 0)?;
    expect("set", t.now(e, NTSYNC_IOC_EVENT_SET, t.mem), 0)?;
    done(&w, "wait-all", 0)?;
    expect("index", t.index(64) as i64, 0)?;
    expect("sem read", t.now(s, NTSYNC_IOC_SEM_READ, t.mem), 0)?;
    expect("sem taken", t.get32(0) as i64, 0)?;
    expect("mutex read", t.now(m, NTSYNC_IOC_MUTEX_READ, t.mem), 0)?;
    expect("mutex owner", t.get32(0) as i64, 4)?;
    expect("event read", t.now(e, NTSYNC_IOC_EVENT_READ, t.mem), 0)?;
    expect("auto event reset", t.get32(4) as i64, 0)?;
    let al = t.create(NTSYNC_IOC_CREATE_EVENT, 1, 0)?;
    let w = t.thread("sc-nt-walert", t.dev, NTSYNC_IOC_WAIT_ALL, t.wait_args(64, &[s, e], al, 4, u64::MAX, 0))?;
    t.queued(1)?;
    expect("set alert", t.now(al, NTSYNC_IOC_EVENT_SET, t.mem), 0)?;
    done(&w, "wait-all ended by its alert", 0)?;
    expect("alert index", t.index(64) as i64, 2)?;
    Ok(())
}

// MUTEX_KILL hands the mutex to a waiter of another owner with EOWNERDEAD (and the index written).
ktest!("sc.ntsync.abandoned", Safe, abandoned);
fn abandoned() -> TestResult {
    let t = Nx::new("sc-nt-dead")?;
    let m = t.create(NTSYNC_IOC_CREATE_MUTEX, 5, 3)?;
    let w = t.thread("sc-nt-wdead", t.dev, NTSYNC_IOC_WAIT_ANY, t.wait_args(64, &[m], 0, 6, u64::MAX, 0))?;
    t.queued(1)?;
    t.put(0, &6u32.to_le_bytes());
    expect("kill by non-owner", t.now(m, NTSYNC_IOC_MUTEX_KILL, t.mem), -EPERM)?;
    t.put(0, &5u32.to_le_bytes());
    expect("kill", t.now(m, NTSYNC_IOC_MUTEX_KILL, t.mem), 0)?;
    done(&w, "waiter of an abandoned mutex", -EOWNERDEAD)?;
    expect("index", t.index(64) as i64, 0)?;
    expect("mutex read", t.now(m, NTSYNC_IOC_MUTEX_READ, t.mem), 0)?;
    expect("owner", t.get32(0) as i64, 6)?;
    expect("count", t.get32(4) as i64, 1)?;
    Ok(())
}

// Absolute monotonic and realtime deadlines a little ahead end with ETIMEDOUT; killing the process ends an
// untimed wait with EINTR.
ktest!("sc.ntsync.timeouts", Safe, timeouts);
fn timeouts() -> TestResult {
    let t = Nx::new("sc-nt-time")?;
    let e = t.create(NTSYNC_IOC_CREATE_EVENT, 0, 0)?;
    let at = time::now_ns() + 20 * MS;
    let w = t.thread("sc-nt-tmono", t.dev, NTSYNC_IOC_WAIT_ANY, t.wait_args(64, &[e], 0, 1, at, 0))?;
    done(&w, "monotonic deadline", -ETIMEDOUT)?;
    let rt = crate::clock::realtime_ns() + 20 * MS;
    let w = t.thread("sc-nt-treal", t.dev, NTSYNC_IOC_WAIT_ALL, t.wait_args(256, &[e], 0, 1, rt, NTSYNC_WAIT_REALTIME))?;
    done(&w, "realtime deadline", -ETIMEDOUT)?;
    let w = t.thread("sc-nt-tkill", t.dev, NTSYNC_IOC_WAIT_ANY, t.wait_args(512, &[e], 0, 1, u64::MAX, 0))?;
    t.queued(1)?;
    t.p.kill(9);
    done(&w, "wait of a killed process", -EINTR)?;
    Ok(())
}

// 8 threads, 400 rounds each, take one mutex (each its own owner id) around a plain counter in user memory:
// the counter is exact (mutual exclusion) and every thread finishes (no lost wake-up).
ktest!("sc.ntsync.mutex_storm", Safe, mutex_storm);
fn mutex_storm() -> TestResult {
    const N: u64 = 8;
    const ROUNDS: u64 = 400;
    let t = Nx::new("sc-nt-storm")?;
    let m = t.create(NTSYNC_IOC_CREATE_MUTEX, 0, 0)?;
    let (dev, counter) = (t.dev, t.mem + 8);
    let mut hs = Vec::new();
    for i in 0..N {
        let owner = i as u32 + 1;
        let wa = t.wait_args(1024 + i * 128, &[m], 0, owner, u64::MAX, 0);
        let unlock = t.pair(3072 + i * 16, owner, 0);
        hs.push(t.spawn(&format!("sc-nt-s{i}"), move |p| {
            for _ in 0..ROUNDS {
                let w = call(p, dev, NTSYNC_IOC_WAIT_ANY, wa);
                if w != 0 {
                    return w;
                }
                let mut b = [0u8; 8];
                let _ = p.mm.copy_from_user(&mut b, counter);
                let _ = p.mm.copy_to_user(counter, &(u64::from_le_bytes(b) + 1).to_le_bytes());
                let _ = p.mm.copy_to_user(unlock, &owner.to_le_bytes());
                let u = call(p, m, NTSYNC_IOC_MUTEX_UNLOCK, unlock);
                if u != 0 {
                    return 1000 + u;
                }
            }
            0
        })?);
    }
    for h in &hs {
        match h.result(60 * SEC) {
            Some(v) => expect("storm thread", v, 0)?,
            None => return Err(String::from("a storm thread is still running after 60 s")),
        }
    }
    let mut b = [0u8; 8];
    let _ = t.p.mm.copy_from_user(&mut b, counter);
    expect("counter", u64::from_le_bytes(b) as i64, (N * ROUNDS) as i64)?;
    expect("mutex read", t.now(m, NTSYNC_IOC_MUTEX_READ, t.mem), 0)?;
    expect("free at the end", t.get32(0) as i64, 0)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Real programs

fn run_abi(argv: &[&str], want: &str) -> TestResult {
    if vfs::root_ns().lookup("/bin/abi-ntsync").is_err() {
        return ktest::fail("/bin/abi-ntsync is not in this image (the ABI-diff programs need UL-1's musl sysroot)");
    }
    let out = proc::stdio::Capture::new(1 << 20);
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with("/bin/abi-ntsync", &o).map_err(|e| format!("spawn: {e}"))?;
    let text = || String::from_utf8_lossy(&out.contents()).into_owned();
    let code = match p.wait(Some(60 * SEC)) {
        Some(c) => c,
        None => {
            p.kill(9);
            let _ = p.wait(Some(10 * SEC));
            return Err(format!("abi-ntsync did not end within 60 s; output so far:\n{}", text()));
        }
    };
    let got = text();
    let known = include_str!("../../../user/abi-tests/expected/ntsync.known");
    let known: Vec<&str> = known.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).filter_map(|l| l.split(": ").next()).collect();
    let g: Vec<&str> = got.lines().collect();
    let mut diffs = Vec::new();
    for (i, w) in want.lines().enumerate() {
        let gl = g.get(i).copied().unwrap_or("<missing>");
        if gl != w && !known.iter().any(|k| w.starts_with(k)) {
            diffs.push(format!("line {}: linux '{w}' got '{gl}'", i + 1));
        }
    }
    if g.len() > want.lines().count() {
        diffs.push(format!("{} extra lines", g.len() - want.lines().count()));
    }
    kassert!(diffs.is_empty() && code == 0, "abi-ntsync {:?} differs from CachyOS (exit {code}):\n{}", &argv[1..], diffs.join("\n"));
    Ok(())
}

// abi-ntsync's transcript equals CachyOS's (expected/ntsync.txt).
ktest!("sc.ntsync.abi", Safe, abi);
fn abi() -> TestResult {
    run_abi(&["abi-ntsync"], include_str!("../../../user/abi-tests/expected/ntsync.txt"))
}

// abi-ntsync threads: sleeping waits ended by other threads and by a signal (expected/ntsync-threads.txt).
ktest!("sc.ntsync.abi_threads", Safe, abi_threads);
fn abi_threads() -> TestResult {
    run_abi(&["abi-ntsync", "threads"], include_str!("../../../user/abi-tests/expected/ntsync-threads.txt"))
}

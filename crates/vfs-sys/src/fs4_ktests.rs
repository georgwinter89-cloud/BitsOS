//! FS-4 ktests: poll, select and epoll waits woken across CPUs.
//!
//! - `fs.epoll.programs_wait`: `/bin/fs-epoll waiter K` (host/tests/fs/progs/fs-epoll.c) sleeps in epoll_wait
//!   (level and EPOLLET), poll, ppoll, select and pselect6 without a timeout until the kernel writes into its
//!   stdin pipe, twice, then sees the end of file: the whole syscall path with real sleeps and wake-ups.
//! - `fs.epoll.kill_wakes_waiter`: a program asleep in epoll_wait(-1) ends when it is killed (B4).
//! - `fs.epoll.lost_wakeup`: ping-pong between two kernel threads through two pipes, one side on an EPOLLET
//!   epoll, the other on a poll table: thousands of wake-ups that each come right when the other side goes to
//!   sleep; any lost one shows as a wait that runs into its 5 s guard.
//! - `fs.epoll.stress`: 64 pipes, 16 writer threads, 8 waiters on one EPOLLET epoll, 2 on an epoll nesting it and
//!   one poll table: every byte written is read (functional only; timing is judged on the Mini-PC). Run it with
//!   `-- -smp 16`.
//!
//! - `fs.epoll.harvest_race` (fixer, finding 1): an outer epoll waits without a slice rescue while another thread
//!   harvests the inner epoll all the time; a look at the inner one during a harvest must be made good.
//! - `fs.epoll.exclusive` (fixer, finding 8): two epolls with a sleeper each watch one pipe with EPOLLEXCLUSIVE;
//!   one write wakes one of them, the pipe's end wakes both.
//! - `fs.epoll.legacy_objects` (fixer, finding 10): objects without a `PollQueue` (a socket-like test object, the
//!   real /dev/tty) are seen by epoll and poll within the 10 ms slices; /dev/null has no poll (EPERM).
//! - `fs.epoll.signal_interrupts` (INT-A2a, FS-4 CR8): handled signals end the seven waits with EINTR; temporary
//!   masks hold until the handler ran (`fs-epoll sigwait` against its CachyOS transcript).
//!
//! All Safe and rerunnable: kernel pipes and epolls only, programs held by a `Cleanup` that kills what still runs.

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use vfs::epoll::{self, Epoll, EPOLLET, EPOLL_CTL_ADD};
use vfs::node::File;
use vfs::poll::{Events, PollTable};
use vfs::OpenFile;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const PROG: &str = "/bin/fs-epoll";

fn have_prog() -> bool {
    vfs::root_ns().lookup(PROG).is_ok()
}

#[derive(Default)]
struct Cleanup {
    progs: Vec<Weak<proc::Process>>,
}

impl Cleanup {
    fn spawn(&mut self, args: &[&str], stdin: Arc<dyn File>, out: Arc<dyn File>) -> Result<Arc<proc::Process>, String> {
        let mut argv = alloc::vec!["fs-epoll"];
        argv.extend_from_slice(args);
        let env = ["HOME=/", "PATH=/bin"];
        let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
        o.stdin = Some(stdin);
        o.stdout = Some(out.clone());
        o.stderr = Some(out);
        let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {args:?}: {e}"))?;
        self.progs.push(Arc::downgrade(&p));
        Ok(p)
    }
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

fn ended(p: &proc::Process, limit_ns: u64, what: &str) -> Result<proc::ExitStatus, String> {
    if p.wait(Some(limit_ns)).is_some()
        && let Some(s) = p.status()
    {
        return Ok(s);
    }
    p.kill(9);
    p.wait(Some(5 * SEC));
    Err(format!("{what}: still running after {} ms", limit_ns / MS))
}

fn sum_of(data: &[u8]) -> u64 {
    data.iter().fold(0u64, |s, &b| s.wrapping_mul(31).wrapping_add(b as u64))
}

/// Each wait kind of fs-epoll sleeps until the kernel writes into its stdin (twice), then reads to EOF.
fn programs_wait() -> TestResult {
    if !have_prog() {
        return ktest::skip("/bin/fs-epoll is not in this image (xtask builds host/tests/fs/progs into every image)");
    }
    for kind in ["epoll", "epoll-et", "poll", "ppoll", "select", "pselect"] {
        let (r, w) = vfs::pipe::kernel_pipe();
        let out = proc::stdio::Capture::new(4096);
        let mut c = Cleanup::default();
        let p = c.spawn(&["waiter", kind], r as Arc<dyn File>, out.clone() as Arc<dyn File>)?;
        let t0 = time::now_ns();
        sched::sleep_ns(30 * MS);
        kassert!(!p.is_finished(), "{kind}: the waiter ended before any data: {}", String::from_utf8_lossy(&out.contents()));
        w.write_stream(b"abc", false).map_err(|e| format!("write: {e}"))?;
        sched::sleep_ns(30 * MS);
        w.write_stream(b"defgh", false).map_err(|e| format!("write: {e}"))?;
        drop(w);
        let s = ended(&p, 10 * SEC, kind)?;
        let us = (time::now_ns() - t0) / 1000;
        klog::event!(Info, "fs", "epoll.program_wait", kind = kind, us = us; "a program woken twice from its wait, then EOF (60 ms of it are the test's own sleeps)");
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        let want = format!("{kind} got 8 sum {}\n", sum_of(b"abcdefgh"));
        kassert!(s == proc::ExitStatus::Exited(0) && text == want, "{kind}: ended {s:?}, said {text:?}, want {want:?}");
    }
    Ok(())
}
ktest!("fs.epoll.programs_wait", Safe, programs_wait);

/// A program asleep in epoll_wait(-1) on a pipe nobody writes is ended by a kill within a wait slice.
fn kill_wakes_waiter() -> TestResult {
    if !have_prog() {
        return ktest::skip("/bin/fs-epoll is not in this image");
    }
    let (r, w) = vfs::pipe::kernel_pipe();
    let out = proc::stdio::Capture::new(4096);
    let mut c = Cleanup::default();
    let p = c.spawn(&["waiter", "epoll"], r as Arc<dyn File>, out.clone() as Arc<dyn File>)?;
    sched::sleep_ns(50 * MS);
    kassert!(!p.is_finished(), "the waiter ended without data: {}", String::from_utf8_lossy(&out.contents()));
    let t0 = time::now_ns();
    p.kill(9);
    let s = ended(&p, 5 * SEC, "killed waiter")?;
    let ms = (time::now_ns() - t0) / MS;
    klog::event!(Info, "fs", "epoll.kill_wake", ms = ms; "a program in epoll_wait(-1) killed");
    kassert!(s == proc::ExitStatus::Signaled(9), "ended {s:?}");
    drop(w);
    Ok(())
}
ktest!("fs.epoll.kill_wakes_waiter", Safe, kill_wakes_waiter);

fn anon_read(r: Arc<vfs::pipe::PipeEnd>) -> Arc<OpenFile> {
    OpenFile::anon(r, vfs::abi::O_RDONLY | vfs::abi::O_NONBLOCK)
}

fn add(ep: &Arc<OpenFile>, f: &Arc<OpenFile>, fd: i32, events: u32, data: u64) -> Result<(), String> {
    Epoll::ctl(ep, EPOLL_CTL_ADD, f, fd, (events, data), false).map_err(|e| format!("epoll_ctl ADD: {e}"))
}

fn ep(of: &Arc<OpenFile>) -> Result<&Epoll, String> {
    of.file.as_epoll().ok_or_else(|| String::from("not an epoll"))
}

/// Ping-pong: the test thread writes `b` and waits on an EPOLLET epoll for `a`; a helper waits for `b` on a poll
/// table and answers with `a`. Every round both sides go to sleep right when the other wakes them.
fn lost_wakeup() -> TestResult {
    const ROUNDS: u64 = 5000;
    let (ar, aw) = vfs::pipe::kernel_pipe();
    let (br, bw) = vfs::pipe::kernel_pipe();
    let ea = epoll::new_epoll();
    let af = anon_read(ar.clone());
    add(&ea, &af, 3, Events::IN | EPOLLET, 1)?;
    let failed = Arc::new(AtomicU64::new(0));
    let f2 = failed.clone();
    let brf: Arc<dyn File> = br.clone();
    let h = sched::spawn("fs-ktest-pong", sched::Class::Normal, move || {
        let mut b = [0u8; 8];
        for round in 0..ROUNDS {
            let mut table = PollTable::new(true);
            let deadline = time::now_ns() + 5 * SEC;
            let ok = loop {
                table.arm();
                if table.poll(&brf, Events::IN | Events::ERR | Events::HUP) & Events::IN != 0 {
                    break true;
                }
                table.stop_registering();
                if table.sleep(Some(deadline), &|| false) {
                    break false;
                }
            };
            drop(table);
            if !ok {
                f2.store(round + 1, Ordering::SeqCst);
                return;
            }
            let _ = br.read_stream(&mut b, true);
            if aw.write_stream(b"a", false).is_err() {
                f2.store(round + 1, Ordering::SeqCst);
                return;
            }
        }
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    let t0 = time::now_ns();
    let mut b = [0u8; 8];
    let mut lost = None;
    for round in 0..ROUNDS {
        if failed.load(Ordering::SeqCst) != 0 {
            break;
        }
        bw.write_stream(b"b", false).map_err(|e| format!("write b: {e}"))?;
        let mut out: Vec<(u32, u64)> = Vec::new();
        let n = ep(&ea)?.wait(&mut out, 1, Some(time::now_ns() + 5 * SEC), &|| false).map_err(|e| format!("epoll wait: {e}"))?;
        if n != 1 {
            lost = Some(round);
            break;
        }
        let _ = ar.read_stream(&mut b, true);
    }
    let joined = h.join_timeout(10 * SEC);
    let us = (time::now_ns() - t0) / 1000;
    klog::event!(Info, "fs", "epoll.pingpong", rounds = ROUNDS, us = us; "ping-pong through an EPOLLET epoll and a poll table");
    kassert!(lost.is_none(), "epoll side: no wake-up in round {:?} (5 s guard)", lost);
    kassert!(failed.load(Ordering::SeqCst) == 0, "poll side: no wake-up in round {}", failed.load(Ordering::SeqCst) - 1);
    kassert!(joined, "the helper did not finish");
    drop(af);
    Ok(())
}
ktest!("fs.epoll.lost_wakeup", Safe, lost_wakeup);

/// Many pipes, writers and waiters (EPOLLET on one epoll, two waiters on an epoll that nests it, one poll
/// table): every byte is read; nothing hangs.
fn stress() -> TestResult {
    const PIPES: usize = 64;
    const WRITERS: usize = 16;
    const PER_PIPE: u64 = 400;
    const WAITERS: usize = 8;
    let total = PIPES as u64 * PER_PIPE;
    let e = epoll::new_epoll();
    let outer = epoll::new_epoll();
    let mut reads = Vec::new();
    let mut files = Vec::new();
    let mut writes = Vec::new();
    for i in 0..PIPES {
        let (r, w) = vfs::pipe::kernel_pipe();
        let rf = anon_read(r.clone());
        add(&e, &rf, i as i32 + 100, Events::IN | EPOLLET, i as u64)?;
        reads.push(r);
        files.push(rf);
        writes.push(w);
    }
    add(&outer, &e, 99, Events::IN, 999)?;
    let reads = Arc::new(reads);
    let got = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let drain = |reads: &Arc<Vec<Arc<vfs::pipe::PipeEnd>>>, got: &Arc<AtomicU64>, i: usize| {
        let mut b = [0u8; 256];
        while let Ok(k) = reads[i].read_stream(&mut b, true) {
            if k == 0 {
                break;
            }
            got.fetch_add(k as u64, Ordering::SeqCst);
        }
    };
    let mut threads = Vec::new();
    for t in 0..WAITERS + 3 {
        let (e, outer, reads, got, done) = (e.clone(), outer.clone(), reads.clone(), got.clone(), done.clone());
        let h = sched::spawn("fs-ktest-epoll-waiter", sched::Class::Normal, move || {
            let (Ok(inner_ep), Ok(outer_ep)) = (ep(&e), ep(&outer)) else { return };
            while !done.load(Ordering::SeqCst) {
                let deadline = Some(time::now_ns() + 200 * MS);
                if t < WAITERS {
                    let mut out: Vec<(u32, u64)> = Vec::new();
                    if let Ok(n) = inner_ep.wait(&mut out, 16, deadline, &|| false) {
                        for (_, d) in out.iter().take(n) {
                            drain(&reads, &got, *d as usize);
                        }
                    }
                } else if t < WAITERS + 2 {
                    // Through the nesting epoll: when it says the inner one is ready, harvest the inner one.
                    let mut out: Vec<(u32, u64)> = Vec::new();
                    if outer_ep.wait(&mut out, 1, deadline, &|| false).unwrap_or(0) == 1 {
                        let mut inner: Vec<(u32, u64)> = Vec::new();
                        if let Ok(n) = inner_ep.wait(&mut inner, 16, Some(0), &|| false) {
                            for (_, d) in inner.iter().take(n) {
                                drain(&reads, &got, *d as usize);
                            }
                        }
                    }
                } else {
                    // A poll table over the first 8 pipes (they are in the epoll too: reads never tear).
                    let mut table = PollTable::new(true);
                    table.arm();
                    let mut any = false;
                    for (i, r) in reads.iter().take(8).enumerate() {
                        let f: Arc<dyn File> = r.clone();
                        if table.poll(&f, Events::IN | Events::ERR | Events::HUP) & Events::IN != 0 {
                            drain(&reads, &got, i);
                            any = true;
                        }
                    }
                    table.stop_registering();
                    if !any {
                        table.sleep(deadline, &|| false);
                    }
                }
                if got.load(Ordering::SeqCst) >= total {
                    done.store(true, Ordering::SeqCst);
                }
            }
        })
        .map_err(|e| format!("spawn waiter: {e:?}"))?;
        threads.push(h);
    }
    let writes = Arc::new(writes);
    let mut writers = Vec::new();
    for t in 0..WRITERS {
        let writes = writes.clone();
        let h = sched::spawn("fs-ktest-epoll-writer", sched::Class::Normal, move || {
            for k in 0..PER_PIPE {
                for p in (t..PIPES).step_by(WRITERS) {
                    let _ = writes[p].write_stream(&[k as u8], false);
                }
                if k % 64 == 0 {
                    sched::yield_now();
                }
            }
        })
        .map_err(|e| format!("spawn writer: {e:?}"))?;
        writers.push(h);
    }
    let t0 = time::now_ns();
    let mut all_written = true;
    for h in &writers {
        all_written &= h.join_timeout(60 * SEC);
    }
    while got.load(Ordering::SeqCst) < total && time::now_ns() - t0 < 60 * SEC {
        sched::sleep_ns(5 * MS);
    }
    let read = got.load(Ordering::SeqCst);
    done.store(true, Ordering::SeqCst);
    let mut joined = true;
    for h in &threads {
        joined &= h.join_timeout(10 * SEC);
    }
    let ms = (time::now_ns() - t0) / MS;
    klog::event!(Info, "fs", "epoll.stress", pipes = PIPES, writers = WRITERS, waiters = WAITERS + 3, bytes = read, ms = ms, cpus = sched::online_cpus(); "epoll/poll stress over many pipes");
    kassert!(all_written, "a writer did not finish within 60 s");
    kassert!(read == total, "read {read} of {total} bytes: a lost wake-up left data behind");
    kassert!(joined, "a waiter did not stop");
    let items = ep(&e)?.len();
    kassert!(items == PIPES, "items {items} of {PIPES}");
    drop(files);
    kassert!(ep(&e)?.is_empty(), "closing the pipes' open files did not take their registrations");
    Ok(())
}
ktest!("fs.epoll.stress", Safe, stress);

/// Finding 1: the inner epoll is always ready (a level-triggered pipe with data) and harvested by a helper in a
/// loop; the outer epoll's waits (1 s guard each) must all report it. Before the fix a look during a harvest
/// answered "not ready" and nothing woke the outer waiter again.
fn harvest_race() -> TestResult {
    let (r, w) = vfs::pipe::kernel_pipe();
    w.write_stream(b"x", false).map_err(|e| format!("write: {e}"))?;
    let rf = anon_read(r);
    let inner = epoll::new_epoll();
    let outer = epoll::new_epoll();
    add(&inner, &rf, 3, Events::IN, 1)?;
    add(&outer, &inner, 4, Events::IN, 2)?;
    let stop = Arc::new(AtomicBool::new(false));
    let harvests = Arc::new(AtomicU64::new(0));
    let (i2, s2, h2) = (inner.clone(), stop.clone(), harvests.clone());
    let h = sched::spawn("fs-ktest-harvester", sched::Class::Normal, move || {
        let Ok(e) = ep(&i2) else { return };
        // Bounded by itself too, and yielding: on a machine with few CPUs a harvester that never gives the CPU
        // away would starve the waiter it races with (the gate runs this with 2 CPUs).
        let t0 = time::now_ns();
        while !s2.load(Ordering::SeqCst) && time::now_ns() - t0 < 3 * SEC {
            let mut out: Vec<(u32, u64)> = Vec::new();
            if e.wait(&mut out, 4, Some(0), &|| false).unwrap_or(0) > 0 {
                h2.fetch_add(1, Ordering::SeqCst);
            }
            sched::yield_now();
        }
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    let t0 = time::now_ns();
    let mut rounds = 0u64;
    let mut missed = None;
    while time::now_ns() - t0 < 300 * MS && rounds < 20_000 {
        let mut out: Vec<(u32, u64)> = Vec::new();
        let n = ep(&outer)?.wait(&mut out, 1, Some(time::now_ns() + SEC), &|| false).map_err(|e| format!("outer wait: {e}"))?;
        if n != 1 {
            missed = Some(rounds);
            break;
        }
        rounds += 1;
    }
    stop.store(true, Ordering::SeqCst);
    let joined = h.join_timeout(5 * SEC);
    klog::event!(Info, "fs", "epoll.harvest_race", rounds = rounds, harvests = harvests.load(Ordering::SeqCst); "outer epoll waits while the inner one is harvested");
    kassert!(missed.is_none(), "outer wait {:?} saw nothing within 1 s although the inner epoll was ready", missed);
    kassert!(joined, "the harvester did not stop");
    kassert!(rounds > 0, "no round");
    Ok(())
}
ktest!("fs.epoll.harvest_race", Safe, harvest_race);

/// Finding 8: one write to a pipe watched exclusively by two epolls with a sleeper each wakes one of them
/// (Linux `add_wait_queue_exclusive`); the end of the pipe (a wake_all) reaches both. A sleeper caught between
/// two wait slices does not count as sleeping (as Linux's `waitqueue_active`), so up to three tries.
fn exclusive() -> TestResult {
    let mut last = String::new();
    for attempt in 0..3 {
        let (r, w) = vfs::pipe::kernel_pipe();
        let rf = anon_read(r);
        let eps = [epoll::new_epoll(), epoll::new_epoll()];
        for (i, e) in eps.iter().enumerate() {
            add(e, &rf, 3, Events::IN | epoll::EPOLLEXCLUSIVE, i as u64)?;
        }
        let woke = Arc::new(AtomicU64::new(0));
        let mut threads = Vec::new();
        for e in &eps {
            let (e, woke) = (e.clone(), woke.clone());
            let h = sched::spawn("fs-ktest-epoll-excl", sched::Class::Normal, move || {
                let Ok(x) = ep(&e) else { return };
                let mut out: Vec<(u32, u64)> = Vec::new();
                if x.wait(&mut out, 4, Some(time::now_ns() + 5 * SEC), &|| false).unwrap_or(0) > 0 {
                    woke.fetch_add(1, Ordering::SeqCst);
                }
            })
            .map_err(|e| format!("spawn: {e:?}"))?;
            threads.push(h);
        }
        sched::sleep_ns(50 * MS);
        w.write_stream(b"x", false).map_err(|e| format!("write: {e}"))?;
        sched::sleep_ns(150 * MS);
        let after_write = woke.load(Ordering::SeqCst);
        drop(w);
        let mut joined = true;
        for h in &threads {
            joined &= h.join_timeout(10 * SEC);
        }
        let after_end = woke.load(Ordering::SeqCst);
        kassert!(joined, "an exclusive waiter did not end after the pipe's end");
        kassert!(after_end == 2, "the pipe's end woke {after_end} of 2 epolls");
        klog::event!(Info, "fs", "epoll.exclusive", attempt = attempt, after_write = after_write; "EPOLLEXCLUSIVE: epolls woken by one write");
        if after_write == 1 {
            return Ok(());
        }
        last = format!("one write woke {after_write} of 2 exclusive watchers");
        drop(rf);
    }
    Err(last)
}
ktest!("fs.epoll.exclusive", Safe, exclusive);

/// A socket-like object without a `PollQueue` (as sockets, tty and evdev are until their owners switch).
struct Legacy(core::sync::atomic::AtomicU32);

impl File for Legacy {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, vfs::Errno> {
        Ok(0)
    }
    fn write_at(&self, _pos: u64, b: &[u8]) -> Result<usize, vfs::Errno> {
        Ok(b.len())
    }
    fn mode(&self) -> u32 {
        vfs::abi::S_IFSOCK | 0o777
    }
    fn events(&self) -> Events {
        Events(self.0.load(Ordering::SeqCst))
    }
}

/// Finding 10: objects without a `PollQueue` are seen by epoll (level and EPOLLET) and poll once they change
/// (the 10 ms slices); the real /dev/tty is accepted by epoll_ctl and reports OUT; /dev/null answers EPERM.
fn legacy_objects() -> TestResult {
    let obj = Arc::new(Legacy(core::sync::atomic::AtomicU32::new(0)));
    let of = OpenFile::anon(obj.clone(), vfs::abi::O_RDWR);
    let lt = epoll::new_epoll();
    let et = epoll::new_epoll();
    add(&lt, &of, 3, Events::IN, 1)?;
    add(&et, &of, 3, Events::IN | EPOLLET, 2)?;
    let o2 = obj.clone();
    let h = sched::spawn("fs-ktest-legacy", sched::Class::Normal, move || {
        sched::sleep_ns(30 * MS);
        o2.0.store(Events::IN, Ordering::SeqCst);
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    let t0 = time::now_ns();
    let mut out: Vec<(u32, u64)> = Vec::new();
    let n = ep(&et)?.wait(&mut out, 4, Some(time::now_ns() + SEC), &|| false).map_err(|e| format!("wait: {e}"))?;
    let ms = (time::now_ns() - t0) / MS;
    kassert!(h.join_timeout(5 * SEC), "the helper did not end");
    kassert!(n == 1 && out.first() == Some(&(Events::IN, 2)), "EPOLLET: {n} {out:?} after {ms} ms");
    let mut out: Vec<(u32, u64)> = Vec::new();
    let n = ep(&lt)?.wait(&mut out, 4, Some(0), &|| false).map_err(|e| format!("wait: {e}"))?;
    kassert!(n == 1, "level-triggered: {n}");
    let mut out: Vec<(u32, u64)> = Vec::new();
    let n = ep(&et)?.wait(&mut out, 4, Some(time::now_ns() + 40 * MS), &|| false).map_err(|e| format!("wait: {e}"))?;
    kassert!(n == 0, "EPOLLET without new bits reported {n}");
    // poll(): a sleeping table looks again.
    obj.0.store(0, Ordering::SeqCst);
    let o3 = obj.clone();
    let h = sched::spawn("fs-ktest-legacy", sched::Class::Normal, move || {
        sched::sleep_ns(30 * MS);
        o3.0.store(Events::IN, Ordering::SeqCst);
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    let f: Arc<dyn File> = obj.clone();
    let mut table = PollTable::new(true);
    let deadline = time::now_ns() + SEC;
    let seen = loop {
        table.arm();
        if table.poll(&f, Events::IN | Events::ERR | Events::HUP) & Events::IN != 0 {
            break true;
        }
        table.stop_registering();
        if table.sleep(Some(deadline), &|| false) {
            break false;
        }
    };
    drop(table);
    kassert!(h.join_timeout(5 * SEC), "the helper did not end");
    kassert!(seen, "poll never looked at the object again");
    // The real terminal (a legacy object until IN names its PollQueue): pollable, writable.
    let tty = OpenFile::anon(Arc::new(vfs::tty::TtyDev::new(vfs::tty::Which::Tty, None)), vfs::abi::O_RDWR);
    let te = epoll::new_epoll();
    add(&te, &tty, 5, Events::OUT, 5)?;
    let mut out: Vec<(u32, u64)> = Vec::new();
    let n = ep(&te)?.wait(&mut out, 4, Some(time::now_ns() + 100 * MS), &|| false).map_err(|e| format!("tty wait: {e}"))?;
    kassert!(n == 1 && out[0].0 & Events::OUT != 0, "/dev/tty: {n} {out:?}");
    let null = OpenFile::anon(Arc::new(vfs::devfs::MemDev(vfs::devfs::Mem::Null)), vfs::abi::O_RDWR);
    let r = Epoll::ctl(&te, EPOLL_CTL_ADD, &null, 6, (Events::IN, 6), false);
    kassert!(r == Err(uapi::errno::EPERM), "/dev/null: {r:?}");
    drop(of);
    Ok(())
}
ktest!("fs.epoll.legacy_objects", Safe, legacy_objects);

/// A handled signal must end a blocked epoll_wait/epoll_pwait/epoll_pwait2/poll/ppoll/select/pselect6 with EINTR
/// (none restarts after a handler, SA_RESTART or not) and leave the temporary mask for the handler
/// (`restore_saved_sigmask_unless`); a signal blocked by the temporary mask waits (FS-4 CR8 with AB-6; INT-A2a).
/// `fs-epoll sigwait` signals itself from a forked child; its output must equal the CachyOS transcript
/// `host/tests/fs/epoll-sigwait-linux.txt` (also the ABI case `abi-epoll sigwait`).
fn signal_interrupts() -> TestResult {
    if !have_prog() {
        return ktest::skip("/bin/fs-epoll is not in this image");
    }
    const LINUX: &str = include_str!("../../../host/tests/fs/epoll-sigwait-linux.txt");
    let (r, w) = vfs::pipe::kernel_pipe();
    let out = proc::stdio::Capture::new(8192);
    let mut c = Cleanup::default();
    let p = c.spawn(&["sigwait"], r as Arc<dyn File>, out.clone() as Arc<dyn File>)?;
    let s = ended(&p, 30 * SEC, "sigwait")?;
    drop(w);
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    if let Some((i, (got, want))) = text.lines().zip(LINUX.lines()).enumerate().find(|(_, (a, b))| a != b) {
        return Err(format!("line {}: got {got:?}, Linux {want:?}", i + 1));
    }
    kassert!(s == proc::ExitStatus::Exited(0) && text == LINUX, "ended {s:?}; {} lines, Linux {}: {text:?}", text.lines().count(), LINUX.lines().count());
    Ok(())
}
ktest!("fs.epoll.signal_interrupts", Safe, signal_interrupts);

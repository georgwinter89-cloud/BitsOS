//! FS-3 ktests that need processes (`/bin/fs-pipe`, host/tests/fs/progs/fs-pipe.c): two programs joined by a
//! kernel pipe (blocking reads and writes across processes and CPUs), eight writers of records on one pipe
//! (PIPE_BUF atomicity across CPUs), SIGPIPE ending a writer whose reader went away (and EPIPE when it is ignored
//! or blocked), a program blocked in a pipe read that is killed, a FIFO open that waits for a writer; and
//! /proc/sys/fs/pipe-max-size. The single-process semantics run with every other file case in
//! fs.calls.linux_cases (host/tests/fs/cases.txt, checked against Linux). Safe and rerunnable: every test holds a
//! `Cleanup` that, on every way out (also a failed kassert), kills the programs it started that still run and
//! removes its FIFO name (names carry a counter); pipe-max-size is set back.

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use vfs::node::File;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const PROG: &str = "/bin/fs-pipe";

fn have_prog() -> bool {
    vfs::root_ns().lookup(PROG).is_ok()
}

fn skip_no_prog() -> TestResult {
    ktest::skip("/bin/fs-pipe is not in this image (xtask builds host/tests/fs/progs into every image)")
}

struct Io {
    stdin: Option<Arc<dyn File>>,
    stdout: Option<Arc<dyn File>>,
    stderr: Option<Arc<dyn File>>,
}

fn spawn(args: &[&str], io: Io) -> Result<Arc<proc::Process>, String> {
    let mut argv = alloc::vec!["fs-pipe"];
    argv.extend_from_slice(args);
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdin = io.stdin;
    o.stdout = io.stdout;
    o.stderr = io.stderr;
    proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {args:?}: {e}"))
}

/// Leaves nothing behind however the test ends: programs still running are killed (and waited for briefly),
/// the FIFO name is removed. Holds the programs weakly, so a test can still let go of a process (its files
/// close only then, see `pipeline`).
#[derive(Default)]
struct Cleanup {
    progs: Vec<Weak<proc::Process>>,
    path: Option<String>,
}

impl Cleanup {
    fn spawn(&mut self, args: &[&str], io: Io) -> Result<Arc<proc::Process>, String> {
        let p = spawn(args, io)?;
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
        if let Some(path) = self.path.take() {
            let _ = vfs::root_ns().remove(&path);
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

/// The checksum `consume` prints for `produce`'s pattern of `n` bytes.
fn pattern_sum(n: u64) -> u64 {
    let mut sum = 0u64;
    for i in 0..n {
        sum = sum.wrapping_mul(31).wrapping_add((i * 7 % 251) as u64);
    }
    sum
}

/// `fs-pipe produce N | fs-pipe consume`: 1 MB through a 64 KiB pipe, so both sides block many times.
fn pipeline() -> TestResult {
    if !have_prog() {
        return skip_no_prog();
    }
    const N: u64 = 1_000_000;
    let (r, w) = vfs::pipe::kernel_pipe();
    let out = proc::stdio::Capture::new(4096);
    let err = proc::stdio::Capture::new(4096);
    let mut c = Cleanup::default();
    let t0 = time::now_ns();
    let producer = c.spawn(&["produce", "1000000"], Io { stdin: None, stdout: Some(w.clone() as Arc<dyn File>), stderr: Some(err.clone() as Arc<dyn File>) })?;
    let consumer = c.spawn(&["consume"], Io { stdin: Some(r.clone() as Arc<dyn File>), stdout: Some(out.clone() as Arc<dyn File>), stderr: Some(err.clone() as Arc<dyn File>) })?;
    // From here the programs hold the only ends: the consumer sees EOF when the producer exits.
    drop((r, w));
    let ps = ended(&producer, 60 * SEC, "producer")?;
    // The producer's files close only when its process is freed (AB: no `exit_files` at exit yet, change
    // request in plan/notes/fs-FS-3.md); this test holds the last reference, so it lets go first.
    drop(producer);
    let cs = ended(&consumer, 60 * SEC, "consumer")?;
    let us = (time::now_ns() - t0) / 1000;
    klog::event!(Info, "fs", "pipe.pipeline", bytes = N, us = us; "two programs through one pipe (spawn to both ends)");
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    let errs = String::from_utf8_lossy(&err.contents()).into_owned();
    kassert!(ps == proc::ExitStatus::Exited(0), "producer ended {ps:?}: {errs}");
    kassert!(cs == proc::ExitStatus::Exited(0), "consumer ended {cs:?}: {text} {errs}");
    let want = format!("got {N} sum {} pattern ok\n", pattern_sum(N));
    kassert!(text == want, "consumer said {text:?}, want {want:?}");
    Ok(())
}
ktest!("fs.pipe.programs_pipeline", Safe, pipeline);

/// A writer whose reader goes away: the default SIGPIPE ends it (signal 13); ignored or blocked, the write
/// answers EPIPE (blocked: SIGPIPE stays pending). Each case also runs in fs-pipe's own pipe (modes sigpipe*).
fn sigpipe() -> TestResult {
    if !have_prog() {
        return skip_no_prog();
    }
    let mut c = Cleanup::default();
    let (r, w) = vfs::pipe::kernel_pipe();
    let err = proc::stdio::Capture::new(4096);
    let p = c.spawn(&["produce-forever"], Io { stdin: None, stdout: Some(w as Arc<dyn File>), stderr: Some(err.clone() as Arc<dyn File>) })?;
    // Take some data (the writer blocks on the full pipe meanwhile), then go away.
    let mut got = 0usize;
    let mut buf = [0u8; 4096];
    let start = time::now_ns();
    while got < 200_000 {
        match r.read_stream(&mut buf, true) {
            Ok(n) => got += n,
            Err(_) => sched::sleep_ns(MS),
        }
        kassert!(time::now_ns() - start < 30 * SEC, "only {got} bytes from the writer in 30 s");
    }
    drop(r);
    let s = ended(&p, 10 * SEC, "produce-forever")?;
    kassert!(s == proc::ExitStatus::Signaled(13), "the writer ended {s:?} (stderr {:?}), want SIGPIPE", String::from_utf8_lossy(&err.contents()));
    for (mode, end, line) in [
        ("sigpipe", proc::ExitStatus::Signaled(13), "writing\n"),
        ("sigpipe-ign", proc::ExitStatus::Exited(0), "writing\nwrite: -EPIPE\n"),
        ("sigpipe-block", proc::ExitStatus::Exited(0), "writing\nwrite: -EPIPE\nSIGPIPE pending: yes\n"),
    ] {
        let out = proc::stdio::Capture::new(4096);
        let p = c.spawn(&[mode], Io { stdin: None, stdout: Some(out.clone() as Arc<dyn File>), stderr: None })?;
        let s = ended(&p, 10 * SEC, mode)?;
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        kassert!(s == end && text == line, "{mode}: ended {s:?} with {text:?}, want {end:?} with {line:?}");
    }
    Ok(())
}
ktest!("fs.pipe.sigpipe", Safe, sigpipe);

/// A program blocked in a pipe read (no writer ever writes) ends soon after it is killed (B4: waits are
/// sliced and look at the kill).
fn kill_wakes_reader() -> TestResult {
    if !have_prog() {
        return skip_no_prog();
    }
    let mut c = Cleanup::default();
    let (r, w) = vfs::pipe::kernel_pipe();
    let p = c.spawn(&["block-read"], Io { stdin: Some(r as Arc<dyn File>), stdout: None, stderr: None })?;
    sched::sleep_ns(300 * MS);
    kassert!(!p.is_finished(), "block-read did not block ({:?})", p.status());
    let t0 = time::now_ns();
    p.kill(9);
    let s = ended(&p, 5 * SEC, "killed block-read")?;
    kassert!(s == proc::ExitStatus::Signaled(9), "ended {s:?}");
    klog::event!(Info, "fs", "pipe.kill_wake", ms = (time::now_ns() - t0) / MS; "a killed reader left its pipe wait");
    drop(w);
    Ok(())
}
ktest!("fs.pipe.kill_wakes_reader", Safe, kill_wakes_reader);

static FIFO_SEQ: AtomicU64 = AtomicU64::new(0);

/// `fs-pipe fifo-read P` blocks in open(P, O_RDONLY) until a writer opens: the kernel opens it for writing
/// (the waiting reader is counted already, so that open does not wait), writes and closes; the program reads
/// to the end.
fn fifo_open_waits() -> TestResult {
    if !have_prog() {
        return skip_no_prog();
    }
    let ns = vfs::root_ns();
    let mut c = Cleanup::default();
    let path = format!("/tmp/fs3-fifo-{}", FIFO_SEQ.fetch_add(1, Ordering::Relaxed));
    ns.create_fifo(path.as_bytes(), 0o666).map_err(|e| format!("mkfifo {path}: {e}"))?;
    c.path = Some(path.clone());
    let out = proc::stdio::Capture::new(4096);
    let p = c.spawn(&["fifo-read", &path], Io { stdin: None, stdout: Some(out.clone() as Arc<dyn File>), stderr: None })?;
    // Wait until the program sits in open (it counts as a reader then), at most 10 s.
    let fifo = ns.lookup(&path).map_err(|e| format!("{path}: {e}"))?;
    let start = time::now_ns();
    let w = loop {
        match ns.open_flags(path.as_bytes(), vfs::abi::O_WRONLY, 0) {
            Ok(w) => break w,
            Err(e) => {
                kassert!(e == uapi::errno::ENXIO, "kernel open for writing: {e}");
                kassert!(time::now_ns() - start < 10 * SEC, "the program never opened the FIFO");
                sched::sleep_ns(5 * MS);
            }
        }
    };
    kassert!(!p.is_finished(), "fifo-read ended before a writer came");
    w.write(b"hello fifo").map_err(|e| format!("write: {e}"))?;
    drop(w);
    let s = ended(&p, 10 * SEC, "fifo-read")?;
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    drop(fifo);
    kassert!(s == proc::ExitStatus::Exited(0) && text == "fifo got 10: hello fifo\n", "fifo-read ended {s:?}: {text:?}");
    Ok(())
}
ktest!("fs.fifo.open_waits", Safe, fifo_open_waits);

/// Eight `fs-pipe records` programs write records of 16..4096 bytes (one write each) into one kernel pipe while
/// `fs-pipe check-records` reads: on several CPUs no record may be torn or interleaved (PIPE_BUF), and every
/// writer's records arrive in order. Run with `-- -smp 16` for real parallelism.
fn many_writers() -> TestResult {
    if !have_prog() {
        return skip_no_prog();
    }
    const WRITERS: usize = 8;
    const EACH: &str = "300";
    let mut c = Cleanup::default();
    let (r, w) = vfs::pipe::kernel_pipe();
    let out = proc::stdio::Capture::new(4096);
    let err = proc::stdio::Capture::new(4096);
    let consumer = c.spawn(&["check-records", "8", EACH], Io { stdin: Some(r as Arc<dyn File>), stdout: Some(out.clone() as Arc<dyn File>), stderr: Some(err.clone() as Arc<dyn File>) })?;
    let mut writers = Vec::new();
    for i in 0..WRITERS {
        let id = format!("{i}");
        writers.push(c.spawn(&["records", &id, EACH], Io { stdin: None, stdout: Some(w.clone() as Arc<dyn File>), stderr: Some(err.clone() as Arc<dyn File>) })?);
    }
    drop(w);
    for (i, p) in writers.into_iter().enumerate() {
        let s = ended(&p, 60 * SEC, "records")?;
        kassert!(s == proc::ExitStatus::Exited(0), "writer {i} ended {s:?}: {}", String::from_utf8_lossy(&err.contents()));
        // Its files close only when the process is freed (change request 1 in plan/notes/fs-FS-3.md).
        drop(p);
    }
    let s = ended(&consumer, 60 * SEC, "check-records")?;
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    kassert!(s == proc::ExitStatus::Exited(0) && text == "records 2400 from 8 writers whole\n", "check-records ended {s:?}: {text:?} {:?}", String::from_utf8_lossy(&err.contents()));
    Ok(())
}
ktest!("fs.pipe.many_writers", Safe, many_writers);

/// Sets pipe-max-size back however the test ends.
struct MaxSizeBack(u64);

impl Drop for MaxSizeBack {
    fn drop(&mut self) {
        let _ = vfs::pipe::set_max_size(self.0);
    }
}

/// /proc/sys/fs/pipe-max-size: root writes it (rounded as round_pipe_size; EINVAL above 2^31 and for junk),
/// F_SETPIPE_SZ of an unprivileged caller and the size of its new pipes follow it; set back at the end.
fn max_size_knob() -> TestResult {
    const PATH: &str = "/proc/sys/fs/pipe-max-size";
    let ns = vfs::root_ns();
    let _back = MaxSizeBack(vfs::pipe::max_size());
    let read = || ns.read_file(PATH, 64).map(|b| String::from_utf8_lossy(&b).into_owned()).map_err(|e| format!("read {PATH}: {e}"));
    let write = |v: &str| -> Result<usize, vfs::Errno> {
        let f = ns.open_flags(PATH.as_bytes(), vfs::abi::O_WRONLY, 0)?;
        f.write(v.as_bytes())
    };
    let st = ns.lookup(PATH).map_err(|e| format!("{PATH}: {e}"))?;
    kassert!(st.inode.meta.mode() & 0o777 == 0o644, "mode {:o}", st.inode.meta.mode());
    kassert!(write("16385\n") == Ok(6), "write 16385");
    let now = read()?;
    kassert!(now == "32768\n", "after 16385: {now:?}");
    kassert!(write("2147483649") == Err(uapi::errno::EINVAL), "above 2^31 must be EINVAL");
    kassert!(write("12x") == Err(uapi::errno::EINVAL), "junk must be EINVAL");
    kassert!(write("  ") == Err(uapi::errno::EINVAL), "white space only must be EINVAL");
    kassert!(read()? == "32768\n", "a refused write changed the value");
    // A new pipe of an unprivileged creator holds at most pipe-max-size; root's keeps 64 KiB.
    let user = vfs::inode::Cred { uid: 1000, gid: 1000 };
    let (_r, w) = vfs::pipe::new_pipe(user, 0).map_err(|e| format!("pipe: {e}"))?;
    let got = w.file.fcntl(vfs::pipe::F_GETPIPE_SZ, 0, user);
    kassert!(matches!(got, Some(Ok(32768))), "unprivileged new pipe: {got:?}");
    let got = w.file.fcntl(vfs::pipe::F_SETPIPE_SZ, 65536, user);
    kassert!(matches!(got, Some(Err(uapi::errno::EPERM))), "unprivileged F_SETPIPE_SZ above the limit: {got:?}");
    let (_r2, w2) = vfs::pipe::new_pipe(vfs::inode::Cred::ROOT, 0).map_err(|e| format!("pipe: {e}"))?;
    let got = w2.file.fcntl(vfs::pipe::F_GETPIPE_SZ, 0, vfs::inode::Cred::ROOT);
    kassert!(matches!(got, Some(Ok(65536))), "root's new pipe: {got:?}");
    Ok(())
}
ktest!("fs.pipe.max_size_knob", Safe, max_size_knob);


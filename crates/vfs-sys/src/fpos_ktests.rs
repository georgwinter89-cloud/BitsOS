//! FSR ktest `fs.fpos.contention` (Safe): eight kernel threads read a regular file by read(2) and write to /dev/null
//! by write(2), once on ONE description in one table all of them use (Linux locks `f_pos_lock` there), once each on
//! its own description in its own table counted for one thread (`vfs::fpos`): the second takes the position lock
//! not once and nobody waits; /dev/null (not FMODE_ATOMIC_POS) never takes it, shared or not. Counted on the
//! test's own descriptions (`OpenFile::pos_locks`): the kernel-wide class `vfs.file.pos` also counts whatever else
//! the booted kernel reads meanwhile (a service, any getdents), which is no measure of these calls.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::EFAULT;
use vfs::devfs::{Mem, MemDev};
use vfs::fd::{FdTable, FsState, OpenFile, OpenFlags};
use vfs::fpos::FdUse;
use vfs::mount_ns::MountNs;
use vfs::sys::{self, Sys, UserMem};

const SEC: u64 = 1_000_000_000;
const THREADS: usize = 8;
const CALLS: u64 = 4000;
const LEN: usize = 256 << 10;
const BUF: u64 = 0x10_0000;

ktest!("fs.fpos.contention", Safe, contention);

/// A program's memory that reads as zeros and takes every write.
struct Sink;

impl UserMem for Sink {
    fn read(&self, _addr: u64, buf: &mut [u8]) -> bool {
        buf.fill(0);
        true
    }
    fn write(&self, _addr: u64, _data: &[u8]) -> bool {
        true
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
}

/// One thread's calls: `CALLS` reads of 16 KiB from `fd` (back to the start at the end) and as many writes to
/// `null`. Returns the calls that failed.
fn calls(ns: &Arc<MountNs>, fds: &FdTable, fd: u64, null: u64) -> u64 {
    let fs = FsState::new();
    let s = Sys { mem: &Sink, fds, fs: &fs, ns: ns, cred: vfs::inode::Cred::ROOT, subject: policy::Subject::KERNEL };
    let mut bad = 0;
    for _ in 0..CALLS {
        let n = sys::read(&s, [fd, BUF, 16 << 10, 0, 0, 0]);
        if n == 0 {
            if sys::lseek(&s, [fd, 0, 0, 0, 0, 0]) != 0 {
                bad += 1;
            }
        } else if n < 0 {
            bad += 1;
        }
        if sys::write(&s, [null, BUF, 16 << 10, 0, 0, 0]) != 16 << 10 {
            bad += 1;
        }
    }
    bad
}

fn file(ns: &MountNs, flags: OpenFlags) -> Result<Arc<OpenFile>, String> {
    let f = ns.open("/fpos").map_err(|e| format!("open: {e}"))?;
    Ok(OpenFile::new(f, flags))
}

fn run(name: &'static str, tables: Vec<(Arc<FdTable>, u64, u64)>, ns: &Arc<MountNs>) -> Result<u64, String> {
    let bad = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for (t, fd, null) in tables {
        let (ns, bad) = (ns.clone(), bad.clone());
        hs.push(sched::spawn(name, sched::Class::Normal, move || {
            bad.fetch_add(calls(&ns, &t, fd, null), Ordering::SeqCst);
        }).map_err(|e| format!("spawn: {e:?}"))?);
    }
    let mut joined = true;
    for h in hs {
        joined &= h.join_timeout(60 * SEC);
    }
    kassert!(joined, "{name}: a thread did not finish in 60 s");
    Ok(bad.load(Ordering::SeqCst))
}

fn contention() -> TestResult {
    let ns = Arc::new(MountNs::new_empty());
    ns.create_file("/fpos", 0o644, alloc::vec![7u8; LEN]).map_err(|e| format!("create: {e}"))?;
    let rd = OpenFlags { read: true, ..Default::default() };
    let wr = OpenFlags { write: true, ..Default::default() };
    let null = OpenFile::new(Arc::new(MemDev(Mem::Null)), wr);
    // One description, one table, eight threads counted on it.
    let shared = Arc::new(FdTable::new());
    let users: Vec<FdUse> = (0..THREADS).map(|_| FdUse::new(&shared)).collect();
    let one = file(&ns, rd)?;
    let fd = shared.install(one.clone()).map_err(|e| format!("install: {e}"))? as u64;
    let nfd = shared.install(null.clone()).map_err(|e| format!("install: {e}"))? as u64;
    let bad_s = run("fs-ktest-fpos-s", (0..THREADS).map(|_| (shared.clone(), fd, nfd)).collect(), &ns)?;
    drop(users);
    let s = one.pos_locks();
    drop(one);
    // A description and a table of its own per thread; /dev/null still one description in all of them.
    let mut tables = Vec::new();
    let mut own = Vec::new();
    let mut kept = Vec::new();
    for _ in 0..THREADS {
        let t = Arc::new(FdTable::new());
        own.push(FdUse::new(&t));
        // The table's slot is the description's only reference, as for a program's own fd; the table is kept to
        // read the count afterwards (that adds no holder of the description).
        let fd = t.install(file(&ns, rd)?).map_err(|e| format!("install: {e}"))? as u64;
        let nfd = t.install(null.clone()).map_err(|e| format!("install: {e}"))? as u64;
        kept.push((t.clone(), fd));
        tables.push((t, fd, nfd));
    }
    let bad_u = run("fs-ktest-fpos-u", tables, &ns)?;
    drop(own);
    let mut u = 0;
    for (t, fd) in &kept {
        u += t.get(*fd as usize).map_err(|e| format!("get: {e}"))?.pos_locks();
    }
    let n = null.pos_locks();
    let calls = THREADS as u64 * CALLS;
    klog::event!(Info, "ktest", "fs.fpos.contention", calls = calls, shared_locks = s, own_locks = u, null_locks = n, cpus = sched::online_cpus() as u64);
    kassert!(bad_s == 0 && bad_u == 0, "failed calls: shared {bad_s}, own {bad_u}");
    // Shared: every read and every lseek of the regular file locks; /dev/null's writes never do.
    kassert!(s >= calls && s <= 2 * calls, "one description of eight threads: {s} locked calls for {calls} reads");
    kassert!(u == 0, "descriptions of one thread each: {u} locked calls");
    kassert!(n == 0, "/dev/null: {n} locked calls");
    Ok(())
}

//! PF-4 ktests: reads of a cached tmpfs file in the kernel (real page-cache frames) copy page by page and end
//! exactly at the first user page that faults (a stand-in user memory that faults from a chosen address; the
//! real PROT_NONE case is abi-pfread `fault`), and readers on several CPUs (`-- -smp 16`) racing truncates and
//! writes of the same file only ever see the file's bytes. Every byte is checked. Rerunnable (Safe): each run
//! makes its own file system view and file.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::EFAULT;
use vfs::fd::{FdTable, FsState, OpenFile, OpenFlags};
use vfs::mount_ns::MountNs;
use vfs::sys::{self, Sys, UserMem};

const SEC: u64 = 1_000_000_000;
const P: u64 = 4096;
const BASE: u64 = 0x10_0000;
const NR_READ: usize = 0;
const NR_PREAD: usize = 17;

ktest!("fs.pf4.partial_fault", Safe, partial_fault);
ktest!("fs.pf4.parallel_readers", Safe, parallel_readers);

fn pat(o: u64) -> u8 {
    (o % 251) as u8 + 1
}

fn pattern(from: u64, n: usize) -> Vec<u8> {
    (0..n as u64).map(|i| pat(from + i)).collect()
}

/// Kernel memory standing in for a program's: writable from `BASE` below `bad`, a fault from there on.
struct FaultMem {
    bytes: RefCell<Vec<u8>>,
    bad: Cell<u64>,
    /// Page by page through `write_page` (`direct_page_copy`); false: the gathering path the kernel uses now.
    direct: Cell<bool>,
}

impl UserMem for FaultMem {
    fn read(&self, _addr: u64, _buf: &mut [u8]) -> bool {
        false
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let Some(end) = addr.checked_add(data.len() as u64) else { return false };
        let mut m = self.bytes.borrow_mut();
        if addr < BASE || end > self.bad.get() || end > BASE + m.len() as u64 {
            return false;
        }
        let o = (addr - BASE) as usize;
        m[o..o + data.len()].copy_from_slice(data);
        true
    }
    fn direct_page_copy(&self) -> bool {
        self.direct.get()
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
}

fn partial_fault() -> TestResult {
    let size = 3 * P as usize + 100;
    let ns = Arc::new(MountNs::new_empty());
    ns.create_file("/pf4", 0o644, pattern(0, size)).map_err(|e| format!("create: {e}"))?;
    let fds = FdTable::new();
    let file = ns.open("/pf4").map_err(|e| format!("open: {e}"))?;
    kassert!(file.read_cache().is_some(), "a tmpfs file reads through its page cache");
    let fd = fds.install(OpenFile::new(file, OpenFlags { read: true, ..Default::default() })).map_err(|e| format!("install: {e}"))? as u64;
    let fs = FsState::new();
    let room = 5 * P as usize;
    let mem = FaultMem { bytes: RefCell::new(vec![0xEE; room]), bad: Cell::new(BASE), direct: Cell::new(false) };
    let s = Sys { mem: &mem, fds: &fds, fs: &fs, ns: &ns, cred: vfs::inode::Cred::ROOT, subject: policy::Subject::KERNEL };
    let mut cases = 0u32;
    for &off in &[0u64, 1, 4095, 4096, 5000, 3 * P] {
        for &mis in &[0u64, 1, 2048, 4095] {
            for (bad_pages, direct) in (0..=4u64).flat_map(|b| [(b, false), (b, true)]) {
                mem.direct.set(direct);
                mem.bytes.borrow_mut().fill(0xEE);
                mem.bad.set(BASE + bad_pages * P);
                let buf = BASE + mis;
                let r = sys::call(&s, NR_PREAD, [fd, buf, 3 * P, off, 0, 0]);
                let want = (size as u64 - off).min(3 * P).min((BASE + bad_pages * P).saturating_sub(buf));
                let expect = if want == 0 { -(EFAULT as i64) } else { want as i64 };
                kassert!(r == expect, "pread at {off} into BASE+{mis}, {bad_pages} pages writable (direct {direct}): {r}, Linux {expect}");
                let m = mem.bytes.borrow();
                let o = mis as usize;
                let n = r.max(0) as usize;
                kassert!(m[o..o + n] == pattern(off, n)[..], "a byte differs (pread at {off} into BASE+{mis})");
                kassert!(m[o + n..].iter().all(|&b| b == 0xEE), "bytes written past the {n} returned");
                cases += 1;
            }
        }
    }
    // read(2): the position moves by what reached the program, the next read goes on there.
    mem.direct.set(false);
    mem.bytes.borrow_mut().fill(0xEE);
    mem.bad.set(BASE + 2 * P);
    let r = sys::call(&s, NR_READ, [fd, BASE + 10, 3 * P, 0, 0, 0]);
    kassert!(r == 2 * P as i64 - 10, "read stopped at {r}, Linux {}", 2 * P - 10);
    mem.bad.set(BASE + 5 * P);
    let r2 = sys::call(&s, NR_READ, [fd, BASE, 4 * P, 0, 0, 0]);
    kassert!(r2 == size as i64 - r, "the next read gave {r2}, want {}", size as i64 - r);
    kassert!(mem.bytes.borrow()[..r2 as usize] == pattern(r as u64, r2 as usize)[..], "the next read's bytes differ");
    fds.close_all();
    klog::event!(Info, "ktest", "fs.pf4.partial_fault", cases = cases as u64; "pread cases, every byte checked");
    Ok(())
}

fn parallel_readers() -> TestResult {
    const READERS: usize = 8;
    const ROUNDS: u64 = 1500;
    let len = 6 * P as usize;
    let all = Arc::new(pattern(0, len));
    let ns = Arc::new(MountNs::new_empty());
    ns.create_file("/pf4r", 0o644, (*all).clone()).map_err(|e| format!("create: {e}"))?;
    let file = ns.open("/pf4r").map_err(|e| format!("open: {e}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let bad = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for t in 0..READERS as u64 {
        let (file, stop, bad, reads) = (file.clone(), stop.clone(), bad.clone(), reads.clone());
        hs.push(
            sched::spawn("fs-ktest-pf4r", sched::Class::Normal, move || {
                let Some(c) = file.read_cache() else { return };
                let mut b = vec![0u8; len];
                let mut k = t + 1;
                let mut mine = 0u64;
                while !stop.load(Ordering::Relaxed) || mine < 50 {
                    k = k.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    let pos = (k >> 33) % len as u64;
                    let want = ((k >> 13) as usize % len).max(1);
                    let Ok(n) = c.read_at(pos, &mut b[..want]) else {
                        bad.fetch_add(1, Ordering::SeqCst);
                        break;
                    };
                    if n > want || b[..n].iter().enumerate().any(|(i, &v)| v != pat(pos + i as u64)) {
                        bad.fetch_add(1, Ordering::SeqCst);
                    }
                    mine += 1;
                    if mine % 64 == 0 {
                        sched::yield_now();
                    }
                }
                reads.fetch_add(mine, Ordering::SeqCst);
            })
            .map_err(|e| format!("spawn: {e:?}"))?,
        );
    }
    let c = file.read_cache().ok_or("no page cache")?;
    let mut k = 7u64;
    let mut err = None;
    for i in 0..ROUNDS {
        k = k.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        // Whole pages only: a cut inside a page zeroes it in place, which a racing read may copy (as on Linux).
        let cut = (k >> 20) % (len as u64 / P) * P;
        if let Err(e) = c.truncate(cut).and_then(|_| c.write_at(cut, &all[cut as usize..])) {
            err = Some(e);
            break;
        }
        if i % 32 == 0 {
            sched::yield_now();
        }
    }
    stop.store(true, Ordering::Relaxed);
    let mut joined = true;
    for h in hs {
        joined &= h.join_timeout(20 * SEC);
    }
    kassert!(joined, "a reader did not finish in 20 s");
    kassert!(err.is_none(), "truncate/write failed: {:?}", err);
    kassert!(bad.load(Ordering::SeqCst) == 0, "{} reads returned a byte the file never held", bad.load(Ordering::SeqCst));
    kassert!(c.size() == len as u64, "size {} after the last write-back", c.size());
    klog::event!(Info, "ktest", "fs.pf4.parallel_readers", reads = reads.load(Ordering::SeqCst), cpus = sched::online_cpus() as u64; "reads racing truncates, every byte checked");
    Ok(())
}

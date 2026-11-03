//! PF-4 host tests: reads of cached pages take no `vfs.inode.data`, copy page by page to the program and end
//! exactly at the first user page that faults (Linux's short copy: the bytes before it, EFAULT when none),
//! and readers racing truncates and writes only ever see bytes the file held.

extern crate std;

use crate::fd::{FdTable, OpenFile, OpenFlags};
use crate::frame::PAGE;
use crate::mount_ns::MountNs;
use crate::pagecache::PageCache;
use crate::pool::Pool;
use crate::sys::{self, Sys, UserMem};
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use uapi::errno::*;

const P: u64 = PAGE as u64;
const BASE: u64 = 0x2000_0000;
const NR_READ: usize = 0;
const NR_PREAD: usize = 17;

/// The byte at file offset `o` (never 0, so a hole read by mistake shows).
fn pat(o: u64) -> u8 {
    (o % 251) as u8 + 1
}

fn pattern(from: u64, n: usize) -> Vec<u8> {
    (0..n as u64).map(|i| pat(from + i)).collect()
}

/// User memory from `BASE`: writable below `bad`, every byte at or above it faults (a PROT_NONE page).
struct FaultMem {
    bytes: RefCell<Vec<u8>>,
    bad: Cell<u64>,
    pages_copied: Cell<u64>,
    writes: Cell<u64>,
    /// Copies page by page (`UserMem::direct_page_copy`); false: the gathering path.
    direct: bool,
    /// PF-5: page copies inside a lockless section too (`UserMem::write_page_nofault`): a fault there ends the
    /// lockless part and the locked path faults (here: the same answer, the page stays bad).
    nofault: bool,
}

impl FaultMem {
    fn new(len: usize, bad: u64) -> FaultMem {
        FaultMem { bytes: RefCell::new(vec![0xEE; len]), bad: Cell::new(bad), pages_copied: Cell::new(0), writes: Cell::new(0), direct: true, nofault: true }
    }
    fn gathering(len: usize, bad: u64) -> FaultMem {
        FaultMem { direct: false, ..FaultMem::new(len, bad) }
    }
    /// Direct copies on the locked path only (no no-fault copy).
    fn faulting(len: usize, bad: u64) -> FaultMem {
        FaultMem { nofault: false, ..FaultMem::new(len, bad) }
    }
    fn of_mode(mode: u8, len: usize, bad: u64) -> FaultMem {
        match mode {
            0 => FaultMem::new(len, bad),
            1 => FaultMem::faulting(len, bad),
            _ => FaultMem::gathering(len, bad),
        }
    }
    fn get(&self, addr: u64, n: usize) -> Vec<u8> {
        let off = (addr - BASE) as usize;
        self.bytes.borrow()[off..off + n].to_vec()
    }
    fn fill(&self, b: u8) {
        self.bytes.borrow_mut().fill(b);
    }
}

impl UserMem for FaultMem {
    fn read(&self, _addr: u64, _buf: &mut [u8]) -> bool {
        false
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.writes.set(self.writes.get() + 1);
        let Some(end) = addr.checked_add(data.len() as u64) else { return false };
        let len = self.bytes.borrow().len() as u64;
        if addr < BASE || end > self.bad.get() || end > BASE + len {
            return false;
        }
        let off = (addr - BASE) as usize;
        self.bytes.borrow_mut()[off..off + data.len()].copy_from_slice(data);
        true
    }
    fn direct_page_copy(&self) -> bool {
        self.direct
    }
    fn write_page(&self, addr: u64, page: &crate::pagecache::CachePage, off: usize, n: usize) -> usize {
        self.pages_copied.set(self.pages_copied.get() + 1);
        crate::sys_page::through_stack(self, addr, page, off, n)
    }
    fn nofault_page_copy(&self) -> bool {
        self.nofault
    }
    fn write_page_nofault(&self, addr: u64, page: Option<&crate::pagecache::CachePage>, off: usize, n: usize) -> usize {
        self.pages_copied.set(self.pages_copied.get() + 1);
        let n = n.min(PAGE - off);
        let mut b = [0u8; PAGE];
        if let Some(p) = page {
            p.read(off, &mut b[..n]);
        }
        // The bytes before the first user page that is not writable.
        let len = self.bytes.borrow().len() as u64;
        let room = self.bad.get().min(BASE + len).saturating_sub(addr).min(n as u64) as usize;
        if addr < BASE || room == 0 {
            return 0;
        }
        let o = (addr - BASE) as usize;
        self.bytes.borrow_mut()[o..o + room].copy_from_slice(&b[..room]);
        room
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, Errno> {
        Err(EFAULT)
    }
}

fn env(size: usize) -> (MountNs, FdTable, i64) {
    let ns = MountNs::new_empty();
    ns.create_file("/f", 0o644, pattern(0, size)).unwrap();
    let fds = FdTable::new();
    let fd = fds.install(OpenFile::new(ns.open("/f").unwrap(), OpenFlags { read: true, ..Default::default() })).unwrap();
    (ns, fds, fd as i64)
}

fn sys_of<'a>(mem: &'a FaultMem, fds: &'a FdTable, ns: &'a Arc<MountNs>, fs: &'a crate::fd::FsState) -> Sys<'a> {
    Sys { mem, fds, fs, ns, cred: crate::cases_test::USER, subject: crate::cases_test::SUBJECT }
}

#[test]
fn a_fault_ends_the_read_at_the_first_user_page_that_faults() {
    let size = 3 * PAGE + 100;
    let (ns, fds, fd) = env(size);
    let ns = Arc::new(ns);
    let fs = crate::fd::FsState::new();
    let room = 5 * PAGE;
    for &off in &[0u64, 1, 100, 4095, 4096, 5000, 3 * P] {
        for &mis in &[0u64, 1, 2048, 4095] {
            for (bad_pages, direct) in (0..=4u64).flat_map(|b| [(b, 0u8), (b, 1), (b, 2)]) {
                let mem = FaultMem::of_mode(direct, room, BASE + bad_pages * P);
                let s = sys_of(&mem, &fds, &ns, &fs);
                let buf = BASE + mis;
                let count = 3 * P;
                let r = sys::call(&s, NR_PREAD, [fd as u64, buf, count, off, 0, 0]);
                let avail = (size as u64 - off).min(count);
                let writable = (BASE + bad_pages * P).saturating_sub(buf);
                let want = avail.min(writable);
                let case = (off, mis, bad_pages, direct);
                if want == 0 {
                    assert_eq!(r, -EFAULT, "{case:?}");
                } else {
                    assert_eq!(r, want as i64, "{case:?}");
                }
                if r > 0 {
                    assert_eq!(mem.get(buf, r as usize), pattern(off, r as usize), "every byte {case:?}");
                }
                // Nothing written past what was returned (inside the writable part).
                let end = buf + r.max(0) as u64;
                let tail = (BASE + bad_pages * P).min(BASE + room as u64).saturating_sub(end) as usize;
                assert!(mem.get(end, tail).iter().all(|&b| b == 0xEE), "untouched after {case:?}");
            }
        }
    }
}

#[test]
fn read_moves_the_position_by_what_reached_the_program() {
    let size = 4 * PAGE;
    let (ns, fds, fd) = env(size);
    let ns = Arc::new(ns);
    let fs = crate::fd::FsState::new();
    // Two user pages writable: a read of three pages from BASE + 10 stops where the third begins.
    let mem = FaultMem::new(4 * PAGE, BASE + 2 * P);
    let s = sys_of(&mem, &fds, &ns, &fs);
    let r = sys::call(&s, NR_READ, [fd as u64, BASE + 10, 3 * P, 0, 0, 0]);
    assert_eq!(r, 2 * P as i64 - 10);
    assert_eq!(mem.get(BASE + 10, r as usize), pattern(0, r as usize));
    // The next read goes on exactly there.
    mem.bad.set(BASE + 4 * P);
    mem.fill(0xEE);
    let r2 = sys::call(&s, NR_READ, [fd as u64, BASE, 4 * P, 0, 0, 0]);
    assert_eq!(r2, size as i64 - r);
    assert_eq!(mem.get(BASE, r2 as usize), pattern(r as u64, r2 as usize));
    assert_eq!(sys::call(&s, NR_READ, [fd as u64, BASE, 4 * P, 0, 0, 0]), 0, "end of file");
    // A first page that faults: EFAULT, the position stays.
    mem.bad.set(BASE);
    assert_eq!(sys::call(&s, 8, [fd as u64, 0, 0, 0, 0, 0]), 0, "lseek to 0");
    assert_eq!(sys::call(&s, NR_READ, [fd as u64, BASE, 16, 0, 0, 0]), -EFAULT);
    mem.bad.set(BASE + P);
    assert_eq!(sys::call(&s, NR_READ, [fd as u64, BASE, 16, 0, 0, 0]), 16);
    assert_eq!(mem.get(BASE, 16), pattern(0, 16));
}

#[test]
fn reads_copy_page_by_page_without_a_bounce() {
    let size = 3 * PAGE;
    let (ns, fds, fd) = env(size);
    let ns = Arc::new(ns);
    let fs = crate::fd::FsState::new();
    let mem = FaultMem::new(4 * PAGE, BASE + 4 * P);
    let s = sys_of(&mem, &fds, &ns, &fs);
    assert_eq!(sys::call(&s, NR_PREAD, [fd as u64, BASE + 7, 3 * P, 5, 0, 0]), 3 * P as i64 - 5);
    // Three pages touched (5..4096, then two whole ones), one copy from each.
    assert_eq!(mem.pages_copied.get(), 3);
    assert_eq!(mem.get(BASE + 7, size - 5), pattern(5, size - 5));
}

#[test]
fn holes_read_as_zeros_and_a_fault_in_one_counts() {
    for direct in [true, false] {
        let ns = MountNs::new_empty();
        ns.create_file("/h", 0o644, Vec::new()).unwrap();
        let file = ns.open("/h").unwrap();
        file.write_at(2 * P, &pattern(2 * P, 100)).unwrap();
        let fds = FdTable::new();
        let fd = fds.install(OpenFile::new(file, OpenFlags { read: true, ..Default::default() })).unwrap() as u64;
        let ns = Arc::new(ns);
        let fs = crate::fd::FsState::new();
        let mem = if direct { FaultMem::new(4 * PAGE, BASE + 4 * P) } else { FaultMem::gathering(4 * PAGE, BASE + 4 * P) };
        let s = sys_of(&mem, &fds, &ns, &fs);
        assert_eq!(sys::call(&s, NR_PREAD, [fd, BASE, 4 * P, 0, 0, 0]), 2 * P as i64 + 100);
        assert!(mem.get(BASE, 2 * PAGE).iter().all(|&b| b == 0));
        assert_eq!(mem.get(BASE + 2 * P, 100), pattern(2 * P, 100));
        mem.fill(0xEE);
        mem.bad.set(BASE + P);
        assert_eq!(sys::call(&s, NR_PREAD, [fd, BASE + 1, 4 * P, 0, 0, 0]), P as i64 - 1, "hole, fault in the 2nd user page ({direct})");
        assert!(mem.get(BASE + 1, PAGE - 1).iter().all(|&b| b == 0));
    }
}

/// Without a copy from the frame the pages of one read reach the program in one user copy per buffer (each
/// takes `mm.as` in the kernel), as many as the bounce path made.
#[test]
fn gathering_copies_once_per_buffer() {
    let size = 40 * PAGE;
    let (ns, fds, fd) = env(size);
    let ns = Arc::new(ns);
    let fs = crate::fd::FsState::new();
    let mem = FaultMem::gathering(40 * PAGE, BASE + 40 * P);
    let s = sys_of(&mem, &fds, &ns, &fs);
    for (count, writes) in [(16u64, 1u64), (4096, 1), (3 * P, 1), (16 * P, 1), (40 * P - 3, 3)] {
        mem.writes.set(0);
        mem.fill(0xEE);
        assert_eq!(sys::call(&s, NR_PREAD, [fd as u64, BASE + 3, count, 1, 0, 0]), count as i64);
        assert_eq!(mem.writes.get(), writes, "user copies for {count} bytes");
        assert_eq!(mem.pages_copied.get(), 0);
        assert_eq!(mem.get(BASE + 3, count as usize), pattern(1, count as usize));
    }
}

#[test]
fn a_cached_read_does_not_wait_for_the_data_lock() {
    let c = Arc::new(PageCache::tmpfs(Arc::new(Pool::new("pf4-lock"))));
    c.write_at(0, &pattern(0, 3 * PAGE)).unwrap();
    let held = c.hold_data_lock();
    let (tx, rx) = mpsc::channel();
    let c2 = c.clone();
    let t = std::thread::spawn(move || {
        let mut b = vec![0u8; 3 * PAGE];
        let n = c2.read_at(10, &mut b).unwrap();
        tx.send((n, b)).unwrap();
    });
    let (n, b) = rx.recv_timeout(Duration::from_secs(20)).expect("the read waited for vfs.inode.data");
    drop(held);
    t.join().unwrap();
    assert_eq!(n, 3 * PAGE - 10);
    assert_eq!(&b[..n], &pattern(10, n)[..]);
}

/// Readers of one file while another thread truncates it to random page boundaries and writes it back: every
/// byte a read returns is the file's byte at that offset (a cut-off page is kept alive by the reader's
/// reference, a page past a shrinking size is never read as a hole, growth shows its pages before its size).
/// A cut inside a page zeroes the rest of that page in place; a read that sampled the old size may copy those
/// zeros, as on Linux (`filemap_read` samples `i_size` before `truncate_inode_partial_folio` zeroes), so the
/// cuts here are whole pages.
#[test]
fn readers_racing_truncate_and_writes_see_only_the_files_bytes() {
    let c = Arc::new(PageCache::tmpfs(Arc::new(Pool::new("pf4-race"))));
    let len = 6 * PAGE;
    let all = pattern(0, len);
    c.write_at(0, &all).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..8u64)
        .map(|t| {
            let (c, stop) = (c.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut b = vec![0u8; len];
                let mut reads = 0u64;
                let mut seen = 0u64;
                let mut k = t;
                while !stop.load(Ordering::Relaxed) || reads < 200 {
                    k = k.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    let pos = (k >> 33) % len as u64;
                    let want = ((k >> 13) as usize % len).max(1);
                    let n = c.read_at(pos, &mut b[..want]).unwrap();
                    assert!(n <= want && pos + n as u64 <= len as u64);
                    for (i, &v) in b[..n].iter().enumerate() {
                        assert_eq!(v, pat(pos + i as u64), "byte {} of a read at {pos}", i);
                    }
                    reads += 1;
                    seen += n as u64;
                }
                (reads, seen)
            })
        })
        .collect();
    let mut k = 99u64;
    for _ in 0..3000 {
        k = k.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let cut = (k >> 20) % (len / PAGE) as u64 * P;
        c.truncate(cut).unwrap();
        // Back to the whole file, by writes only (a growing truncate would add real zeros).
        c.write_at(cut, &all[cut as usize..]).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let mut total = 0;
    for r in readers {
        let (reads, seen) = r.join().unwrap();
        assert!(reads >= 200);
        total += seen;
    }
    assert!(total > 0);
    assert_eq!(c.size(), len as u64);
}

/// A page something besides a read holds (here a probe's reference, as a mapping's pin) is not taken by a hole
/// punch (it is zeroed in place) and leaves with the next punch once free.
#[test]
fn a_held_page_is_zeroed_in_place_by_a_punch() {
    use crate::abi::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE};
    let c = PageCache::tmpfs(Arc::new(Pool::new("pf4-punch")));
    c.write_at(0, &pattern(0, 2 * PAGE)).unwrap();
    let held = c.cached(0).unwrap();
    c.fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, 2 * P).unwrap();
    assert!(c.cached(1).is_none(), "the free page left");
    let still = c.cached(0).expect("the held page stays at its index");
    let mut b = vec![1u8; PAGE];
    still.read(0, &mut b);
    assert!(b.iter().all(|&x| x == 0));
    drop((held, still));
    c.fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, P).unwrap();
    assert!(c.cached(0).is_none());
    assert_eq!(c.size(), 2 * P);
}

/// A page only a read holds for its copy leaves the index with the punch at once (SEEK_HOLE, the page count
/// and the charge see the hole while the read still copies), the read gets the old bytes, and the frame and a
/// size-charged file's extra page of charge go when the read ends. Both charge kinds.
#[test]
fn a_read_held_page_leaves_with_the_punch() {
    use crate::abi::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE};
    for lazy in [true, false] {
        let pool = Arc::new(Pool::new("pf4-punch-read"));
        let c = if lazy { PageCache::tmpfs(pool.clone()) } else { PageCache::tmpfs_accounted(pool.clone()) };
        if !lazy {
            c.truncate(2 * P).unwrap();
        }
        let data = pattern(0, 2 * PAGE);
        c.write_at(0, &data).unwrap();
        let before = pool.used();
        let mut got = vec![0u8; PAGE];
        let mut during = None;
        let r = c.read_direct(0, PAGE, &mut |page, off, n, done| {
            let p = page.expect("page 0 is cached");
            c.fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, P).unwrap();
            during = Some((c.cached(0).is_none(), c.seek_data(0, true), c.pages(), pool.used()));
            p.read(off, &mut got[done..done + n]);
            n
        });
        assert_eq!(r, Ok((PAGE, false)));
        // PF-5: the read's reference is gone; the index's leaves through the grace queue.
        mm_core::grace::expedite();
        assert_eq!(got, data[..PAGE], "the read copies the bytes of before the punch (lazy {lazy})");
        let (gone, hole, pages, used) = during.unwrap();
        assert!(gone, "the read-held page left the index (lazy {lazy})");
        assert_eq!(hole, Some(0), "SEEK_HOLE sees the hole during the read (lazy {lazy})");
        assert_eq!(pages, 1);
        // Lazy: the page keeps its own charge until its frame goes; size-charged: one extra page meanwhile.
        assert_eq!(used, if lazy { before } else { before + 1 }, "lazy {lazy}");
        assert_eq!(pool.used(), if lazy { before - 1 } else { before }, "the charge after the read (lazy {lazy})");
        let mut b = vec![1u8; PAGE];
        assert_eq!(c.read_at(0, &mut b), Ok(PAGE));
        assert!(b.iter().all(|&x| x == 0));
    }
}

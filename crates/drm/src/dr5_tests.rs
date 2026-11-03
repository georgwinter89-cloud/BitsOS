//! DR-5 host tests: /dev/udmabuf's create calls and their errors, the memfd's pages shared with the buffer, the
//! dma-buf file (fstat, lseek, read/write, fdinfo, link, names), DMA_BUF_IOCTL_SYNC, poll with fences (keys,
//! wakes from another thread), EXPORT/IMPORT_SYNC_FILE, the reservation object's rules and no callbacks or
//! registry entries left behind.

use crate::dma_buf::{self, DmaBuf, MapOps};
use crate::fence::{self, Fence};
use crate::resv::{Resv, Usage};
use crate::sync_file::{self, SyncFile};
use crate::udmabuf::{self, Item, EBADFD, UDMABUF_CREATE, UDMABUF_CREATE_LIST, UDMABUF_FLAGS_CLOEXEC};
use crate::uapi::*;
use crate::wire::Wire;
use core::sync::atomic::{AtomicBool, Ordering};
use linux::errno::*;
use mm::device::DeviceOps;
use std::string::String;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::vec::Vec;
use std::{thread, vec};
use vfs::fd::{FdTable, OpenFile};
use vfs::poll::Events;
use vfs::File;

const BASE: u64 = 0x10000;
const P: u64 = 4096;
const SHRINK: u32 = vfs::memfd::F_SEAL_SHRINK;

struct Mem {
    b: Mutex<Vec<u8>>,
    kill: AtomicBool,
}

impl Mem {
    fn new() -> Mem {
        Mem { b: Mutex::new(vec![0; 8192]), kill: AtomicBool::new(false) }
    }
    fn set(&self, off: usize, d: &[u8]) {
        self.b.lock().unwrap()[off..off + d.len()].copy_from_slice(d);
    }
    fn get(&self, off: usize, n: usize) -> Vec<u8> {
        self.b.lock().unwrap()[off..off + n].to_vec()
    }
    fn put<T: Wire>(&self, off: usize, v: &T) {
        let mut b = vec![0u8; T::SIZE];
        v.encode(&mut b).unwrap();
        self.set(off, &b);
    }
    fn take<T: Wire>(&self, off: usize) -> T {
        T::decode(&self.get(off, T::SIZE)).unwrap()
    }
    /// `struct udmabuf_create` at 0.
    fn create(&self, memfd: u32, flags: u32, offset: u64, size: u64) {
        let mut b = vec![0u8; 24];
        b[0..4].copy_from_slice(&memfd.to_le_bytes());
        b[4..8].copy_from_slice(&flags.to_le_bytes());
        b[8..16].copy_from_slice(&offset.to_le_bytes());
        b[16..24].copy_from_slice(&size.to_le_bytes());
        self.set(0, &b);
    }
    /// `struct udmabuf_create_list` with its items at 0.
    fn list(&self, flags: u32, count: u32, items: &[Item]) {
        let mut b = vec![0u8; 8 + 24 * items.len()];
        b[0..4].copy_from_slice(&flags.to_le_bytes());
        b[4..8].copy_from_slice(&count.to_le_bytes());
        for (k, it) in items.iter().enumerate() {
            let o = 8 + 24 * k;
            b[o..o + 4].copy_from_slice(&it.memfd.to_le_bytes());
            b[o + 8..o + 16].copy_from_slice(&it.offset.to_le_bytes());
            b[o + 16..o + 24].copy_from_slice(&it.size.to_le_bytes());
        }
        self.set(0, &b);
    }
}

impl vfs::sys::UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let b = self.b.lock().unwrap();
        let Some(s) = addr.checked_sub(BASE).and_then(|s| usize::try_from(s).ok()) else { return false };
        if s + buf.len() > b.len() {
            return false;
        }
        buf.copy_from_slice(&b[s..s + buf.len()]);
        true
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let mut b = self.b.lock().unwrap();
        let Some(s) = addr.checked_sub(BASE).and_then(|s| usize::try_from(s).ok()) else { return false };
        if s + data.len() > b.len() {
            return false;
        }
        b[s..s + data.len()].copy_from_slice(data);
        true
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, vfs::Errno> {
        let mut out = Vec::new();
        for k in 0..max as u64 {
            let mut c = [0u8];
            if !self.read(addr + k, &mut c) {
                return Err(EFAULT);
            }
            if c[0] == 0 {
                return Ok(out);
            }
            out.push(c[0]);
        }
        Err(ENAMETOOLONG)
    }
    fn interrupted(&self) -> bool {
        self.kill.load(Ordering::SeqCst)
    }
}

/// A memfd of `pages` pages with `seals` and its fd in `fds`.
fn memfd(fds: &FdTable, pages: u64, seals: u32) -> (Arc<OpenFile>, u32) {
    let f = vfs::memfd::create(b"dr5", vfs::memfd::MFD_ALLOW_SEALING, vfs::inode::Cred::ROOT).unwrap();
    f.inode().unwrap().truncate(pages * P).unwrap();
    if seals != 0 {
        vfs::memfd::seal_fcntl(&f, vfs::memfd::F_ADD_SEALS, seals as u64).unwrap();
    }
    let n = fds.install_flags(f.clone(), 0, false).unwrap() as u32;
    (f, n)
}

fn buf_of(fds: &FdTable, fd: i64) -> Arc<DmaBuf> {
    dma_buf::as_dma_buf(&fds.get(fd as usize).unwrap().file).expect("a dma-buf")
}

/// A udmabuf of 4 pages of a fresh memfd: (fds, memfd, buffer fd).
fn fixture(m: &Mem) -> (Arc<FdTable>, Arc<OpenFile>, i64) {
    let fds = Arc::new(FdTable::new());
    let (f, n) = memfd(&fds, 4, SHRINK);
    m.create(n, 0, 0, 4 * P);
    let fd = udmabuf::ioctl_with(UDMABUF_CREATE, BASE, m, Some(&fds)).unwrap();
    (fds, f, fd)
}

#[test]
fn udmabuf_shares_the_memfd_pages() {
    let m = Mem::new();
    let (fds, f, fd) = fixture(&m);
    assert!(fd >= 0);
    let b = buf_of(&fds, fd);
    assert_eq!(b.size(), 4 * P);
    assert_eq!(fds.cloexec(fd as usize), Ok(false));
    assert!(fds.get(fd as usize).unwrap().can_write(), "O_RDWR as Linux's udmabuf");
    f.file.write_at(P + 5, b"through-memfd").unwrap();
    let mut got = [0u8; 13];
    assert!(b.exporter().read(P + 5, &mut got));
    assert_eq!(&got, b"through-memfd");
    assert!(b.exporter().write(2 * P - 3, b"across"), "a write across a page boundary");
    let mut back = [0u8; 6];
    f.file.read_at(2 * P - 3, &mut back).unwrap();
    assert_eq!(&back, b"across");
    assert!(!b.exporter().write(4 * P - 1, b"xy"), "past the end");
    // The pages outlive the memfd.
    let fd_m = fds.open_fds().into_iter().find(|(_, o)| Arc::ptr_eq(o, &f)).unwrap().0;
    fds.close(fd_m).unwrap();
    drop(f);
    assert!(b.exporter().read(P + 5, &mut got));
    assert_eq!(&got, b"through-memfd");
    // A second buffer at an offset sees the same pages.
    let (f2, n2) = memfd(&fds, 2, SHRINK);
    f2.file.write_at(P, b"second").unwrap();
    m.create(n2, UDMABUF_FLAGS_CLOEXEC, P, P);
    let fd2 = udmabuf::ioctl_with(UDMABUF_CREATE, BASE, &m, Some(&fds)).unwrap();
    assert_eq!(fds.cloexec(fd2 as usize), Ok(true));
    let b2 = buf_of(&fds, fd2);
    let mut s = [0u8; 6];
    assert!(b2.exporter().read(0, &mut s));
    assert_eq!(&s, b"second");
    assert_ne!(b.ino(), b2.ino(), "an inode per buffer");
}

#[test]
fn udmabuf_errors_match_linux() {
    let m = Mem::new();
    let fds = FdTable::new();
    let io = |m: &Mem| udmabuf::ioctl_with(UDMABUF_CREATE, BASE, m, Some(&fds));
    let (_a, plain) = memfd(&fds, 4, 0);
    let (_b, wsealed) = memfd(&fds, 4, SHRINK | vfs::memfd::F_SEAL_WRITE);
    let (_c, ok) = memfd(&fds, 4, SHRINK);
    let efd = fds.install_flags(vfs::eventfd::new_eventfd(0, 0), 0, false).unwrap() as u32;
    let cases: &[(u32, u64, u64, i64, &str)] = &[
        (plain, 0, 4 * P, EINVAL, "no F_SEAL_SHRINK"),
        (wsealed, 0, 4 * P, EINVAL, "F_SEAL_WRITE"),
        (ok, 1, P, EINVAL, "offset unaligned"),
        (ok, 0, P + 1, EINVAL, "size unaligned"),
        (ok, 0, 0, EINVAL, "size 0"),
        (ok, 3 * P, 2 * P, EINVAL, "past the memfd's end"),
        (efd, 0, P, EBADFD, "an eventfd"),
        (u32::MAX, 0, P, EBADFD, "fd -1"),
        (1000, 0, P, EBADFD, "a closed fd"),
        // The size checks come before the fd.
        (u32::MAX, 0, P + 1, EINVAL, "fd -1 with a bad size"),
    ];
    let before = dma_buf::live();
    for &(fd, off, size, e, what) in cases {
        m.create(fd, 0, off, size);
        assert_eq!(io(&m), Err(e), "{what}");
    }
    assert_eq!(udmabuf::ioctl_with(UDMABUF_CREATE, 0x1, &m, Some(&fds)), Err(EFAULT));
    assert_eq!(udmabuf::ioctl_with(0x4008_7544, BASE, &m, Some(&fds)), Err(ENOTTY));
    m.list(0, 0, &[]);
    assert_eq!(udmabuf::ioctl_with(UDMABUF_CREATE_LIST, BASE, &m, Some(&fds)), Err(EINVAL), "count 0");
    m.list(0, 1025, &[]);
    assert_eq!(udmabuf::ioctl_with(UDMABUF_CREATE_LIST, BASE, &m, Some(&fds)), Err(EINVAL), "count past list_limit");
    m.list(0, 400, &[]);
    assert_eq!(udmabuf::ioctl_with(UDMABUF_CREATE_LIST, BASE, &m, Some(&fds)), Err(EFAULT), "items past the user memory");
    assert!(dma_buf::live() <= before + 16, "the refusals made no buffer (other tests run in parallel)");
}

#[test]
fn udmabuf_list_joins_memfds() {
    let m = Mem::new();
    let fds = FdTable::new();
    let (f1, n1) = memfd(&fds, 2, SHRINK);
    let (f2, n2) = memfd(&fds, 3, SHRINK | vfs::memfd::F_SEAL_GROW);
    f1.file.write_at(P, b"first").unwrap();
    f2.file.write_at(2 * P, b"second").unwrap();
    m.list(UDMABUF_FLAGS_CLOEXEC, 2, &[Item { memfd: n1, offset: P, size: P }, Item { memfd: n2, offset: P, size: 2 * P }]);
    let fd = udmabuf::ioctl_with(UDMABUF_CREATE_LIST, BASE, &m, Some(&fds)).unwrap();
    assert_eq!(fds.cloexec(fd as usize), Ok(true));
    let b = buf_of(&fds, fd);
    assert_eq!(b.size(), 3 * P);
    let all = dma_buf::read_all(&b).unwrap();
    assert_eq!(&all[..5], b"first");
    assert_eq!(&all[2 * P as usize..2 * P as usize + 6], b"second");
}

#[test]
fn dma_buf_file_answers_like_linux() {
    let m = Mem::new();
    let (fds, _f, fd) = fixture(&m);
    let of = fds.get(fd as usize).unwrap();
    let b = buf_of(&fds, fd);
    let a = of.file.attr().unwrap();
    assert_eq!((a.mode, a.size, a.blocks, a.nlink, a.ino), (0o600, 4 * P, 4 * P / 512, 1, b.ino()));
    assert_eq!(of.file.lseek(0, vfs::abi::SEEK_END), Some(Ok(4 * P)));
    assert_eq!(of.file.lseek(0, vfs::abi::SEEK_SET), Some(Ok(0)));
    assert_eq!(of.file.lseek(0, vfs::abi::SEEK_CUR), Some(Err(EINVAL)));
    assert_eq!(of.file.lseek(1, vfs::abi::SEEK_END), Some(Err(EINVAL)));
    assert_eq!(of.file.lseek(4096, vfs::abi::SEEK_SET), Some(Err(EINVAL)));
    assert_eq!(of.seek(0, vfs::abi::SEEK_END), Ok(4 * P));
    let mut c = [0u8; 1];
    assert_eq!(of.file.read_at(0, &mut c), Err(EINVAL));
    assert_eq!(of.file.write_at(0, &c), Err(EINVAL));
    assert_eq!(of.file.link_name().as_deref(), Some("/dmabuf:"));
    let info = |f: &Arc<OpenFile>| {
        let mut s = String::new();
        f.file.show_fdinfo(&mut s);
        s
    };
    drop(b);
    assert_eq!(info(&of), "size:\t16384\ncount:\t1\nexp_name:\tudmabuf\n");
    let b = buf_of(&fds, fd);
    m.set(64, b"abi-name\0");
    assert_eq!(b.ioctl_with(DMA_BUF_SET_NAME_B, BASE + 64, &m, Some(&fds)), Ok(0));
    assert_eq!(of.file.link_name().as_deref(), Some("/dmabuf:abi-name"));
    m.set(64, b"second\0");
    assert_eq!(b.ioctl_with(DMA_BUF_SET_NAME_A, BASE + 64, &m, Some(&fds)), Ok(0));
    assert_eq!(b.name().as_deref(), Some("second"));
    m.set(64, &[b'x'; 40]);
    assert_eq!(b.ioctl_with(DMA_BUF_SET_NAME_B, BASE + 64, &m, Some(&fds)), Err(EINVAL), "no NUL in 32 bytes");
    m.set(64, &[b'y'; 31]);
    m.set(95, b"\0");
    assert_eq!(b.ioctl_with(DMA_BUF_SET_NAME_B, BASE + 64, &m, Some(&fds)), Ok(0), "31 bytes and the NUL");
    assert_eq!(b.ioctl_with(DMA_BUF_SET_NAME_B, 0x1, &m, Some(&fds)), Err(EFAULT));
    assert_eq!(b.ioctl_with(0x4008_6209, BASE, &m, Some(&fds)), Err(ENOTTY));
    assert!(of.file.poll_exact() && of.file.can_poll());
    drop(b);
    assert_eq!(info(&of), std::format!("size:\t16384\ncount:\t1\nexp_name:\tudmabuf\nname:\t{}\n", "y".repeat(31)));
}

#[test]
fn map_ops_check_the_range_and_the_exporter() {
    let m = Mem::new();
    let (fds, _f, fd) = fixture(&m);
    let b = buf_of(&fds, fd);
    let ops = MapOps::new(b.exporter().clone(), 4);
    assert_eq!(ops.check(0, 4, 3, true), Ok(()));
    assert_eq!(ops.check(3, 1, 3, true), Ok(()));
    assert_eq!(ops.check(1, 4, 1, true), Err(EINVAL), "past the end");
    assert_eq!(ops.check(u64::MAX, 2, 1, true), Err(EINVAL), "overflow");
    assert_eq!(ops.check(0, 1, 3, false), Err(EINVAL), "udmabuf: MAP_SHARED only");
    assert_eq!(ops.fault(3, true).map(|p| (p.aperture, p.page, p.writable)), Ok((0, 3, true)));
    assert!(ops.fault(4, false).is_err());
}

#[test]
fn sync_ioctl_checks_flags_and_order() {
    let m = Mem::new();
    let (fds, _f, fd) = fixture(&m);
    let b = buf_of(&fds, fd);
    let sync = |flags: u64| {
        m.put(0, &dma_buf_sync { flags });
        b.ioctl_with(DMA_BUF_IOCTL_SYNC, BASE, &m, Some(&fds))
    };
    assert_eq!(sync(4 | 3), Err(EINVAL), "udmabuf: end before the first start");
    assert_eq!(sync(0), Err(EINVAL));
    assert_eq!(sync(4), Err(EINVAL));
    assert_eq!(sync(3 | 8), Err(EINVAL));
    assert_eq!(sync(3), Ok(0));
    assert_eq!(sync(4 | 3), Ok(0));
    assert_eq!(sync(1), Ok(0));
    assert_eq!(sync(4 | 2), Ok(0));
    assert_eq!(b.ioctl_with(DMA_BUF_IOCTL_SYNC, 0x1, &m, Some(&fds)), Err(EFAULT));
}

fn pending() -> Arc<Fence> {
    Fence::new(fence::context_alloc(1), 1, "dr5", "test")
}

/// A sync_file fd of `f` in `fds`.
fn sync_fd(fds: &FdTable, f: &Arc<Fence>) -> i32 {
    sync_file::install(Some(fds), SyncFile::new(f.clone(), b"")).unwrap()
}

fn import(b: &DmaBuf, m: &Mem, fds: &FdTable, flags: u32, fd: i32) -> Result<i64, vfs::Errno> {
    m.put(0, &dma_buf_import_sync_file { flags, fd });
    b.ioctl_with(DMA_BUF_IOCTL_IMPORT_SYNC_FILE, BASE, m, Some(fds))
}

fn export(b: &DmaBuf, m: &Mem, fds: &FdTable, flags: u32) -> Result<Arc<SyncFile>, vfs::Errno> {
    m.put(0, &dma_buf_export_sync_file { flags, fd: -1 });
    b.ioctl_with(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, BASE, m, Some(fds))?;
    let fd = m.take::<dma_buf_export_sync_file>(0).fd;
    assert_eq!(fds.cloexec(fd as usize), Ok(true));
    let f = fds.get(fd as usize).unwrap().file.clone();
    fds.close(fd as usize).unwrap();
    Ok(sync_file::as_sync_file(&f).expect("a sync_file"))
}

#[test]
fn poll_follows_readers_and_writers() {
    let m = Mem::new();
    let (fds, _f, fd) = fixture(&m);
    let b = buf_of(&fds, fd);
    assert_eq!(b.events().0, Events::IN | Events::OUT, "no fences");
    let r = pending();
    let rfd = sync_fd(&fds, &r);
    assert_eq!(import(&b, &m, &fds, DMA_BUF_SYNC_READ, rfd), Ok(0));
    assert_eq!(b.events().0, Events::IN, "a reader: reading may start, writing waits");
    let w = pending();
    let wfd = sync_fd(&fds, &w);
    assert_eq!(import(&b, &m, &fds, DMA_BUF_SYNC_WRITE, wfd), Ok(0));
    assert_eq!(b.events().0, 0);
    let seq = b.poll_head().unwrap().seq();
    w.signal();
    assert_eq!(b.events().0, Events::IN);
    assert!(b.poll_head().unwrap().seq() > seq, "the writer's signal woke the queue");
    let seq = b.poll_head().unwrap().seq();
    r.signal();
    assert_eq!(b.events().0, Events::IN | Events::OUT);
    assert!(b.poll_head().unwrap().seq() > seq);
    assert_eq!((r.callbacks(), w.callbacks()), (0, 0));
}

#[test]
fn export_and_import_sync_files() {
    let m = Mem::new();
    let (fds, _f, fd) = fixture(&m);
    let b = buf_of(&fds, fd);
    let s = export(&b, &m, &fds, DMA_BUF_SYNC_WRITE).unwrap();
    assert!(fence::is_signaled(s.fence()), "no fences: the signaled stub");
    assert_eq!(fence::unwrap(s.fence()).len(), 1);
    for flags in [0, 4, 7, 8 | 1] {
        assert_eq!(export(&b, &m, &fds, flags).err(), Some(EINVAL), "export flags {flags}");
        assert_eq!(import(&b, &m, &fds, flags, -1), Err(EINVAL), "import flags {flags}");
    }
    assert_eq!(b.ioctl_with(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, 0x1, &m, Some(&fds)), Err(EFAULT));
    assert_eq!(import(&b, &m, &fds, DMA_BUF_SYNC_RW, -1), Err(EINVAL), "fd -1");
    assert_eq!(import(&b, &m, &fds, DMA_BUF_SYNC_RW, fd as i32), Err(EINVAL), "a dma-buf is no sync_file");
    let w = pending();
    let r = pending();
    let (wfd, rfd) = (sync_fd(&fds, &w), sync_fd(&fds, &r));
    import(&b, &m, &fds, DMA_BUF_SYNC_WRITE, wfd).unwrap();
    import(&b, &m, &fds, DMA_BUF_SYNC_READ, rfd).unwrap();
    // A reader waits for the writer only; a writer for both.
    let rd = export(&b, &m, &fds, DMA_BUF_SYNC_READ).unwrap();
    assert!(Arc::ptr_eq(rd.fence(), &w));
    let wr = export(&b, &m, &fds, DMA_BUF_SYNC_WRITE).unwrap();
    assert_eq!(fence::unwrap(wr.fence()).len(), 2);
    assert!(!fence::is_signaled(wr.fence()));
    w.signal();
    r.signal();
    assert!(fence::is_signaled(wr.fence()));
    // A merged sync_file is unwrapped into the set.
    let (x, y) = (pending(), pending());
    let merged = SyncFile::new(fence::merge(&[&x, &y]), b"");
    let mfd = sync_file::install(Some(&fds), merged).unwrap();
    import(&b, &m, &fds, DMA_BUF_SYNC_WRITE, mfd).unwrap();
    assert_eq!(b.resv().fences(Usage::Write).len(), 2);
    x.signal();
    y.signal();
    assert!(b.resv().test_signaled(Usage::Bookkeep));
}

#[test]
fn resv_replaces_and_prunes() {
    let r = Resv::new();
    let ctx = fence::context_alloc(1);
    let a = Fence::new(ctx, 1, "dr5", "t");
    let b2 = Fence::new(ctx, 2, "dr5", "t");
    r.add_fence(&a, Usage::Write).unwrap();
    r.add_fence(&b2, Usage::Write).unwrap();
    assert_eq!(r.len(), 1, "a later fence of the same context replaces the older");
    assert_eq!(a.callbacks(), 0, "the replaced fence's callback is gone");
    let rd = Fence::new(ctx, 3, "dr5", "t");
    r.add_fence(&rd, Usage::Read).unwrap();
    assert_eq!(r.len(), 2, "a reader does not replace a writer of its context");
    let ctx2 = fence::context_alloc(1);
    let (r1, w2) = (Fence::new(ctx2, 1, "dr5", "t"), Fence::new(ctx2, 2, "dr5", "t"));
    r.add_fence(&r1, Usage::Read).unwrap();
    r.add_fence(&w2, Usage::Write).unwrap();
    assert_eq!(r.len(), 3, "a later writer replaces a reader of its context (dma_resv_add_fence)");
    assert_eq!(r1.callbacks(), 0);
    w2.signal();
    let k = pending();
    r.add_fence(&k, Usage::Kernel).unwrap();
    assert_eq!(r.len(), 3, "the signaled writer left");
    assert_eq!(r.fences(Usage::Kernel).len(), 1);
    assert_eq!(r.fences(Usage::Write).len(), 2);
    assert_eq!(r.fences(Usage::Read).len(), 3);
    let bk = pending();
    r.add_fence(&bk, Usage::Bookkeep).unwrap();
    assert!(!r.test_signaled(Usage::Bookkeep) && r.fences(Usage::Read).len() == 3, "bookkeeping fences stay out of implicit sync");
    k.signal();
    rd.signal();
    assert!(!r.test_signaled(Usage::Read) && r.test_signaled(Usage::Kernel));
    b2.signal();
    assert!(r.test_signaled(Usage::Read));
    let n = pending();
    r.add_fence(&n, Usage::Read).unwrap();
    assert_eq!(r.len(), 2, "signaled fences left at the next add");
    assert!(r.singleton(Usage::Write).is_none());
    assert!(Arc::ptr_eq(&r.singleton(Usage::Read).unwrap(), &n));
    drop(r);
    assert_eq!((n.callbacks(), bk.callbacks()), (0, 0), "the set's callbacks go with it");
}

#[test]
fn begin_cpu_access_waits_for_the_fences() {
    let m = Arc::new(Mem::new());
    let (fds, _f, fd) = fixture(&m);
    let b = buf_of(&fds, fd);
    let w = pending();
    let wfd = sync_fd(&fds, &w);
    import(&b, &m, &fds, DMA_BUF_SYNC_WRITE, wfd).unwrap();
    let w2 = w.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        w2.signal();
    });
    let t0 = Instant::now();
    m.put(0, &dma_buf_sync { flags: DMA_BUF_SYNC_READ as u64 });
    assert_eq!(b.ioctl_with(DMA_BUF_IOCTL_SYNC, BASE, &*m, Some(&fds)), Ok(0));
    assert!(t0.elapsed() >= Duration::from_millis(40), "waited for the writer");
    t.join().unwrap();
    // A kill ends the wait.
    let r = pending();
    let rfd = sync_fd(&fds, &r);
    import(&b, &m, &fds, DMA_BUF_SYNC_READ, rfd).unwrap();
    let m2 = m.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        m2.kill.store(true, Ordering::SeqCst);
    });
    m.put(0, &dma_buf_sync { flags: DMA_BUF_SYNC_WRITE as u64 });
    assert_eq!(b.ioctl_with(DMA_BUF_IOCTL_SYNC, BASE, &*m, Some(&fds)), Err(EINTR));
    t.join().unwrap();
    assert_eq!(r.callbacks(), 2, "the sync_file's and the set's, none of the wait");
    drop(fds);
    drop(b);
    assert_eq!(r.callbacks(), 0);
}

/// FS-17: a thread asleep in epoll_wait for EPOLLOUT on a buffer with a pending reader is woken by the
/// reader's signal on another thread, with the OUT key.
#[test]
fn blocking_epoll_on_a_dma_buf_is_woken() {
    use vfs::epoll::{self, Epoll, EPOLL_CTL_ADD};
    let m = Mem::new();
    let (fds, _f, fd) = fixture(&m);
    let b = buf_of(&fds, fd);
    let r = pending();
    let rfd = sync_fd(&fds, &r);
    import(&b, &m, &fds, DMA_BUF_SYNC_READ, rfd).unwrap();
    let ep = epoll::new_epoll();
    Epoll::ctl(&ep, EPOLL_CTL_ADD, &fds.get(fd as usize).unwrap(), fd as i32, (Events::OUT, 5), false).unwrap();
    let r2 = r.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        r2.signal();
    });
    let mut out: Vec<(u32, u64)> = Vec::new();
    let t0 = Instant::now();
    let n = ep.file.as_epoll().unwrap().wait(&mut out, 4, Some(fence::now().unwrap() + 3_000_000_000), &|| false).unwrap();
    t.join().unwrap();
    assert_eq!((n, out), (1, vec![(Events::OUT, 5)]), "woke after {:?}", t0.elapsed());
}

#[test]
fn nothing_left_behind() {
    let m = Mem::new();
    let (fds, f, fd) = fixture(&m);
    let b = buf_of(&fds, fd);
    let w = pending();
    let wfd = sync_fd(&fds, &w);
    import(&b, &m, &fds, DMA_BUF_SYNC_WRITE, wfd).unwrap();
    assert_eq!(w.callbacks(), 2, "the sync_file's and the set's");
    let file: Arc<dyn File> = b.clone();
    let weak = Arc::downgrade(&b);
    drop(b);
    drop(fds);
    drop(f);
    assert!(dma_buf::as_dma_buf(&file).is_some(), "alive while a reference is");
    drop(file);
    assert!(weak.upgrade().is_none());
    assert_eq!(w.callbacks(), 0);
}

struct Count(std::sync::atomic::AtomicUsize);

impl crate::resv::ResvWatch for Count {
    fn changed(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn resv_signaled_on_add_wakes() {
    // Checker finding 2: a fence that signals between entering the set and its callback being armed gets no
    // callback, so the set must tell its watchers itself (else a poller that saw "not ready" sleeps for ever).
    let r = Resv::new();
    let c = Arc::new(Count(std::sync::atomic::AtomicUsize::new(0)));
    let w: std::sync::Weak<dyn crate::resv::ResvWatch> = Arc::downgrade(&(c.clone() as Arc<dyn crate::resv::ResvWatch>));
    r.watch(w).unwrap();
    let f = pending();
    f.signal();
    r.add_fence(&f, Usage::Write).unwrap();
    assert_eq!(c.0.load(Ordering::SeqCst), 1, "an already signaled fence wakes the watchers once");
    let g = pending();
    r.add_fence(&g, Usage::Write).unwrap();
    assert_eq!(c.0.load(Ordering::SeqCst), 1, "a pending fence wakes nobody yet");
    g.signal();
    assert_eq!(c.0.load(Ordering::SeqCst), 2, "its callback wakes them when it signals");
}

#[test]
fn udmabuf_ref_limit_bounds_the_page_lists() {
    // Checker finding 3: overlapping items repeat references (one list entry each), so the lists are bounded by
    // the reference budget, not by the memfd sizes.
    let held = core::sync::atomic::AtomicU64::new(0);
    assert_eq!(udmabuf::reserve(&held, 8, 12), Ok(()));
    assert_eq!(udmabuf::reserve(&held, 5, 12), Err(8), "past the limit: refused, nothing taken");
    assert_eq!(udmabuf::reserve(&held, 4, 12), Ok(()));
    assert_eq!(udmabuf::reserve(&held, u64::MAX, u64::MAX), Err(12), "no overflow");
    assert_eq!(held.load(Ordering::SeqCst), 12);

    let m = Mem::new();
    let fds = FdTable::new();
    let (_f, n) = memfd(&fds, 4, SHRINK);
    let it = Item { memfd: n, offset: 0, size: 4 * P };
    udmabuf::set_test_ref_limit(3);
    m.list(0, 3, &[it, it, it]);
    assert_eq!(udmabuf::ioctl_with(UDMABUF_CREATE_LIST, BASE, &m, Some(&fds)), Err(ENOMEM), "12 references over a limit of 3");
    udmabuf::set_test_ref_limit(0);
    let fd = udmabuf::ioctl_with(UDMABUF_CREATE_LIST, BASE, &m, Some(&fds)).unwrap();
    assert_eq!(buf_of(&fds, fd).size(), 12 * P, "overlap is allowed within the budget");
    assert!(udmabuf::refs_held() >= 12);
}

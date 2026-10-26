//! FS-1 fixer and FS-5: the tmpfs pools and the B1 freeze with writers in flight. Its own test binary, because
//! both change process-wide state (the budget, the interrupt check) that the unit tests must not see.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use uapi::errno::*;
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::inode::{set_tmpfs_page_limit, tmpfs_pages_used, PAGE};
use vfs::pool::{system_charged, system_meta, system_used};
use vfs::{Ctx, FsFreeze, MountNs};

static KILLED: AtomicBool = AtomicBool::new(false);
fn killed() -> bool {
    KILLED.load(Ordering::SeqCst)
}

#[test]
fn budget_then_freeze() {
    budget();
    metadata();
    freeze_waits_for_writers_and_kill_ends_a_wait();
}

/// Review INT-A1d fs 1: inodes and names are charged to the system pool too (whole pages as a mount's metadata
/// grows), so empty files, directories and symlinks end in ENOSPC at the pool's end instead of a kernel heap
/// that runs out; removing them gives the pages back.
fn metadata() {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    let (data0, meta0) = (system_charged(), system_meta());
    set_tmpfs_page_limit(Some(system_used() + 2));
    let mut made = Vec::new();
    let refused = loop {
        let name = format!("/e{}", made.len());
        match c.open(None, name.as_bytes(), O_CREAT | O_RDWR, 0o600) {
            Ok(f) => {
                drop(f);
                made.push(name);
                assert!(made.len() < 100, "empty files never ran into the pool");
            }
            Err(e) => break e,
        }
    };
    assert_eq!(refused, ENOSPC, "empty files at the pool's end");
    // (1024 + 512 + 3) bytes each: five fit into two pages.
    assert_eq!(made.len(), 5, "files made before the pool was full");
    assert_eq!(system_charged(), data0, "no data was charged");
    assert_eq!(c.mkdir(None, b"/d", 0o700).err(), Some(ENOSPC));
    assert_eq!(c.symlink(&[b'x'; 4000], None, b"/l").err(), Some(ENOSPC));
    c.unlink(None, made.pop().unwrap().as_bytes()).unwrap();
    c.unlink(None, made.pop().unwrap().as_bytes()).unwrap();
    c.mkdir(None, b"/d", 0o700).expect("room again after two unlinks");
    c.rmdir(None, b"/d").unwrap();
    for n in made.drain(..) {
        c.unlink(None, n.as_bytes()).unwrap();
    }
    assert_eq!(system_meta(), meta0, "every metadata page given back");
    set_tmpfs_page_limit(None);
}

/// FS-5, FS-14: tmpfs files are charged page by page when a page is allocated (Linux VM_NORESERVE); holes and a
/// growing ftruncate cost nothing. A refusal of the system pool or of a mount's own size is ENOSPC (shmem's
/// `shmem_inode_acct_block`; a write first gets what fits); truncating or removing a file gives its pages back.
fn budget() {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    // The file first: its inode and name take one page of the pool (metadata); the limit leaves 4 data pages
    // (the second file's metadata fits into that page too).
    let f = c.open(None, b"/big", O_CREAT | O_RDWR, 0o644).unwrap();
    let base = system_charged();
    set_tmpfs_page_limit(Some(system_used() + 4));
    let chunk = vec![7u8; 6 * PAGE as usize];
    assert_eq!(f.write(&chunk).unwrap(), 4 * PAGE as usize, "short write at the pool's end");
    assert_eq!(f.write(b"x").err(), Some(ENOSPC));
    assert_eq!(system_charged(), base + 4);
    // Growth by ftruncate is free, even far past the pool.
    f.inode().unwrap().truncate(1 << 30).unwrap();
    assert_eq!((f.file.size(), system_charged()), (1 << 30, base + 4));
    f.inode().unwrap().truncate(PAGE).unwrap();
    assert_eq!(system_charged(), base + 1);
    assert_eq!(f.pwrite(2 * PAGE, b"abc").unwrap(), 3);
    assert_eq!(system_charged(), base + 2, "the hole page costs nothing");
    assert_eq!(tmpfs_pages_used() >= 2, true);
    let g = c.open(None, b"/second", O_CREAT | O_RDWR, 0o644).unwrap();
    assert_eq!(g.write(&chunk).unwrap(), 2 * PAGE as usize);
    drop(f);
    c.unlink(None, b"/big").unwrap();
    assert_eq!(system_charged(), base + 2, "the removed file's pages are given back");
    drop(g);
    c.unlink(None, b"/second").unwrap();
    assert_eq!(system_charged(), base);
    set_tmpfs_page_limit(None);

    // A mount with its own size (Linux tmpfs size=): ENOSPC there, the system pool untouched.
    let m = vfs::tmpfs::mount_new(&ns, "/shm", 0o1777, Some(3)).unwrap();
    let h = c.open(None, b"/shm/a", O_CREAT | O_RDWR, 0o600).unwrap();
    h.inode().unwrap().truncate(4 * PAGE).unwrap();
    assert_eq!(h.pwrite(0, &chunk).unwrap(), 3 * PAGE as usize, "short write at the mount's size");
    assert_eq!(h.pwrite(3 * PAGE, b"y").err(), Some(ENOSPC));
    assert_eq!(m.sb.pool.used(), 3);
    drop(h);
    c.unlink(None, b"/shm/a").unwrap();
    assert_eq!(m.sb.pool.used(), 0);
    assert_eq!(system_charged(), base);
}

/// B1: freeze waits for a write under way, holds new writers back until thaw, and a killed writer leaves
/// its wait with EINTR.
fn freeze_waits_for_writers_and_kill_ends_a_wait() {
    vfs::dentry::set_interrupt_check(killed);
    let ns = Arc::new(MountNs::new_empty());
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/f", O_CREAT | O_RDWR, 0o644).unwrap();
    let sb = f.path.as_ref().unwrap().dentry.sb.clone();
    // A change under way: freeze must wait for its guard.
    let g = sb.write_begin().unwrap();
    let frozen_at = Arc::new(AtomicUsize::new(0));
    let t0 = Instant::now();
    let freezer = {
        let (sb, at) = (sb.clone(), frozen_at.clone());
        std::thread::spawn(move || {
            sb.freeze().unwrap();
            at.store(t0.elapsed().as_millis() as usize + 1, Ordering::SeqCst);
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(frozen_at.load(Ordering::SeqCst), 0, "freeze returned while a write was under way");
    drop(g);
    freezer.join().unwrap();
    assert!(sb.is_frozen());
    assert_eq!(sb.freeze(), Err(EBUSY));
    // A writer now waits; the "kill" ends its wait with EINTR and nothing was written.
    let writer = {
        let f = f.clone();
        std::thread::spawn(move || f.write(b"late"))
    };
    std::thread::sleep(Duration::from_millis(250));
    assert!(!writer.is_finished(), "a writer passed a frozen file system");
    KILLED.store(true, Ordering::SeqCst);
    assert_eq!(writer.join().unwrap(), Err(EINTR));
    KILLED.store(false, Ordering::SeqCst);
    assert_eq!(f.file.size(), 0);
    // Another writer waits and finishes after thaw.
    let writer = {
        let f = f.clone();
        std::thread::spawn(move || f.write(b"after"))
    };
    std::thread::sleep(Duration::from_millis(150));
    sb.thaw();
    assert_eq!(writer.join().unwrap(), Ok(5));
    assert_eq!(sb.writers(), 0);
}

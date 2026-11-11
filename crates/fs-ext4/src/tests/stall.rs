//! FS-STALL: a sync waiting on the device must not hold up another program's small read, stat, open or write.
//! Judged by order, not by time: the device's cache flush is held shut (`Gate`) while one thread is in fsync, and
//! everything else must finish before it opens. Before FS-STALL the commit flushed under `ext4.vol` and all of
//! them waited for the gate.

use super::util::*;
use crate::dev::{Dev, MemDev};
use crate::mount;
use std::string::String;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use std::{format, vec};
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

/// A memory device whose cache flush waits while the gate is shut.
struct Gate {
    mem: MemDev,
    shut: AtomicBool,
    inside: AtomicU32,
    /// The next flush after the gate fails (a media error, not a timeout).
    fail_once: AtomicBool,
    /// The next flush after the gate times out (ETIMEDOUT: the volume stays writable).
    timeout_once: AtomicBool,
}

impl Dev for Gate {
    fn size(&self) -> u64 {
        self.mem.size()
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        self.mem.read(at, buf)
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        self.mem.write(at, data)
    }
    fn flush(&self) -> Result<(), i64> {
        if self.shut.load(Ordering::Acquire) {
            self.inside.fetch_add(1, Ordering::AcqRel);
            while self.shut.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.inside.fetch_sub(1, Ordering::AcqRel);
            if self.fail_once.swap(false, Ordering::AcqRel) {
                return Err(crate::errno::EIO);
            }
            if self.timeout_once.swap(false, Ordering::AcqRel) {
                return Err(crate::errno::ETIMEDOUT);
            }
        }
        self.mem.flush()
    }
    fn take_timed_out(&self) -> bool {
        self.mem.take_timed_out()
    }
}

/// Opens the gate when the test ends, also on a failed assert (the blocked threads would hang the run).
struct Opener(Arc<Gate>);

impl Drop for Opener {
    fn drop(&mut self) {
        self.0.shut.store(false, Ordering::Release);
    }
}

fn wait_inside(g: &Gate, n: u32) {
    for _ in 0..10_000 {
        if g.inside.load(Ordering::Acquire) >= n {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("no fsync reached the device flush");
}

/// Runs `f` on a thread of its own; its answer within `limit`, or the name of what is stuck.
fn bounded<T: Send + 'static>(what: &str, limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).map_err(|_| format!("{what} waited for the fsync's device flush"))
}

fn gated(s: &Scratch) -> (Arc<Gate>, Arc<vfs::MountNs>, Arc<crate::Volume>) {
    let gate = Arc::new(Gate { mem: MemDev::new(mkfs(s, "64M", &[], None)), shut: AtomicBool::new(false), inside: AtomicU32::new(0), fail_once: AtomicBool::new(false), timeout_once: AtomicBool::new(false) });
    let ns = Arc::new(vfs::MountNs::new_empty());
    let (vol, _m) = mount::mount_dev(gate.clone(), None, &ns, "/data", opts()).unwrap();
    (gate, ns, vol)
}

#[test]
fn a_sync_at_the_device_holds_up_no_small_read_stat_open_or_write() {
    let s = Scratch::new();
    let (gate, ns, vol) = gated(&s);
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/data/cache", 0o700).unwrap();
    for i in 0..50 {
        ns.create_file_bytes(format!("/data/cache/f{i}").as_bytes(), 0o600, &vec![i as u8; 3000]).unwrap();
    }
    ns.create_file_bytes(b"/data/hot", 0o600, b"hot data").unwrap();
    ns.create_file_bytes(b"/data/busy", 0o600, &vec![1u8; 9000]).unwrap();
    for p in ["/data/hot", "/data/busy", "/data/cache/f7"] {
        ns.lookup(p).unwrap().inode.sync(SyncKind::Fsync).unwrap();
    }
    let cold = ns.lookup("/data/cache/f7").unwrap().inode.meta.ino as u32;

    gate.shut.store(true, Ordering::Release);
    let _open = Opener(gate.clone());
    // The fsyncing program: a change, then fsync, which commits and waits at the shut flush.
    let n1 = ns.clone();
    let first = std::thread::spawn(move || {
        let c = Ctx::kernel(&n1);
        let f = c.open(None, b"/data/busy", O_RDWR, 0).unwrap();
        f.pwrite(100, b"changed before the fsync").unwrap();
        n1.lookup("/data/busy").unwrap().inode.sync(SyncKind::Fsync)
    });
    wait_inside(&gate, 1);

    let limit = Duration::from_secs(10);
    let n = ns.clone();
    let st = bounded("stat", limit, move || Ctx::kernel(&n).stat(None, b"/data/hot", true).map(|a| a.size)).unwrap();
    assert_eq!(st, Ok(8));
    let n = ns.clone();
    let rd = bounded("a cached read", limit, move || n.read_file("/data/hot", 100)).unwrap();
    assert_eq!(rd.as_deref(), Ok(&b"hot data"[..]));
    let v = vol.clone();
    let cold_read = bounded("a read from the volume", limit, move || {
        let mut b = vec![0u8; 3000];
        v.read(cold, 0, &mut b).map(|_| b)
    })
    .unwrap();
    assert_eq!(cold_read, Ok(vec![7u8; 3000]));
    let n = ns.clone();
    let listed = bounded("a directory listing", limit, move || n.list_dir("/data/cache").map(|l| l.len())).unwrap();
    assert_eq!(listed, Ok(50));
    let n = ns.clone();
    let opened = bounded("open, small write, close", limit, move || {
        let c = Ctx::kernel(&n);
        let f = c.open(None, b"/data/cache/f3", O_RDWR, 0)?;
        f.pwrite(0, b"small")
    })
    .unwrap();
    assert_eq!(opened, Ok(5));
    let n = ns.clone();
    let made = bounded("a new file", limit, move || Ctx::kernel(&n).open(None, b"/data/new", O_RDWR | O_CREAT, 0o600).map(|_| ())).unwrap();
    assert_eq!(made, Ok(()));
    assert!(!first.is_finished(), "fsync returned before the device flushed");

    // A second fsync during the first: its change is in a later transaction, so it must not return before a flush
    // after its data either.
    let n2 = ns.clone();
    let second = std::thread::spawn(move || {
        let c = Ctx::kernel(&n2);
        let f = c.open(None, b"/data/cache/f9", O_RDWR, 0).unwrap();
        f.pwrite(0, b"second").unwrap();
        n2.lookup("/data/cache/f9").unwrap().inode.sync(SyncKind::Fsync)
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(!second.is_finished(), "the second fsync returned before the device flushed");

    gate.shut.store(false, Ordering::Release);
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    for p in ["/data/new", "/data/cache/f3", "/data/hot"] {
        ns.lookup(p).unwrap().inode.sync(SyncKind::Fsync).unwrap();
    }
    vol.sync_fs().unwrap();
    let img = gate.mem.image();
    assert_clean(&s, &img, "after the gated syncs");
    let (_d, v2) = mount(img);
    let ino = lookup(&v2, "/busy");
    let mut b = vec![0u8; 124];
    v2.read(ino, 0, &mut b).unwrap();
    assert_eq!(&b[100..], b"changed before the fsync");
    let ino = lookup(&v2, "/cache/f9");
    v2.read(ino, 0, &mut b[..6]).unwrap();
    assert_eq!(&b[..6], b"second");
}

/// A commit in flight fails (the device times out) while newer changes joined the next running transaction: it goes
/// back under them, the volume stays writable, and the next fsync makes both durable.
#[test]
fn a_commit_that_fails_in_flight_goes_back_under_the_newer_changes() {
    let s = Scratch::new();
    let (gate, ns, vol) = gated(&s);
    for p in ["/data/a", "/data/b"] {
        ns.create_file_bytes(p.as_bytes(), 0o600, &vec![0u8; 5000]).unwrap();
        ns.lookup(p).unwrap().inode.sync(SyncKind::Fsync).unwrap();
    }
    gate.shut.store(true, Ordering::Release);
    let _open = Opener(gate.clone());
    let n1 = ns.clone();
    let first = std::thread::spawn(move || {
        let f = Ctx::kernel(&n1).open(None, b"/data/a", O_RDWR, 0).unwrap();
        f.pwrite(8000, b"grown in the failed commit").unwrap();
        n1.lookup("/data/a").unwrap().inode.sync(SyncKind::Fsync)
    });
    wait_inside(&gate, 1);
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/data/b", O_RDWR, 0).unwrap();
    f.pwrite(9000, b"grown while it was in flight").unwrap();
    ns.lookup("/data/b").unwrap().inode.sync(SyncKind::Background).unwrap();
    c.open(None, b"/data/c", O_RDWR | O_CREAT, 0o600).unwrap();
    gate.mem.time_out(true);
    gate.shut.store(false, Ordering::Release);
    assert_eq!(first.join().unwrap(), Err(crate::errno::EIO));
    gate.mem.time_out(false);
    assert!(vol.writable(), "a timeout must not stop the volume");
    ns.lookup("/data/a").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    vol.sync_fs().unwrap();
    let img = gate.mem.image();
    assert_clean(&s, &img, "after the failed commit and the next one");
    let (_d, v2) = mount(img);
    let mut buf = vec![0u8; 64];
    let a = lookup(&v2, "/a");
    assert_eq!(v2.stat(a).unwrap().size(), 8026);
    v2.read(a, 8000, &mut buf[..26]).unwrap();
    assert_eq!(&buf[..26], b"grown in the failed commit");
    let b = lookup(&v2, "/b");
    assert_eq!(v2.stat(b).unwrap().size(), 9028);
    v2.read(b, 9000, &mut buf[..28]).unwrap();
    assert_eq!(&buf[..28], b"grown while it was in flight");
    lookup(&v2, "/c");
}

/// Review E1: an fsync with nothing running waits for the commit in flight (it may hold its file's change); when
/// that commit fails, the fsync must not answer Ok (before: it flushed the device and did).
#[test]
fn an_fsync_behind_a_failed_commit_does_not_answer_ok() {
    let s = Scratch::new();
    let (gate, ns, vol) = gated(&s);
    ns.create_file_bytes(b"/data/a", 0o600, &vec![0u8; 5000]).unwrap();
    ns.lookup("/data/a").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    gate.shut.store(true, Ordering::Release);
    let _open = Opener(gate.clone());
    let n1 = ns.clone();
    let first = std::thread::spawn(move || {
        let f = Ctx::kernel(&n1).open(None, b"/data/a", O_RDWR, 0).unwrap();
        f.pwrite(7000, b"in the commit that fails").unwrap();
        n1.lookup("/data/a").unwrap().inode.sync(SyncKind::Fsync)
    });
    wait_inside(&gate, 1);
    // The second fsync of the same file: its change is in the commit in flight, nothing is running.
    let v = vol.clone();
    let second = std::thread::spawn(move || v.flush());
    std::thread::sleep(Duration::from_millis(200));
    assert!(!second.is_finished(), "the second fsync did not wait for the commit in flight");
    gate.fail_once.store(true, Ordering::Release);
    gate.shut.store(false, Ordering::Release);
    assert!(first.join().unwrap().is_err(), "the failed commit's fsync");
    assert!(second.join().unwrap().is_err(), "an fsync behind a failed commit answered Ok");
}

/// Round 2 item 1: an fsync behind a commit in flight that times out, while another program fsyncs all the time:
/// it answers only once its change is committed (here by the retry it makes itself), and Ok then.
#[test]
fn an_fsync_behind_a_timed_out_commit_answers_once_it_is_committed() {
    let s = Scratch::new();
    let (gate, ns, vol) = gated(&s);
    for p in ["/data/a", "/data/b"] {
        ns.create_file_bytes(p.as_bytes(), 0o600, &vec![0u8; 5000]).unwrap();
        ns.lookup(p).unwrap().inode.sync(SyncKind::Fsync).unwrap();
    }
    gate.shut.store(true, Ordering::Release);
    let _open = Opener(gate.clone());
    let n1 = ns.clone();
    let first = std::thread::spawn(move || {
        let f = Ctx::kernel(&n1).open(None, b"/data/a", O_RDWR, 0).unwrap();
        f.pwrite(7000, b"in the commit that times out").unwrap();
        n1.lookup("/data/a").unwrap().inode.sync(SyncKind::Fsync)
    });
    wait_inside(&gate, 1);
    let v = vol.clone();
    let second = std::thread::spawn(move || v.flush());
    let n3 = ns.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let s3 = stop.clone();
    let busy = std::thread::spawn(move || {
        let f = Ctx::kernel(&n3).open(None, b"/data/b", O_RDWR, 0).unwrap();
        let mut k = 0u32;
        while !s3.load(Ordering::Acquire) {
            f.pwrite(u64::from(k % 4000), b"x").unwrap();
            let _ = n3.lookup("/data/b").unwrap().inode.sync(SyncKind::Fsync);
            k += 1;
        }
    });
    std::thread::sleep(Duration::from_millis(100));
    gate.timeout_once.store(true, Ordering::Release);
    gate.shut.store(false, Ordering::Release);
    assert_eq!(first.join().unwrap(), Err(crate::errno::EIO), "the timed-out commit's own fsync");
    assert_eq!(second.join().unwrap(), Ok(()), "the fsync behind it, after its retry");
    stop.store(true, Ordering::Release);
    busy.join().unwrap();
    assert!(vol.writable());
    // What the second fsync answered Ok for is on the disk.
    let (_d, v2) = mount(gate.mem.image());
    let a = lookup(&v2, "/a");
    let mut b = vec![0u8; 28];
    v2.read(a, 7000, &mut b).unwrap();
    assert_eq!(&b, b"in the commit that times out");
}

/// The home writes of a commit find no buffer (the hook) for a while.
fn home_nomem(vol: &crate::Volume, times: u32) {
    let st = vol.lock();
    st.jnl.as_ref().unwrap().test_home_nomem.store(times, Ordering::Release);
}

/// Round 3 item 1: home writes owed that keep finding no memory: an fsync of a newer change answers ENOMEM after a
/// few rounds (it looped forever, holding the commit lock), the volume stays writable, and once memory is back
/// the next fsync makes everything durable.
#[test]
fn owed_home_writes_without_memory_end_an_fsync_with_enomem() {
    let s = Scratch::new();
    let (gate, ns, vol) = gated(&s);
    for p in ["/data/a", "/data/b"] {
        ns.create_file_bytes(p.as_bytes(), 0o600, &vec![0u8; 5000]).unwrap();
        ns.lookup(p).unwrap().inode.sync(SyncKind::Fsync).unwrap();
    }
    home_nomem(&vol, u32::MAX);
    let c = Ctx::kernel(&ns);
    c.open(None, b"/data/a", O_RDWR, 0).unwrap().pwrite(6000, b"logged, home writes owed").unwrap();
    // The log holds it: committed, Ok, though its home writes found no memory.
    ns.lookup("/data/a").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    c.open(None, b"/data/b", O_RDWR, 0).unwrap().pwrite(6000, b"newer").unwrap();
    let n = ns.clone();
    let second = bounded("an fsync behind owed home writes", Duration::from_secs(20), move || n.lookup("/data/b").unwrap().inode.sync(SyncKind::Fsync)).unwrap();
    assert_eq!(second, Err(crate::errno::ENOMEM));
    assert!(vol.writable(), "no memory must not stop the volume");
    home_nomem(&vol, 0);
    ns.lookup("/data/b").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    vol.sync_fs().unwrap();
    let img = gate.mem.image();
    assert_clean(&s, &img, "after the owed home writes");
    let (_d, v2) = mount(img);
    let mut buf = vec![0u8; 24];
    v2.read(lookup(&v2, "/a"), 6000, &mut buf).unwrap();
    assert_eq!(&buf, b"logged, home writes owed");
    v2.read(lookup(&v2, "/b"), 6000, &mut buf[..5]).unwrap();
    assert_eq!(&buf[..5], b"newer");
}

/// Round 3 item 2: syncfs while home writes are owed and memory is short answers ENOMEM and leaves the volume
/// writable and dirty (it made the volume read-only for good).
#[test]
fn syncfs_without_memory_for_owed_home_writes_keeps_the_volume_writable() {
    let s = Scratch::new();
    let (gate, ns, vol) = gated(&s);
    ns.create_file_bytes(b"/data/a", 0o600, &vec![0u8; 5000]).unwrap();
    home_nomem(&vol, u32::MAX);
    ns.lookup("/data/a").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    assert_eq!(vol.sync_fs(), Err(crate::errno::ENOMEM));
    assert!(vol.writable(), "no memory must not stop the volume");
    home_nomem(&vol, 0);
    vol.sync_fs().unwrap();
    assert_clean(&s, &gate.mem.image(), "after the retried syncfs");
}

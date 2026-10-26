//! FS-12: the FAT code's request priorities (`lift`): requests made under the volume lock are lifted (at least
//! Normal, Interactive while an Interactive thread waits); file data reads and the fsync's cache flush, made
//! without the lock, keep the caller's.

use super::util::*;
use crate::dev::Dev;
use crate::lift::Lift;
use crate::{Clock, MemDev, Options, Volume};
use devmodel::block::IoPriority::{self, *};
use ksync::SpinLock;
use std::sync::Arc;
use std::vec::Vec;
use vfs::pagecache::SyncKind;

#[test]
fn lift_floor() {
    let l = Lift::new();
    let me = ksync::thread_id();
    assert_eq!(l.prio_for(Background, me), Background, "not the holder: the caller's own");
    let held = l.hold();
    assert_eq!(l.prio_for(Background, me), Normal, "the holder goes at least as Normal");
    assert_eq!(l.prio_for(Interactive, me), Interactive, "never lowered");
    assert_eq!(l.prio_for(Background, me + 1), Background, "another thread is not lifted");
    assert!(l.wait_as(Normal).is_none(), "only Interactive waiters count");
    let w = l.wait_as(Interactive).unwrap();
    assert_eq!(l.prio_for(Background, me), Interactive, "an Interactive waiter lifts the holder");
    drop(w);
    assert_eq!(l.prio_for(Background, me), Normal);
    drop(held);
    assert_eq!(l.prio_for(Background, me), Background, "the mark goes with the lock");
}

/// A MemDev that notes, per call, the priority `lift` gives a Background caller.
struct Noted {
    mem: Arc<MemDev>,
    lift: Lift,
    calls: SpinLock<Vec<(&'static str, IoPriority)>>,
}

impl Noted {
    fn note(&self, op: &'static str) {
        let p = self.lift.prio_for(Background, ksync::thread_id());
        self.calls.lock().push((op, p));
    }
}

impl Dev for Noted {
    fn sector_size(&self) -> usize {
        self.mem.sector_size()
    }
    fn sectors(&self) -> u64 {
        self.mem.sectors()
    }
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), i64> {
        self.note("read");
        self.mem.read(sector, buf)
    }
    fn write(&self, sector: u64, data: &[u8]) -> Result<(), i64> {
        self.note("write");
        self.mem.write(sector, data)
    }
    fn flush(&self) -> Result<(), i64> {
        self.note("flush");
        self.mem.flush()
    }
    fn lift(&self) -> Option<&Lift> {
        Some(&self.lift)
    }
}

#[test]
fn requests_under_the_volume_lock_are_lifted() {
    let dir = scratch("prio");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    let dev = Arc::new(Noted { mem: load(&img), lift: Lift::new(), calls: SpinLock::new(Vec::new()) });
    let vol = Volume::mount(dev.clone(), Options { writable: true, stick: false }, Clock::UTC_EPOCH).unwrap();
    let ns = view(&vol);
    let data = pattern(200_000, 3);
    ns.create_file_bytes(b"/boot/p.bin", 0o644, &data).unwrap().inode.sync(SyncKind::Fsync).unwrap();
    let calls = core::mem::take(&mut *dev.calls.lock());
    assert!(calls.iter().any(|c| c.0 == "write"));
    assert!(calls.iter().filter(|c| c.0 == "write").all(|c| c.1 == Normal), "every write goes under the lock: {calls:?}");
    // The cache flush of an fsync runs after the lock is released: the fsync caller's own priority.
    assert!(calls.iter().filter(|c| c.0 == "flush").all(|c| c.1 == Background), "{calls:?}");
    // A fresh mount reads the file: its data reads run without the lock, at the caller's own priority.
    let vol2 = Volume::mount(dev.clone(), Options { writable: false, stick: false }, Clock::UTC_EPOCH).unwrap();
    let ns2 = view(&vol2);
    dev.calls.lock().clear();
    assert!(ns2.read_file("/boot/p.bin", 1 << 20).unwrap() == data);
    let calls = core::mem::take(&mut *dev.calls.lock());
    assert!(calls.iter().any(|c| c.1 == Background), "data reads keep the caller's priority: {calls:?}");
    assert!(calls.iter().all(|c| c.0 == "read"));
}

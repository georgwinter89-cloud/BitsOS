//! The FAT code on a block-layer disk (`DiskDev`) and FS-7's write gate on the boot stick (`StickGate`): the
//! block layer's last word on that disk. It allows exactly the sectors the FAT code announces while it writes
//! them, and only inside the FATs, the data area and the FSInfo sector; never the partition table, the boot
//! sector or its backup, never a discard, and nothing at all unless armed (the identity confirmed, `stick`).

use crate::dev::Dev;
use crate::lift::Lift;
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::{Disk, WriteCheck, WriteGate};
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::SpinLock;

/// The FAT code's sectors on a `block::Disk`: a partition (or the whole disk) from `start`.
pub struct DiskDev {
    disk: Arc<Disk>,
    start: u64,
    sectors: u64,
    bs: usize,
    gate: Option<Arc<StickGate>>,
    lift: Lift,
}

impl DiskDev {
    pub fn new(disk: Arc<Disk>, start: u64, sectors: u64, gate: Option<Arc<StickGate>>) -> DiskDev {
        let bs = disk.block_size() as usize;
        DiskDev { disk, start, sectors, bs, gate, lift: Lift::new() }
    }
}

/// FS-12: a request goes at the calling thread's priority (`block::prio::current`), lifted while the thread
/// holds the volume lock (`lift`: at least Normal, Interactive while an Interactive thread waits for the lock).
/// So a file read without the lock of the program in front goes as Interactive, and the write-back thread's
/// requests never make a waiter of the lock wait behind Background traffic. Each call waits for its request,
/// so the crash-safe order of one write-back (data, FATs, directory entry, FSInfo) does not depend on priorities.
impl Dev for DiskDev {
    fn sector_size(&self) -> usize {
        self.bs
    }
    fn sectors(&self) -> u64 {
        self.sectors
    }
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), i64> {
        self.disk.read(self.start + sector, buf, self.lift.prio()).map_err(|e| e.errno())
    }
    fn write(&self, sector: u64, data: &[u8]) -> Result<(), i64> {
        let lba = self.start + sector;
        let blocks = (data.len() / self.bs) as u64;
        let _w = self.gate.as_ref().map(|g| g.announce(lba, blocks));
        self.disk.write(lba, data, false, self.lift.prio()).map_err(|e| e.errno())
    }
    fn flush(&self) -> Result<(), i64> {
        self.disk.flush(self.lift.prio()).map_err(|e| e.errno())
    }
    fn lift(&self) -> Option<&Lift> {
        Some(&self.lift)
    }
}

/// FS-7's gate on the boot stick (see the module text).
pub struct StickGate {
    armed: AtomicBool,
    /// Sectors writes may touch: [first FAT sector, end of the volume), plus the FSInfo sector.
    area: SpinLock<Option<(u64, u64, Option<u64>)>>,
    inflight: SpinLock<Vec<(u64, u64)>>,
}

/// One announced write (removed when dropped).
pub struct Announced<'a>(&'a StickGate, (u64, u64));

impl Drop for Announced<'_> {
    fn drop(&mut self) {
        let mut l = self.0.inflight.lock();
        if let Some(i) = l.iter().position(|r| *r == self.1) {
            l.swap_remove(i);
        }
    }
}

impl StickGate {
    pub fn new() -> Arc<StickGate> {
        Arc::new(StickGate { armed: AtomicBool::new(false), area: SpinLock::named("fat.gate.area", None), inflight: SpinLock::named("fat.gate", Vec::new()) })
    }
    pub fn arm(&self, fat_start: u64, end: u64, fsinfo: Option<u64>) {
        *self.area.lock() = Some((fat_start, end, fsinfo));
        self.armed.store(true, Ordering::Release);
    }
    pub fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
    }
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }
    pub fn announce(&self, lba: u64, blocks: u64) -> Announced<'_> {
        self.inflight.lock().push((lba, blocks));
        Announced(self, (lba, blocks))
    }
    /// The gate's answer (pure part; host-tested).
    pub fn decide(&self, w: &WriteCheck<'_>) -> bool {
        if !self.armed() {
            return false;
        }
        let Some((lba, n)) = w.range else { return true };
        if !matches!(w.op, block::Kind::Write { .. }) {
            return false;
        }
        let Some((fat_start, end, fsinfo)) = *self.area.lock() else { return false };
        let inside = (lba >= fat_start && lba.saturating_add(n) <= end) || (Some(lba) == fsinfo && n == 1);
        inside && self.inflight.lock().iter().any(|&(a, k)| lba >= a && lba + n <= a + k)
    }
}

impl WriteGate for StickGate {
    fn name(&self) -> &'static str {
        "fat.stick"
    }
    fn allows(&self, w: &WriteCheck<'_>) -> bool {
        self.decide(w)
    }
    fn opens_boot_stick(&self) -> bool {
        self.armed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use block::DiskKind;

    #[test]
    fn gate_allows_only_announced_sectors() {
        use block::Kind;
        let g = StickGate::new();
        let w = |range, op| WriteCheck { disk: "msd0", kind: DiskKind::Usb, op, range };
        let wr = Kind::Write { fua: false, preflush: false };
        assert!(!g.decide(&w(None, wr)), "unarmed: nothing");
        g.arm(100, 10_000, Some(1));
        assert!(g.decide(&w(None, wr)), "armed: some write may pass");
        assert!(!g.decide(&w(Some((200, 1)), wr)), "not announced");
        {
            let _a = g.announce(200, 8);
            assert!(g.decide(&w(Some((200, 8)), wr)));
            assert!(g.decide(&w(Some((204, 2)), wr)));
            assert!(!g.decide(&w(Some((204, 8)), wr)), "past the announced range");
            assert!(!g.decide(&w(Some((200, 8)), Kind::Discard)), "never a discard");
        }
        assert!(!g.decide(&w(Some((200, 8)), wr)), "the announcement ended");
        let _b = g.announce(0, 1);
        assert!(!g.decide(&w(Some((0, 1)), wr)), "the boot sector / partition table: never");
        let _c = g.announce(1, 1);
        assert!(g.decide(&w(Some((1, 1)), wr)), "FSInfo");
        let _d = g.announce(9_999, 4);
        assert!(!g.decide(&w(Some((9_999, 4)), wr)), "past the volume");
        g.disarm();
        assert!(!g.decide(&w(Some((1, 1)), wr)));
    }
}

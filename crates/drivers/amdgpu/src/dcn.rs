//! `C: DcnRegs` (owner GX): the one owner of BAR5 display registers. Every write is logged and read back; there
//! is no write switch (R8c, Georg 21.10.): each display sequence carries its own gates and undo. Index/data pairs
//! (AZ endpoint 0xE118/0xE11C, SMN) are taken under one lock by every user (SN-5's DP audio included).
//!
//! INT-W8 (SN-5 change request 1): the attach. The first AMD display function (vendor 0x1002, class 0x03) with
//! memory decode on and a 32-bit memory BAR5 is mapped uncached (spec/gpu.md §1.2 steps 2-3), then the read gate
//! 0xE318 == 0x1002AA01 (the Azalia codec ID seen through segment 2, saved-5.json) must hold; otherwise there is
//! no engine. Nothing is written to PCI configuration space. QEMU has no such function: `attached()` is `None`.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use hal::mmio::IoMem;
use crate::layout as l;
use ksync::{IrqSpinLock, IrqSpinLockGuard, Once};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcnError {
    NotAttached,
    OutOfRange,
    /// The write happened; the register reads back another value.
    ReadBackMismatch { offset: u32, wrote: u32, read: u32 },
}

/// Gate register and its value on the Mini-PC's DCN 3.1.4 (codec ID 1002:AA01 through segment 2).
pub const GATE_REG: u32 = 0xE318;
pub const GATE_VALUE: u32 = 0x1002_AA01;
/// The AZ endpoint index/data pair (AZF0ENDPOINT0_AZALIA_F0_CODEC_ENDPOINT_INDEX/_DATA, seg2 0x34C0 + 0x386/0x387).
pub const AZ_ENDPOINT_INDEX: u32 = 0xE118;
pub const AZ_ENDPOINT_DATA: u32 = 0xE11C;
/// `try_pair` gives up after this many tries: a holder on another CPU is done with one access within
/// microseconds; one that never lets go (a stopped CPU) is not waited for (B4).
const PAIR_TRIES: u32 = 1 << 16;
/// GX-6 fixer: the MM_INDEX/MM_DATA/MM_INDEX_HI window (byte 0x0, 0x4, 0x18; linux-6.6 amdgpu_discovery.c:94-97).
/// `read`/`write` refuse these offsets (OutOfRange): only the holder of [`MmWindow`] touches them, so nobody can
/// move the index under a reader's feet.
pub const MM_INDEX: u32 = 0x0;
pub const MM_DATA: u32 = 0x4;
pub const MM_INDEX_HI: u32 = 0x18;
const MM_WINDOW: [u32; 3] = [MM_INDEX, MM_DATA, MM_INDEX_HI];

/// The AZ endpoint pair's lock, interrupts off while held (V1 took `cli` around the pair; spec/gpu.md X10).
pub static AZ_ENDPOINT_PAIR: IrqSpinLock<()> = IrqSpinLock::named("dcn.az_pair", ());

/// The write log: an exact count, and the most recent writes in a lock-free ring (Quiesce writes too, with
/// interrupts off and maybe on the panic path, so it takes no lock and does not log through klog). The ring is a
/// diagnostic window, not a limit: every write is counted and made.
const LOG_SLOTS: usize = 64;
struct WriteLog {
    total: AtomicU64,
    mismatched: AtomicU64,
    /// owner << 61 | offset << 32 | value written (bit 63: masked); the read-back sits in `read` at the same slot.
    slots: [AtomicU64; LOG_SLOTS],
    read: [AtomicU32; LOG_SLOTS],
    /// GX-7m2: the compositor's own writes (`Owner::Flip`, `Owner::Cursor`), and per pipe the flips (a write of
    /// HUBP n's primary surface address) and cursor updates (a write of pipe n's cursor position, control or
    /// address): the picture proof lists them apart from the writes it must not see.
    flips: AtomicU64,
    cursor: AtomicU64,
    flip_pipes: [AtomicU64; l::PIPES as usize],
    cursor_pipes: [AtomicU64; l::PIPES as usize],
}

#[allow(clippy::declare_interior_mutable_const)]
const SLOT: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const RSLOT: AtomicU32 = AtomicU32::new(0);
static LOG: WriteLog = WriteLog {
    total: AtomicU64::new(0),
    mismatched: AtomicU64::new(0),
    slots: [SLOT; LOG_SLOTS],
    read: [RSLOT; LOG_SLOTS],
    flips: AtomicU64::new(0),
    cursor: AtomicU64::new(0),
    flip_pipes: [SLOT; l::PIPES as usize],
    cursor_pipes: [SLOT; l::PIPES as usize],
};

/// GX-7m2: who made a display write. The picture proof of a GX run (GX-D9) counts every write but the
/// compositor's own page flips (`flip::order`) and cursor updates (`cursor`), which it lists apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Other,
    Flip,
    Cursor,
}

impl Owner {
    fn bits(self) -> u64 {
        match self {
            Owner::Other => 0,
            Owner::Flip => 1,
            Owner::Cursor => 2,
        }
    }

    fn from_bits(b: u64) -> Owner {
        match b & 3 {
            1 => Owner::Flip,
            2 => Owner::Cursor,
            _ => Owner::Other,
        }
    }
}

/// GX-7m2: the write counts by owner at one moment (`marks`); two of them bound a run. `total` is read before the
/// owners' counts and `total_after` after them (a writer adds to the total first), so a write in flight at either
/// end is counted against the run, never hidden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub total: u64,
    pub total_after: u64,
    pub flips: u64,
    pub cursor: u64,
    pub flip_pipes: [u64; l::PIPES as usize],
    pub cursor_pipes: [u64; l::PIPES as usize],
}

impl Marks {
    /// Every write between `self` and `later` (at least).
    pub fn all_until(&self, later: &Marks) -> u64 {
        later.total_after.wrapping_sub(self.total)
    }

    /// Writes between `self` and `later` that were not the compositor's (at least).
    pub fn others_until(&self, later: &Marks) -> u64 {
        self.all_until(later).saturating_sub(later.flips.wrapping_sub(self.flips)).saturating_sub(later.cursor.wrapping_sub(self.cursor))
    }

    /// The pipes the compositor flipped (`flip`) or moved the cursor on between `self` and `later`.
    pub fn pipes_until(&self, later: &Marks, flip: bool) -> impl Iterator<Item = u32> + '_ {
        let (a, b) = if flip { (self.flip_pipes, later.flip_pipes) } else { (self.cursor_pipes, later.cursor_pipes) };
        (0..l::PIPES).filter(move |&p| a[p as usize] != b[p as usize])
    }
}

pub fn marks() -> Marks {
    let ld = |a: &AtomicU64| a.load(Ordering::Acquire);
    let total = ld(&LOG.total);
    let flips = ld(&LOG.flips);
    let cursor = ld(&LOG.cursor);
    let flip_pipes = core::array::from_fn(|i| ld(&LOG.flip_pipes[i]));
    let cursor_pipes = core::array::from_fn(|i| ld(&LOG.cursor_pipes[i]));
    Marks { total, total_after: ld(&LOG.total), flips, cursor, flip_pipes, cursor_pipes }
}

/// The pipe an owned write names (`None` for the lock and other registers of the sequence).
fn pipe_of(owner: Owner, offset: u32) -> Option<usize> {
    (0..l::PIPES).find(|&p| match owner {
        Owner::Flip => offset == l::hubp(p, l::HUBP_ADDR),
        Owner::Cursor => [l::CUR_POSITION, l::CUR_CONTROL, l::CUR_ADDR].iter().any(|&r| offset == l::hubp(p, r)),
        Owner::Other => false,
    }).map(|p| p as usize)
}

/// The window's lock, interrupts off while held (Linux: mmio_idx_lock, amdgpu_device.c:260).
static MM_INDEX_PAIR: IrqSpinLock<()> = IrqSpinLock::named("amdgpu.mm_index", ());
/// Index writes through the window since boot: counted here, not in the display log, so a discovery read
/// (about 2 100 index writes) does not push the display writes out of the ring that read-back and undo use.
static WINDOW_WRITES: AtomicU64 = AtomicU64::new(0);

/// Index writes made through [`MmWindow`] since boot.
pub fn window_writes() -> u64 {
    WINDOW_WRITES.load(Ordering::Acquire)
}

/// The held MM_INDEX window: the only way to the three window offsets.
pub struct MmWindow<'a> {
    regs: &'a DcnRegs,
    _g: IrqSpinLockGuard<'static, ()>,
}

impl MmWindow<'_> {
    fn check(&self, offset: u32) -> Result<usize, DcnError> {
        if !MM_WINDOW.contains(&offset) || offset as usize + 4 > self.regs.io.size() {
            return Err(DcnError::OutOfRange);
        }
        Ok(offset as usize)
    }

    /// MM_INDEX, MM_INDEX_HI or MM_DATA.
    pub fn read(&self, offset: u32) -> Result<u32, DcnError> {
        let o = self.check(offset)?;
        self.regs.io.read32(o).map_err(|_| DcnError::OutOfRange)
    }

    /// MM_INDEX or MM_INDEX_HI (MM_DATA writes graphics memory: refused). Counted in `window_writes`, not read
    /// back (Linux does not either; the caller restores both index registers and the ktest compares them).
    pub fn write_index(&self, offset: u32, value: u32) -> Result<(), DcnError> {
        if offset == MM_DATA {
            return Err(DcnError::OutOfRange);
        }
        let o = self.check(offset)?;
        self.regs.io.write32(o, value).map_err(|_| DcnError::OutOfRange)?;
        WINDOW_WRITES.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

/// One logged write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoggedWrite {
    pub seq: u64,
    pub offset: u32,
    pub wrote: u32,
    pub read: u32,
    /// Only some bits were compared (`write_masked`): a differing `read` there is status, not a mismatch.
    pub masked: bool,
    /// GX-7m2: who made it.
    pub owner: Owner,
}

/// Bit 63 of a log slot: the write compared only some bits.
const MASKED: u64 = 1 << 63;
/// Bits 62:61 of a log slot: the owner (GX-7m2); the offset keeps bits 60:32.
const OWNER_SHIFT: u32 = 61;

/// Writes made since boot, and how many of them read back another value.
pub fn write_counts() -> (u64, u64) {
    (LOG.total.load(Ordering::Acquire), LOG.mismatched.load(Ordering::Acquire))
}

/// The most recent writes, oldest first (at most the ring's size; `seq` numbers them since boot).
pub fn recent_writes() -> alloc::vec::Vec<LoggedWrite> {
    let total = LOG.total.load(Ordering::Acquire);
    let first = total.saturating_sub(LOG_SLOTS as u64);
    (first..total)
        .map(|seq| {
            let i = (seq % LOG_SLOTS as u64) as usize;
            let s = LOG.slots[i].load(Ordering::Acquire);
            LoggedWrite {
                seq,
                offset: (s >> 32) as u32 & 0x1FFF_FFFF,
                wrote: s as u32,
                read: LOG.read[i].load(Ordering::Acquire),
                masked: s & MASKED != 0,
                owner: Owner::from_bits(s >> OWNER_SHIFT),
            }
        })
        .collect()
}

pub struct DcnRegs {
    io: IoMem,
    /// The display function, for logs.
    function: alloc::string::String,
    /// The function as the scan found it (GX-2's gates and the device object read its BARs and bridges).
    pci: pci::PciFunction,
    /// BAR5 phys, size.
    bar5: (u64, u64),
}

static ATTACH: Once<Option<DcnRegs>> = Once::new();

fn refuse(function: &str, why: &str) -> Option<DcnRegs> {
    klog::event!(Warn, "dcn", "attach.refused", function = function, why = why; "no display engine");
    None
}

fn probe() -> Option<DcnRegs> {
    let f = pci::devices().into_iter().find(|f| f.vendor == 0x1002 && f.class_code() == 0x03)?;
    let name = alloc::format!("{}", f.addr);
    if f.command() & 0x2 == 0 {
        return refuse(&name, "memory decode off");
    }
    let Some(pci::Bar::Mem { phys, size, is64: false, .. }) = f.bar(5) else {
        return refuse(&name, "BAR5 is not a 32-bit memory BAR");
    };
    if phys == 0 || size < u64::from(GATE_REG) + 4 {
        return refuse(&name, "BAR5 empty or too small");
    }
    let io = match hal::mmio::map_device(phys, size as usize, hal::paging::CacheType::Uncached) {
        Ok(io) => io,
        Err(e) => return refuse(&name, &alloc::format!("map BAR5: {e:?}")),
    };
    let gate = io.read32(GATE_REG as usize).unwrap_or(u32::MAX);
    if gate != GATE_VALUE {
        klog::event!(Warn, "dcn", "attach.gate", function = name.as_str(), reg = GATE_REG, read = gate, want = GATE_VALUE;
            "read gate failed: no display engine");
        return None;
    }
    klog::event!(Info, "dcn", "attached", function = name.as_str(), bar5 = phys, size = size;
        "display registers mapped uncached");
    Some(DcnRegs { io, function: name, pci: f, bar5: (phys, size) })
}

impl DcnRegs {
    /// The display engine, attached on first use (thread context: it maps BAR5 and logs).
    pub fn attached() -> Option<&'static DcnRegs> {
        ATTACH.get_or_init(probe).as_ref()
    }

    /// The engine only if an earlier call attached it: for Quiesce and interrupt context (never maps or logs).
    pub fn attached_now() -> Option<&'static DcnRegs> {
        ATTACH.get().and_then(Option::as_ref)
    }

    pub fn function(&self) -> &str {
        &self.function
    }

    pub fn pci(&self) -> &pci::PciFunction {
        &self.pci
    }

    /// BAR5 (phys, size) as mapped.
    pub fn bar5(&self) -> (u64, u64) {
        self.bar5
    }

    fn check(&self, offset: u32) -> Result<usize, DcnError> {
        let o = offset as usize;
        if offset % 4 != 0 || o.checked_add(4).is_none_or(|e| e > self.io.size()) || MM_WINDOW.contains(&offset) {
            return Err(DcnError::OutOfRange);
        }
        Ok(o)
    }

    /// Any dword-aligned byte offset inside BAR5.
    pub fn read(&self, offset: u32) -> Result<u32, DcnError> {
        let o = self.check(offset)?;
        self.io.read32(o).map_err(|_| DcnError::OutOfRange)
    }

    /// Writes, reads back and logs (no lock, no sleep, no klog: safe with interrupts off, so SN-5's undo can run
    /// in Quiesce).
    pub fn write(&self, offset: u32, value: u32) -> Result<(), DcnError> {
        self.write_masked(offset, value, u32::MAX)
    }

    /// `write` that compares only the bits in `check` on the read-back (GX-3: registers with status bits the
    /// hardware sets, e.g. OTG_MASTER_UPDATE_LOCK's UPDATE_LOCK_STATUS, DCSURF_FLIP_CONTROL's PENDING). The log
    /// keeps the whole value read; a mismatch is counted only inside `check`.
    pub fn write_masked(&self, offset: u32, value: u32, check: u32) -> Result<(), DcnError> {
        self.write_owned(offset, value, check, Owner::Other)
    }

    /// `write_masked` on behalf of `owner` (GX-7m2: the compositor's flips and cursor updates are told apart).
    pub fn write_owned(&self, offset: u32, value: u32, check: u32, owner: Owner) -> Result<(), DcnError> {
        let o = self.check(offset)?;
        self.io.write32(o, value).map_err(|_| DcnError::OutOfRange)?;
        let read = self.io.read32(o).unwrap_or(u32::MAX);
        // The total before the owner's counts (`marks` reads the total on both sides of them): a write in flight
        // while a run is bounded is counted against the run, never hidden.
        let seq = LOG.total.fetch_add(1, Ordering::AcqRel);
        match owner {
            Owner::Other => {}
            Owner::Flip => {
                LOG.flips.fetch_add(1, Ordering::AcqRel);
            }
            Owner::Cursor => {
                LOG.cursor.fetch_add(1, Ordering::AcqRel);
            }
        }
        if let Some(p) = pipe_of(owner, offset) {
            let per = if owner == Owner::Flip { &LOG.flip_pipes } else { &LOG.cursor_pipes };
            per[p].fetch_add(1, Ordering::AcqRel);
        }
        let i = (seq % LOG_SLOTS as u64) as usize;
        let flag = if check == u32::MAX { 0 } else { MASKED };
        LOG.slots[i].store(flag | owner.bits() << OWNER_SHIFT | u64::from(offset & 0x1FFF_FFFF) << 32 | u64::from(value), Ordering::Release);
        LOG.read[i].store(read, Ordering::Release);
        if (read ^ value) & check != 0 {
            LOG.mismatched.fetch_add(1, Ordering::AcqRel);
            return Err(DcnError::ReadBackMismatch { offset, wrote: value, read });
        }
        Ok(())
    }

    /// `f` with the AZ endpoint pair held (interrupts off): every user of 0xE118/0xE11C goes through this.
    pub fn with_az_endpoint_pair<T>(&self, f: impl FnOnce(&DcnRegs) -> T) -> T {
        let _g = AZ_ENDPOINT_PAIR.lock();
        f(self)
    }

    /// GX-7d (7b's D8): a write outside the display's log, for the bring-up owner only (`psplive::LiveHw`),
    /// which logs every write with its read-back in its own log: PSP mailbox writes must not count as display
    /// writes (the display log and `dcn_writes` stay the scanout's evidence). The same range check as `write`
    /// (never the MM_INDEX window). No lock, no sleep, no klog: the Quiesce hook uses it too.
    pub fn write_raw(&self, offset: u32, value: u32) -> Result<(), DcnError> {
        let o = self.check(offset)?;
        self.io.write32(o, value).map_err(|_| DcnError::OutOfRange)
    }

    /// The MM_INDEX window, held (interrupts off) until the `MmWindow` is dropped; bounded tries like the AZ pair
    /// (B4), `None` = busy.
    pub fn try_mm_window(&self) -> Option<MmWindow<'_>> {
        for _ in 0..PAIR_TRIES {
            if let Some(g) = MM_INDEX_PAIR.try_lock() {
                return Some(MmWindow { regs: self, _g: g });
            }
            core::hint::spin_loop();
        }
        None
    }

    /// For Quiesce: bounded tries; `None` = the pair is busy (SN maps it to `RegError::PairBusy`).
    pub fn try_with_az_endpoint_pair<T>(&self, f: impl FnOnce(&DcnRegs) -> T) -> Option<T> {
        let g = try_pair()?;
        let r = f(self);
        drop(g);
        Some(r)
    }
}

/// The gates' and the snapshot's reader (GX-2): refuses what `read` refuses (the MM_INDEX window too) and, in addition, the NBIO seg0
/// aliases (X8), the AZ index/data pair (read only under the pair lock by its user) and anything past the display
/// window (0x80000).
impl crate::attach::Regs for DcnRegs {
    fn read(&self, byte: u32) -> Option<u32> {
        if !crate::snapshot::readable(byte) {
            return None;
        }
        DcnRegs::read(self, byte).ok()
    }
}

/// The pair lock with bounded tries (shared by `try_with_az_endpoint_pair` and users that hold it by guard).
pub fn try_pair() -> Option<IrqSpinLockGuard<'static, ()>> {
    for _ in 0..PAIR_TRIES {
        if let Some(g) = AZ_ENDPOINT_PAIR.try_lock() {
            return Some(g);
        }
        core::hint::spin_loop();
    }
    None
}

/// On the Mini-PC (QEMU has no engine): the gate reads, bad offsets are refused before any access and not
/// logged, the pair is free.
fn contract() -> ktest::TestResult {
    if let Some(r) = DcnRegs::attached() {
        let (before, _) = write_counts();
        ktest::kassert!(r.read(GATE_REG) == Ok(GATE_VALUE), "gate reads {:?}", r.read(GATE_REG));
        ktest::kassert!(r.read(GATE_REG + 1) == Err(DcnError::OutOfRange), "unaligned read accepted");
        ktest::kassert!(r.read(u32::MAX - 3) == Err(DcnError::OutOfRange), "read past BAR5 accepted");
        ktest::kassert!(r.write(u32::MAX - 3, 0) == Err(DcnError::OutOfRange), "write past BAR5 accepted");
        ktest::kassert!(r.write(GATE_REG + 1, 0) == Err(DcnError::OutOfRange), "unaligned write accepted");
        ktest::kassert!(write_counts().0 == before, "a refused write was logged as made");
        for o in MM_WINDOW {
            ktest::kassert!(r.read(o) == Err(DcnError::OutOfRange), "window offset {o:#x} readable outside MmWindow");
            ktest::kassert!(r.write(o, 0) == Err(DcnError::OutOfRange), "window offset {o:#x} writable outside MmWindow");
        }
        ktest::kassert!(write_counts().0 == before, "a refused window write was logged as made");
        ktest::kassert!(r.try_with_az_endpoint_pair(|_| 1) == Some(1), "free pair reported busy");
    }
    Ok(())
}
ktest::ktest!("contract.dcn_regs", Safe, contract);

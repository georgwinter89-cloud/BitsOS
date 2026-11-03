//! GX-7i: the SDMA stage (roadmap wave 25, milestone M4): SDMA 0's ring 0 started as Linux starts it, then the
//! test ladder's rungs, each checked by the CPU:
//! 1. WRITE_LINEAR of 0xDEADBEEF into a carve-out dword (Linux's ring test) and the rptr writeback;
//! 2. CONST_FILL of 4 KiB with fillsize 0 (byte 0xA5) and 4 KiB with fillsize 2 (dword 0x11223344), then a FENCE;
//! 3. the capture's golden IB (the raw-IB harness's WRITE_LINEAR, its target patched) through INDIRECT + FENCE;
//! 4. COPY_LINEAR of 8 KiB from the carve-out into two system pages bound in the GART (the first GTT pages), a
//!    64-bit FENCE + TRAP, the pages read back by the CPU, then unbound and the TLB flushed;
//! 5. the doorbell: the ring stopped and started again with its doorbell (NBIO aperture and CSDMA range on), a
//!    WRITE_LINEAR + FENCE committed by a 64-bit write to BAR2 instead of the wptr register.
//!
//! Everything the engine reads or writes (ring, writeback slots, IB, targets) lies in one carve-out BO kept for
//! the boot (roadmap risk 2: the first SDMA buffers in the carve-out, reached through GFXHUB's system aperture;
//! the GART only for rung 4). Generic over the device like `gmc`: the fakes run the same rungs through GR-2's
//! interpreter (`sdmafake`), the live owner on the Mini-PC. Every register write goes through the bring-up owner
//! (logged with its read-back) and the scanout watcher.
//!
//! Linux 6.6 (amdgpu/sdma_v6_0.c): start :781 (enable :439, ctxempty :415), gfx_resume :467-619 (the register
//! order; Linux 7.1 on the 780M adds WATCHDOG_CNTL, the capture), set_wptr :156 (the wptr shadow, then the doorbell
//! or RB_WPTR/RB_WPTR_HI), ring_test_ring :873 (0xCAFEDEAD -> 0xDEADBEEF, 100 ms), emit_fence :338, gfx_stop :379;
//! nbio_v7_7.c:65-118 (CSDMA doorbell range through RSMU, the doorbell aperture); amdgpu_sdma.c:187 (burst NOP).

use super::bringup::{Bringup, Outcome as WriteOutcome};
use super::carvetable::CarveGart;
use super::disc::Discovery;
use super::gmc::{self, FaultRead, Flush, Geometry, HubAddrs};
use super::hwid;
use super::psp::{BoAlloc, Hw};
use super::ucode::{FwSet, Kind as FwKind};
use super::watch::Watch;
use crate::bo::{BoError, PAGE};
use crate::ipmod::nbio7;
use crate::ipmod::sdma6::{self, R, f};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use amdgpu_fmt::fence::{Half, Timeline, read_split};
use amdgpu_fmt::gart::{Gart, GartPage};
use amdgpu_fmt::pte::Target;
use amdgpu_fmt::ring::{Kind, Ring, RingError};
use amdgpu_fmt::sdma as pkt;
use amdgpu_fmt::wb::WbPool;

/// The BO's layout (carve-out offsets from its start).
pub mod lay {
    pub const RING: u64 = 0;
    pub const WB: u64 = 0x2000;
    pub const TARGET: u64 = 0x3000;
    pub const IB: u64 = 0x4000;
    pub const FILL_A: u64 = 0x5000;
    pub const FILL_B: u64 = 0x6000;
    pub const SRC: u64 = 0x7000;
    pub const END: u64 = 0x9000;
}
/// The BO the stage asks for (the live allocator rounds it up to a whole 2 MiB piece).
pub const BO_BYTES: u64 = 0x1_0000;
pub const FILL_BYTES: u64 = 4096;
pub const COPY_BYTES: u64 = 8192;
/// System pages rung 4 binds (two: the copy crosses a page boundary).
pub const SYS_PAGES: u64 = 2;
/// Where in the GART window they are bound (the first pages; unbound again after the rung).
pub const GART_OFFSET: u64 = 0;
/// Linux's ring test poison (sdma_v6_0.c:881).
pub const POISON: u32 = 0xCAFE_DEAD;
pub const RING_TEST: u32 = 0xDEAD_BEEF;
pub const FILL_BYTE_VALUE: u32 = 0xA5;
pub const FILL_DWORD_VALUE: u32 = 0x1122_3344;
pub const DOORBELL_VALUE: u32 = 0xD00D_BE11;
/// The 64-bit fence's timeline starts below a 2^32 boundary, so its value changes both halves.
pub const FENCE64_START: u64 = 0x1_ffff_ffff;
/// The raw-IB harness's SDMA IB on the real 780M (plan/hw capture, bundle/ibtest.out): WRITE_LINEAR of four
/// dwords to VA 0x1_0000_2000. Words 1-2 (the address) are patched with the stage's target.
pub const GOLDEN_IB: [u32; 8] = [0x0000_0002, 0x0000_2000, 0x0000_0001, 0x0000_0003, 0xdead_beef, 0x0bad_f00d, 0x1234_5678, 0x9abc_def0];
/// What the capture read back after it ("RESULT sdma ok got deadbeef 0badf00d 12345678 9abcdef0").
pub const GOLDEN_RESULT: [u32; 4] = [0xdead_beef, 0x0bad_f00d, 0x1234_5678, 0x9abc_def0];

/// The rungs' names (also the checks' names), in order.
pub const RUNGS: [&str; 5] = [
    "SDMA rung 1 WRITE_LINEAR",
    "SDMA rung 2 CONST_FILL + FENCE",
    "SDMA golden IB (capture)",
    "SDMA rung 3 COPY_LINEAR into GART pages",
    "SDMA rung 4 doorbell",
];

/// NBIO's doorbell side as BAR5 bytes (from the discovery table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NbioDoorbell {
    /// RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN.
    pub aper_en: u32,
    /// BIF_BX_PF0_RSMU_INDEX / _DATA (the PCIE port window).
    pub rsmu_index: u32,
    pub rsmu_data: u32,
    /// The RSMU index value for GDC0_BIF_CSDMA_DOORBELL_RANGE (its byte address in NBIO's segment 3).
    pub csdma_range: u32,
}

impl NbioDoorbell {
    /// The only value the RSMU index may be written with.
    pub fn index_ok(&self, byte: u32, value: u32) -> bool {
        byte == self.rsmu_index && value == self.csdma_range
    }
}

/// SDMA 0's registers as BAR5 bytes (GC's segment bases, as Linux's get_reg_offset), NBIO's doorbell side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdmaAddrs {
    base: [u32; 2],
    pub nbio: Option<NbioDoorbell>,
}

impl SdmaAddrs {
    pub fn from_discovery(d: &Discovery) -> Option<SdmaAddrs> {
        let base = [d.reg(hwid::GC, 0, 0, 0)?, d.reg(hwid::GC, 0, 1, 0)?];
        let nb = |at: (usize, u32)| d.reg(hwid::NBIF, 0, at.0, at.1).and_then(|x| x.checked_mul(4));
        let nbio = (|| {
            Some(NbioDoorbell {
                aper_en: nb(nbio7::RCC_DOORBELL_APER_EN)?,
                rsmu_index: nb(sdma6::RSMU_INDEX)?,
                rsmu_data: nb(sdma6::RSMU_DATA)?,
                csdma_range: nb(sdma6::CSDMA_DOORBELL_RANGE)?,
            })
        })();
        Some(SdmaAddrs { base, nbio })
    }

    pub fn byte(&self, r: R) -> Option<u32> {
        let (seg, dw) = r.at();
        self.base.get(seg)?.checked_add(dw)?.checked_mul(4)
    }

    /// The register's name at `byte` (SDMA0 and NBIO's doorbell registers).
    pub fn name_at(&self, byte: u32) -> Option<&'static str> {
        if let Some(r) = R::ALL.into_iter().find(|&r| self.byte(r) == Some(byte)) {
            return Some(r.name());
        }
        let n = self.nbio?;
        [(n.aper_en, "RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN"), (n.rsmu_index, "BIF_BX_PF0_RSMU_INDEX"), (n.rsmu_data, "BIF_BX_PF0_RSMU_DATA")]
            .into_iter()
            .find(|(b, _)| *b == byte)
            .map(|(_, s)| s)
    }

    /// Every register the stage reads.
    pub fn readable(&self) -> Vec<u32> {
        let mut v: Vec<u32> = R::ALL.into_iter().filter_map(|r| self.byte(r)).collect();
        if let Some(n) = self.nbio {
            v.extend([n.aper_en, n.rsmu_index, n.rsmu_data]);
        }
        v
    }

    /// Every register the stage writes (the RSMU index only with `NbioDoorbell::index_ok`'s value: the live
    /// owner checks it).
    pub fn writable(&self) -> Vec<u32> {
        let w = [
            R::Cntl,
            R::SemWaitFailTimer,
            R::Watchdog,
            R::Utcl1Cntl,
            R::Utcl1Page,
            R::RbCntl,
            R::RbBase,
            R::RbBaseHi,
            R::RbRptr,
            R::RbRptrHi,
            R::RbWptr,
            R::RbWptrHi,
            R::RptrAddrHi,
            R::RptrAddrLo,
            R::IbCntl,
            R::Doorbell,
            R::DoorbellOffset,
            R::WptrPollHi,
            R::WptrPollLo,
            R::MinorPtrUpdate,
            R::F32Cntl,
            R::QueueResetReq,
        ];
        let mut v: Vec<u32> = w.into_iter().filter_map(|r| self.byte(r)).collect();
        if let Some(n) = self.nbio {
            v.extend([n.aper_en, n.rsmu_data]);
        }
        v
    }
}

/// System pages for rung 4 (live: MM-14's `DmaPages`, pinned while the GPU may use them; fakes: the chip fake's
/// system memory).
pub trait SysPages {
    fn pages(&self) -> u64;
    /// The page's address as the device sees it (the GART entry's physical address).
    fn device_addr(&self, page: u64) -> Option<u64>;
    fn read(&mut self, off: u64, out: &mut [u8]) -> bool;
    fn write(&mut self, off: u64, data: &[u8]) -> bool;
    /// CPU stores reach memory before the GPU reads it.
    fn barrier(&mut self) {}
}

/// Where rung 4's pages come from.
pub trait SysAlloc {
    fn alloc(&self, pages: u64) -> Result<Box<dyn SysPages>, String>;
}

/// The buffer of the stage (kept for the boot: the engine's ring and writeback live in it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdmaBo {
    pub handle: u32,
    /// Carve-out offsets.
    pub offset: u64,
    pub size: u64,
}

/// What the GMC stage left: the GART window and GFXHUB (the SDMA's hub) for flushes and faults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GartUse {
    pub geo: Geometry,
    pub gfx: HubAddrs,
    pub flush_us: u64,
}

#[derive(Clone, Copy)]
pub struct SdmaInputs<'a> {
    pub addrs: SdmaAddrs,
    pub carve: Carveout,
    /// The firmware's burst NOP (`burst_nop`).
    pub burst_nop: bool,
    pub timeout_us: u64,
    /// The BO of an earlier run in this boot (reused).
    pub bo: Option<SdmaBo>,
    /// Rung 4's pages; `None`: rung 4 fails as "no system pages".
    pub sys: Option<&'a dyn SysAlloc>,
    /// Called once, after SDMA 0 read powered and before the stage's first write: the live side arms its Quiesce
    /// halt here, so a hand-over in the middle of the stage halts the engine too.
    pub arm: Option<&'a dyn Fn(SdmaAddrs)>,
}

impl SdmaInputs<'_> {
    pub fn new(d: &Discovery, carve: Carveout, burst_nop: bool) -> Option<SdmaInputs<'static>> {
        Some(SdmaInputs { addrs: SdmaAddrs::from_discovery(d)?, carve, burst_nop, timeout_us: sdma6::LINUX_TIMEOUT_US, bo: None, sys: None, arm: None })
    }
}

/// Linux's burst NOP rule: the SDMA firmware's feature version (header +32, v1 and v2) at least 20.
pub fn burst_nop(fw: &FwSet) -> bool {
    fw.get(FwKind::Sdma).is_some_and(|f| {
        u64::from(f.header.header_size_bytes) >= 36 && f.bytes.get(32..36).is_some_and(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) >= sdma6::BURST_NOP_FEATURE)
    })
}

/// Where everything lies (MC addresses through GFXHUB's system aperture).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub bo_mc: u64,
    pub ring: u64,
    pub rptr: u64,
    pub wptr: u64,
    pub fence32: u64,
    pub fence64: u64,
    pub fence_ib: u64,
    pub fence_db: u64,
    pub target: u64,
    pub golden: u64,
    pub db_target: u64,
    pub ib: u64,
    pub fill_a: u64,
    pub fill_b: u64,
    pub src: u64,
}

impl Layout {
    /// The writeback slots from GR-2's pool (32 bytes each), in the order Linux's ring takes them.
    pub fn new(bo_mc: u64) -> Option<Layout> {
        let mut wb = WbPool::new(((lay::TARGET - lay::WB) / 32) as u32);
        let wbase = bo_mc + lay::WB;
        let mut slot = || wb.get().ok().map(|s| s.gpu_addr(wbase));
        let t = bo_mc + lay::TARGET;
        Some(Layout {
            bo_mc,
            ring: bo_mc + lay::RING,
            rptr: slot()?,
            wptr: slot()?,
            fence32: slot()?,
            fence64: slot()?,
            fence_ib: slot()?,
            fence_db: slot()?,
            target: t,
            golden: t + 0x40,
            db_target: t + 0x80,
            ib: bo_mc + lay::IB,
            fill_a: bo_mc + lay::FILL_A,
            fill_b: bo_mc + lay::FILL_B,
            src: bo_mc + lay::SRC,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SdmaError {
    NoAddress,
    /// SDMA0_F32_CNTL reads all ones: GC is gated (nothing written).
    GcOff { byte: u32 },
    Read { byte: u32 },
    Write { byte: u32 },
    Bo(BoError),
    BoTooSmall { have: u64, need: u64 },
    Mem { mc: u64 },
    Ring(RingError),
    /// A rung's rptr writeback or result never came.
    Timeout { rung: &'static str, rptr: Option<u64>, want: u64 },
    /// The rung completed but its memory did not read as expected.
    Result { rung: &'static str },
    /// The engine did not start: F32_CNTL / RB_CNTL as read after the resume.
    NotStarted { f32: Option<u32>, rb: Option<u32> },
    NoSys(String),
    Gart(String),
    Flush,
    /// No NBIO addresses, or the doorbell write was refused (no BAR2).
    NoDoorbell,
    /// GX-7i2: after the resume the queue is not idle at its start (RB_RPTR and RB_WPTR, bytes, differ): the
    /// pointers of an earlier run were neither reset nor taken over.
    StalePointers { rptr: u64, wptr: u64 },
}

impl SdmaError {
    pub fn errno(&self) -> i32 {
        match self {
            SdmaError::Bo(_) | SdmaError::BoTooSmall { .. } | SdmaError::NoSys(_) => 12,
            SdmaError::Timeout { .. } | SdmaError::Flush => 110,
            SdmaError::Mem { .. } => 14,
            SdmaError::Ring(e) => e.errno(),
            SdmaError::Gart(_) => 22,
            SdmaError::NoAddress | SdmaError::NoDoorbell => 19,
            _ => 5,
        }
    }

    pub fn advice(&self) -> &'static str {
        match self {
            SdmaError::GcOff { .. } => ": GC is not on: run the GFX and GMC stages first (until=gart); nothing was written",
            SdmaError::Timeout { .. } => ": the engine did not finish the rung within Linux's 100 ms: it is halted again; report the lines",
            SdmaError::Result { .. } => ": the rung completed but its result differs (see its line): the engine is halted again; report it",
            SdmaError::NotStarted { .. } => ": the engine did not read back started after the resume: it is halted again",
            SdmaError::NoDoorbell => ": no doorbell (BAR2 not mapped or NBIO unknown): the register path stands",
            SdmaError::Flush => ": GFXHUB did not acknowledge the TLB flush around the GART pages: the pages are kept (never freed)",
            SdmaError::StalePointers { .. } => ": RB_RPTR and RB_WPTR still hold an earlier run's values after the restart (see 'sdma restart'): the engine is halted again; report the lines",
            _ => "",
        }
    }
}

/// One rung's result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rung {
    pub name: &'static str,
    pub pass: bool,
    /// The rptr writeback reached the write pointer and the rung's own completion (fence or target) came.
    pub done: bool,
    pub doorbell: bool,
    /// The committed write pointer (dwords) and the rptr writeback read (bytes).
    pub wptr: u64,
    pub rptr: Option<u64>,
    pub fence: Option<u64>,
    pub polls: u32,
    pub us: u64,
    pub detail: String,
}

/// Rung 4's GART pages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GartPages {
    pub mc: u64,
    pub phys: Vec<u64>,
    pub unbound: bool,
    pub flushes: Vec<Flush>,
    /// The pages could not be proven unbound: kept (never freed) for the boot.
    pub leaked: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SdmaReport {
    pub layout: Option<Layout>,
    pub bo: Option<SdmaBo>,
    pub reused_bo: bool,
    pub burst_nop: bool,
    /// The registers as found (before anything was written).
    pub before: Vec<(R, Option<u32>)>,
    /// NBIO's doorbell side before / after (aperture enable, CSDMA range).
    pub nbio: Vec<(&'static str, Option<u32>, Option<u32>)>,
    /// The engine ran or was unhalted when found: stopped first.
    pub stopped_first: bool,
    /// Each resume: doorbell on, its writes, and the owner's write count when it began / ended.
    pub resumes: Vec<(bool, usize, u64, u64)>,
    /// F32_CNTL and RB_CNTL read right after the first resume.
    pub started: Option<(Option<u32>, Option<u32>)>,
    /// GX-7i2: each restart of a queue that held an earlier run's pointers (before its resume).
    pub rewinds: Vec<Rewind>,
    pub rungs: Vec<Rung>,
    /// Doorbell writes: (dword index, value, accepted).
    pub doorbells: Vec<(u32, u64, bool)>,
    pub gart: Option<GartPages>,
    pub fault_before: Option<FaultRead>,
    pub fault_after: Option<FaultRead>,
    pub after: Vec<(R, Option<u32>)>,
    /// The engine stays running (Linux's state); otherwise halted again.
    pub left_running: bool,
    pub error: Option<SdmaError>,
}

/// GX-7i2: a restart's pointer reset (before the resume): the queue's ring pointers (bytes) and DOORBELL as found
/// and after the reset, and where the queue starts (bytes: 0, or the RB_RPTR the engine kept).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rewind {
    pub found: (u64, u64, u32),
    pub after: (u64, u64, u32),
    pub start: u64,
}

/// GX-7i2: NBIO's doorbell side as this boot's first SDMA stage found it (what a stop puts back).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NbioFound {
    pub aper: u32,
    pub range: u32,
}

impl SdmaReport {
    /// NBIO's doorbell side as this stage found it (both read).
    pub fn nbio_found(&self) -> Option<NbioFound> {
        match self.nbio.as_slice() {
            [(_, Some(aper), _), (_, Some(range), _)] => Some(NbioFound { aper: *aper, range: *range }),
            _ => None,
        }
    }

    /// This stage left the doorbell aperture on where it found it off.
    pub fn turned_aperture_on(&self) -> bool {
        matches!(self.nbio.first(), Some((_, Some(b), Some(a))) if b & 1 == 0 && a & 1 != 0)
    }

    pub fn passed(&self) -> bool {
        self.error.is_none() && checks(self).iter().all(|c| c.pass)
    }

    pub fn before_of(&self, r: R) -> Option<u32> {
        self.before.iter().find(|x| x.0 == r).and_then(|x| x.1)
    }
}

/// The registers the stage reads first (and the Linux plan's reads).
const BEFORE: [R; 13] = [R::F32Cntl, R::Cntl, R::Status, R::UcodeChecksum, R::Ucode1Checksum, R::RbCntl, R::IbCntl, R::Doorbell, R::DoorbellOffset, R::Watchdog, R::Utcl1Cntl, R::Utcl1Page, R::Freeze];

/// Register access for the resume: the owner (logged) or a recorder (the Linux plan).
trait Regs {
    fn rd(&mut self, byte: u32) -> Option<u32>;
    fn wr(&mut self, byte: u32, value: u32) -> bool;
}

impl<H: Hw> Regs for Bringup<Watch<H>> {
    fn rd(&mut self, byte: u32) -> Option<u32> {
        self.reg_read(byte)
    }
    fn wr(&mut self, byte: u32, value: u32) -> bool {
        matches!(self.write(byte, value).outcome, WriteOutcome::Ok | WriteOutcome::Mismatch)
    }
}

/// Answers reads from what was found, records the writes (`linux_sequence`).
struct Rec {
    vals: Vec<(u32, u32)>,
    writes: Vec<(u32, u32)>,
}

impl Regs for Rec {
    fn rd(&mut self, byte: u32) -> Option<u32> {
        Some(self.vals.iter().rev().find(|x| x.0 == byte).map_or(0, |x| x.1))
    }
    fn wr(&mut self, byte: u32, value: u32) -> bool {
        self.writes.push((byte, value));
        self.vals.push((byte, value));
        true
    }
}

fn rd(o: &mut dyn Regs, a: &SdmaAddrs, r: R) -> Result<u32, SdmaError> {
    let b = a.byte(r).ok_or(SdmaError::NoAddress)?;
    match o.rd(b) {
        Some(u32::MAX) if r == R::F32Cntl => Err(SdmaError::GcOff { byte: b }),
        Some(v) => Ok(v),
        None => Err(SdmaError::Read { byte: b }),
    }
}

fn wr(o: &mut dyn Regs, a: &SdmaAddrs, r: R, v: u32, n: &mut usize) -> Result<(), SdmaError> {
    let b = a.byte(r).ok_or(SdmaError::NoAddress)?;
    if !o.wr(b, v) {
        return Err(SdmaError::Write { byte: b });
    }
    *n += 1;
    Ok(())
}

/// `sdma_v6_0_start` on bare metal with PSP loading (no microcode write): unhalt, the context-empty interrupt,
/// then `gfx_resume` in the capture's order. `doorbell`: the queue's doorbell and NBIO's CSDMA range on (Linux's
/// default); else both off and the wptr goes through RB_WPTR (sdma_v6_0.c:527 allows it on bare metal).
/// `start`: the write pointer (bytes) the queue starts at: Linux's `ring->wptr = 0`, or the RB_RPTR a restarted
/// engine kept (`start` below).
fn resume(o: &mut dyn Regs, a: &SdmaAddrs, l: &Layout, doorbell: bool, timeout_us: u64, start: u64) -> Result<usize, SdmaError> {
    let mut n = 0;
    let v = rd(o, a, R::F32Cntl)?;
    wr(o, a, R::F32Cntl, sdma6::f32_cntl(v, true), &mut n)?;
    let v = rd(o, a, R::Cntl)?;
    wr(o, a, R::Cntl, v | f::CNTL_CTXEMPTY_INT_ENABLE, &mut n)?;
    wr(o, a, R::SemWaitFailTimer, 0, &mut n)?;
    let sized = sdma6::rb_cntl_size(rd(o, a, R::RbCntl)?, sdma6::RING_DW);
    wr(o, a, R::RbCntl, sized, &mut n)?;
    for r in [R::RbRptr, R::RbRptrHi, R::RbWptr, R::RbWptrHi] {
        wr(o, a, r, 0, &mut n)?;
    }
    wr(o, a, R::WptrPollLo, l.wptr as u32, &mut n)?;
    wr(o, a, R::WptrPollHi, (l.wptr >> 32) as u32, &mut n)?;
    wr(o, a, R::RptrAddrHi, (l.rptr >> 32) as u32, &mut n)?;
    wr(o, a, R::RptrAddrLo, l.rptr as u32 & 0xffff_fffc, &mut n)?;
    wr(o, a, R::RbBase, (l.ring >> 8) as u32, &mut n)?;
    wr(o, a, R::RbBaseHi, (l.ring >> 40) as u32, &mut n)?;
    wr(o, a, R::MinorPtrUpdate, 1, &mut n)?;
    wr(o, a, R::RbWptr, start as u32, &mut n)?;
    wr(o, a, R::RbWptrHi, (start >> 32) as u32, &mut n)?;
    let db = rd(o, a, R::Doorbell)?;
    let off = rd(o, a, R::DoorbellOffset)?;
    wr(o, a, R::Doorbell, sdma6::doorbell(db, doorbell), &mut n)?;
    wr(o, a, R::DoorbellOffset, if doorbell { sdma6::doorbell_offset(off, sdma6::DOORBELL_DW) } else { off }, &mut n)?;
    if doorbell {
        // nbio_v7_7_sdma_doorbell_range through the RSMU window: index, read, index, write.
        let nb = a.nbio.ok_or(SdmaError::NoDoorbell)?;
        let cur = port_read(o, &nb)?;
        port_write(o, &nb, sdma6::csdma_range(cur, sdma6::DOORBELL_DW, sdma6::DOORBELL_RANGE))?;
        n += 3;
    }
    wr(o, a, R::MinorPtrUpdate, 0, &mut n)?;
    let v = rd(o, a, R::Watchdog)?;
    wr(o, a, R::Watchdog, sdma6::watchdog(v, timeout_us), &mut n)?;
    let v = rd(o, a, R::Utcl1Cntl)?;
    wr(o, a, R::Utcl1Cntl, sdma6::utcl1_cntl(v), &mut n)?;
    let v = rd(o, a, R::Utcl1Page)?;
    wr(o, a, R::Utcl1Page, sdma6::utcl1_page(v), &mut n)?;
    let v = rd(o, a, R::F32Cntl)?;
    wr(o, a, R::F32Cntl, sdma6::f32_cntl(v, true), &mut n)?;
    wr(o, a, R::RbCntl, sdma6::rb_cntl_run(sized, true), &mut n)?;
    let v = rd(o, a, R::IbCntl)?;
    wr(o, a, R::IbCntl, v | f::IB_ENABLE, &mut n)?;
    Ok(n)
}

fn port_read(o: &mut dyn Regs, nb: &NbioDoorbell) -> Result<u32, SdmaError> {
    if !o.wr(nb.rsmu_index, nb.csdma_range) {
        return Err(SdmaError::Write { byte: nb.rsmu_index });
    }
    o.rd(nb.rsmu_data).ok_or(SdmaError::Read { byte: nb.rsmu_data })
}

fn port_write(o: &mut dyn Regs, nb: &NbioDoorbell, v: u32) -> Result<(), SdmaError> {
    if !o.wr(nb.rsmu_index, nb.csdma_range) {
        return Err(SdmaError::Write { byte: nb.rsmu_index });
    }
    if !o.wr(nb.rsmu_data, v) {
        return Err(SdmaError::Write { byte: nb.rsmu_data });
    }
    Ok(())
}

/// `sdma_v6_0_enable(false)`: ring and IBs off (gfx_stop), then the F32 halted. Logged when `o` is the owner.
fn stop_regs(o: &mut dyn Regs, a: &SdmaAddrs) -> Result<(), SdmaError> {
    let mut n = 0;
    let v = rd(o, a, R::RbCntl)?;
    wr(o, a, R::RbCntl, v & !f::RB_ENABLE, &mut n)?;
    let v = rd(o, a, R::IbCntl)?;
    wr(o, a, R::IbCntl, v & !f::IB_ENABLE, &mut n)?;
    let v = rd(o, a, R::F32Cntl)?;
    wr(o, a, R::F32Cntl, sdma6::f32_cntl(v, false), &mut n)
}

/// The ring pointers (bytes): RB_RPTR, RB_WPTR (each with its _HI).
fn ptrs(o: &mut dyn Regs, a: &SdmaAddrs) -> Result<(u64, u64), SdmaError> {
    let lo = rd(o, a, R::RbRptr)?;
    let hi = rd(o, a, R::RbRptrHi)?;
    let wlo = rd(o, a, R::RbWptr)?;
    let whi = rd(o, a, R::RbWptrHi)?;
    Ok((u64::from(hi) << 32 | u64::from(lo), u64::from(whi) << 32 | u64::from(wlo)))
}

/// GX-7i2: before a resume of a queue that holds an earlier run's state (pointers not 0, the doorbell on or a
/// doorbell captured), on the halted engine: the doorbell off first (`doorbell_off_steps`), then RB_RPTR and
/// RB_WPTR 0 under MINOR_PTR_UPDATE (sdma_v6_0.c:524: a smaller wptr needs it), then read back. `None` (nothing
/// written) on a cold queue, so a first start writes exactly Linux's sequence. The 28.10. `again` run found
/// DOORBELL 0x50000000 and kept RB_RPTR = RB_WPTR = 0x40 through a resume that only wrote the pointers 0.
fn rewind(o: &mut dyn Regs, a: &SdmaAddrs) -> Result<Option<Rewind>, SdmaError> {
    let (rp, wp) = ptrs(o, a)?;
    let db = rd(o, a, R::Doorbell)?;
    if rp == 0 && wp == 0 && db & (f::DOORBELL_ENABLE | f::DOORBELL_CAPTURED) == 0 {
        return Ok(None);
    }
    let mut n = 0;
    if db & (f::DOORBELL_ENABLE | f::DOORBELL_CAPTURED) != 0 {
        for v in sdma6::doorbell_off_steps(db) {
            wr(o, a, R::Doorbell, v, &mut n)?;
        }
    }
    wr(o, a, R::MinorPtrUpdate, 1, &mut n)?;
    for r in [R::RbRptr, R::RbRptrHi, R::RbWptr, R::RbWptrHi] {
        wr(o, a, r, 0, &mut n)?;
    }
    wr(o, a, R::MinorPtrUpdate, 0, &mut n)?;
    let (rp2, wp2) = ptrs(o, a)?;
    let db2 = rd(o, a, R::Doorbell)?;
    Ok(Some(Rewind { found: (rp, wp, db), after: (rp2, wp2, db2), start: 0 }))
}

/// GX-7i2: what `until=stop` (or a run that keeps nothing) puts back after SDMA 0 is halted: the queue's doorbell
/// off, then NBIO's CSDMA range and the doorbell aperture to the values this boot's first SDMA stage found (the
/// reverse of rung 4's order). Each register is written only when it differs. `(every write landed, lines)`.
pub fn doorbell_back<H: Hw>(o: &mut Bringup<Watch<H>>, a: &SdmaAddrs, found: NbioFound) -> (bool, Vec<String>) {
    let mut lines = Vec::new();
    let mut ok = true;
    let mut n = 0;
    match rd(o, a, R::Doorbell) {
        Ok(u32::MAX) | Err(_) => lines.push(String::from("stop: SDMA 0's queue doorbell not read (GC off?): left")),
        Ok(db) if db & (f::DOORBELL_ENABLE | f::DOORBELL_CAPTURED) == 0 => lines.push(format!("stop: SDMA 0's queue doorbell already off ({db:#x})")),
        Ok(db) => {
            let res = sdma6::doorbell_off_steps(db).into_iter().try_for_each(|v| wr(o, a, R::Doorbell, v, &mut n));
            ok &= res.is_ok();
            let now = a.byte(R::Doorbell).and_then(|b| o.reg_read(b));
            lines.push(format!("stop: SDMA 0's queue doorbell off: DOORBELL {db:#x} -> {}{}", hx32(now), if res.is_ok() { "" } else { " (a write was refused)" }));
        }
    }
    let Some(nb) = a.nbio else {
        lines.push(String::from("stop: no NBIO addresses: doorbell aperture and CSDMA range not touched"));
        return (false, lines);
    };
    match port_read(o, &nb) {
        Ok(cur) if cur == found.range => lines.push(format!("stop: GDC0_BIF_CSDMA_DOORBELL_RANGE {cur:#x}, as found")),
        Ok(cur) => {
            let res = port_write(o, &nb, found.range);
            ok &= res.is_ok();
            lines.push(format!("stop: GDC0_BIF_CSDMA_DOORBELL_RANGE back to {:#x} as found (was {cur:#x}){}", found.range, if res.is_ok() { "" } else { ": REFUSED" }));
        }
        Err(e) => {
            ok = false;
            lines.push(format!("stop: GDC0_BIF_CSDMA_DOORBELL_RANGE not read ({e:?}): left"));
        }
    }
    match o.reg_read(nb.aper_en) {
        Some(cur) if cur == found.aper => lines.push(format!("stop: RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN {cur:#x}, as found")),
        Some(cur) => {
            let landed = Regs::wr(o, nb.aper_en, found.aper);
            ok &= landed;
            lines.push(format!("stop: RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN back to {:#x} as found (was {cur:#x}){}", found.aper, if landed { "" } else { ": REFUSED" }));
        }
        None => {
            ok = false;
            lines.push(String::from("stop: RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN not read: left"));
        }
    }
    klog::event!(Info, "gx", "sdma.doorbell_back", ok = ok, aper = klog::Hex(u64::from(found.aper)), range = klog::Hex(u64::from(found.range)));
    (ok, lines)
}

/// `stop_regs`, then proof that the engine stopped: F32_CNTL reads HALT, RB_CNTL reads the ring off and
/// STATUS_REG reads IDLE within `us`. Anything else (a refused write, GC gated, a busy engine) is `false`: pages
/// the engine may still write are then never given back.
fn halt_proven<H: Hw>(o: &mut Bringup<Watch<H>>, a: &SdmaAddrs, us: u64) -> bool {
    if stop_regs(o, a).is_err() {
        return false;
    }
    let mut polls = 0;
    let ok = gmc::poll(o, us, &mut polls, &mut |hw| {
        let mut r = |x: R| a.byte(x).and_then(|b| hw.reg_read(b)).filter(|&v| v != u32::MAX);
        let f32_halted = r(R::F32Cntl).is_some_and(|v| v & f::F32_HALT != 0);
        let rb_off = r(R::RbCntl).is_some_and(|v| v & f::RB_ENABLE == 0);
        let idle = r(R::Status).is_some_and(|v| v & f::STATUS_IDLE != 0);
        f32_halted && rb_off && idle
    });
    if !ok {
        klog::event!(Error, "gx", "sdma.halt_unproven", polls = polls; "SDMA 0 not proven halted: its pages are kept for the boot");
    }
    ok
}

/// GX-7m3: a queue stalled at a VM fault (7.1 sdma_v6_0_reset_queue): SDMA0_QUEUE_RESET_REQ's queue 0 bit, polled
/// (mes_v11_0_reset_queue_mmio, 7.1), then the stage's own restart: stop, the pointers rewound, ring and pointer
/// slots zeroed, the resume with the doorbell (the path a kept `until=sdma again` takes, which the chip runs). The
/// caller finds the queue at its wptr shadow again. Returns what happened, for the rung's line.
pub(super) fn restart_stalled<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &SdmaAddrs, l: &Layout, burst_nop: bool, timeout_us: u64) -> Result<String, SdmaError> {
    let mut n = 0;
    wr(owner, a, R::QueueResetReq, 1, &mut n)?;
    let b = a.byte(R::QueueResetReq).ok_or(SdmaError::NoAddress)?;
    let mut polls = 0;
    let done = super::gmc::poll(owner, timeout_us, &mut polls, &mut |hw| hw.reg_read(b).is_some_and(|v| v & 1 == 0));
    klog::event!(Info, "gx", "sdma.queue_reset", done = done, polls = polls);
    stop_regs(owner, a)?;
    let kind = Kind::Sdma { burst_nop };
    let ring = Ring::new(kind, sdma6::RING_DW, 0).map_err(SdmaError::Ring)?;
    let mut q = Queue { o: owner, a: *a, l: *l, kind, ring, shadow: Vec::new(), rptr_dw: 0, doorbell: true, t: timeout_us, doorbells: Vec::new() };
    let mut r = SdmaReport::default();
    start(&mut q, &mut r, true, timeout_us)?;
    Ok(format!("SDMA0_QUEUE_RESET_REQ <- 1 {} after {polls} polls; stopped and resumed with its doorbell", if done { "cleared" } else { "did NOT clear (going on: the stop and resume stand on their own)" }))
}

/// The Quiesce path's halt (no log, no allocation, no lock): ring and IBs off, the F32 halted. `true` when every
/// write landed (GC off reads all ones: nothing to halt, `true`).
pub fn halt(hw: &mut dyn Hw, a: &SdmaAddrs) -> bool {
    struct Raw<'a>(&'a mut dyn Hw);
    impl Regs for Raw<'_> {
        fn rd(&mut self, byte: u32) -> Option<u32> {
            self.0.reg_read(byte)
        }
        fn wr(&mut self, byte: u32, value: u32) -> bool {
            self.0.reg_write(byte, value)
        }
    }
    match stop_regs(&mut Raw(hw), a) {
        Ok(()) | Err(SdmaError::GcOff { .. }) => true,
        Err(_) => false,
    }
}

/// The resume Linux writes on a cold start in one go (doorbell on), from the registers as the stage found them:
/// what the Linux diff compares (`regdiff` phase `sdma`).
pub fn linux_sequence(r: &SdmaReport, a: &SdmaAddrs) -> Vec<(u32, u32)> {
    let Some(l) = r.layout else { return Vec::new() };
    let mut vals: Vec<(u32, u32)> = r.before.iter().filter_map(|&(x, v)| Some((a.byte(x)?, v?))).collect();
    if let (Some(nb), Some((_, Some(range), _))) = (a.nbio, r.nbio.iter().find(|x| x.0 == "GDC0_BIF_CSDMA_DOORBELL_RANGE")) {
        vals.push((nb.rsmu_data, *range));
    }
    let mut rec = Rec { vals, writes: Vec::new() };
    let _ = resume(&mut rec, a, &l, true, sdma6::LINUX_TIMEOUT_US, 0);
    rec.writes
}

/// One SDMA queue as the CPU drives it.
struct Queue<'o, H: Hw> {
    o: &'o mut Bringup<Watch<H>>,
    a: SdmaAddrs,
    l: Layout,
    kind: Kind,
    ring: Ring,
    shadow: Vec<u32>,
    rptr_dw: u64,
    doorbell: bool,
    t: u64,
    doorbells: Vec<(u32, u64, bool)>,
}

fn rd32(hw: &mut dyn Hw, mc: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    hw.mem_read(mc, &mut b).then(|| u32::from_le_bytes(b))
}

fn rd64(hw: &mut dyn Hw, mc: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    hw.mem_read(mc, &mut b).then(|| u64::from_le_bytes(b))
}

fn dwords(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|d| d.to_le_bytes()).collect()
}

impl<H: Hw> Queue<'_, H> {
    /// `start_dw`: where the queue's pointers start (0, or the RB_RPTR a restarted engine kept).
    fn reset(&mut self, doorbell: bool, start_dw: u64) -> Result<(), SdmaError> {
        self.ring = Ring::new(self.kind, sdma6::RING_DW, start_dw).map_err(SdmaError::Ring)?;
        self.shadow = vec![0; sdma6::RING_DW as usize];
        self.rptr_dw = start_dw;
        self.doorbell = doorbell;
        // The ring zeroed (a stale packet of an earlier run must never look valid), both pointer slots at the start.
        let zero = vec![0u8; (sdma6::RING_DW * 4) as usize];
        let at = self.kind.hw_ptr(start_dw).to_le_bytes();
        self.mem(self.l.ring, &zero)?;
        self.mem(self.l.rptr, &at)?;
        self.mem(self.l.wptr, &at)?;
        self.o.store_barrier();
        Ok(())
    }

    fn mem(&mut self, mc: u64, data: &[u8]) -> Result<(), SdmaError> {
        if self.o.mem_write(mc, data) { Ok(()) } else { Err(SdmaError::Mem { mc }) }
    }

    /// Packets into the ring, then the commit: the wptr shadow (Linux's wptr slot), a store barrier, then the
    /// doorbell or RB_WPTR/RB_WPTR_HI (sdma_v6_0.c:156-215). Returns the new write pointer (dwords).
    fn submit(&mut self, ndw: u64, build: impl FnOnce(&mut Ring) -> Result<(), RingError>) -> Result<u64, SdmaError> {
        self.ring.begin(ndw, self.rptr_dw).map_err(SdmaError::Ring)?;
        if let Err(e) = build(&mut self.ring) {
            self.ring.undo();
            return Err(SdmaError::Ring(e));
        }
        let old = self.ring.wptr();
        let new = self.ring.commit(&mut self.shadow).map_err(SdmaError::Ring)?;
        let (size, mask) = (self.ring.size_dw(), self.ring.mask());
        let mut p = old;
        while p != new {
            let i = p & mask;
            let n = (new - p).min(size - i);
            let bytes = dwords(&self.shadow[i as usize..(i + n) as usize]);
            self.mem(self.l.ring + i * 4, &bytes)?;
            p += n;
        }
        self.o.store_barrier();
        let hw = self.kind.hw_ptr(new);
        if !self.o.mem_write64(self.l.wptr, hw) {
            return Err(SdmaError::Mem { mc: self.l.wptr });
        }
        self.o.store_barrier();
        if self.doorbell {
            let ok = self.o.doorbell_write64(sdma6::DOORBELL_DW, hw);
            self.doorbells.push((sdma6::DOORBELL_DW, hw, ok));
            klog::event!(Info, "gx", "sdma.doorbell", index = sdma6::DOORBELL_DW, value = klog::Hex(hw), accepted = ok);
            if !ok {
                return Err(SdmaError::NoDoorbell);
            }
        } else {
            let mut n = 0;
            wr(&mut *self.o, &self.a, R::RbWptr, hw as u32, &mut n)?;
            wr(&mut *self.o, &self.a, R::RbWptrHi, (hw >> 32) as u32, &mut n)?;
        }
        Ok(new)
    }

    /// Polls the rptr writeback up to the write pointer and `done`, Linux's 100 ms.
    fn wait(&mut self, name: &'static str, wptr: u64, done: &mut dyn FnMut(&mut dyn Hw) -> bool) -> Rung {
        let want = self.kind.hw_ptr(wptr);
        let rptr_mc = self.l.rptr;
        let t0 = self.o.now_us();
        let mut polls = 0;
        let mut last = None;
        let ok = gmc::poll(&mut *self.o, self.t, &mut polls, &mut |hw| {
            last = rd64(hw, rptr_mc);
            last == Some(want) && done(hw)
        });
        if ok {
            self.rptr_dw = wptr;
        }
        let us = self.o.now_us().saturating_sub(t0);
        if ok {
            klog::event!(Info, "gx", "sdma.rung", rung = name, done = true, wptr = wptr, polls = polls, us = us);
        } else {
            klog::event!(Error, "gx", "sdma.rung_timeout", rung = name, wptr = wptr, rptr = klog::Hex(last.unwrap_or(u64::MAX)), polls = polls, us = us);
        }
        Rung { name, pass: ok, done: ok, doorbell: self.doorbell, wptr, rptr: last, fence: None, polls, us, detail: String::new() }
    }
}

/// Bytes of carve-out memory as read by the CPU.
fn read_area(hw: &mut dyn Hw, mc: u64, len: u64) -> Option<Vec<u8>> {
    let mut b = vec![0u8; len as usize];
    hw.mem_read(mc, &mut b).then_some(b)
}

/// The rung 4 source pattern (no two dwords alike, no zero).
pub fn pattern(i: u64) -> u32 {
    (0x5d3a_0001u32 ^ (i as u32).wrapping_mul(0x9e37_79b9)) | 1
}

/// The stage. Nothing is written before SDMA0_F32_CNTL reads powered.
pub fn enable<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &SdmaInputs, gart: &GartUse) -> SdmaReport {
    let mut r = SdmaReport { burst_nop: inp.burst_nop, ..SdmaReport::default() };
    let res = run(owner, bo, inp, gart, &mut r);
    if let Err(e) = res {
        klog::event!(Error, "gx", "sdma.failed", error = format!("{e:?}").as_str(), rungs = r.rungs.len());
        r.error = Some(e);
    }
    if r.layout.is_some() {
        r.fault_after = Some(gmc::read_fault(owner, &gart.gfx));
    }
    // A new VM fault fails the stage like a rung: the engine is not left running on a hub that faulted.
    let all = r.error.is_none() && r.rungs.len() == RUNGS.len() && r.rungs.iter().all(|x| x.pass) && no_new_fault(&r);
    if !all && !matches!(r.error, Some(SdmaError::GcOff { .. } | SdmaError::NoAddress | SdmaError::Read { .. })) && r.layout.is_some() {
        // A failed rung or stage: the engine halted again (no engine left running on our buffers half-way).
        let _ = stop_regs(owner, &inp.addrs);
        klog::event!(Warn, "gx", "sdma.halted"; "a rung or the stage failed: SDMA 0 halted again");
    }
    r.left_running = all;
    for x in R::ALL {
        if let Some(b) = inp.addrs.byte(x)
            && matches!(x, R::F32Cntl | R::RbCntl | R::IbCntl | R::Status | R::RbRptr | R::RbWptr | R::Doorbell | R::ContextStatus)
        {
            let v = owner.reg_read(b);
            r.after.push((x, v));
        }
    }
    if r.fault_after.is_none() {
        r.fault_after = Some(gmc::read_fault(owner, &gart.gfx));
    }
    if all {
        klog::event!(Info, "gx", "sdma.enabled", rungs = r.rungs.len(), doorbell = true; "SDMA 0 runs: every rung passed");
    }
    r
}

fn run<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &SdmaInputs, gart: &GartUse, r: &mut SdmaReport) -> Result<(), SdmaError> {
    let a = inp.addrs;
    // 1. The engine as found (GC gated reads all ones: refused before any write).
    for x in BEFORE {
        let v = a.byte(x).and_then(|b| owner.reg_read(b));
        r.before.push((x, v));
    }
    let f32_0 = rd(owner, &a, R::F32Cntl)?;
    let rb0 = rd(owner, &a, R::RbCntl)?;
    r.fault_before = Some(gmc::read_fault(owner, &gart.gfx));
    if let Some(arm) = inp.arm {
        arm(a);
    }
    if let Some(nb) = a.nbio {
        let aper = owner.reg_read(nb.aper_en);
        let range = port_read(owner, &nb).ok();
        r.nbio.push(("RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN", aper, None));
        r.nbio.push(("GDC0_BIF_CSDMA_DOORBELL_RANGE", range, None));
    }
    // 2. The buffer (kept for the boot; an earlier run's reused).
    let b = match inp.bo {
        Some(b) if b.size >= lay::END => {
            r.reused_bo = true;
            b
        }
        Some(b) => return Err(SdmaError::BoTooSmall { have: b.size, need: lay::END }),
        None => {
            let (handle, range) = bo.alloc(BO_BYTES, PAGE).map_err(SdmaError::Bo)?;
            SdmaBo { handle, offset: range.start, size: range.end - range.start }
        }
    };
    r.bo = Some(b);
    if b.size < lay::END {
        return Err(SdmaError::BoTooSmall { have: b.size, need: lay::END });
    }
    let l = Layout::new(inp.carve.mc_base() + b.offset).ok_or(SdmaError::NoAddress)?;
    r.layout = Some(l);
    klog::event!(Info, "gx", "sdma.bo", mc = klog::Hex(l.bo_mc), reused = r.reused_bo, burst_nop = inp.burst_nop);
    // 3. A running or unhalted engine (an earlier run, a kernel before) is stopped first.
    if rb0 & f::RB_ENABLE != 0 || f32_0 & f::F32_HALT == 0 {
        klog::event!(Warn, "gx", "sdma.stop_first", f32 = klog::Hex(u64::from(f32_0)), rb = klog::Hex(u64::from(rb0)); "SDMA 0 found running: stopped before the resume");
        stop_regs(owner, &a)?;
        r.stopped_first = true;
    }
    let kind = Kind::Sdma { burst_nop: inp.burst_nop };
    let mut q = Queue {
        o: owner,
        a,
        l,
        kind,
        ring: Ring::new(kind, sdma6::RING_DW, 0).map_err(SdmaError::Ring)?,
        shadow: Vec::new(),
        rptr_dw: 0,
        doorbell: false,
        t: inp.timeout_us,
        doorbells: Vec::new(),
    };
    let res = rungs(&mut q, inp, gart, r);
    r.doorbells = core::mem::take(&mut q.doorbells);
    if let Some(nb) = a.nbio
        && r.nbio.len() == 2
    {
        let aper = q.o.reg_read(nb.aper_en);
        let range = port_read(q.o, &nb).ok();
        r.nbio[0].2 = aper;
        r.nbio[1].2 = range;
    }
    res
}

/// A (re)start on the halted engine: the earlier run's pointers reset (`rewind`), the queue's memory zeroed, the
/// resume, then proof the queue is idle at its start. When RB_RPTR kept an earlier value through the reset, the
/// queue starts there instead (Linux's `ring->wptr` set to it before the resume's RB_WPTR writes): a fallback.
fn start<H: Hw>(q: &mut Queue<'_, H>, r: &mut SdmaReport, doorbell: bool, timeout_us: u64) -> Result<(), SdmaError> {
    let mut start = 0;
    if let Some(mut w) = rewind(&mut *q.o, &q.a)? {
        let (rp, wp, db) = w.found;
        let (rp2, wp2, db2) = w.after;
        klog::event!(Info, "gx", "sdma.rewind", rptr = klog::Hex(rp), wptr = klog::Hex(wp), doorbell = klog::Hex(u64::from(db)), rptr_after = klog::Hex(rp2), wptr_after = klog::Hex(wp2), doorbell_after = klog::Hex(u64::from(db2)));
        let align = q.kind.hw_ptr(q.kind.ring_align_dw());
        if rp2 != 0 && rp2 % align == 0 {
            klog::event!(Warn, "gx", "sdma.ptr_kept", rptr = klog::Hex(rp2); "RB_RPTR kept its value through the reset: the queue starts there");
            start = rp2;
        } else if rp2 != 0 {
            r.rewinds.push(w);
            return Err(SdmaError::StalePointers { rptr: rp2, wptr: wp2 });
        }
        w.start = start;
        r.rewinds.push(w);
    }
    let start_dw = start / q.kind.hw_ptr(1);
    q.reset(doorbell, start_dw)?;
    let s0 = q.o.log().totals().writes;
    let n = resume(&mut *q.o, &q.a, &q.l, doorbell, timeout_us, start)?;
    r.resumes.push((doorbell, n, s0, q.o.log().totals().writes));
    let (rp, wp) = ptrs(&mut *q.o, &q.a)?;
    if rp != start || wp != start {
        klog::event!(Error, "gx", "sdma.stale_pointers", rptr = klog::Hex(rp), wptr = klog::Hex(wp), start = klog::Hex(start));
        return Err(SdmaError::StalePointers { rptr: rp, wptr: wp });
    }
    let f32 = q.a.byte(R::F32Cntl).and_then(|b| q.o.reg_read(b));
    let rb = q.a.byte(R::RbCntl).and_then(|b| q.o.reg_read(b));
    if r.started.is_none() {
        r.started = Some((f32, rb));
    }
    klog::event!(Info, "gx", "sdma.resumed", doorbell = doorbell, writes = n, f32 = klog::Hex(u64::from(f32.unwrap_or(u32::MAX))), rb = klog::Hex(u64::from(rb.unwrap_or(u32::MAX))));
    if f32.is_none_or(|v| v == u32::MAX || v & f::F32_HALT != 0) || rb.is_none_or(|v| v & f::RB_ENABLE == 0) {
        return Err(SdmaError::NotStarted { f32, rb });
    }
    Ok(())
}

/// Stops at the first rung that fails (the engine is then halted by `enable`).
fn rungs<H: Hw>(q: &mut Queue<'_, H>, inp: &SdmaInputs, gart: &GartUse, r: &mut SdmaReport) -> Result<(), SdmaError> {
    let l = q.l;
    let mut t32 = Timeline::new(0, 16);
    let mut t64 = Timeline::new(FENCE64_START, 16);
    start(q, r, false, inp.timeout_us)?;
    // Rung 1: Linux's ring test into a carve-out dword.
    q.mem(l.target, &POISON.to_le_bytes())?;
    q.o.store_barrier();
    let w = q.submit(5, |g| g.emit(|v| pkt::write_linear(v, l.target, &[RING_TEST])))?;
    let mut x = q.wait(RUNGS[0], w, &mut |hw| rd32(hw, l.target) == Some(RING_TEST));
    let got = rd32(q.o, l.target);
    x.detail = format!("target MC {:#x} reads {}", l.target, hx32(got));
    if !push(r, x) {
        return Err(timeout(r, w, q.kind));
    }
    // Rung 2: both fills, then a 32-bit fence.
    let zero = vec![0u8; FILL_BYTES as usize];
    q.mem(l.fill_a, &zero)?;
    q.mem(l.fill_b, &zero)?;
    q.mem(l.fence32, &[0; 8])?;
    q.o.store_barrier();
    let seq = t32.emit().map_err(|_| SdmaError::Ring(RingError::State))?;
    let w = q.submit(32, |g| {
        g.emit(|v| pkt::const_fill(v, l.fill_a, FILL_BYTE_VALUE, FILL_BYTES, pkt::FILL_BYTE))?;
        g.emit(|v| pkt::const_fill(v, l.fill_b, FILL_DWORD_VALUE, FILL_BYTES, pkt::FILL_DWORD))?;
        g.emit_fence(l.fence32, seq, false, false)
    })?;
    let mut x = q.wait(RUNGS[1], w, &mut |hw| rd32(hw, l.fence32) == Some(seq as u32));
    x.fence = rd32(q.o, l.fence32).map(u64::from);
    if x.pass {
        t32.observe(seq);
        let fa = read_area(q.o, l.fill_a, FILL_BYTES);
        let fb = read_area(q.o, l.fill_b, FILL_BYTES);
        let (ok_a, ta) = check_byte_fill(fa.as_deref());
        let (ok_b, tb) = check_dword_fill(fb.as_deref(), FILL_DWORD_VALUE);
        x.pass = ok_a && ok_b;
        x.detail = format!("fence {seq} read {}; fill A (FILL_BYTE {FILL_BYTE_VALUE:#x}): {ta}; fill B (FILL_DWORD {FILL_DWORD_VALUE:#x}): {tb}", hx(x.fence));
    } else {
        x.detail = format!("fence {seq} read {}", hx(x.fence));
    }
    if !push(r, x) {
        return Err(timeout(r, w, q.kind));
    }
    // The golden IB: the capture's packet, its target in the carve-out, run through INDIRECT in VMID 0.
    let mut ib = GOLDEN_IB;
    ib[1] = l.golden as u32;
    ib[2] = (l.golden >> 32) as u32;
    q.mem(l.ib, &dwords(&ib))?;
    q.mem(l.golden, &dwords(&[POISON; 4]))?;
    q.mem(l.fence_ib, &[0; 8])?;
    q.o.store_barrier();
    let seq = t32.emit().map_err(|_| SdmaError::Ring(RingError::State))?;
    let w = q.submit(24, |g| {
        g.emit_ib(0, l.ib, ib.len() as u32, 0, false)?;
        g.emit_fence(l.fence_ib, seq, false, false)
    })?;
    let mut x = q.wait(RUNGS[2], w, &mut |hw| rd32(hw, l.fence_ib) == Some(seq as u32));
    x.fence = rd32(q.o, l.fence_ib).map(u64::from);
    let got = read_area(q.o, l.golden, 16).map(|b| b.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect::<Vec<u32>>());
    if x.pass {
        t32.observe(seq);
        x.pass = got.as_deref() == Some(&GOLDEN_RESULT[..]);
    }
    x.detail = format!(
        "IB at MC {:#x} ({} dwords, the capture's), target MC {:#x} reads {} (the capture: deadbeef 0badf00d 12345678 9abcdef0), fence {seq} read {}",
        l.ib,
        ib.len(),
        l.golden,
        got.map_or(String::from("unread"), |g| g.iter().map(|w| format!("{w:08x}")).collect::<Vec<_>>().join(" ")),
        hx(x.fence)
    );
    if !push(r, x) {
        return Err(timeout(r, w, q.kind));
    }
    // Rung 3: COPY_LINEAR into GART-mapped system pages, 64-bit fence + TRAP.
    copy_rung(q, inp, gart, r, &mut t64)?;
    // Rung 4: the doorbell instead of the wptr register.
    let nb = q.a.nbio.ok_or(SdmaError::NoDoorbell)?;
    stop_regs(q.o, &q.a)?;
    let aper = q.o.reg_read(nb.aper_en).ok_or(SdmaError::Read { byte: nb.aper_en })?;
    // nbio_v7_7_enable_doorbell_aperture: read-modify-write (the firmware leaves it 0 on the Mini-PC).
    if !Regs::wr(&mut *q.o, nb.aper_en, aper | 1) {
        return Err(SdmaError::Write { byte: nb.aper_en });
    }
    start(q, r, true, inp.timeout_us)?;
    q.mem(l.db_target, &POISON.to_le_bytes())?;
    q.mem(l.fence_db, &[0; 8])?;
    q.o.store_barrier();
    let seq = t32.emit().map_err(|_| SdmaError::Ring(RingError::State))?;
    let w = q.submit(16, |g| {
        g.emit(|v| pkt::write_linear(v, l.db_target, &[DOORBELL_VALUE]))?;
        g.emit_fence(l.fence_db, seq, false, false)
    })?;
    let mut x = q.wait(RUNGS[4], w, &mut |hw| rd32(hw, l.fence_db) == Some(seq as u32) && rd32(hw, l.db_target) == Some(DOORBELL_VALUE));
    x.fence = rd32(q.o, l.fence_db).map(u64::from);
    let db = q.a.byte(R::Doorbell).and_then(|b| q.o.reg_read(b));
    if x.pass {
        t32.observe(seq);
        x.pass = db.is_some_and(|v| v & f::DOORBELL_ENABLE != 0);
    }
    x.detail = format!(
        "BAR2 dword {:#x} (byte {:#x}) <- {:#x}; target MC {:#x} reads {}; fence {seq} read {}; SDMA0_QUEUE0_DOORBELL {}",
        sdma6::DOORBELL_DW,
        sdma6::DOORBELL_DW * 4,
        q.kind.hw_ptr(w),
        l.db_target,
        hx32(rd32(q.o, l.db_target)),
        hx(x.fence),
        hx32(db)
    );
    if !push(r, x) {
        return Err(timeout(r, w, q.kind));
    }
    Ok(())
}

fn copy_rung<H: Hw>(q: &mut Queue<'_, H>, inp: &SdmaInputs, gart: &GartUse, r: &mut SdmaReport, t64: &mut Timeline) -> Result<(), SdmaError> {
    let l = q.l;
    let sys = inp.sys.ok_or_else(|| SdmaError::NoSys(String::from("no system page allocator")))?;
    let mut pages = sys.alloc(SYS_PAGES).map_err(SdmaError::NoSys)?;
    let phys: Vec<u64> = (0..SYS_PAGES).map(|i| pages.device_addr(i)).collect::<Option<Vec<u64>>>().ok_or_else(|| SdmaError::NoSys(String::from("a page has no device address")))?;
    let zero = vec![0u8; COPY_BYTES as usize];
    if !pages.write(0, &zero) {
        return Err(SdmaError::NoSys(String::from("CPU write to the system pages failed")));
    }
    pages.barrier();
    let src: Vec<u32> = (0..COPY_BYTES / 4).map(pattern).collect();
    q.mem(l.src, &dwords(&src))?;
    q.mem(l.fence64, &FENCE64_START.to_le_bytes())?;
    q.o.store_barrier();
    let g = gart.geo;
    let ga = Gart::new(g.gart_start, g.gart_end + 1 - g.gart_start, g.dummy_phys, inp.carve).map_err(|e| SdmaError::Gart(format!("{e:?}")))?;
    let page = GartPage { target: Target::System { snooped: true }, writeable: true };
    let mut gp = GartPages { phys: phys.clone(), ..GartPages::default() };
    let bound = {
        let mut t = CarveGart::new(&mut *q.o, g.table_mc, g.entries());
        let m = ga.map(&mut t, GART_OFFSET, &phys, page);
        (m, t.failed())
    };
    let mc = match bound {
        (Ok(mc), false) => mc,
        (m, _) => {
            // Nothing bound for sure: unbind what may have landed before giving the pages back.
            gp.leaked = !unbind(q, &ga, gart, &mut gp);
            leak_if(gp.leaked, pages);
            r.gart = Some(gp);
            return Err(SdmaError::Gart(format!("bind: {m:?}")));
        }
    };
    gp.mc = mc;
    q.o.store_barrier();
    let (fl, e) = gmc::flush(q.o, &gart.gfx, gart.flush_us);
    gp.flushes.push(fl);
    klog::event!(Info, "gx", "sdma.gart_bound", mc = klog::Hex(mc), pages = SYS_PAGES, acked = fl.acked);
    if e.is_some() {
        gp.leaked = !unbind(q, &ga, gart, &mut gp);
        leak_if(gp.leaked, pages);
        r.gart = Some(gp);
        return Err(SdmaError::Flush);
    }
    let seq = t64.emit().map_err(|_| SdmaError::Ring(RingError::State))?;
    let sub = q.submit(32, |g| {
        g.emit(|v| pkt::copy_linear(v, l.src, mc, COPY_BYTES, false))?;
        g.emit_fence(l.fence64, seq, true, true)
    });
    let mut halted = true;
    let mut x = match sub {
        Ok(w) => q.wait(RUNGS[3], w, &mut |hw| fence64(hw, l.fence64) == Some(seq)),
        Err(e) => {
            // The shadow wptr (or part of the commit) may have landed: the engine may be copying already. Halted
            // and proven so before the pages go back, or they stay for the boot.
            let halted = halt_proven(q.o, &q.a, q.t);
            let unbound = unbind(q, &ga, gart, &mut gp);
            gp.unbound = unbound;
            gp.leaked = !halted || !unbound;
            leak_if(gp.leaked, pages);
            r.gart = Some(gp);
            return Err(e);
        }
    };
    x.fence = fence64(q.o, l.fence64);
    let mut got = vec![0u8; COPY_BYTES as usize];
    let read = pages.read(0, &mut got);
    let mut detail = format!("{} bytes MC {:#x} -> GART {:#x} (system pages {}), 64-bit fence {seq:#x} read {}", COPY_BYTES, l.src, mc, phys.iter().map(|p| format!("{p:#x}")).collect::<Vec<_>>().join(" "), hx(x.fence));
    if x.pass {
        t64.observe(seq);
        let first_bad = got.chunks(4).enumerate().find(|(i, c)| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) != src[*i]).map(|(i, c)| (i, u32::from_le_bytes([c[0], c[1], c[2], c[3]])));
        x.pass = read && first_bad.is_none();
        detail += &match (read, first_bad) {
            (false, _) => String::from("; the CPU could not read the pages"),
            (true, None) => String::from("; the pages read back equal to the source"),
            (true, Some((i, v))) => format!("; dword {i} reads {v:#x}, want {:#x}", src[i]),
        };
    } else {
        // The engine may still copy: halted and proven so before the pages go back (enable halts again, harmless).
        halted = halt_proven(q.o, &q.a, q.t);
        if !halted {
            detail += "; SDMA 0 not proven halted: the pages are kept for the boot";
        }
    }
    x.detail = detail;
    let pass = x.pass;
    r.rungs.push(x);
    let unbound = unbind(q, &ga, gart, &mut gp);
    gp.unbound = unbound;
    gp.leaked = !halted || !unbound;
    leak_if(gp.leaked, pages);
    r.gart = Some(gp.clone());
    if !unbound {
        return Err(SdmaError::Flush);
    }
    if !pass {
        return Err(timeout(r, 0, q.kind));
    }
    Ok(())
}


/// The pages' entries back on the dummy page and the TLB flushed; `false` when that is not proven.
fn unbind<H: Hw>(q: &mut Queue<'_, H>, ga: &Gart, gart: &GartUse, gp: &mut GartPages) -> bool {
    let g = gart.geo;
    let ok = {
        let mut t = CarveGart::new(&mut *q.o, g.table_mc, g.entries());
        ga.unmap(&mut t, GART_OFFSET, SYS_PAGES).is_ok() && !t.failed()
    };
    q.o.store_barrier();
    let (fl, e) = gmc::flush(q.o, &gart.gfx, gart.flush_us);
    gp.flushes.push(fl);
    ok && e.is_none()
}

/// Pages the GART or a running engine may still reach are never given back (kept for the boot).
fn leak_if(leak: bool, pages: Box<dyn SysPages>) {
    if leak {
        klog::event!(Error, "gx", "sdma.pages_kept"; "GART pages not proven unbound or the engine not proven halted: kept for the boot, never freed");
        core::mem::forget(pages);
    }
}

fn fence64(hw: &mut dyn Hw, mc: u64) -> Option<u64> {
    let mut bad = false;
    let v = read_split(|h| {
        let at = if h == Half::High { mc + 4 } else { mc };
        rd32(hw, at).unwrap_or_else(|| {
            bad = true;
            0
        })
    });
    (!bad).then_some(v)
}

fn push(r: &mut SdmaReport, x: Rung) -> bool {
    let p = x.pass;
    r.rungs.push(x);
    p
}

fn timeout(r: &SdmaReport, wptr: u64, kind: Kind) -> SdmaError {
    match r.rungs.last() {
        Some(x) if x.done => SdmaError::Result { rung: x.name },
        last => SdmaError::Timeout { rung: last.map_or("?", |x| x.name), rptr: last.and_then(|x| x.rptr), want: kind.hw_ptr(last.map_or(wptr, |x| x.wptr)) },
    }
}

/// Fill A: every byte 0xA5. A dword fill (0x000000A5 per dword) is named: GR-2's open question (fillsize 0).
pub fn check_byte_fill(b: Option<&[u8]>) -> (bool, String) {
    let Some(b) = b else { return (false, String::from("unread")) };
    if b.iter().all(|&x| u32::from(x) == FILL_BYTE_VALUE) {
        return (true, String::from("every byte 0xa5"));
    }
    if b.chunks(4).all(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) == FILL_BYTE_VALUE) {
        return (false, String::from("every dword 0x000000a5: fillsize 0 is a DWORD fill on this chip (tell GR: sim exec_sdma FILL_BYTE)"));
    }
    let i = b.iter().position(|&x| u32::from(x) != FILL_BYTE_VALUE).unwrap_or(0);
    (false, format!("byte {i} reads {:#x}", b[i]))
}

pub fn check_dword_fill(b: Option<&[u8]>, want: u32) -> (bool, String) {
    let Some(b) = b else { return (false, String::from("unread")) };
    match b.chunks(4).enumerate().find(|(_, c)| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) != want) {
        None => (true, format!("every dword {want:#x}")),
        Some((i, c)) => (false, format!("dword {i} reads {:#x}", u32::from_le_bytes([c[0], c[1], c[2], c[3]]))),
    }
}

fn hx(v: Option<u64>) -> String {
    v.map_or(String::from("unread"), |x| format!("{x:#x}"))
}

fn hx32(v: Option<u32>) -> String {
    v.map_or(String::from("unread"), |x| format!("{x:#x}"))
}

/// One check of the stage (as `pspbringup::Check`).
pub struct SdmaCheck {
    pub name: &'static str,
    pub pass: bool,
    pub why: String,
}

/// The pass rules (roadmap wave 25 GX-7i, M4: the fill lands, the picture untouched; the picture is the
/// caller's DCN snapshot and CRC).
pub fn checks(r: &SdmaReport) -> Vec<SdmaCheck> {
    let mut v = Vec::new();
    let (pass, why) = match r.started {
        Some((Some(f), Some(rb))) => (f & f::F32_HALT == 0 && rb & f::RB_ENABLE != 0, format!("after the resume F32_CNTL {f:#x} (HALT 0), RB_CNTL {rb:#x} (RB_ENABLE 1)")),
        Some((f, rb)) => (false, format!("after the resume F32_CNTL {} RB_CNTL {}", hx32(f), hx32(rb))),
        None => (false, String::from("not resumed")),
    };
    v.push(SdmaCheck { name: "SDMA engine started", pass, why });
    for (i, name) in RUNGS.iter().enumerate() {
        match r.rungs.get(i) {
            Some(x) => v.push(SdmaCheck {
                name,
                pass: x.pass,
                why: format!("wptr {} (bytes {:#x}), rptr writeback {}, {} polls, {} us{}; {}", x.wptr, x.wptr << 2, hx(x.rptr), x.polls, x.us, if x.doorbell { ", doorbell" } else { ", RB_WPTR" }, x.detail),
            }),
            None => v.push(SdmaCheck { name, pass: false, why: String::from("not run (an earlier rung or step failed)") }),
        }
    }
    let same = no_new_fault(r);
    v.push(SdmaCheck {
        name: "SDMA no new VM fault",
        pass: same,
        why: format!(
            "GFXHUB before: {}; after: {}",
            r.fault_before.map_or(String::from("unread"), |f| f.text()),
            r.fault_after.map_or(String::from("unread"), |f| f.text())
        ),
    });
    let gart_ok = r.gart.as_ref().is_some_and(|g| g.unbound && !g.leaked);
    v.push(SdmaCheck {
        name: "SDMA GART pages unbound again",
        pass: gart_ok,
        why: r.gart.as_ref().map_or(String::from("rung 3 did not bind pages"), |g| {
            format!("GART {:#x}: {} flushes acked {}, unbound {}, kept {}", g.mc, g.flushes.len(), g.flushes.iter().all(|f| f.acked), g.unbound, g.leaked)
        }),
    });
    v
}

/// GFXHUB's fault registers read the same before and after the stage (readable both times).
fn no_new_fault(r: &SdmaReport) -> bool {
    match (r.fault_before, r.fault_after) {
        (Some(b), Some(a)) => b.status.is_some() && b.status == a.status && b.addr == a.addr,
        _ => false,
    }
}

/// The stage's lines for the answer.
pub fn render(r: &SdmaReport) -> Vec<String> {
    let mut v = Vec::new();
    let found: Vec<String> = r.before.iter().map(|(x, val)| format!("{} {}", x.name(), hx32(*val))).collect();
    v.push(format!("sdma found: {}", found.join(", ")));
    if let Some(l) = r.layout {
        v.push(format!(
            "sdma: buffer MC {:#x}{} (ring {:#x}, {} dwords; rptr writeback {:#x}, wptr shadow {:#x}; fences {:#x} {:#x} {:#x} {:#x}); burst NOP {}",
            l.bo_mc,
            if r.reused_bo { " (an earlier run's, reused)" } else { "" },
            l.ring,
            sdma6::RING_DW,
            l.rptr,
            l.wptr,
            l.fence32,
            l.fence64,
            l.fence_ib,
            l.fence_db,
            r.burst_nop
        ));
    }
    if r.stopped_first {
        v.push(String::from("sdma: found running or unhalted: stopped first (RB off, IB off, HALT)"));
    }
    for w in &r.rewinds {
        let ((rp, wp, db), (rp2, wp2, db2)) = (w.found, w.after);
        v.push(format!(
            "sdma restart: found RB_RPTR {rp:#x} RB_WPTR {wp:#x} DOORBELL {db:#x}; after the reset (doorbell off, pointers 0 under MINOR_PTR_UPDATE) RB_RPTR {rp2:#x} RB_WPTR {wp2:#x} DOORBELL {db2:#x}; the queue starts at {:#x}{}",
            w.start,
            if w.start != 0 { " (RB_RPTR kept its value: taken over)" } else { "" }
        ));
    }
    for (i, &(db, n, ..)) in r.resumes.iter().enumerate() {
        v.push(format!("sdma resume {}: {n} writes as Linux's sdma_v6_0_start, doorbell {}", i + 1, if db { "on (BAR2 dword 0x200, NBIO CSDMA range 0x200 + 20)" } else { "off (wptr through RB_WPTR)" }));
    }
    for &(n, b, a) in &r.nbio {
        v.push(format!("sdma nbio {n}: before {} after {}", hx32(b), hx32(a)));
    }
    for x in &r.rungs {
        v.push(format!("sdma {}: {} ({})", x.name, if x.pass { "ok" } else { "FAILED" }, x.detail));
    }
    for &(i, val, ok) in &r.doorbells {
        v.push(format!("sdma doorbell: BAR2 dword {i:#x} <- {val:#x} ({})", if ok { "written" } else { "REFUSED" }));
    }
    if let Some(g) = &r.gart {
        for f in &g.flushes {
            v.push(format!("sdma gart flush: GFXHUB engine 17 acked {} after {} polls, {} us", f.acked, f.polls, f.after_us));
        }
    }
    let after: Vec<String> = r.after.iter().map(|(x, val)| format!("{} {}", x.name(), hx32(*val))).collect();
    v.push(format!("sdma after: {}", after.join(", ")));
    v.push(format!("sdma: engine {}", if r.left_running { "left running (Linux's state; until=stop and the Quiesce hook halt it)" } else { "halted" }));
    if let Some(e) = &r.error {
        v.push(format!("sdma error: {e:?} (errno {}){}", e.errno(), e.advice()));
    }
    v
}

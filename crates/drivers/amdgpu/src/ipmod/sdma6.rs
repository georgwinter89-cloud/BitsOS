//! SDMA 6.0.1: Linux `sdma_v6_0` (linux-6.6 amdgpu/sdma_v6_0.c). Its registers sit in the GC 11 register map
//! (Linux reaches them through GC's segment bases, sdma_v6_0.c:123-139 `sdma_v6_0_get_reg_offset`).
//!
//! GX-7i: the registers the bring-up touches (`R`, as (segment, dword) of gc_11_0_0_offset.h for instance 0),
//! Linux's values (`sdma_v6_0_gfx_resume` :467-619 and `sdma_v6_0_enable` :439, checked against the 27.10. capture
//! of the 780M, Linux 7.1) and the doorbell numbers (amdgpu_doorbell.h:209, soc21.c:499-510, nbio_v7_7.c:65-85).
//! The run is `gpucore::sdma`.

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::fw::Need;

pub static MODULE: Module = Module {
    name: "sdma6_0_1",
    role: Role::Sdma,
    versions: &[v(6, 0, 1)],
    linux: "sdma_v6_0",
    linux_also: &[v(6, 0, 0), v(6, 0, 2), v(6, 0, 3), v(6, 1, 0)],
    firmware: Firmware::Files { prefix: "sdma", needs: &[Need::one("")] },
    state: State::Present("gpucore::sdma (GX-7i: ring 0 in the carve-out, WRITE_LINEAR, CONST_FILL + FENCE, COPY_LINEAR into GART pages, the golden IB, then the doorbell)"),
};

/// One SDMA0 register (instance 0; Linux's `regSDMA0_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum R {
    /// GX-7m3: a stalled queue's reset (mes_v11_0_reset_queue_mmio, 7.1) and the stall's report.
    QueueResetReq,
    Status4,
    Utcl1RdXnack0,
    Utcl1WrXnack0,
    Cntl,
    SemWaitFailTimer,
    Status,
    UcodeChecksum,
    Freeze,
    Watchdog,
    Utcl1Cntl,
    Utcl1Page,
    Ucode1Checksum,
    RbCntl,
    RbBase,
    RbBaseHi,
    RbRptr,
    RbRptrHi,
    RbWptr,
    RbWptrHi,
    RptrAddrHi,
    RptrAddrLo,
    IbCntl,
    ContextStatus,
    Doorbell,
    DoorbellOffset,
    WptrPollHi,
    WptrPollLo,
    MinorPtrUpdate,
    F32Cntl,
}

impl R {
    pub const ALL: [R; 30] = [
        R::Cntl,
        R::SemWaitFailTimer,
        R::Status,
        R::UcodeChecksum,
        R::Freeze,
        R::Watchdog,
        R::Utcl1Cntl,
        R::Utcl1Page,
        R::Ucode1Checksum,
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
        R::ContextStatus,
        R::Doorbell,
        R::DoorbellOffset,
        R::WptrPollHi,
        R::WptrPollLo,
        R::MinorPtrUpdate,
        R::F32Cntl,
        R::QueueResetReq,
        R::Status4,
        R::Utcl1RdXnack0,
        R::Utcl1WrXnack0,
    ];

    /// (segment, dword) in GC's bases (gc_11_0_0_offset.h; F32_CNTL is the only segment-1 one here).
    pub fn at(self) -> (usize, u32) {
        let d = match self {
            R::Cntl => 0x1c,
            R::SemWaitFailTimer => 0x22,
            R::Status => 0x25,
            R::UcodeChecksum => 0x29,
            R::Freeze => 0x2b,
            R::Watchdog => 0x2e,
            R::Utcl1Cntl => 0x3c,
            R::Utcl1Page => 0x3f,
            R::Ucode1Checksum => 0x7d,
            R::RbCntl => 0x80,
            R::RbBase => 0x81,
            R::RbBaseHi => 0x82,
            R::RbRptr => 0x83,
            R::RbRptrHi => 0x84,
            R::RbWptr => 0x85,
            R::RbWptrHi => 0x86,
            R::RptrAddrHi => 0x88,
            R::RptrAddrLo => 0x89,
            R::IbCntl => 0x8a,
            R::ContextStatus => 0x91,
            R::Doorbell => 0x92,
            R::DoorbellOffset => 0xab,
            R::WptrPollHi => 0xb2,
            R::WptrPollLo => 0xb3,
            R::MinorPtrUpdate => 0xb5,
            R::QueueResetReq => 0x7b,
            R::Status4 => 0x76,
            R::Utcl1RdXnack0 => 0x45,
            R::Utcl1WrXnack0 => 0x47,
            R::F32Cntl => return (1, 0x589a),
        };
        (0, d)
    }

    pub fn name(self) -> &'static str {
        match self {
            R::QueueResetReq => "SDMA0_QUEUE_RESET_REQ",
            R::Status4 => "SDMA0_STATUS4_REG",
            R::Utcl1RdXnack0 => "SDMA0_UTCL1_RD_XNACK0",
            R::Utcl1WrXnack0 => "SDMA0_UTCL1_WR_XNACK0",
            R::Cntl => "SDMA0_CNTL",
            R::SemWaitFailTimer => "SDMA0_SEM_WAIT_FAIL_TIMER_CNTL",
            R::Status => "SDMA0_STATUS_REG",
            R::UcodeChecksum => "SDMA0_UCODE_CHECKSUM",
            R::Freeze => "SDMA0_FREEZE",
            R::Watchdog => "SDMA0_WATCHDOG_CNTL",
            R::Utcl1Cntl => "SDMA0_UTCL1_CNTL",
            R::Utcl1Page => "SDMA0_UTCL1_PAGE",
            R::Ucode1Checksum => "SDMA0_UCODE1_CHECKSUM",
            R::RbCntl => "SDMA0_QUEUE0_RB_CNTL",
            R::RbBase => "SDMA0_QUEUE0_RB_BASE",
            R::RbBaseHi => "SDMA0_QUEUE0_RB_BASE_HI",
            R::RbRptr => "SDMA0_QUEUE0_RB_RPTR",
            R::RbRptrHi => "SDMA0_QUEUE0_RB_RPTR_HI",
            R::RbWptr => "SDMA0_QUEUE0_RB_WPTR",
            R::RbWptrHi => "SDMA0_QUEUE0_RB_WPTR_HI",
            R::RptrAddrHi => "SDMA0_QUEUE0_RB_RPTR_ADDR_HI",
            R::RptrAddrLo => "SDMA0_QUEUE0_RB_RPTR_ADDR_LO",
            R::IbCntl => "SDMA0_QUEUE0_IB_CNTL",
            R::ContextStatus => "SDMA0_QUEUE0_CONTEXT_STATUS",
            R::Doorbell => "SDMA0_QUEUE0_DOORBELL",
            R::DoorbellOffset => "SDMA0_QUEUE0_DOORBELL_OFFSET",
            R::WptrPollHi => "SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_HI",
            R::WptrPollLo => "SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_LO",
            R::MinorPtrUpdate => "SDMA0_QUEUE0_MINOR_PTR_UPDATE",
            R::F32Cntl => "SDMA0_F32_CNTL",
        }
    }
}

/// Field masks (gc_11_0_0_sh_mask.h).
pub mod f {
    pub const F32_HALT: u32 = 1;
    pub const F32_TH0_RESET: u32 = 1 << 9;
    pub const F32_TH0_ENABLE: u32 = 1 << 10;
    pub const F32_TH1_RESET: u32 = 1 << 13;
    pub const CNTL_TRAP_ENABLE: u32 = 1;
    pub const CNTL_CTXEMPTY_INT_ENABLE: u32 = 1 << 28;
    pub const RB_ENABLE: u32 = 1;
    pub const RB_SIZE_MASK: u32 = 0x3e;
    pub const RB_WPTR_POLL_ENABLE: u32 = 1 << 8;
    pub const RB_F32_WPTR_POLL_ENABLE: u32 = 1 << 11;
    pub const RB_RPTR_WRITEBACK_ENABLE: u32 = 1 << 12;
    pub const RB_PRIV: u32 = 1 << 23;
    pub const IB_ENABLE: u32 = 1;
    pub const DOORBELL_ENABLE: u32 = 1 << 28;
    pub const DOORBELL_CAPTURED: u32 = 1 << 30;
    pub const DOORBELL_OFFSET_MASK: u32 = 0x0fff_fffc;
    pub const STATUS_IDLE: u32 = 1;
    pub const WATCHDOG_HANG_COUNT_MASK: u32 = 0xff;
    pub const UTCL1_REDO_DELAY_MASK: u32 = 0x1f;
    pub const UTCL1_RESP_MODE_MASK: u32 = 0x600;
    pub const UTCL1_PAGE_LLC_NOALLOC: u32 = 1 << 24;
}

/// Linux's ring: 8 KiB (sdma_v6_0.c sw_init, ring_size 1024 * 4 * 2 rounded: 2048 dwords, RB_SIZE 11).
pub const RING_DW: u64 = 2048;
/// Linux's waits (adev->usec_timeout).
pub const LINUX_TIMEOUT_US: u64 = 100_000;
/// The firmware takes the counted NOP from this feature version on (amdgpu_sdma.c:187-188).
pub const BURST_NOP_FEATURE: u32 = 20;

/// SDMA engine 0's doorbell in 64-bit doorbell units (AMDGPU_NAVI10_DOORBELL_sDMA_ENGINE0, soc21.c:499).
pub const DOORBELL_INDEX64: u32 = 0x100;
/// The same as a dword index (sdma_v6_0.c sw_init `<< 1`): the BAR2 byte offset is this times 4.
pub const DOORBELL_DW: u32 = DOORBELL_INDEX64 << 1;
/// Doorbells NBIO routes to SDMA (soc21.c:510 sdma_doorbell_range 20, one instance).
pub const DOORBELL_RANGE: u32 = 20;
/// GDC0_BIF_CSDMA_DOORBELL_RANGE in NBIO 7.7's segment 3 (nbio_7_7_0_offset.h), reached through RSMU.
pub const CSDMA_DOORBELL_RANGE: (usize, u32) = (3, 0x4f_0afb);
/// BIF_BX_PF0_RSMU_INDEX/DATA in NBIO segment 1 (nbio_v7_7.c:212-220: the PCIE port window).
pub const RSMU_INDEX: (usize, u32) = (1, 0);
pub const RSMU_DATA: (usize, u32) = (1, 1);

/// RB_CNTL as `sdma_v6_0_gfx_resume` first writes it on a cold engine (RB_SIZE, RB_PRIV; read-modify-write).
/// RB_ENABLE and RPTR_WRITEBACK_ENABLE are cleared: a restarted ring still holds them from its last resume, and
/// carried over they made the second resume's first write 0x00841816 where Linux writes 0x00840816 (28.10.).
pub fn rb_cntl_size(v: u32, ring_dw: u64) -> u32 {
    let log2 = ring_dw.trailing_zeros();
    (v & !(f::RB_SIZE_MASK | f::RB_ENABLE | f::RB_RPTR_WRITEBACK_ENABLE)) | ((log2 << 1) & f::RB_SIZE_MASK) | f::RB_PRIV
}

/// The same with the writeback and the firmware's wptr poll (WPTR_POLL off, F32 poll on); `enable` adds RB_ENABLE.
pub fn rb_cntl_run(sized: u32, enable: bool) -> u32 {
    let v = (sized | f::RB_RPTR_WRITEBACK_ENABLE | f::RB_F32_WPTR_POLL_ENABLE) & !f::RB_WPTR_POLL_ENABLE;
    if enable { v | f::RB_ENABLE } else { v }
}

/// UTCL1_CNTL: RESP_MODE 3, REDO_DELAY 9 (:556-560).
pub fn utcl1_cntl(v: u32) -> u32 {
    (v & !(f::UTCL1_RESP_MODE_MASK | f::UTCL1_REDO_DELAY_MASK)) | (3 << 9) | 9
}

/// UTCL1_PAGE: read policy NOA, write policy BYPASS, LLC_NOALLOC (:562-569, sdma_common.h).
pub fn utcl1_page(v: u32) -> u32 {
    (v & 0x00ff_0fff) | (2 << 12) | (3 << 14) | f::UTCL1_PAGE_LLC_NOALLOC
}

/// F32_CNTL unhalted (HALT 0, TH1_RESET 0, and TH0_RESET/TH0_ENABLE 0 as on the cold engine), or halted. A
/// halted engine that ran reads TH0_RESET and TH0_ENABLE set (the Mini-PC 28.10.: 0x08084600); carried over they
/// differ from Linux's cold 0x08084000.
pub fn f32_cntl(v: u32, run: bool) -> u32 {
    if run { v & !(f::F32_HALT | f::F32_TH0_RESET | f::F32_TH0_ENABLE | f::F32_TH1_RESET) } else { v | f::F32_HALT }
}

/// WATCHDOG_CNTL's queue hang count in 100 ms units: Linux 7.1 writes max(usec_timeout / 100000, 1) (the
/// capture; 6.6 leaves it).
pub fn watchdog(v: u32, timeout_us: u64) -> u32 {
    let n = (timeout_us / 100_000).clamp(1, 0xff) as u32;
    (v & !f::WATCHDOG_HANG_COUNT_MASK) | n
}

/// DOORBELL / DOORBELL_OFFSET as resume writes them. CAPTURED (a doorbell the queue took) is written 0, the cold
/// engine's value (the capture writes 0x10000000).
pub fn doorbell(v: u32, on: bool) -> u32 {
    let v = v & !f::DOORBELL_CAPTURED;
    if on { v | f::DOORBELL_ENABLE } else { v & !f::DOORBELL_ENABLE }
}

/// The restart's doorbell off: two writes that leave ENABLE and CAPTURED 0 whether CAPTURED is cleared by writing
/// 1 or by writing 0 (its write semantics are not documented in the headers).
pub fn doorbell_off_steps(v: u32) -> [u32; 2] {
    let off = v & !(f::DOORBELL_ENABLE | f::DOORBELL_CAPTURED);
    [off | f::DOORBELL_CAPTURED, off]
}

pub fn doorbell_offset(v: u32, index_dw: u32) -> u32 {
    (v & !f::DOORBELL_OFFSET_MASK) | ((index_dw << 2) & f::DOORBELL_OFFSET_MASK)
}

/// GDC0_BIF_CSDMA_DOORBELL_RANGE with OFFSET (dword index, bits 11:2) and SIZE (23:16) (nbio_v7_7.c:65-85).
pub fn csdma_range(v: u32, index_dw: u32, size: u32) -> u32 {
    (v & !0x00ff_0ffc) | ((index_dw << 2) & 0xffc) | ((size << 16) & 0x00ff_0000)
}

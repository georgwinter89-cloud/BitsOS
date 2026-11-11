//! GX-7a: the PSP's command protocol, version-independent (linux-6.6 amdgpu/amdgpu_psp.c, psp_gfx_if.h): the KM
//! ring of 64-byte frames in a GPU-visible buffer, one 1024-byte command buffer per command, a fence value per
//! frame written back by the PSP, and the bring-up order of `psp_load_fw` for an APU whose secure OS the BIOS
//! started: ring create, LOAD_TOC (the TMR size), the TMR in the carve-out (SETUP_TMR), then LOAD_IP_FW for each
//! microcode in Linux's order with RLC autoload after RLC_G. The register half of the ring is the IP module's
//! (`ipmod::psp13::Mailbox13`); buffers come from the BO manager (GX-D3).
//!
//! Everything goes through `Hw` (registers, GPU memory by MC address, a clock): `gpucore::pspfake` in tests,
//! the live BAR5/carve-out owner (`psplive`) on the Mini-PC. Every wait is bounded by time and by a try count
//! derived from it (B4); a timeout, a register that reads all ones and a write pointer outside the ring are
//! errors with a klog event; after a fence timeout the ring is wedged (the PSP may still run that frame) and
//! refuses further commands.
//!
//! GX-7d, stricter than Linux: any non-zero command status is fatal (`CmdStatus`: stop, undo, report; Linux only
//! warns, amdgpu_psp.c:681-699, and the 27.10. capture had none); a ring or TMR an earlier kernel left is
//! destroyed first (`Prior::Stale`: DESTROY_RINGS before create, DESTROY_TMR before LOAD_TOC); DMCUB is never in
//! a list (GX-D9) and VCN only when `allow_vcn`; `Until` stops after M1 (ring + LOAD_TOC) or M2 (all firmware).
//! `stop` neither allocates nor sleeps beyond `Hw::delay_us`, so the Quiesce hook can run it.

use crate::bo::{BoError, BoManager, PAGE, Placement};
use crate::gpucore::ucode::{FwSet, Kind, Ucode, UcodeId};
use alloc::vec::Vec;
use core::ops::Range;

/// Bit 31 of C2PMSG_64: the PSP answered (psp_gfx_if.h:84).
pub const FLAG_RESPONSE: u32 = 0x8000_0000;
pub const STATUS_MASK: u32 = 0x0000_FFFF;
/// `PSP_RING_TYPE__KM` (amdgpu_psp.h:90); `KM << 16` is GFX_CTRL_CMD_ID_INIT_GPCOM_RING.
pub const RING_TYPE_KM: u32 = 2;
pub const CTRL_INIT_GPCOM_RING: u32 = 0x0002_0000;
pub const CTRL_DESTROY_RINGS: u32 = 0x0003_0000;

/// Ring-buffer command IDs (psp_gfx_if.h:87-110).
pub mod cmd {
    pub const LOAD_TA: u32 = 0x01;
    pub const SETUP_TMR: u32 = 0x05;
    pub const LOAD_IP_FW: u32 = 0x06;
    pub const DESTROY_TMR: u32 = 0x07;
    pub const LOAD_TOC: u32 = 0x20;
    pub const AUTOLOAD_RLC: u32 = 0x21;
}
pub const PSP_ERR_UNKNOWN_COMMAND: u32 = 0x100;

/// Sizes Linux uses (amdgpu_psp.h:35-39, amdgpu_psp.c:64): a 4 KiB ring, command and fence buffers, the 1 MiB
/// private buffer the TOC is copied to, the TMR aligned to 1 MiB, 4 MiB TMR when there is no TOC.
pub const RING_BYTES: u32 = 0x1000;
pub const FRAME_BYTES: u32 = 64;
pub const CMD_BUF_BYTES: u64 = 0x1000;
pub const CMD_RESP_BYTES: usize = 1024;
/// `psp_gfx_cmd_resp`: cmd_id @8, the command union @28, the response @864 (psp_gfx_if.h:418-430).
pub const CMD_ID_AT: usize = 8;
pub const CMD_AT: usize = 28;
pub const RESP_AT: usize = 864;
pub const FENCE_BUF_BYTES: u64 = 0x1000;
pub const PRI_BUF_BYTES: u64 = 0x10_0000;
pub const TMR_ALIGN: u64 = 0x10_0000;
pub const TMR_DEFAULT: u64 = 0x40_0000;
/// `cmd_setup_tmr.bitfield.virt_phy_addr`: both addresses given (amdgpu_psp.c:737).
pub const TMR_VIRT_PHY: u32 = 1 << 1;

/// What the PSP code needs from the device: BAR5 registers, GPU memory by MC address (the carve-out as the GPU
/// sees it) and a microsecond clock with a delay. The real one is `psplive::PspHw` (BAR5 through `DcnRegs`,
/// driven under GX-7d's `Watch`; the carve-out mapping and the kernel clock); the fake is `pspfake`.
pub trait Hw {
    fn reg_read(&mut self, byte: u32) -> Option<u32>;
    /// `false`: the write was refused.
    fn reg_write(&mut self, byte: u32, value: u32) -> bool;
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool;
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool;
    fn now_us(&mut self) -> u64;
    fn delay_us(&mut self, us: u64);
    /// Linux flushes HDP after writing a frame and invalidates it before each fence read
    /// (amdgpu_psp.c:2984, :656); fakes need neither.
    fn flush_hdp(&mut self) {}
    fn invalidate_hdp(&mut self) {}
    /// GX-7h: one 8-byte store the GPU never sees half of (a GART or page-table entry); `mc` 8-byte aligned.
    fn mem_write64(&mut self, mc: u64, value: u64) -> bool {
        mc.is_multiple_of(8) && self.mem_write(mc, &value.to_le_bytes())
    }
    /// GX-7h: earlier memory stores reach the GPU before what follows (a TLB invalidate after table writes).
    fn store_barrier(&mut self) {}
    /// GX-7i: one 64-bit doorbell write at dword index `index_dw` of the doorbell BAR (BAR2; Linux WDOORBELL64).
    /// `false`: no doorbell BAR, or the index is not one this owner may ring.
    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        let _ = (index_dw, value);
        false
    }
}

/// The register half of the KM ring: one implementation per PSP version (`ipmod::psp13::Mailbox13`).
pub trait Mailbox {
    /// The sOS sign of life (non-zero: the secure OS runs).
    fn sos(&self, hw: &mut dyn Hw) -> Result<u32, PspError>;
    fn ring_create(&self, hw: &mut dyn Hw, t: &Timeouts, mc: u64, size: u32) -> Result<(), PspError>;
    fn ring_stop(&self, hw: &mut dyn Hw, t: &Timeouts) -> Result<(), PspError>;
    /// The write pointer in dwords.
    fn wptr(&self, hw: &mut dyn Hw) -> Result<u32, PspError>;
    fn set_wptr(&self, hw: &mut dyn Hw, dwords: u32) -> Result<(), PspError>;
    /// The ring size register as it reads before create (non-zero: an earlier kernel left a ring there).
    fn ring_size_reg(&self, hw: &mut dyn Hw) -> Result<u32, PspError>;
    /// The mailbox registers read back for the step report (name, value; `None`: unreadable).
    fn regs(&self, _hw: &mut dyn Hw) -> Vec<(&'static str, Option<u32>)> {
        Vec::new()
    }
}

/// Where the PSP's buffers come from: the BO manager directly (tests) or the device object's shared one, locked
/// per call (the live run sleeps between allocations, so it never holds the lock across the run).
pub trait BoAlloc {
    /// A carve-out BO: (handle, carve-out offsets).
    fn alloc(&mut self, size: u64, align: u64) -> Result<(u32, Range<u64>), BoError>;
    fn release(&mut self, handle: u32);
}

impl BoAlloc for BoManager {
    fn alloc(&mut self, size: u64, align: u64) -> Result<(u32, Range<u64>), BoError> {
        let h = self.create(size, Placement::Vram, align, None)?;
        match self.get(h) {
            Some(b) => Ok((h, b.range.clone())),
            None => Err(BoError::NoHandle),
        }
    }
    fn release(&mut self, handle: u32) {
        BoManager::release(self, handle);
    }
}

/// The waits. Linux: 100 ms register waits polled every 1 us (`psp_wait_for`, amdgpu_psp.c:539,
/// AMDGPU_MAX_USEC_TIMEOUT), 20 ms settle after a ring command, fences 20000 polls of 10-100 us (:629).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    pub reg_us: u64,
    pub poll_us: u64,
    pub settle_us: u64,
    pub fence_us: u64,
    pub fence_poll_us: u64,
}

impl Timeouts {
    pub const LINUX: Timeouts = Timeouts { reg_us: 100_000, poll_us: 10, settle_us: 20_000, fence_us: 2_000_000, fence_poll_us: 100 };
    /// The Quiesce hook's (interrupts off, every wait a busy wait): Linux's register waits and settle, the fence
    /// of DESTROY_TMR bounded to 100 ms (the whole undo stays under half a second).
    pub const QUIESCE: Timeouts = Timeouts { reg_us: 100_000, poll_us: 10, settle_us: 20_000, fence_us: 100_000, fence_poll_us: 10 };
}

pub const ETIME: i32 = 62;
pub const EINVAL: i32 = 22;
pub const ENOMEM: i32 = 12;
pub const ENODEV: i32 = 19;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PspError {
    /// A register read failed or read all ones (the device is gone or the BAR is off).
    Gone { byte: u32 },
    WriteRefused { byte: u32 },
    /// C2PMSG_81 reads 0: the secure OS does not run, and BitsOS has no bootloader images for this PSP
    /// (neither does Linux: 13.0.4 ships only toc/ta).
    SosNotRunning,
    Timeout { what: &'static str, last: u32 },
    /// Ring create answered with a non-zero status.
    RingStatus { status: u16 },
    /// The write pointer register points outside the ring or between frames.
    RingBounds { wptr: u32 },
    /// Every frame but one is still waiting for its fence.
    RingFull,
    /// A fence timed out earlier (the PSP may still run that frame), or an earlier undo failed (the PSP may still
    /// hold this PSP's buffers, `quiet` is false): nothing more is sent, the buffers are not reused.
    Wedged,
    NoRing,
    Mem { mc: u64 },
    Bo(BoError),
    /// The TOC asked for a TMR of 0 bytes.
    TmrSize(u32),
    /// The TOC is larger than the private buffer.
    TocSize(usize),
    /// A KM ring that is not a whole number of frames (at least one) or larger than its buffer.
    RingSize(u32),
    /// RLC autoload was asked for, but the list has no RLC_G to start it after (nothing would start the RLC).
    NoRlcG,
    /// `start` on a PSP whose ring still runs: `stop` first.
    Started,
    /// GX-7d: a command answered a non-zero status (fatal; `ucode` for LOAD_IP_FW).
    CmdStatus { cmd: u32, status: u32, ucode: Option<UcodeId> },
    /// The write pointer did not read 0 after ring create: a frame there would make the PSP run the old ring's
    /// frames before it (or skip ours); nothing is sent.
    RingNotReset { wptr: u32 },
    /// A microcode BitsOS never loads here: DMCUB (GX-D9, the running picture's firmware) or VCN (not this item).
    Forbidden(UcodeId),
}

impl PspError {
    /// The Linux errno of the same failure (psp_wait_for -ETIME, psp_ring_cmd_submit -EINVAL, BO -ENOMEM).
    pub fn errno(&self) -> i32 {
        match self {
            PspError::Timeout { .. } | PspError::RingStatus { .. } => ETIME,
            PspError::Bo(_) => ENOMEM,
            PspError::Gone { .. } => ENODEV,
            // Linux returns -EINVAL for a failed command it does not tolerate (amdgpu_psp.c:693).
            _ => EINVAL,
        }
    }
}

/// A register read; `None` and all ones are `Gone` (a master abort reads 0xFFFFFFFF, and bit 31 of it would
/// look like a PSP answer).
pub fn read(hw: &mut dyn Hw, byte: u32) -> Result<u32, PspError> {
    match hw.reg_read(byte) {
        Some(v) if v != u32::MAX => Ok(v),
        _ => {
            klog::event!(Error, "gx", "psp.gone", reg = byte; "register read failed or all ones");
            Err(PspError::Gone { byte })
        }
    }
}

pub fn write(hw: &mut dyn Hw, byte: u32, value: u32) -> Result<(), PspError> {
    if hw.reg_write(byte, value) {
        Ok(())
    } else {
        klog::event!(Error, "gx", "psp.write_refused", reg = byte, value = value);
        Err(PspError::WriteRefused { byte })
    }
}

/// `psp_wait_for`: until `(reg & mask) == value`; returns the value read.
pub fn wait_for(hw: &mut dyn Hw, t: &Timeouts, byte: u32, value: u32, mask: u32, what: &'static str) -> Result<u32, PspError> {
    let start = hw.now_us();
    let tries = t.reg_us / t.poll_us.max(1) + 1;
    let mut last = 0;
    for _ in 0..=tries {
        last = read(hw, byte)?;
        if last & mask == value {
            return Ok(last);
        }
        if hw.now_us().saturating_sub(start) >= t.reg_us {
            break;
        }
        hw.delay_us(t.poll_us.max(1));
    }
    klog::event!(Warn, "gx", "psp.timeout", what = what, reg = byte, last = last, waited_us = t.reg_us);
    Err(PspError::Timeout { what, last })
}

/// A BO from the manager with its MC address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buf {
    pub handle: u32,
    pub offset: u64,
    pub size: u64,
    pub mc: u64,
}

/// Where the carve-out lies for the GPU (MC) and for the CPU (physical); the TMR command carries both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carve {
    pub mc_base: u64,
    pub phys_base: u64,
}

fn alloc(bo: &mut dyn BoAlloc, c: &Carve, size: u64, align: u64) -> Result<Buf, PspError> {
    let (handle, range) = bo.alloc(size, align).map_err(PspError::Bo)?;
    let Some(mc) = c.mc_base.checked_add(range.start) else {
        bo.release(handle);
        return Err(PspError::Mem { mc: c.mc_base });
    };
    Ok(Buf { handle, offset: range.start, size: range.end - range.start, mc })
}

fn put(hw: &mut dyn Hw, mc: u64, data: &[u8]) -> Result<(), PspError> {
    if hw.mem_write(mc, data) { Ok(()) } else { Err(PspError::Mem { mc }) }
}

fn get32(hw: &mut dyn Hw, mc: u64) -> Result<u32, PspError> {
    let mut b = [0u8; 4];
    if hw.mem_read(mc, &mut b) { Ok(u32::from_le_bytes(b)) } else { Err(PspError::Mem { mc }) }
}

/// One frame (`psp_gfx_rb_frame`, psp_gfx_if.h:442): command buffer address, fence address and value; the
/// rest zero (Linux leaves cmd_buf_size 0 too, amdgpu_psp.c:2976-2983).
pub fn frame(cmd_mc: u64, fence_mc: u64, value: u32) -> [u8; FRAME_BYTES as usize] {
    let mut f = [0u8; FRAME_BYTES as usize];
    let words = [(cmd_mc as u32), (cmd_mc >> 32) as u32, 0, fence_mc as u32, (fence_mc >> 32) as u32, value];
    for (i, w) in words.iter().enumerate() {
        f[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    f
}

/// The KM ring: frames written at the PSP's write pointer, fence values counted so a full ring is refused.
#[derive(Clone, Debug)]
pub struct KmRing {
    pub buf: Buf,
    bytes: u32,
    last: u32,
    done: u32,
}

impl KmRing {
    /// `None` unless `bytes` is a whole number of frames, at least one, inside `buf`: the slot arithmetic of
    /// `push` divides by the ring's size.
    pub fn new(buf: Buf, bytes: u32) -> Option<KmRing> {
        if bytes < FRAME_BYTES || bytes % FRAME_BYTES != 0 || u64::from(bytes) > buf.size {
            klog::event!(Error, "gx", "psp.ring.size", bytes = bytes, buf = buf.size);
            return None;
        }
        Some(KmRing { buf, bytes, last: 0, done: 0 })
    }

    pub fn bytes(&self) -> u32 {
        self.bytes
    }

    /// A fresh ring after ring create: the PSP starts at write pointer 0 and nothing is in flight.
    pub fn reset(&mut self) {
        self.last = 0;
        self.done = 0;
    }

    pub fn frames(&self) -> u32 {
        self.bytes / FRAME_BYTES
    }

    /// Frames whose fence has not been seen.
    pub fn in_flight(&self) -> u32 {
        self.last.wrapping_sub(self.done)
    }

    /// The fence value `value` was seen: it and every frame before it are done.
    pub fn complete(&mut self, value: u32) {
        self.done = value;
    }

    /// `psp_ring_cmd_submit` (amdgpu_psp.c:2940): the frame at the write pointer, the pointer one frame on.
    pub fn push(&mut self, hw: &mut dyn Hw, mb: &dyn Mailbox, cmd_mc: u64, fence_mc: u64, value: u32) -> Result<u32, PspError> {
        if self.in_flight() >= self.frames().saturating_sub(1) {
            klog::event!(Warn, "gx", "psp.ring.full", in_flight = self.in_flight());
            return Err(PspError::RingFull);
        }
        let w = mb.wptr(hw)?;
        let ring_dw = self.bytes / 4;
        let frame_dw = FRAME_BYTES / 4;
        if ring_dw == 0 {
            return Err(PspError::RingBounds { wptr: w });
        }
        // Linux: a pointer that is a multiple of the ring is its start, otherwise frame w/frame_dw, refused past
        // the last frame. BitsOS also refuses a pointer between frames (Linux would round it down).
        let slot = if w % ring_dw == 0 { 0 } else { w / frame_dw };
        if w % frame_dw != 0 || slot >= self.frames() {
            klog::event!(Error, "gx", "psp.ring.bounds", wptr = w, ring_dw = ring_dw);
            return Err(PspError::RingBounds { wptr: w });
        }
        let at = self.buf.mc + u64::from(slot * FRAME_BYTES);
        put(hw, at, &frame(cmd_mc, fence_mc, value))?;
        hw.flush_hdp();
        mb.set_wptr(hw, (slot * frame_dw + frame_dw) % ring_dw)?;
        self.last = value;
        Ok(slot)
    }
}

/// `psp_gfx_resp` (psp_gfx_if.h:398): the PSP's answer in the command buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Resp {
    pub status: u32,
    pub session_id: u32,
    pub fw_addr: u64,
    pub tmr_size: u32,
}

/// A command buffer: the ID at 8 and the command's dwords from 28, the rest zero.
pub fn encode(id: u32, words: &[u32]) -> Vec<u8> {
    encode_arr(id, words).to_vec()
}

/// `encode` on the stack (the Quiesce path allocates nothing).
pub fn encode_arr(id: u32, words: &[u32]) -> [u8; CMD_RESP_BYTES] {
    let mut b = [0u8; CMD_RESP_BYTES];
    b[CMD_ID_AT..CMD_ID_AT + 4].copy_from_slice(&id.to_le_bytes());
    for (i, w) in words.iter().enumerate().take((RESP_AT - CMD_AT) / 4) {
        b[CMD_AT + i * 4..CMD_AT + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    b
}

fn lo(x: u64) -> u32 {
    x as u32
}
fn hi(x: u64) -> u32 {
    (x >> 32) as u32
}

/// One loaded microcode as the bring-up report shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub id: UcodeId,
    pub fw_type: u32,
    pub mc: u64,
    pub size: u32,
    /// Where the PSP put it inside the TMR (its answer).
    pub tmr_addr: u64,
    pub status: u32,
}

/// What the bring-up did, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    SosAlive(u32),
    /// DESTROY_RINGS for a ring an earlier kernel left (its size and write pointer registers as found).
    StaleRingDestroyed { size: u32, wptr: u32 },
    RingCreated { mc: u64 },
    /// DESTROY_TMR for a TMR an earlier kernel may have left (no register shows one): its status, tolerated.
    StaleTmrDestroyed { status: u32 },
    Toc { tmr_size: u32 },
    Tmr { mc: u64, size: u64 },
    Load(UcodeId),
    AutoloadRlc,
}

/// How far `start_until` goes (gx-scout-bringup.md M1/M2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Until {
    /// M1: the ring answers (create, the stale TMR destroyed, LOAD_TOC with the TMR size).
    Ring,
    /// M2: the firmware is accepted (SETUP_TMR, every LOAD_IP_FW, AUTOLOAD_RLC).
    Fw,
    /// GX-7e M3: M2, then GC powered by the SMU (`gpucore::gfxup`; for the PSP the same as `Fw`).
    Gfx,
    /// GX-7h: M3, then the GART and VMID 0 on the hubs (`gpucore::gmc`; for the PSP the same as `Fw`).
    Gart,
    /// GX-7i: the GART, then SDMA 0's ring and its rungs (`gpucore::sdma`; for the PSP the same as `Fw`).
    Sdma,
    /// GX-7j: the GART, then the GFX tail and GFX ring 0 driven by the CP directly (`gpucore::gfxring`; M5).
    GfxRing,
    /// GX-7k: the GART, then the GFX tail and Linux's CP unhalt (`gfxring`, `Variant::Mes`), then MES: both pipes,
    /// the KIQ, the scheduler, SET_HW_RESOURCES and the self test (`gpucore::mes`).
    Mes,
    /// GX-7k: `Mes`, then a compute ring and GFX ring 0 mapped through MES with their ring, IB and fence tests.
    GfxMes,
    /// GX-7m: a test VM in the rings (`gpucore::vmring`): on the live chip it continues a passed `GfxMes` of this
    /// boot (its MES-mapped queues and a kept SDMA 0 queue); the fake engine runs `Sdma`, then `GfxMes`, then it.
    Vm,
    /// GX-8a: the fake engine's chain under the hardware backend, the two stages of `BACKEND_STAGES` in one run, kept.
    /// Only the fakes get it: the live tool runs the stages one by one (`psplive::backend`).
    Backend,
}

impl Until {
    /// What `until=backend` runs on the live chip, in order, and so what `Backend` must stand for on the fakes
    /// (`tests/psp.rs` holds the two together).
    pub const BACKEND_STAGES: [Until; 2] = [Until::Sdma, Until::GfxMes];

    /// The run loads the firmware (M2 or more).
    pub fn loads_fw(self) -> bool {
        matches!(self, Until::Fw | Until::Gfx | Until::Gart | Until::Sdma | Until::GfxRing | Until::Mes | Until::GfxMes | Until::Vm | Until::Backend)
    }

    /// The run powers GC (the SMU/GC stage).
    pub fn runs_gc(self) -> bool {
        matches!(self, Until::Gfx | Until::Gart | Until::Sdma | Until::GfxRing | Until::Mes | Until::GfxMes | Until::Vm | Until::Backend)
    }

    /// GX-7i: the run programs the GART (the GMC stage).
    pub fn runs_gmc(self) -> bool {
        matches!(self, Until::Gart | Until::Sdma | Until::GfxRing | Until::Mes | Until::GfxMes | Until::Vm | Until::Backend)
    }

    /// GX-7j: a stage that runs after a passed GMC stage (SDMA or the GFX ring) and continues a kept run.
    pub fn after_gart(self) -> bool {
        matches!(self, Until::Sdma | Until::GfxRing | Until::Mes | Until::GfxMes | Until::Vm | Until::Backend)
    }

    /// GX-7k: the run starts MES (the GFX stage in its `mes` order first).
    pub fn runs_mes(self) -> bool {
        matches!(self, Until::Mes | Until::GfxMes)
    }

    /// The run has a GFX stage (the direct ring, or the CP start for MES).
    pub fn runs_gfx_stage(self) -> bool {
        matches!(self, Until::GfxRing | Until::Mes | Until::GfxMes)
    }

    /// GX-7m: the fake engine's chain for `Vm` (the live chip continues a kept gfx_mes instead): SDMA, the GFX stage
    /// in the MES order, MES with the queues, then the VM stage.
    pub fn fake_chain_sdma(self) -> bool {
        matches!(self, Until::Sdma | Until::Vm | Until::Backend)
    }

    pub fn fake_chain_mes(self) -> bool {
        self.runs_mes() || matches!(self, Until::Vm | Until::Backend)
    }

    /// MES maps the compute ring and GFX ring 0 in this run (`mesq`).
    pub fn maps_queues(self) -> bool {
        matches!(self, Until::GfxMes | Until::Vm | Until::Backend)
    }
}

/// What the PSP was found with before create.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prior {
    /// Firmware state: no ring size or write pointer set, and the classifier said cold.
    Fresh,
    /// A ring (or TMR) an earlier kernel may have left: destroy first.
    Stale,
}

impl Prior {
    /// `hint`: the start-state classifier did not say cold (after Linux, a Reload, a crash).
    pub fn of(size: u32, wptr: u32, hint: bool) -> Prior {
        if hint || size != 0 || wptr != 0 { Prior::Stale } else { Prior::Fresh }
    }
}

/// The mailbox registers and the fence as read back after one step.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snap {
    pub regs: Vec<(&'static str, Option<u32>)>,
    pub fence: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub steps: Vec<Step>,
    /// One per step, the registers read back right after it.
    pub snaps: Vec<Snap>,
    pub loaded: Vec<Loaded>,
    /// Statuses tolerated: only the precautionary DESTROY_TMR of a stale start, (command, status). Every other
    /// non-zero status is fatal (`PspError::CmdStatus`).
    pub warnings: Vec<(u32, u32)>,
    /// C2PMSG_71 before create (non-zero: an earlier kernel's ring).
    pub stale_ring_size: u32,
    /// C2PMSG_67 before create.
    pub stale_wptr: u32,
    pub prior: Option<Prior>,
    /// The first error, when the run failed (the report keeps the steps done before it).
    pub error: Option<PspError>,
}

/// The PSP of one GPU: its buffers, the KM ring and the fence counter.
pub struct Psp<M: Mailbox> {
    pub mb: M,
    pub t: Timeouts,
    pub carve: Carve,
    pub ring: KmRing,
    pub cmd: Buf,
    pub fence: Buf,
    pub pri: Buf,
    pub tmr: Option<Buf>,
    pub fw: Option<Buf>,
    fence_value: u32,
    wedged: bool,
    created: bool,
    /// The PSP may still own (read, write or protect) these buffers: set once a ring create may have reached it,
    /// cleared only by a `stop` that fully succeeded on a ring that was not wedged (GX-7d fixer: `quiet`).
    dirty: bool,
    /// SETUP_TMR was answered: only then does `stop` send DESTROY_TMR.
    tmr_set: bool,
    /// The start-state classifier did not say cold: destroy a stale ring and TMR first even when the
    /// registers look clean (`Prior::of`).
    pub stale_hint: bool,
    /// Whether VCN's microcode may be loaded (GX-7d: no).
    pub allow_vcn: bool,
    /// The report of the last `start`, kept after a failure too.
    pub last: Report,
}

impl<M: Mailbox> Psp<M> {
    /// `psp_sw_init`/`psp_ring_init`: ring, fence, command and private buffers in the carve-out (BitsOS has no
    /// GART yet, GX-D7; Linux allows VRAM for all four). Nothing is written to the device.
    pub fn new(bo: &mut dyn BoAlloc, mb: M, carve: Carve, t: Timeouts) -> Result<Psp<M>, PspError> {
        let mut got: Vec<u32> = Vec::new();
        let mut one = |size, align| -> Result<Buf, PspError> {
            let b = alloc(&mut *bo, &carve, size, align)?;
            got.push(b.handle);
            Ok(b)
        };
        let made = (|| {
            let ring = one(u64::from(RING_BYTES), PAGE)?;
            let ring = KmRing::new(ring, RING_BYTES).ok_or(PspError::RingSize(RING_BYTES))?;
            Ok((ring, one(FENCE_BUF_BYTES, PAGE)?, one(CMD_BUF_BYTES, PAGE)?, one(PRI_BUF_BYTES, PRI_BUF_BYTES)?))
        })();
        match made {
            Ok((ring, fence, cmd, pri)) => Ok(Psp {
                mb,
                t,
                carve,
                ring,
                cmd,
                fence,
                pri,
                tmr: None,
                fw: None,
                fence_value: 0,
                wedged: false,
                created: false,
                dirty: false,
                tmr_set: false,
                stale_hint: false,
                allow_vcn: false,
                last: Report::default(),
            }),
            Err(e) => {
                for h in got {
                    bo.release(h);
                }
                Err(e)
            }
        }
    }

    pub fn wedged(&self) -> bool {
        self.wedged
    }

    pub fn fence_value(&self) -> u32 {
        self.fence_value
    }

    /// `psp_cmd_submit_buf` (amdgpu_psp.c:622-710): command into the command buffer, a frame with the next fence
    /// value, then poll the fence; the answer is read back from the command buffer.
    pub fn submit(&mut self, hw: &mut dyn Hw, cmd_bytes: &[u8]) -> Result<Resp, PspError> {
        if self.wedged {
            return Err(PspError::Wedged);
        }
        if !self.created {
            return Err(PspError::NoRing);
        }
        // The whole `psp_gfx_cmd_resp` (1024 bytes, Linux clears it per command): on the stack, no allocation.
        let mut page = [0u8; CMD_RESP_BYTES];
        let n = cmd_bytes.len().min(page.len());
        page[..n].copy_from_slice(&cmd_bytes[..n]);
        put(hw, self.cmd.mc, &page)?;
        let id = u32::from_le_bytes([page[8], page[9], page[10], page[11]]);
        let mut index = self.fence_value.wrapping_add(1);
        if index == 0 {
            index = 1; // 0 is what the cleared fence buffer holds
        }
        self.ring.push(hw, &self.mb, self.cmd.mc, self.fence.mc, index)?;
        self.fence_value = index;
        let start = hw.now_us();
        let tries = self.t.fence_us / self.t.fence_poll_us.max(1) + 1;
        let mut seen = 0;
        let mut ok = false;
        for _ in 0..=tries {
            hw.invalidate_hdp();
            seen = get32(hw, self.fence.mc)?;
            if seen == index {
                ok = true;
                break;
            }
            if hw.now_us().saturating_sub(start) >= self.t.fence_us {
                break;
            }
            hw.delay_us(self.t.fence_poll_us.max(1));
        }
        if !ok {
            self.wedged = true;
            klog::event!(Error, "gx", "psp.fence.timeout", cmd = id, want = index, seen = seen; "ring wedged");
            return Err(PspError::Timeout { what: "fence", last: seen });
        }
        self.ring.complete(index);
        let mut r = [0u8; 20];
        if !hw.mem_read(self.cmd.mc + RESP_AT as u64, &mut r) {
            return Err(PspError::Mem { mc: self.cmd.mc + RESP_AT as u64 });
        }
        let w = |i: usize| u32::from_le_bytes([r[i], r[i + 1], r[i + 2], r[i + 3]]);
        let resp = Resp { status: w(0), session_id: w(4), fw_addr: u64::from(w(8)) | (u64::from(w(12)) << 32), tmr_size: w(16) };
        if resp.status != 0 {
            klog::event!(Warn, "gx", "psp.cmd.status", cmd = id, status = resp.status);
        }
        Ok(resp)
    }

    /// A command whose non-zero status is fatal (GX-7d).
    fn checked(&mut self, hw: &mut dyn Hw, id: u32, words: &[u32], ucode: Option<UcodeId>) -> Result<Resp, PspError> {
        let r = self.submit(hw, &encode_arr(id, words))?;
        if r.status != 0 {
            klog::event!(Error, "gx", "psp.cmd.failed", cmd = id, status = r.status, ucode = ucode.map_or(0, |u| u.fw_type()); "non-zero status: fatal");
            return Err(PspError::CmdStatus { cmd: id, status: r.status, ucode });
        }
        Ok(r)
    }

    fn snap(&mut self, rep: &mut Report, hw: &mut dyn Hw, step: Step) {
        let regs = self.mb.regs(hw);
        let fence = get32(hw, self.fence.mc).ok();
        rep.steps.push(step);
        rep.snaps.push(Snap { regs, fence });
    }

    /// `psp_load_fw` for an APU with a running sOS (amdgpu_psp.c:2619-2650, `psp_hw_start` :2092-2200,
    /// `psp_load_non_psp_fw` :2560): sOS check, ring create, TMR (size from LOAD_TOC), SETUP_TMR, then each
    /// microcode of `list` (from `FwSet::load_list`) with AUTOLOAD_RLC after RLC_G when `autoload`.
    ///
    /// On an error after ring create, `start` runs `stop` itself (DESTROY_TMR only if SETUP_TMR was answered,
    /// then the ring), so the PSP is left as found. The buffers stay with the `Psp`: `free` is always the
    /// caller's last call, after an error too (no Drop: the BO manager is not owned here). A `Psp` can start
    /// again after `stop`: a new ring, fence count and TMR; refused while its ring still runs (`Started`).
    pub fn start(&mut self, hw: &mut dyn Hw, bo: &mut dyn BoAlloc, fw: &FwSet, list: &[Ucode], autoload: bool) -> Result<Report, PspError> {
        self.start_until(hw, bo, fw, list, autoload, Until::Fw)
    }

    /// `start` that stops after M1 (`Until::Ring`: the ring stays up, no TMR) or M2 (`Until::Fw`). The report
    /// of the run, failed or not, stays in `last`.
    pub fn start_until(&mut self, hw: &mut dyn Hw, bo: &mut dyn BoAlloc, fw: &FwSet, list: &[Ucode], autoload: bool, until: Until) -> Result<Report, PspError> {
        if self.created {
            return Err(PspError::Started);
        }
        if self.dirty {
            return Err(PspError::Wedged);
        }
        if let Some(u) = list.iter().find(|u| u.id == UcodeId::Dmcub || (u.id == UcodeId::Vcn && !self.allow_vcn)) {
            klog::event!(Error, "gx", "psp.forbidden", fw_type = u.id.fw_type(); "DMCUB is never reloaded (GX-D9); VCN is not loaded here");
            return Err(PspError::Forbidden(u.id));
        }
        if until.loads_fw() && autoload && !list.iter().any(|u| u.id == UcodeId::RlcG) {
            klog::event!(Error, "gx", "psp.autoload.no_rlc_g", count = list.len(); "autoload asked, no RLC_G in the list");
            return Err(PspError::NoRlcG);
        }
        // Buffers of an earlier start (stopped since) go back first.
        for b in [self.tmr.take(), self.fw.take()].into_iter().flatten() {
            bo.release(b.handle);
        }
        self.tmr_set = false;
        let mut rep = Report::default();
        let r = self.bring_up(&mut rep, hw, bo, fw, list, autoload, until);
        if let Err(e) = &r {
            rep.error = Some(e.clone());
            if self.created {
                klog::event!(Warn, "gx", "psp.start.undo", err = alloc::format!("{e:?}"); "bring-up failed after ring create");
                if let Err(u) = self.stop(hw) {
                    klog::event!(Error, "gx", "psp.start.undo_failed", err = alloc::format!("{u:?}"));
                }
            }
        }
        self.last = rep.clone();
        r.map(|()| rep)
    }

    #[allow(clippy::too_many_arguments)]
    fn bring_up(&mut self, rep: &mut Report, hw: &mut dyn Hw, bo: &mut dyn BoAlloc, fw: &FwSet, list: &[Ucode], autoload: bool, until: Until) -> Result<(), PspError> {
        let sol = self.mb.sos(hw)?;
        if sol == 0 {
            klog::event!(Error, "gx", "psp.sos.dead"; "C2PMSG_81 reads 0: no secure OS");
            return Err(PspError::SosNotRunning);
        }
        self.snap(rep, hw, Step::SosAlive(sol));
        put(hw, self.fence.mc, &[0u8; FENCE_BUF_BYTES as usize])?;
        // The ring's memory may hold anything (an earlier kernel's frames): cleared before the PSP sees it.
        put(hw, self.ring.buf.mc, &[0u8; RING_BYTES as usize])?;
        rep.stale_ring_size = self.mb.ring_size_reg(hw)?;
        rep.stale_wptr = self.mb.wptr(hw)?;
        let prior = Prior::of(rep.stale_ring_size, rep.stale_wptr, self.stale_hint);
        rep.prior = Some(prior);
        if prior == Prior::Stale {
            // psp_v13_0_4_ring_stop (:194-216): the old ring goes before ours is made (gx-GX-7c.md findings).
            klog::event!(Warn, "gx", "psp.ring.stale", size = rep.stale_ring_size, wptr = rep.stale_wptr, hint = self.stale_hint; "destroying it first");
            self.mb.ring_stop(hw, &self.t)?;
            self.snap(rep, hw, Step::StaleRingDestroyed { size: rep.stale_ring_size, wptr: rep.stale_wptr });
        }
        let made = self.mb.ring_create(hw, &self.t, self.ring.buf.mc, self.ring.bytes());
        // Only "never ready" and a refused register write (the command register C2PMSG_64 was never written) and a
        // status answer (the PSP said no) leave our buffers ours.
        if !matches!(made, Err(PspError::Timeout { what: "ring.ready", .. } | PspError::RingStatus { .. } | PspError::WriteRefused { .. })) {
            self.dirty = true;
        }
        made?;
        self.created = true;
        // A new ring: write pointer 0, nothing in flight, the fence buffer cleared above.
        self.ring.reset();
        self.fence_value = 0;
        self.wedged = false;
        self.snap(rep, hw, Step::RingCreated { mc: self.ring.buf.mc });
        klog::event!(Info, "gx", "psp.ring.created", mc = self.ring.buf.mc);
        let w = self.mb.wptr(hw)?;
        if w != 0 {
            klog::event!(Error, "gx", "psp.ring.not_reset", wptr = w; "write pointer not 0 after create: nothing sent");
            return Err(PspError::RingNotReset { wptr: w });
        }
        if prior == Prior::Stale {
            // No register shows a TMR left behind (gx-GX-7c.md): destroy whatever is there. A non-zero answer
            // may mean "no TMR" and is tolerated here only; SETUP_TMR's status is the proof.
            let r = self.submit(hw, &encode_arr(cmd::DESTROY_TMR, &[]))?;
            if r.status != 0 {
                rep.warnings.push((cmd::DESTROY_TMR, r.status));
            }
            self.snap(rep, hw, Step::StaleTmrDestroyed { status: r.status });
        }

        // psp_tmr_init (:785): the TOC tells the TMR size.
        let mut tmr_size = TMR_DEFAULT;
        if let Some(toc) = fw.get(Kind::Toc) {
            let p = toc.payload();
            if p.len() as u64 > PRI_BUF_BYTES {
                return Err(PspError::TocSize(p.len()));
            }
            put(hw, self.pri.mc, p)?;
            let r = self.checked(hw, cmd::LOAD_TOC, &[lo(self.pri.mc), hi(self.pri.mc), p.len() as u32], None)?;
            if r.tmr_size == 0 {
                klog::event!(Error, "gx", "psp.tmr.size", size = 0u32; "LOAD_TOC answered a TMR of 0 bytes");
                return Err(PspError::TmrSize(0));
            }
            self.snap(rep, hw, Step::Toc { tmr_size: r.tmr_size });
            tmr_size = u64::from(r.tmr_size);
        }
        if until == Until::Ring {
            klog::event!(Info, "gx", "psp.m1", tmr_size = tmr_size; "the ring answers");
            return Ok(());
        }
        let tmr = alloc(bo, &self.carve, tmr_size, TMR_ALIGN)?;
        self.tmr = Some(tmr);
        // psp_prep_tmr_cmd_buf (:718): MC address, size, both-addresses flag, CPU physical address.
        let pa = self.carve.phys_base + tmr.offset;
        self.checked(hw, cmd::SETUP_TMR, &[lo(tmr.mc), hi(tmr.mc), tmr.size as u32, TMR_VIRT_PHY, lo(pa), hi(pa)], None)?;
        self.tmr_set = true;
        self.snap(rep, hw, Step::Tmr { mc: tmr.mc, size: tmr.size });
        klog::event!(Info, "gx", "psp.tmr", mc = tmr.mc, size = tmr.size);

        // amdgpu_ucode_init_bo: every microcode page-aligned in one buffer.
        let total: u64 = list.iter().map(|u| u64::from(u.size()).div_ceil(PAGE) * PAGE).sum();
        let mut at = Vec::with_capacity(list.len());
        if total > 0 {
            let b = alloc(bo, &self.carve, total, PAGE)?;
            self.fw = Some(b);
            let mut off = 0u64;
            for u in list {
                put(hw, b.mc + off, fw.bytes(u))?;
                at.push(b.mc + off);
                off += u64::from(u.size()).div_ceil(PAGE) * PAGE;
            }
        }
        for (u, &mc) in list.iter().zip(&at) {
            let r = self.checked(hw, cmd::LOAD_IP_FW, &[lo(mc), hi(mc), u.size(), u.id.fw_type()], Some(u.id))?;
            rep.loaded.push(Loaded { id: u.id, fw_type: u.id.fw_type(), mc, size: u.size(), tmr_addr: r.fw_addr, status: r.status });
            self.snap(rep, hw, Step::Load(u.id));
            if autoload && u.id == UcodeId::RlcG {
                self.checked(hw, cmd::AUTOLOAD_RLC, &[], None)?;
                self.snap(rep, hw, Step::AutoloadRlc);
            }
        }
        klog::event!(Info, "gx", "psp.loaded", count = rep.loaded.len(), warnings = rep.warnings.len());
        Ok(())
    }

    /// The undo before Reload (`psp_hw_fini`: TMR destroy, then the ring, amdgpu_psp.c:902/:290). After a
    /// timeout no command is sent (the ring is wedged), only the ring is stopped. The ring is stopped even when
    /// DESTROY_TMR fails (its error is returned after). No allocation (the Quiesce hook runs it).
    pub fn stop(&mut self, hw: &mut dyn Hw) -> Result<(), PspError> {
        if !self.created {
            return Ok(());
        }
        let mut first = Ok(());
        if self.tmr_set && !self.wedged {
            first = match self.submit(hw, &encode_arr(cmd::DESTROY_TMR, &[])) {
                Ok(r) if r.status != 0 => {
                    klog::event!(Error, "gx", "psp.tmr.destroy_failed", status = r.status);
                    Err(PspError::CmdStatus { cmd: cmd::DESTROY_TMR, status: r.status, ucode: None })
                }
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            };
            if first.is_ok() {
                self.tmr_set = false;
            }
        }
        // After a wedge or a failed DESTROY_TMR DESTROY_TMR was not sent: the TMR (and the lost frame) stay the PSP's.
        let tmr_left = self.tmr_set;
        let ring = self.mb.ring_stop(hw, &self.t);
        self.created = false;
        let r = first.and(ring);
        if r.is_ok() && !self.wedged && !tmr_left {
            self.dirty = false;
        }
        klog::event!(Info, "gx", "psp.ring.stopped", wedged = self.wedged, ok = r.is_ok(), quiet = !self.dirty);
        r
    }

    /// Whether the PSP provably holds none of these buffers any more: nothing reached it, or `stop` fully
    /// succeeded on a ring that was not wedged. Only then may `free` give them back (GX-7d fixer: a TMR or a
    /// frame still in the PSP's hands must never become a display BO).
    pub fn quiet(&self) -> bool {
        !self.dirty
    }

    /// Every buffer of this PSP (ring, fence, command, private, TMR, firmware).
    pub fn buffers(&self) -> impl Iterator<Item = Buf> {
        [Some(self.ring.buf), Some(self.fence), Some(self.cmd), Some(self.pri), self.tmr, self.fw].into_iter().flatten()
    }

    /// Whether the ring runs (started, not stopped).
    pub fn running(&self) -> bool {
        self.created
    }

    /// Gives every buffer back to the manager.
    pub fn free(self, bo: &mut dyn BoAlloc) {
        for b in [Some(self.ring.buf), Some(self.fence), Some(self.cmd), Some(self.pri), self.tmr, self.fw].into_iter().flatten() {
            bo.release(b.handle);
        }
    }
}

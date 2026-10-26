//! GX-3: the register sequences of a page flip, pure over a register owner (`Rw`), so the host tests run them
//! against a fake engine (`tests::flip`). `scanout` holds the state (which buffer is shown, the fence numbers).
//!
//! Linux 6.6 (DCN 3.1.4 hooks, dc/dcn314/dcn314_optc.c:210-240, dc/dcn31/dcn31_hubp.c:68-92):
//! - order = `dcn10_update_plane_addr` (dc/dcn10/dcn10_hw_sequencer.c:1720) inside `dcn20_pipe_control_lock`
//!   (dc/dcn20/dcn20_hwseq.c:1222), i.e. `optc3_lock` (dc/dcn30/dcn30_optc.c:119: LOCK_SEL, LOCK = 1, wait for
//!   UPDATE_LOCK_STATUS 10 x 1 us) ... `hubp3_program_surface_flip_and_addr` (dc/dcn30/dcn30_hubp.c:68: FLIP_TYPE
//!   first, then ADDRESS_HIGH, then ADDRESS: "program high first and then the low addr, order matters!") ...
//!   `optc1_unlock` (dc/dcn10/dcn10_optc.c:682).
//! - landed = not `hubp2_is_flip_pending` (dc/dcn20/dcn20_hubp.c:907): PENDING clear and EARLIEST_INUSE equal to
//!   the address asked for, low half read first.
//! - vblank counter = `optc1_get_vblank_counter` (dcn10_optc.c:655), position = `optc1_get_position` (:692).
//! - CRC = `optc1_configure_crc` / `optc1_get_crc` (dcn10_optc.c:1456, :1515) with dc.c:600-626's default
//!   windows (the whole active area, union of A and B, continuous).
//! V1: `P/gpu/dcnflip.asm:8-123` (high first, landed check), X1 (never leave the lock set), X4 (no back buffer
//! under an immediate flip type), X26 (0x1B8A is the LOCK on 3.1.4).

use crate::attach::{Pipe, Regs};
use crate::dcn::{DcnError, DcnRegs};
use crate::layout as l;

/// A register owner that also writes: every write is read back; only the bits in `check` must match.
pub trait Rw: Regs {
    fn write(&self, byte: u32, value: u32, check: u32) -> Result<(), DcnError>;
}

impl Rw for DcnRegs {
    fn write(&self, byte: u32, value: u32, check: u32) -> Result<(), DcnError> {
        DcnRegs::write_masked(self, byte, value, check)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipError {
    /// A register could not be read.
    Read(u32),
    /// A write was refused or read back another value.
    Write(u32),
    /// OTG `k`'s master update lock still reads set after the unlock.
    LockStuck(u8),
}

impl From<DcnError> for FlipError {
    fn from(e: DcnError) -> FlipError {
        match e {
            DcnError::ReadBackMismatch { offset, .. } => FlipError::Write(offset),
            DcnError::NotAttached | DcnError::OutOfRange => FlipError::Write(0),
        }
    }
}

/// Reads of UPDATE_LOCK_STATUS after the lock write. Linux waits 10 x 1 us and goes on either way (REG_WAIT only
/// logs); an uncached read takes about a microsecond, so 16 reads are that wait without a delay loop.
pub const LOCK_READS: u32 = 16;

/// Which SURFACE_FLIP_TYPE the order leaves on each pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Types {
    /// 0 = at VUPDATE (every flip: X4, an immediate type tears a back buffer).
    Vsync,
    /// What the attach found (`Pipe::flip_type`): the restore.
    Found,
}

/// What an order saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ordered {
    /// UPDATE_LOCK_STATUS read set within `LOCK_READS` on every OTG.
    pub lock_seen: bool,
}

fn rd(r: &impl Regs, b: u32) -> Result<u32, FlipError> {
    r.read(b).ok_or(FlipError::Read(b))
}

/// `optc3_lock`: OTG_MASTER_UPDATE_LOCK_SEL = k (written only if it is not), LOCK = 1, the status polled.
pub(crate) fn lock(r: &impl Rw, k: u32) -> Result<bool, FlipError> {
    let sel = rd(r, l::otg(k, l::OTG_GLOBAL_CONTROL2))?;
    if (sel & l::LOCK_SEL) >> l::LOCK_SEL_SHIFT != k {
        let v = (sel & !l::LOCK_SEL) | (k << l::LOCK_SEL_SHIFT);
        r.write(l::otg(k, l::OTG_GLOBAL_CONTROL2), v, l::LOCK_SEL)?;
    }
    r.write(l::otg(k, l::OTG_LOCK), l::LOCK, l::LOCK)?;
    for _ in 0..LOCK_READS {
        if rd(r, l::otg(k, l::OTG_LOCK))? & l::LOCK_STATUS != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `optc1_unlock`, then a read that the lock bit is clear (X1).
pub(crate) fn unlock(r: &impl Rw, k: u32) -> Result<(), FlipError> {
    let w = r.write(l::otg(k, l::OTG_LOCK), 0, l::LOCK);
    match r.read(l::otg(k, l::OTG_LOCK)) {
        Some(v) if v & l::LOCK == 0 => w.map_err(FlipError::from),
        _ => Err(FlipError::LockStuck(k as u8)),
    }
}

/// `hubp3_program_surface_flip_and_addr` for one pipe: FLIP_TYPE (only if it differs), ADDRESS_HIGH (bits
/// [31:16] kept), ADDRESS. Each write is read back whole except FLIP_CONTROL (PENDING is status).
fn program(r: &impl Rw, p: &Pipe, addr: u64, types: Types) -> Result<(), FlipError> {
    let h = u32::from(p.hubp);
    let want = match types {
        Types::Vsync => 0,
        Types::Found if p.flip_type != 0 => l::FLIP_TYPE,
        Types::Found => 0,
    };
    let flip = rd(r, l::hubp(h, l::HUBP_FLIP))?;
    if flip & l::FLIP_TYPE != want {
        r.write(l::hubp(h, l::HUBP_FLIP), (flip & !l::FLIP_TYPE) | want, l::FLIP_TYPE)?;
    }
    let hi = rd(r, l::hubp(h, l::HUBP_ADDR_HI))?;
    r.write(l::hubp(h, l::HUBP_ADDR_HI), (hi & !l::ADDR_HI) | ((addr >> 32) as u32 & l::ADDR_HI), u32::MAX)?;
    r.write(l::hubp(h, l::HUBP_ADDR), addr as u32, u32::MAX)?;
    Ok(())
}

/// Orders `addr` on every pipe under each OTG's master update lock. The unlock runs on every path, also after
/// a failed lock or address write (X1); the first error is returned. Allocates nothing (Quiesce runs it with
/// interrupts off): the OTGs are a bit mask (OTG indexes are below `layout::OTGS`, gate 6).
pub fn order(r: &impl Rw, pipes: &[Pipe], addr: u64, types: Types) -> Result<Ordered, FlipError> {
    let otgs = pipes.iter().filter(|p| u32::from(p.otg) < 32).fold(0u32, |m, p| m | 1 << p.otg);
    let mut locked = 0u32;
    let res = locked_section(r, pipes, addr, types, otgs, &mut locked);
    let mut un = Ok(());
    for k in (0..32).rev().filter(|k| locked & (1 << k) != 0) {
        if let Err(e) = unlock(r, k) {
            un = un.and(Err(e));
        }
    }
    let ordered = res?;
    un.map(|()| ordered)
}

fn locked_section(r: &impl Rw, pipes: &[Pipe], addr: u64, types: Types, otgs: u32, locked: &mut u32) -> Result<Ordered, FlipError> {
    let mut seen = true;
    for k in (0..32).filter(|k| otgs & (1 << k) != 0) {
        // Marked before the lock write: a write that fails half-way still gets its unlock.
        *locked |= 1 << k;
        seen &= lock(r, k)?;
    }
    for p in pipes {
        program(r, p, addr, types)?;
    }
    Ok(Ordered { lock_seen: seen })
}

/// `hubp2_is_flip_pending` for one pipe and the address it was asked to show.
pub fn pending(r: &impl Regs, p: &Pipe, addr: u64) -> Result<bool, FlipError> {
    let h = u32::from(p.hubp);
    if rd(r, l::hubp(h, l::HUBP_FLIP))? & l::FLIP_PENDING != 0 {
        return Ok(true);
    }
    let lo = rd(r, l::hubp(h, l::HUBP_INUSE))?;
    let hi = rd(r, l::hubp(h, l::HUBP_INUSE_HI))? & l::ADDR_HI;
    Ok(u64::from(hi) << 32 | u64::from(lo) != addr)
}

/// Every pipe shows `addr`.
pub fn landed(r: &impl Regs, pipes: &[Pipe], addr: u64) -> Result<bool, FlipError> {
    for p in pipes {
        if pending(r, p, addr)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// OTG_FRAME_COUNT (24 bits).
pub fn frame_count(r: &impl Regs, k: u32) -> Result<u32, FlipError> {
    Ok(rd(r, l::otg(k, l::OTG_FRAME_COUNT))? & l::FRAME_COUNT)
}

/// OTG_VERT_COUNT: the line the OTG is on.
pub fn line(r: &impl Regs, k: u32) -> Result<u32, FlipError> {
    Ok(rd(r, l::otg(k, l::OTG_POSITION))? & l::VERT_COUNT)
}

/// CRC registers as found, for the undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrcSaved {
    pub cntl: u32,
    pub windows: [u32; 4],
}

const CRC_WINDOWS: [u32; 4] = [l::OTG_CRC0_WINDOWA_X, l::OTG_CRC0_WINDOWA_Y, l::OTG_CRC0_WINDOWB_X, l::OTG_CRC0_WINDOWB_Y];

/// `optc1_configure_crc` with dc.c's default: windows A and B = the active `w` x `h`, union, continuous. None:
/// the CRC already ran (nothing written, nothing to undo).
pub fn crc_on(r: &impl Rw, k: u32, w: u32, h: u32) -> Result<Option<CrcSaved>, FlipError> {
    let cntl = rd(r, l::otg(k, l::OTG_CRC_CNTL))?;
    if cntl & l::CRC_EN != 0 {
        return Ok(None);
    }
    let mut windows = [0; 4];
    for (v, &b) in windows.iter_mut().zip(&CRC_WINDOWS) {
        *v = rd(r, l::otg(k, b))?;
    }
    let saved = CrcSaved { cntl, windows };
    r.write(l::otg(k, l::OTG_CRC_CNTL), 0, u32::MAX)?;
    let (x, y) = ((w & 0x7FFF) << 16, (h & 0x7FFF) << 16);
    for (&b, v) in CRC_WINDOWS.iter().zip([x, y, x, y]) {
        r.write(l::otg(k, b), v, u32::MAX)?;
    }
    // OTG_CRC0_SELECT 0 = UNION_WINDOW_A_B (dc/inc/hw/timing_generator.h:91).
    r.write(l::otg(k, l::OTG_CRC_CNTL), l::CRC_CONT_EN | l::CRC_EN, l::CRC_CONT_EN | l::CRC_EN)?;
    Ok(Some(saved))
}

/// The CRC16s of red, green, blue packed `r << 32 | g << 16 | b`; None while the CRC is off.
pub fn crc_read(r: &impl Regs, k: u32) -> Result<Option<u64>, FlipError> {
    if rd(r, l::otg(k, l::OTG_CRC_CNTL))? & l::CRC_EN == 0 {
        return Ok(None);
    }
    let rg = rd(r, l::otg(k, l::OTG_CRC0_DATA_RG))?;
    let b = rd(r, l::otg(k, l::OTG_CRC0_DATA_B))? & 0xFFFF;
    Ok(Some(u64::from(rg & 0xFFFF) << 32 | u64::from(rg >> 16) << 16 | u64::from(b)))
}

/// Puts the CRC registers back as `crc_on` found them (CNTL first: the CRC stops before its windows change).
pub fn crc_undo(r: &impl Rw, k: u32, s: &CrcSaved) -> Result<(), FlipError> {
    r.write(l::otg(k, l::OTG_CRC_CNTL), s.cntl, l::CRC_EN | l::CRC_CONT_EN)?;
    for (&b, &v) in CRC_WINDOWS.iter().zip(&s.windows) {
        r.write(l::otg(k, b), v, u32::MAX)?;
    }
    Ok(())
}

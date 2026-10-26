//! GX-5a: the DCN 3.1.4 hardware cursor, pure over a register owner (`Rw`) so the host tests run it against the
//! fake engine (`tests::cursor`). `scanout` holds the state under its register-pair lock and the surface buffer.
//!
//! Linux 6.6 (dc/dcn314 uses the DCN 2.0 HUBP and DCN 3.0 DPP cursor hooks, dcn31/dcn31_hubp.c:77-78,
//! dcn30/dcn30_dpp.c:1457-1459, dcn314/dcn314_init.c:66,83-85):
//! - image = `dcn10_set_cursor_attribute` (dc/dcn10/dcn10_hw_sequencer.c:3635): `hubp2_cursor_set_attributes`
//!   (dc/dcn20/dcn20_hubp.c:589: ADDRESS_HIGH, ADDRESS, SIZE, CONTROL mode/2x/pitch/lines per chunk,
//!   CURSOR_SETTINGS), `dpp3_set_cursor_attributes` (dc/dcn30/dcn30_dpp.c:351: CUR0_MODE, EXPANSION_MODE 0, ROM_EN
//!   0), then `dcn10_set_cursor_sdr_white_level` (:3645: FP scale 1.0, bias 0).
//! - position = `dcn10_set_cursor_position` (:3397: a negative position goes into the hotspot, :3476-3483),
//!   `hubp2_cursor_set_position` (dc/dcn20/dcn20_hubp.c:970: off all four edges = disabled, ENABLE, POSITION,
//!   HOT_SPOT, DST_OFFSET), `dpp1_set_cursor_position` (dc/dcn10/dcn10_dpp.c:431: CUR0_ENABLE). The pointer's
//!   top-left corner as `amdgpu_dm_plane.c:1180-1220` makes it: x < 0 is position 0 with hotspot -x.
//! - both inside `dcn10_cursor_lock` (:1960) = `mpc1_cursor_lock` (dc/dcn10/dcn10_mpc.c:476:
//!   CUR_VUPDATE_LOCK_SET[opp]): the registers are double buffered and taken together at the VUPDATE after the
//!   unlock, so a move never needs the plane programmed again and never shows half an update.
//! The cursor is in plane space (the mode's pixels; the scaler of a GX-4 switch scales it with the picture, as
//! Linux says of a scaled plane at :3436-3441).

use crate::attach::{Pipe, Regs};
use crate::flip::{FlipError, Rw};
use crate::layout as l;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use gopfb::FbTarget;

/// `dc->caps.max_cursor_size` of DCN 3.1.4 (linux-6.6 dc/dcn314/dcn314_resource.c:1860): a hardware fact.
pub const MAX: u32 = 256;
/// One surface: MAX x MAX ARGB8888 (CURSOR_PITCH 256).
pub const SLOT_BYTES: u64 = (MAX as u64) * (MAX as u64) * 4;
/// Two surfaces: a new image goes into the one not shown, then the address moves under the cursor lock.
pub const BYTES: u64 = 2 * SLOT_BYTES;
/// CUR0_FP_SCALE 1.0 as s5.10 float (`hw_scale = 0x3c00`, dcn10_hw_sequencer.c:3650), bias 0.
const FP_SCALE_ONE: u32 = 0x3C00;
/// CURSOR0_CHUNK_HDL_ADJUST 3, CURSOR0_DST_Y_OFFSET 0 (dcn20_hubp.c:614-618, REG_SET: the rest 0).
const SETTINGS: u32 = 3 << 8;

/// How the image's colour relates to its alpha (`enum dc_cursor_color_format`, dc_hw_types.h:529-536).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alpha {
    /// CURSOR_MODE_COLOR_PRE_MULTIPLIED_ALPHA (2): what the hardware blends directly.
    Premultiplied,
    /// CURSOR_MODE_COLOR_UN_PRE_MULTIPLIED_ALPHA (3).
    Straight,
}

impl Alpha {
    fn mode(self) -> u32 {
        match self {
            Alpha::Premultiplied => 2,
            Alpha::Straight => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorError {
    /// No hardware cursor here (QEMU, no display taken over, a split pipe, no buffer): draw it in software.
    NoDevice(&'static str),
    /// The request itself is wrong (size, hotspot, pixel count, a move before any image).
    Invalid(&'static str),
    /// Another image is being written.
    Busy,
    /// A register could not be read or read back another value.
    Reg(FlipError),
}

impl From<FlipError> for CursorError {
    fn from(e: FlipError) -> CursorError {
        CursorError::Reg(e)
    }
}

/// An image's geometry: 1..=256 pixels each way, the hotspot (the pixel on the pointer's position) inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub w: u32,
    pub h: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub alpha: Alpha,
}

impl Shape {
    pub fn new(w: u32, h: u32, hot_x: u32, hot_y: u32, alpha: Alpha) -> Result<Shape, CursorError> {
        if w == 0 || h == 0 || w > MAX || h > MAX {
            return Err(CursorError::Invalid("cursor size is 1 to 256 pixels each way"));
        }
        if hot_x >= w || hot_y >= h {
            return Err(CursorError::Invalid("hotspot outside the image"));
        }
        Ok(Shape { w, h, hot_x, hot_y, alpha })
    }

    /// The square surface the hardware fetches: 64, 128 or 256 (`hubp1_get_cursor_pitch`, dcn10_hubp.c:1100),
    /// the image in its top-left corner and transparent elsewhere, so width = height = pitch.
    pub fn pitch(&self) -> u32 {
        match self.w.max(self.h) {
            0..=64 => 64,
            65..=128 => 128,
            _ => 256,
        }
    }
}

fn pitch_code(pitch: u32) -> u32 {
    match pitch {
        64 => 0,
        128 => 1,
        _ => 2,
    }
}

/// `hubp2_get_lines_per_chunk` for the 32-bit colour modes (dcn20_hubp.c:563-571; enum
/// `cursor_lines_per_chunk`, inc/hw/hubp.h:42-48).
pub fn lines_per_chunk(width: u32) -> u32 {
    match width {
        0..=32 => 4,
        33..=64 => 3,
        65..=128 => 2,
        _ => 1,
    }
}

/// Row `y` of the surface for `shape` from its `px` (w x h, row by row): the image, then transparent.
pub fn surface_row(shape: &Shape, px: &[u32], y: u32, row: &mut [u32]) {
    row.fill(0);
    if y < shape.h {
        let s = (y * shape.w) as usize;
        if let Some(src) = px.get(s..s + shape.w as usize) {
            row[..src.len()].copy_from_slice(src);
        }
    }
}

/// What the position registers hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub enable: bool,
    /// CURSOR_POSITION: X [29:16], Y [13:0].
    pub position: u32,
    /// CURSOR_HOT_SPOT: X [23:16], Y [7:0].
    pub hot_spot: u32,
    /// CURSOR_DST_X_OFFSET [12:0] (reference clocks from the line start, for the fetch deadline).
    pub dst_offset: u32,
}

impl Place {
    pub const OFF: Place = Place { enable: false, position: 0, hot_spot: 0, dst_offset: 0 };
}

/// Where `shape` goes with its hotspot on `tip` (plane pixels, any value) on a plane of `view` pixels:
/// disabled when no pixel of the surface is on the plane (dcn20_hubp.c:1035-1045); a top-left corner left of
/// or above the plane is position 0 with that much hotspot (amdgpu_dm_plane.c:1206-1218,
/// dcn10_hw_sequencer.c:3476-3483). `dst_q19`: `dst_factor`.
pub fn place(tip: (i32, i32), shape: &Shape, view: (u32, u32), dst_q19: u64) -> Place {
    let size = i64::from(shape.pitch());
    let tx = i64::from(tip.0) - i64::from(shape.hot_x);
    let ty = i64::from(tip.1) - i64::from(shape.hot_y);
    if tx >= i64::from(view.0) || ty >= i64::from(view.1) || tx + size <= 0 || ty + size <= 0 {
        return Place::OFF;
    }
    let (px, hx) = if tx < 0 { (0, -tx) } else { (tx, 0) };
    let (py, hy) = if ty < 0 { (0, -ty) } else { (ty, 0) };
    // dcn20_hubp.c:1023-1031: src_x_offset * ref_clk / pix_clk / h_scale_ratio.
    let dst = ((px as u64).saturating_mul(dst_q19) >> 19).min(0x1FFF);
    Place {
        enable: true,
        position: ((px as u32) & 0x3FFF) << 16 | ((py as u32) & 0x3FFF),
        hot_spot: (hx as u32 & 0xFF) << 16 | (hy as u32 & 0xFF),
        dst_offset: dst as u32,
    }
}

/// REF_FREQ_TO_PIX_FREQ (reference / pixel clock, 2^19 fixed point as the DLG programs it,
/// dml/dcn31/display_rq_dlg_calc_31.c:981) times recout / viewport width (1 / h_scale_ratio): plane pixels to
/// reference clocks, 2^19 fixed point. 0 (no factor read) gives DST_X_OFFSET 0: the earliest deadline.
pub fn dst_factor(ref_to_pix: u32, recout_w: u32, view_w: u32) -> u64 {
    if view_w == 0 {
        return 0;
    }
    u64::from(ref_to_pix & 0x1F_FFFF) * u64::from(recout_w) / u64::from(view_w)
}

/// The pipe the cursor is on and the OPP whose cursor lock holds its updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hw {
    pub pipe: u32,
    pub opp: u32,
}

fn rd(r: &impl Regs, b: u32) -> Result<u32, CursorError> {
    r.read(b).ok_or(CursorError::Reg(FlipError::Read(b)))
}

fn wr(r: &impl Rw, b: u32, v: u32, check: u32) -> Result<(), CursorError> {
    r.write(b, v, check).map_err(|e| CursorError::Reg(FlipError::from(e)))
}

/// The cursor's pipe: HUBP n feeds DPP n, DPP n is MPCC n, MPCCn_MPCC_OPP_ID names the OPP.
pub fn hw(r: &impl Regs, pipe: &Pipe) -> Result<Hw, CursorError> {
    let p = u32::from(pipe.hubp);
    let opp = rd(r, l::mpcc(p, l::MPCC_OPP_ID))? & l::MPCC_OPP_ID_MASK;
    if opp >= l::OPPS {
        return Err(CursorError::NoDevice("the pipe's MPC tree has no OPP"));
    }
    Ok(Hw { pipe: p, opp })
}

/// `dst_factor` from the pipe's registers.
pub fn factor(r: &impl Regs, hw: Hw) -> Result<u64, CursorError> {
    let q = rd(r, l::hubp(hw.pipe, l::REF_FREQ_TO_PIX_FREQ))?;
    let recout = rd(r, l::dpp(hw.pipe, l::RECOUT_SIZE))? & 0x3FFF;
    let view = rd(r, l::hubp(hw.pipe, l::HUBP_VIEWPORT))? & 0x3FFF;
    Ok(dst_factor(q, recout, view))
}

/// `mpc1_cursor_lock` around `f`; the unlock runs on every path (X1). Linux only writes the lock (REG_SET), so
/// it is not read back.
pub fn locked<T>(r: &impl Rw, hw: Hw, f: impl FnOnce() -> Result<T, CursorError>) -> Result<T, CursorError> {
    wr(r, l::cur_lock(hw.opp), 1, 0)?;
    let res = f();
    let un = wr(r, l::cur_lock(hw.opp), 0, 0);
    let v = res?;
    un.map(|()| v)
}

/// The image's registers (HUBP, then DPP, then the FP scale); the enable comes with the position.
pub fn attributes(r: &impl Rw, hw: Hw, addr: u64, shape: &Shape) -> Result<(), CursorError> {
    let p = hw.pipe;
    let pitch = shape.pitch();
    let hi = rd(r, l::hubp(p, l::CUR_ADDR_HI))?;
    wr(r, l::hubp(p, l::CUR_ADDR_HI), (hi & !l::CUR_ADDR_HI_MASK) | ((addr >> 32) as u32 & l::CUR_ADDR_HI_MASK), l::CUR_ADDR_HI_MASK)?;
    wr(r, l::hubp(p, l::CUR_ADDR), addr as u32, u32::MAX)?;
    wr(r, l::hubp(p, l::CUR_SIZE), pitch << 16 | pitch, u32::MAX)?;
    let fields = l::CUR_MODE | l::CUR_2X_MAGNIFY | l::CUR_PITCH | l::CUR_LPC;
    let c = rd(r, l::hubp(p, l::CUR_CONTROL))?;
    let v = (c & !fields)
        | shape.alpha.mode() << l::CUR_MODE_SHIFT
        | pitch_code(pitch) << l::CUR_PITCH_SHIFT
        | lines_per_chunk(pitch) << l::CUR_LPC_SHIFT;
    wr(r, l::hubp(p, l::CUR_CONTROL), v, fields)?;
    wr(r, l::hubp(p, l::CUR_SETTINGS), SETTINGS, u32::MAX)?;
    let dfields = l::CUR0_MODE | l::CUR0_EXPANSION_MODE | l::CUR0_ROM_EN;
    let d = rd(r, l::dpp(p, l::CNVC_CUR_CONTROL))?;
    wr(r, l::dpp(p, l::CNVC_CUR_CONTROL), (d & !dfields & !l::CUR0_UPDATE_PENDING) | shape.alpha.mode() << l::CUR0_MODE_SHIFT, dfields)?;
    wr(r, l::dpp(p, l::CNVC_CUR_FP_SCALE_BIAS), FP_SCALE_ONE, u32::MAX)
}

/// Both enables (HUBP CURSOR_ENABLE, DPP CUR0_ENABLE), read-modify-write.
fn enable(r: &impl Rw, hw: Hw, on: bool) -> Result<(), CursorError> {
    let set = |v: u32, bit: u32| if on { v | bit } else { v & !bit };
    let c = rd(r, l::hubp(hw.pipe, l::CUR_CONTROL))?;
    wr(r, l::hubp(hw.pipe, l::CUR_CONTROL), set(c, l::CUR_ENABLE), l::CUR_ENABLE)?;
    let d = rd(r, l::dpp(hw.pipe, l::CNVC_CUR_CONTROL))? & !l::CUR0_UPDATE_PENDING;
    wr(r, l::dpp(hw.pipe, l::CNVC_CUR_CONTROL), set(d, l::CUR0_ENABLE), l::CUR0_ENABLE)
}

/// Writes `to` where it differs from `from` (what the registers hold; None: unknown, all written). A move on
/// screen is POSITION (and HOT_SPOT / DST_OFFSET when they change); only a change of visibility touches the
/// control registers. Allocates nothing.
pub fn position(r: &impl Rw, hw: Hw, to: Place, from: Option<Place>) -> Result<(), CursorError> {
    let p = hw.pipe;
    let known = from.filter(|f| f.enable);
    if to.enable {
        if known.is_none_or(|f| f.position != to.position) {
            wr(r, l::hubp(p, l::CUR_POSITION), to.position, u32::MAX)?;
        }
        if known.is_none_or(|f| f.hot_spot != to.hot_spot) {
            wr(r, l::hubp(p, l::CUR_HOT_SPOT), to.hot_spot, u32::MAX)?;
        }
        if known.is_none_or(|f| f.dst_offset != to.dst_offset) {
            wr(r, l::hubp(p, l::CUR_DST_OFFSET), to.dst_offset, u32::MAX)?;
        }
    }
    if from.is_none_or(|f| f.enable != to.enable) {
        enable(r, hw, to.enable)?;
    }
    Ok(())
}

/// Quiesce (Reload/reboot/panic: interrupts off, no allocation, no sleep): both enables off, then the cursor
/// lock released (a writer stopped on another CPU may have left it set). Both are tried; the first error wins.
pub fn off(r: &impl Rw, hw: Hw) -> Result<(), CursorError> {
    let e = enable(r, hw, false);
    let u = wr(r, l::cur_lock(hw.opp), 0, 0);
    e.and(u)
}

/// Whether the pipe's cursor is on (a kernel before this one, or the firmware, left it).
pub fn found_on(r: &impl Regs, pipe: &Pipe) -> bool {
    r.read(l::hubp(u32::from(pipe.hubp), l::CUR_CONTROL)).is_some_and(|c| c & l::CUR_ENABLE != 0)
}

/// The cursor's state, kept under the scanout's register-pair lock (one register writer at a time).
#[derive(Debug, Default)]
pub struct State {
    pub shape: Option<Shape>,
    pub hw: Option<Hw>,
    /// The pointer's position as last asked (plane pixels).
    pub tip: (i32, i32),
    /// Asked to be visible (an image or a move since the last hide).
    pub shown: bool,
    /// What the position registers hold (None: unknown, e.g. after a failed write).
    pub regs: Option<Place>,
    pub dst_q19: u64,
}

impl State {
    pub const fn new() -> State {
        State { shape: None, hw: None, tip: (0, 0), shown: false, regs: None, dst_q19: 0 }
    }

    /// A new image at `addr` (its surface written already), shown with its hotspot on `tip`.
    /// On failure the cursor is turned off (best effort) and forgets its image: the registers may point at
    /// either surface, so `Buf::with_image` counts the new one as fetched and the next image goes to the other.
    pub fn image(&mut self, r: &impl Rw, pipe: &Pipe, addr: u64, shape: Shape, tip: (i32, i32), view: (u32, u32)) -> Result<(), CursorError> {
        let res = self.try_image(r, pipe, addr, shape, tip, view);
        if res.is_err() {
            self.shape = None;
            self.shown = false;
            self.regs = None;
            if let Some(hw) = self.hw {
                if off(r, hw).is_ok() {
                    self.regs = Some(Place::OFF);
                }
            }
        }
        res
    }

    fn try_image(&mut self, r: &impl Rw, pipe: &Pipe, addr: u64, shape: Shape, tip: (i32, i32), view: (u32, u32)) -> Result<(), CursorError> {
        let hw = match self.hw {
            Some(h) => h,
            None => hw(r, pipe)?,
        };
        self.hw = Some(hw);
        self.dst_q19 = factor(r, hw)?;
        let to = place(tip, &shape, view, self.dst_q19);
        self.regs = None;
        locked(r, hw, || {
            attributes(r, hw, addr, &shape)?;
            position(r, hw, to, None)
        })?;
        self.regs = Some(to);
        self.shape = Some(shape);
        self.tip = tip;
        self.shown = true;
        Ok(())
    }

    /// The pointer moved (and is shown). The hot path: no allocation, no log, nothing written when nothing
    /// changes.
    pub fn moved(&mut self, r: &impl Rw, tip: (i32, i32), view: (u32, u32)) -> Result<(), CursorError> {
        let (Some(shape), Some(hw)) = (self.shape, self.hw) else {
            return Err(CursorError::Invalid("no cursor image set"));
        };
        self.tip = tip;
        self.shown = true;
        let to = place(tip, &shape, view, self.dst_q19);
        if self.regs == Some(to) {
            return Ok(());
        }
        let from = self.regs.take();
        locked(r, hw, || position(r, hw, to, from))?;
        self.regs = Some(to);
        Ok(())
    }

    /// Hidden until the next image or move.
    pub fn hide(&mut self, r: &impl Rw) -> Result<(), CursorError> {
        self.shown = false;
        let Some(hw) = self.hw else { return Ok(()) };
        if self.regs.is_some_and(|p| !p.enable) {
            return Ok(());
        }
        let from = self.regs.take();
        locked(r, hw, || position(r, hw, Place::OFF, from))?;
        self.regs = Some(Place::OFF);
        Ok(())
    }

    /// After a mode switch (GX-4): the new plane size clips, the scaler's new ratio sets the fetch offset.
    pub fn follow(&mut self, r: &impl Rw, view: (u32, u32)) -> Result<(), CursorError> {
        let (Some(shape), Some(hw)) = (self.shape, self.hw) else { return Ok(()) };
        self.dst_q19 = factor(r, hw)?;
        if !self.shown {
            return Ok(());
        }
        let to = place(self.tip, &shape, view, self.dst_q19);
        self.regs = None;
        locked(r, hw, || position(r, hw, to, None))?;
        self.regs = Some(to);
        Ok(())
    }
}

/// The two cursor surfaces in graphics memory (a VRAM BO the HUBP fetches like the scanout buffers: never
/// system RAM, CONTRACTS 6b) and the CPU's write window onto them.
pub struct Buf {
    target: Arc<dyn FbTarget>,
    gpu: u64,
    /// The surface the registers point at (0 or 1).
    shown: AtomicU8,
    /// An image is being written (image updates are rare; a second one at the same time is `Busy`).
    busy: AtomicBool,
}

/// The `busy` flag, given back on every path.
struct Claim<'a>(&'a AtomicBool);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Buf {
    /// `target` writes the BO whose GPU address is `gpu`; None if it holds less than `BYTES`.
    pub fn new(target: Arc<dyn FbTarget>, gpu: u64) -> Option<Buf> {
        (target.len() as u64 >= BYTES).then(|| Buf { target, gpu, shown: AtomicU8::new(1), busy: AtomicBool::new(false) })
    }

    /// Writes `px` (w x h of `shape`) into the surface not shown, padded to its pitch with transparent pixels,
    /// and runs `f` with that surface's GPU address (the register writes). The surface counts as shown after
    /// `f` either way: a failed `f` may have moved the address already (it then turns the cursor off), so the
    /// next image must not be written into the surface the hardware may fetch.
    pub fn with_image<T>(&self, shape: &Shape, px: &[u32], f: impl FnOnce(u64) -> Result<T, CursorError>) -> Result<T, CursorError> {
        if px.len() != (shape.w * shape.h) as usize {
            return Err(CursorError::Invalid("pixel count is not width x height"));
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(CursorError::Busy);
        }
        let _claim = Claim(&self.busy);
        let slot = 1 - (self.shown.load(Ordering::Acquire) & 1);
        let pitch = shape.pitch();
        let base = slot as usize * SLOT_BYTES as usize;
        let mut row = alloc::vec![0u32; pitch as usize];
        for y in 0..pitch {
            surface_row(shape, px, y, &mut row);
            if !self.target.write_pixels(base + (y * pitch * 4) as usize, &row) {
                return Err(CursorError::NoDevice("cursor surface does not fit its window"));
            }
        }
        self.target.flush();
        let v = f(self.gpu + u64::from(slot) * SLOT_BYTES);
        self.shown.store(slot, Ordering::Release);
        v
    }

    pub fn gpu(&self) -> u64 {
        self.gpu
    }
}

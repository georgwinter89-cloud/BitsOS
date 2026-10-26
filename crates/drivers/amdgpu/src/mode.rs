//! GX-4 (spec/gpu.md §3.2, D3; V1 G-3c, `G/plan/G3-PLAN.md` §3): the register side of a mode switch at runtime,
//! pure over a register owner (`flip::Rw`) so the host tests run it against the fake engine (`tests::mode`).
//! `switch` holds the state (confirmation, rollback), `live` the kernel glue.
//!
//! **What changes (G-3c R-min, scaled).** The scanned surface: its viewport (PRI/SEC_VIEWPORT_DIMENSION) becomes
//! the new size inside the unchanged GOP framebuffer (same pitch, same A0/A1 addresses, so no buffer moves), and
//! the DPP scaler stretches it to the OTG's active area with AUTOCAL_MODE = AUTOSCALE ("Autocal calculate the
//! scaling ratio and initial phase and the DSCL_MODE_SEL must be set to 1",
//! linux-6.6/drivers/gpu/drm/amd/display/dc/dcn10/dcn10_dpp_dscl.c:62-77). The OTG timing, the DP link, the DTO
//! and the stream stay as the firmware lit them: the monitor keeps its native timing and rate, so a mode is any
//! size the monitor offers up to the link's active area, at the link's rate (a new timing needs the "new timing,
//! same link" sequence of `G/doku/14-modeset.md` §14.2, unmeasured: open question GX-4 Q1).
//!
//! When the GOP left the scaler in bypass (the native mode at 1:1), the switch first writes V1's measured
//! scaling state of this very pipe (the GOP's own 1024x768 -> 2560x1440 programming, G3-PLAN §3.0 A7 and §3.6
//! B1-B5: DSCL_MODE 1, LB_MEMORY_CTRL 0x10103F00, DSCL_CONTROL 1, AUTOCAL 1); going back to the native size
//! writes the found values back in V1's W order (§3.3 W1, W2, W6, W8, W9, W11).
//!
//! GX-4b: the Mini-PC's GOP leaves the native mode in a third state, manual 1:1 (AUTOCAL off, DSCL_MODE 1,
//! ratios 1.0, one tap; plan/hw/dcn-ref-native-2026-10-22.json). The way to a smaller size is the same AUTOSCALE
//! sequence (DSCL_MODE is already 1, the line buffer keeps its found partitions); the way back writes the found
//! AUTOCAL, DSCL_CONTROL, SCL_MODE, LB and then the found ratios, phases and taps (Linux's manual order,
//! dcn10_dpp_dscl.c:613-690). Never written: pitch, address, DCHUBP_CNTL, DLG/TTU/RQ, coefficients,
//! MPC/OPP/OTG/DCCG (§3.3 "Not written"); ratios and taps only back to their found values.

use crate::attach::{Pipe, Regs};
use crate::flip::{self, FlipError, Rw};
use crate::layout as l;
use alloc::string::String;
use alloc::vec::Vec;

/// A screen size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

impl Size {
    pub const fn new(w: u32, h: u32) -> Size {
        Size { w, h }
    }
    /// The viewport/recout layout: height [29:16], width [13:0].
    pub const fn dim(self) -> u32 {
        (self.h & 0x3FFF) << 16 | (self.w & 0x3FFF)
    }
    pub const fn of_dim(v: u32) -> Size {
        Size { w: v & 0x3FFF, h: (v >> 16) & 0x3FFF }
    }
    pub fn fits_in(self, o: Size) -> bool {
        self.w <= o.w && self.h <= o.h
    }
}

impl core::fmt::Display for Size {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}x{}", self.w, self.h)
    }
}

/// The smallest mode: the console needs a text grid and the scaler a sane ratio. A bound on input, not a count.
pub const MIN: Size = Size::new(320, 200);
/// The viewport fields hold 14 bits: the largest size a request may name (input bound).
pub const MAX_DIM: u32 = 0x3FFF;
/// A requested rate within this of the link's counts as the link's (60 asks for 59.950).
pub const RATE_SLACK_MHZ: u32 = 500;

/// A request "WxH" or "WxH@R" (R in Hz, e.g. 60 or 59.95).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub size: Size,
    pub refresh_mhz: Option<u32>,
}

/// Parses "WxH[@R]"; never panics on any input.
pub fn parse(s: &str) -> Result<Request, String> {
    let s = s.trim();
    let (dims, rate) = match s.split_once('@') {
        Some((d, r)) => (d, Some(r)),
        None => (s, None),
    };
    let (w, h) = dims.split_once(['x', 'X']).ok_or_else(|| alloc::format!("{s:?}: want WxH or WxH@R, e.g. 1920x1080@60"))?;
    let num = |t: &str| -> Option<u32> {
        let t = t.trim();
        (!t.is_empty() && t.len() <= 5 && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse().ok()).flatten()
    };
    let (Some(w), Some(h)) = (num(w), num(h)) else {
        return Err(alloc::format!("{s:?}: width and height are whole numbers"));
    };
    if w > MAX_DIM || h > MAX_DIM || w < MIN.w || h < MIN.h {
        return Err(alloc::format!("{w}x{h}: between {MIN} and {MAX_DIM}x{MAX_DIM}"));
    }
    let refresh_mhz = match rate {
        None => None,
        Some(r) => Some(parse_rate_mhz(r).ok_or_else(|| alloc::format!("{r:?}: a rate in Hz between 1 and 1000, e.g. 60 or 59.95"))?),
    };
    Ok(Request { size: Size::new(w, h), refresh_mhz })
}

/// "60", "59.95", "59.940" -> millihertz (1..=1000 Hz, at most three decimals).
pub fn parse_rate_mhz(r: &str) -> Option<u32> {
    let r = r.trim().trim_end_matches("Hz").trim_end_matches("hz").trim();
    let (int, frac) = r.split_once('.').unwrap_or((r, ""));
    if int.is_empty() || int.len() > 4 || frac.len() > 3 || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let i: u32 = int.parse().ok()?;
    let mut f: u32 = if frac.is_empty() { 0 } else { frac.parse().ok()? };
    for _ in frac.len()..3 {
        f *= 10;
    }
    let mhz = i.checked_mul(1000)?.checked_add(f)?;
    (1000..=1_000_000).contains(&mhz).then_some(mhz)
}

/// "59.950" from millihertz.
pub fn rate_text(mhz: u32) -> String {
    alloc::format!("{}.{:03}", mhz / 1000, mhz % 1000)
}

/// Where the list of modes came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The monitor's EDID (BootInfo tag 21): its detailed, standard and established timings.
    Edid,
    /// The GOP's mode list (BootInfo MODE_LIST): sizes only.
    Gop,
    /// Neither: only the framebuffer's own size.
    Link,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Edid => "edid",
            Source::Gop => "gop",
            Source::Link => "link",
        }
    }
}

/// One size the monitor (or the GOP) offers, with its rates (mHz; empty = not known, the GOP list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offered {
    pub size: Size,
    pub rates_mhz: Vec<u32>,
}

/// The modes on offer: the EDID's progressive detailed, standard and established timings when it has any (Linux's
/// order), else the GOP's list, else `home` (the GOP framebuffer's size) alone; `home` is always in it. Largest
/// first, each size once.
pub fn offered(edid: Option<&[u8]>, gop: &[Size], home: Size) -> (Vec<Offered>, Source) {
    let mut v: Vec<Offered> = Vec::new();
    let add = |v: &mut Vec<Offered>, s: Size, rate: Option<u32>| {
        let i = match v.iter().position(|o| o.size == s) {
            Some(i) => i,
            None => {
                v.push(Offered { size: s, rates_mhz: Vec::new() });
                v.len() - 1
            }
        };
        if let Some(r) = rate
            && !v[i].rates_mhz.contains(&r)
        {
            v[i].rates_mhz.push(r);
        }
    };
    let mut source = Source::Link;
    if let Some(e) = edid
        && let Ok(ts) = display::edid::timings(e)
    {
        for t in ts.filter(|t| !t.interlaced) {
            add(&mut v, Size::new(t.hactive, t.vactive), Some(t.refresh_mhz()));
            source = Source::Edid;
        }
        for (s, mhz) in crate::edid_modes::extra(e) {
            add(&mut v, s, Some(mhz));
            source = Source::Edid;
        }
    }
    if source == Source::Link {
        for &s in gop.iter().filter(|s| s.w > 0 && s.h > 0) {
            add(&mut v, s, None);
            source = Source::Gop;
        }
    }
    if !v.iter().any(|o| o.size == home) {
        add(&mut v, home, None);
    }
    v.sort_by(|a, b| (b.size.w * b.size.h, b.size.w).cmp(&(a.size.w * a.size.h, a.size.w)));
    (v, source)
}

/// Why the mode list is not the EDID's (`offered` fell back to the GOP's list or the link): for the list text and
/// the `mode.list` event, so a missing tag 21 (a bitsboot from before BT-edid) and a refused EDID read differently.
pub fn edid_note(edid: Option<&[u8]>) -> &'static str {
    match edid.map(display::edid::timings) {
        None => "no EDID from the loader (bitsboot before BT-edid, or the firmware has none)",
        Some(Err(_)) => "EDID refused (short, bad header or base block checksum)",
        Some(Ok(_)) => "EDID without progressive timings",
    }
}

/// The header line of `display_mode {}`'s list, from the same EDID `offered` used: "modes (edid):", or the
/// fallback's source and why the EDID gave no list.
pub fn list_header(src: Source, edid: Option<&[u8]>) -> String {
    match src {
        Source::Edid => String::from("modes (edid):"),
        _ => alloc::format!("modes ({}; {}):", src.name(), edid_note(edid)),
    }
}

/// The size a round-trip check switches to: the largest offered size below `home` (at least `MIN`), one with the
/// link's aspect ratio first (shown unstretched), so a 16:9 link tries 1920x1080 before a 4:3 1920x1440.
pub fn trial(list: &[Offered], home: Size, link: Size) -> Option<Size> {
    let fit = |z: &Size| *z != home && z.fits_in(home) && z.w >= MIN.w && z.h >= MIN.h;
    let same = |z: &Size| u64::from(z.w) * u64::from(link.h) == u64::from(z.h) * u64::from(link.w);
    let mut sizes = list.iter().map(|o| o.size).filter(fit);
    let first = sizes.clone().next();
    sizes.find(same).or(first)
}

/// Why a request names no mode the switch can show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unoffered {
    /// Larger than the GOP framebuffer (the scaled picture is a part of it, same pitch) or the link.
    Larger { max: Size },
    /// Smaller than `MIN`.
    Smaller,
    /// Not in the monitor's (or GOP's) list.
    NotOffered,
    /// Another rate than the link runs: needs a new OTG timing (not built, open question GX-4 Q1).
    Rate { asked_mhz: u32, link_mhz: u32 },
}

impl core::fmt::Display for Unoffered {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unoffered::Larger { max } => write!(f, "larger than {max} (the framebuffer the firmware set up; the scaler only stretches a smaller picture)"),
            Unoffered::Smaller => write!(f, "smaller than {MIN}"),
            Unoffered::NotOffered => write!(f, "not a mode the monitor offers (see the list)"),
            Unoffered::Rate { asked_mhz, link_mhz } => write!(
                f,
                "{} Hz: the link runs {} Hz and a new rate needs a new link timing (not built yet)",
                rate_text(*asked_mhz),
                rate_text(*link_mhz)
            ),
        }
    }
}

/// The size to show for `req`, if the switch can show it at the link's timing: at most `home` (the GOP
/// framebuffer's size, within the link's active area).
pub fn choose(req: &Request, offered: &[Offered], home: Size, link_mhz: u32) -> Result<Size, Unoffered> {
    let s = req.size;
    if !s.fits_in(home) {
        return Err(Unoffered::Larger { max: home });
    }
    if s.w < MIN.w || s.h < MIN.h {
        return Err(Unoffered::Smaller);
    }
    if let Some(r) = req.refresh_mhz
        && link_mhz != 0
        && r.abs_diff(link_mhz) > RATE_SLACK_MHZ
    {
        return Err(Unoffered::Rate { asked_mhz: r, link_mhz });
    }
    if s != home && !offered.iter().any(|o| o.size == s) {
        return Err(Unoffered::NotOffered);
    }
    Ok(s)
}

/// The viewport and scaler registers of one pipe (HUBP n, DPP n).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PipeRegs {
    pub pri_vp: u32,
    pub sec_vp: u32,
    pub pri_start: u32,
    pub sec_start: u32,
    pub autocal: u32,
    pub dscl_control: u32,
    pub scl_mode: u32,
    pub lb_mem: u32,
    pub recout: u32,
    pub mpc: u32,
    /// The manual ratios, initial phases and taps (`l::SCL_FILTER` order).
    pub filter: [u32; FILTER],
}

const FILTER: usize = l::SCL_FILTER.len();

/// The fields the switch writes, in one fixed order (Quiesce stores the found ones in atomics).
pub const SAVED: usize = 6 + FILTER;

/// Per `to_saved` entry, the bits `plan` checks after writing it (the viewports whole).
pub const SAVED_CHECKED: [u32; SAVED] = {
    let mut m = [u32::MAX; SAVED];
    m[2] = l::AUTOCAL_FIELDS;
    m[3] = l::SCL_BOUNDARY_MODE;
    m[4] = l::DSCL_MODE;
    m[5] = LB_CHECK;
    let mut i = 0;
    while i < FILTER {
        m[6 + i] = l::SCL_FILTER[i].1;
        i += 1;
    }
    m
};

impl PipeRegs {
    /// AUTOCAL_MODE AUTOSCALE with DSCL_MODE 1 (444 RGB scaling).
    pub fn scaling(&self) -> bool {
        self.autocal & l::AUTOCAL_MODE == 1 && self.scl_mode & l::DSCL_MODE == 1
    }
    /// AUTOCAL off, DSCL_MODE 0 (444 bypass): the native mode at 1:1.
    pub fn bypass(&self) -> bool {
        self.autocal & l::AUTOCAL_MODE == 0 && self.scl_mode & l::DSCL_MODE == 0
    }
    /// AUTOCAL off, DSCL_MODE 1 (444 RGB scaling) with manual luma ratios of exactly 1.0: the native mode at 1:1
    /// as the Mini-PC's GOP leaves it (plan/hw/dcn-ref-native-2026-10-22.json). Linux picks 444_BYPASS for all
    /// ratios 1.0 and 444_RGB_ENABLE only with `debug.always_scale` (dcn10_dpp_dscl.c:123-155): this is that form.
    pub fn manual(&self) -> bool {
        let ratio = |i: usize| self.filter[i] & l::SCL_FILTER[i].1;
        self.autocal & l::AUTOCAL_MODE == 0
            && self.scl_mode & l::DSCL_MODE == 1
            && ratio(0) == l::SCALE_ONE
            && ratio(1) == l::SCALE_ONE
    }
    /// One of the three scaler states the switch knows as a boot state (its way back): bypass, AUTOSCALE or
    /// manual 1:1. `gates` and gx.mode.gates both ask this (GX-4c: the ktest asked only the first two and failed
    /// on the Mini-PC's fresh-boot manual state, which the switch handles).
    pub fn known(&self) -> bool {
        self.scaling() || self.bypass() || self.manual()
    }
    /// True when every bit the switch checks after its writes (`SAVED_CHECKED`) equals `o`'s: the way back
    /// restored the found state even if the hardware recomputed an unchecked bit (LB NUM_PARTITIONS).
    pub fn same_checked(&self, o: &PipeRegs) -> bool {
        let (a, b) = (self.to_saved(), o.to_saved());
        (0..SAVED).all(|i| a[i] & SAVED_CHECKED[i] == b[i] & SAVED_CHECKED[i])
    }
    pub fn to_saved(&self) -> [u32; SAVED] {
        let head = [self.pri_vp, self.sec_vp, self.autocal, self.dscl_control, self.scl_mode, self.lb_mem];
        core::array::from_fn(|i| if i < head.len() { head[i] } else { self.filter[i - head.len()] })
    }
    pub fn from_saved(v: [u32; SAVED]) -> PipeRegs {
        PipeRegs {
            pri_vp: v[0],
            sec_vp: v[1],
            autocal: v[2],
            dscl_control: v[3],
            scl_mode: v[4],
            lb_mem: v[5],
            filter: core::array::from_fn(|i| v[6 + i]),
            ..PipeRegs::default()
        }
    }
}

fn rd(r: &impl Regs, b: u32) -> Result<u32, FlipError> {
    r.read(b).ok_or(FlipError::Read(b))
}

/// Reads pipe `p`'s viewport and scaler registers.
pub fn read(r: &impl Regs, p: &Pipe) -> Result<PipeRegs, FlipError> {
    let (h, d) = (u32::from(p.hubp), u32::from(p.hubp));
    Ok(PipeRegs {
        pri_vp: rd(r, l::hubp(h, l::HUBP_VIEWPORT))?,
        sec_vp: rd(r, l::hubp(h, l::HUBP_SEC_VIEWPORT))?,
        pri_start: rd(r, l::hubp(h, l::HUBP_VIEWPORT_START))?,
        sec_start: rd(r, l::hubp(h, l::HUBP_SEC_VIEWPORT_START))?,
        autocal: rd(r, l::dpp(d, l::DSCL_AUTOCAL))?,
        dscl_control: rd(r, l::dpp(d, l::DSCL_CONTROL))?,
        scl_mode: rd(r, l::dpp(d, l::SCL_MODE))?,
        lb_mem: rd(r, l::dpp(d, l::LB_MEMORY_CTRL))?,
        recout: rd(r, l::dpp(d, l::RECOUT_SIZE))?,
        mpc: rd(r, l::dpp(d, l::MPC_SIZE))?,
        filter: {
            let mut f = [0; FILTER];
            for (v, &(reg, _)) in f.iter_mut().zip(&l::SCL_FILTER) {
                *v = rd(r, l::dpp(d, reg))?;
            }
            f
        },
    })
}

/// Why the gates refused a switch (reads only; nothing was written).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// The switch handles one pipe (V1 A1: one pipe in the table).
    Pipes(usize),
    Read(u32),
    /// A viewport starts elsewhere than 0,0 (V1 A5).
    ViewportStart { pri: u32, sec: u32 },
    /// Before the first switch the viewport is not the GOP framebuffer's size (V1 A5: a state left by a kernel
    /// that switched and did not put it back; its way back is not known).
    Viewport { pri: Size, sec: Size, home: Size },
    /// The scaler's output (RECOUT, MPC_SIZE) is not the link's active area (V1 A8).
    Output { recout: Size, mpc: Size, link: Size },
    /// The OTG's master update lock is held (V1 A12).
    Locked(u32),
    /// A flip is pending (V1 A13).
    FlipPending,
    /// The scaler is neither in bypass, nor in AUTOSCALE, nor manual at 1:1 (programmed some way this switch does
    /// not know).
    Scaler { autocal: u32, scl_mode: u32, h_ratio: u32, v_ratio: u32 },
    /// The scaler must be switched on but its LUT memory is powered down (V1 A9, B15).
    ScalerMemory(u32),
}

impl core::fmt::Display for Gate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Gate::Pipes(n) => write!(f, "{n} pipes show the picture (the switch handles one)"),
            Gate::Read(b) => write!(f, "register {b:#07x} unreadable"),
            Gate::ViewportStart { pri, sec } => write!(f, "viewport starts {pri:#x}/{sec:#x}, not 0"),
            Gate::Viewport { pri, sec, home } => write!(f, "viewport {pri} (stereo copy {sec}) is not the GOP framebuffer's {home}"),
            Gate::Output { recout, mpc, link } => write!(f, "scaler output {recout} (MPC {mpc}) is not the link's {link}"),
            Gate::Locked(v) => write!(f, "OTG master update lock held ({v:#x})"),
            Gate::FlipPending => write!(f, "a flip is pending"),
            Gate::Scaler { autocal, scl_mode, h_ratio, v_ratio } => write!(
                f,
                "scaler programmed in an unknown way (AUTOCAL {autocal:#x}, SCL_MODE {scl_mode:#x}, ratio H {h_ratio:#x} V {v_ratio:#x})"
            ),
            Gate::ScalerMemory(v) => write!(f, "scaler memory powered down (DSCL_MEM_PWR_STATUS {v:#x})"),
        }
    }
}

impl From<FlipError> for Gate {
    fn from(e: FlipError) -> Gate {
        match e {
            FlipError::Read(b) | FlipError::Write(b) => Gate::Read(b),
            FlipError::LockStuck(k) => Gate::Locked(u32::from(k)),
        }
    }
}

/// The gates before a switch (reads only): `link` is the OTG's active area, `home` the GOP framebuffer's size.
/// `first`: the state found is the boot state the switch keeps as its way back, so it must be one this switch
/// knows (the GOP's viewport, the scaler in bypass, AUTOSCALE or manual 1:1).
pub fn gates(r: &impl Regs, pipes: &[Pipe], link: Size, home: Size, first: bool) -> Result<PipeRegs, Gate> {
    let [p] = pipes else {
        return Err(Gate::Pipes(pipes.len()));
    };
    let found = read(r, p)?;
    if found.pri_start != 0 || found.sec_start != 0 {
        return Err(Gate::ViewportStart { pri: found.pri_start, sec: found.sec_start });
    }
    let (recout, mpc) = (Size::of_dim(found.recout), Size::of_dim(found.mpc));
    if recout != link || mpc != link {
        return Err(Gate::Output { recout, mpc, link });
    }
    let lock = rd(r, l::otg(u32::from(p.otg), l::OTG_LOCK))?;
    if lock & l::LOCK_HELD != 0 {
        return Err(Gate::Locked(lock));
    }
    if rd(r, l::hubp(u32::from(p.hubp), l::HUBP_FLIP))? & l::FLIP_PENDING != 0 {
        return Err(Gate::FlipPending);
    }
    if first {
        let (pri, sec) = (Size::of_dim(found.pri_vp), Size::of_dim(found.sec_vp));
        if pri != home || sec != home {
            return Err(Gate::Viewport { pri, sec, home });
        }
        if !found.known() {
            let (h_ratio, v_ratio) = (found.filter[0], found.filter[1]);
            return Err(Gate::Scaler { autocal: found.autocal, scl_mode: found.scl_mode, h_ratio, v_ratio });
        }
        if !found.scaling() {
            let pwr = rd(r, l::dpp(u32::from(p.hubp), l::DSCL_MEM_PWR_STATUS))?;
            if pwr & l::LUT_MEM_PWR_STATE != 0 {
                return Err(Gate::ScalerMemory(pwr));
            }
        }
    }
    Ok(found)
}

/// One register write of a plan: the value and the bits the read-back must match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct W {
    pub reg: u32,
    pub value: u32,
    pub check: u32,
}

/// The longest plan: back to native from the manual state (four scaler writes, the filter, two viewports). A
/// fixed register sequence, not a count of things.
pub const PLAN_MAX: usize = 4 + FILTER + 2;

/// The writes of one switch, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    w: [W; PLAN_MAX],
    n: usize,
}

impl Plan {
    fn new() -> Plan {
        Plan { w: [W::default(); PLAN_MAX], n: 0 }
    }
    fn push(&mut self, reg: u32, value: u32, check: u32) {
        if let Some(slot) = self.w.get_mut(self.n) {
            *slot = W { reg, value, check };
            self.n += 1;
        }
    }
    pub fn writes(&self) -> &[W] {
        &self.w[..self.n]
    }
}

/// V1's measured LB_MEMORY_CTRL of this pipe while the GOP scaled (G3-PLAN §3.0 A7); NUM_PARTITIONS [30:16] may be
/// computed by the hardware, so only [15:0] (MEMORY_CONFIG, LB_MAX_PARTITIONS) is compared (§3.3 W8).
pub const LB_MEMORY_SCALED: u32 = 0x1010_3F00;
const LB_CHECK: u32 = 0xFFFF;

/// The writes that make pipe `hubp` show `to` (a size within `home`, the GOP framebuffer's), from the boot
/// state `boot` the first switch found. Allocates nothing (Quiesce runs the plan back to `home` with interrupts
/// off).
pub fn plan(boot: &PipeRegs, hubp: u8, home: Size, to: Size) -> Plan {
    let (h, d) = (u32::from(hubp), u32::from(hubp));
    let mut p = Plan::new();
    let native = to == home;
    let (pri, sec) = if native { (boot.pri_vp, boot.sec_vp) } else { (to.dim(), to.dim()) };
    if !boot.scaling() {
        if native {
            // V1 §3.3 order: AUTOCAL off first, boundary mode, bypass, LB as found.
            p.push(l::dpp(d, l::DSCL_AUTOCAL), boot.autocal, l::AUTOCAL_FIELDS);
            p.push(l::dpp(d, l::DSCL_CONTROL), boot.dscl_control, l::SCL_BOUNDARY_MODE);
            p.push(l::dpp(d, l::SCL_MODE), boot.scl_mode, l::DSCL_MODE);
            p.push(l::dpp(d, l::LB_MEMORY_CTRL), boot.lb_mem, LB_CHECK);
            if boot.manual() {
                // The manual 1:1 state: its ratios, phases and taps as found, in Linux's order after the line
                // buffer (dpp1_dscl_set_scaler_manual_scale, dcn10_dpp_dscl.c:667-690). AUTOSCALE may have left
                // its own values in them (unknown), so they are written back, not assumed.
                for (&v, &(reg, bits)) in boot.filter.iter().zip(&l::SCL_FILTER) {
                    p.push(l::dpp(d, reg), v, bits);
                }
            }
        } else {
            // V1 §3.6 order (the way to the GOP's scaled state): mode, line buffer, boundary, AUTOCAL last. From the
            // manual state the line buffer already serves the scaler (DSCL_MODE 1): its found partition counts stay.
            let lb = if boot.manual() { (boot.lb_mem & !LB_CHECK) | (LB_MEMORY_SCALED & LB_CHECK) } else { LB_MEMORY_SCALED };
            p.push(l::dpp(d, l::SCL_MODE), (boot.scl_mode & !l::DSCL_MODE) | 1, l::DSCL_MODE);
            p.push(l::dpp(d, l::LB_MEMORY_CTRL), lb, LB_CHECK);
            p.push(l::dpp(d, l::DSCL_CONTROL), (boot.dscl_control & !l::SCL_BOUNDARY_MODE) | 1, l::SCL_BOUNDARY_MODE);
            p.push(l::dpp(d, l::DSCL_AUTOCAL), (boot.autocal & !l::AUTOCAL_MODE) | 1, l::AUTOCAL_FIELDS);
        }
    }
    // DIMENSION only (the starts stay 0, gate): primary, then the stereo copy the GOP keeps equal (§3.3 W9, W11).
    p.push(l::hubp(h, l::HUBP_VIEWPORT), pri, l::VIEWPORT_DIM);
    p.push(l::hubp(h, l::HUBP_SEC_VIEWPORT), sec, l::VIEWPORT_DIM);
    p
}

/// Writes `plan` on pipe `p` under its OTG's master update lock (optc3_lock, linux-6.6/drivers/gpu/drm/amd/display/
/// dc/dcn30/dcn30_optc.c:119); the unlock runs on every path (X1), the first error is returned. True when the
/// lock status was seen. No allocation, no sleep (Quiesce).
pub fn apply(r: &impl Rw, p: &Pipe, plan: &Plan) -> Result<bool, FlipError> {
    let k = u32::from(p.otg);
    let res = flip::lock(r, k).and_then(|seen| {
        for w in plan.writes() {
            r.write(w.reg, w.value, w.check)?;
        }
        Ok(seen)
    });
    let un = flip::unlock(r, k);
    let seen = res?;
    un.map(|()| seen)
}

/// The update has not taken effect yet: the scaler's double-buffered update or the OTG's register update is
/// still pending (V1 §3.4 step 5: DSCL_UPDATE bit 0, OTG_PIPE_UPDATE_STATUS).
pub fn pending(r: &impl Regs, p: &Pipe) -> Result<bool, FlipError> {
    let scl = rd(r, l::dpp(u32::from(p.hubp), l::DSCL_UPDATE))? & l::SCL_UPDATE_PENDING != 0;
    let otg = rd(r, l::otg(u32::from(p.otg), l::OTG_PIPE_UPDATE_STATUS))? & l::OTG_DC_REG_UPDATE_PENDING != 0;
    Ok(scl || otg)
}

/// Every write of `plan` reads back (in its check bits); Err(offset) of the first that does not (V1 §3.4 step 6).
pub fn verify(r: &impl Regs, plan: &Plan) -> Result<(), u32> {
    for w in plan.writes() {
        match r.read(w.reg) {
            Some(v) if (v ^ w.value) & w.check == 0 => {}
            _ => return Err(w.reg),
        }
    }
    Ok(())
}

/// HUBP_UNDERFLOW_STATUS of pipe `p` (sticky; read only, never cleared by the switch).
pub fn underflow(r: &impl Regs, p: &Pipe) -> Result<u32, FlipError> {
    Ok(rd(r, l::hubp(u32::from(p.hubp), l::HUBP_CNTL))? & l::HUBP_UNDERFLOW_STATUS)
}

/// The size pipe `p` shows now (its primary viewport).
pub fn shown(r: &impl Regs, p: &Pipe) -> Result<Size, FlipError> {
    Ok(Size::of_dim(rd(r, l::hubp(u32::from(p.hubp), l::HUBP_VIEWPORT))?))
}

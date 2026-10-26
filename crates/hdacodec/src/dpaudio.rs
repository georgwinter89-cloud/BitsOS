//! The display side of HDMI audio on the Mini-PC (SN-5): DIG0 of the Radeon 780M runs DP SST into the on-board
//! DP->HDMI converter, so the samples of AZ endpoint 0 (pin 3 of the AA01 codec) only reach the monitor once the
//! DCN 3.1.4 display engine sends DP audio secondary packets. Ported 1:1 from V1 (`BitsOS-ASM/snd/dpaudio.asm`,
//! `dpaudreg.asm`, `dpaudtab.asm`, which are the DP audio kit of 04.10.2026, `werkzeuge/plattform/dpaudio`,
//! DP-AUDIO.md sections 4 and 8): 14 read-only gates before the first write (any failing: nothing written), 48
//! writes in Linux 6.6's order for a firmware-lit DP display (`link_dpms.c` enable_audio_packets ->
//! `dce_aud_wall_dto_setup`, `afmt3_se_audio_setup`, `dce_aud_az_configure`, `dce_aud_az_enable`,
//! `enc3_se_dp_audio_enable`, `afmt3_audio_mute_control`), each read back (a surprise: stop and undo at once),
//! and the 41-write undo with the firmware's values (saved-5.json) in Linux's off order. What changed against
//! V1: the sink's endpoint values come from the monitor's EDID ([`crate::eld`]) instead of the hard-coded iiyama.
//!
//! Nothing here touches hardware: registers come through [`Regs`] (the kernel's `amdgpu::dcn::DcnRegs`, a register
//! image in host tests). Never written: OTG, HUBP, DPP, MPC, OPP, the DIG back end, link, PHY, pixel clock.

use crate::eld::AzConfig;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// `AZF0ENDPOINT0_AZALIA_F0_CODEC_ENDPOINT_INDEX` / `_DATA` (seg2 0x34C0 + 0x386/0x387).
pub const EP_INDEX: u32 = 0xE118;
pub const EP_DATA: u32 = 0xE11C;
/// `AZALIA_ENDPOINT_REG_INDEX`: the index register's bits that select the endpoint register.
pub const EP_INDEX_BITS: u32 = 0x3FFF;

/// A register: a BAR5 byte offset, or an index of endpoint 0 behind `EP_INDEX`/`EP_DATA`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Loc {
    Bar(u32),
    Ep(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegError {
    /// No display engine (QEMU, no GPU attach).
    NotAttached,
    OutOfRange,
    /// The write happened but read back as `read` (the owner's full compare; the engine compares by its mask).
    ReadBack { read: u32 },
    /// The endpoint index/data pair is held by someone else and the caller must not wait (Quiesce): the endpoint
    /// register was not touched.
    PairBusy,
}

/// Register access as the display owner gives it.
pub trait Regs {
    fn read(&mut self, byte: u32) -> Result<u32, RegError>;
    fn write(&mut self, byte: u32, v: u32) -> Result<(), RegError>;
    /// Runs `f` with the endpoint index/data pair held: nothing else may use the pair in between. A caller that must
    /// not wait answers `PairBusy` without running `f` when the pair is taken.
    fn with_pair<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, RegError>) -> Result<T, RegError>
    where
        Self: Sized,
    {
        f(self)
    }
}

/// A read-only check: `(value & mask) == want`, or `!=` with `differ`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gate {
    pub loc: Loc,
    pub mask: u32,
    pub want: u32,
    pub differ: bool,
    pub name: &'static str,
}

const fn gate(loc: Loc, mask: u32, want: u32, differ: bool, name: &'static str) -> Gate {
    Gate { loc, mask, want, differ, name }
}

/// In this order. The first two confirm the segment bases (the undo needs only them). The 14th is the kernel's
/// own: the undo writes DP_SEC_CNTL 0 whole, which must take away only what `on` added.
pub const GATES: [Gate; 14] = [
    gate(Loc::Bar(0xE318), !0, 0x1002AA01, false, "codec_id 1002AA01: segment 2 base"),
    gate(Loc::Bar(0x508), !0, 600_000_000, false, "DP_DTO0_MODULO 600 MHz: segment 1 base"),
    gate(Loc::Bar(0x504), !0, 0, true, "DP_DTO0_PHASE set: segment 1 base"),
    gate(Loc::Bar(0x500), 0x90, 0x90, false, "OTG0 pixel clock from the DP DTO: segment 1 base"),
    gate(Loc::Bar(0x155C8), 1, 1, false, "DIG0 back end on"),
    gate(Loc::Bar(0x155C4), 0x70100, 0x100, false, "DIG0 in DP SST mode from front end A"),
    gate(Loc::Bar(0x1552C), 0x1000007, 0x1000000, false, "front end A from OTG0, its clock on"),
    gate(Loc::Bar(0x15720), 0x10, 0x10, false, "DP0 link trained"),
    gate(Loc::Bar(0x15730), 1, 1, false, "DP0 video stream on"),
    gate(Loc::Bar(0x14004), 1, 1, false, "OTG0 running"),
    gate(Loc::Bar(0xE32C), 0xFFF, 0x70, false, "AZ rates 32/44.1/48 kHz"),
    gate(Loc::Bar(0xE334), 0xC000_0000, 0xC000_0000, false, "AZ CLKSTOP and EPSS"),
    gate(Loc::Ep(0x56), 0xC000_0000, 0x4000_0000, true, "endpoint 0 has a port"),
    gate(Loc::Bar(0x157AC), 0x1FF0_0000, 0, false, "DP0 sends no other secondary packets"),
];

/// One write: `(old & and) | or`; a read-back differing in a bit of `check` is a surprise (check leaves out
/// status and strobe bits). Stage 0 is the undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    pub stage: u8,
    pub loc: Loc,
    pub and: u32,
    pub or: u32,
    pub check: u32,
}

const fn w(stage: u8, loc: Loc, and: u32, or: u32, check: u32) -> Write {
    Write { stage, loc, and, or, check }
}
const fn b(x: u32) -> Loc {
    Loc::Bar(x)
}
const fn e(x: u32) -> Loc {
    Loc::Ep(x)
}

pub const ON_WRITES: usize = 48;
pub const OFF_WRITES: usize = 41;
/// The endpoint registers stage 3 sets to 0 (AUDIO_DESCRIPTOR1-13 but 8 and 12) take the sink's descriptors.
const DESCRIPTORS: [u32; 12] = [0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F, 0x31, 0x32, 0x33, 0x35];

/// The 48 writes for a sink, in Linux 6.6's order (`link_dpms.c:2428-2436` for a DP display the firmware lit).
pub fn on_writes(az: &AzConfig) -> Vec<Write> {
    let mut v = Vec::with_capacity(ON_WRITES);
    // 1: dce_aud_wall_dto_setup, DP branch: the audio clock DTO1 at 24 MHz from 600 MHz
    v.push(w(1, b(0x5AC), 0xFFFF_FF8F, 0x10, !0)); // DCCG_AUDIO_DTO_SOURCE.DCCG_AUDIO_DTO_SEL = 1 (DTO1)
    v.push(w(1, b(0x5BC), 0, 6_000_000, !0)); // DCCG_AUDIO_DTO1_MODULE = 600000 kHz * 10
    v.push(w(1, b(0x5B8), 0, 240_000, !0)); // DCCG_AUDIO_DTO1_PHASE = 24 MHz in 100 Hz
    v.push(w(1, b(0x5AC), 0xFFEF_FFFF, 0x10_0000, !0)); // DCCG_AUDIO_DTO2_USE_512FBR_DTO = 1
    // 2: afmt3_se_audio_setup for endpoint 0, FL and FR
    v.push(w(2, b(0x15514), 0, 0, 7)); // AFMT_AUDIO_SRC_CONTROL = 0 (REG_SET)
    v.push(w(2, b(0x154D4), 0xFFFF_00FF, 0x300, 0xFF00)); // AFMT_AUDIO_PACKET_CONTROL2.AFMT_AUDIO_CHANNEL_ENABLE = 3
    // 3: dce_aud_az_configure for DP
    v.push(w(3, e(0x54), !1, 1, 0x8000_0001)); // HOT_PLUG_CONTROL.CLOCK_GATING_DISABLE = 1
    // CHANNEL_SPEAKER: the sink's speakers, LFE 0, HDMI 0, DP 1, EXTRA bit 0 clear
    v.push(w(3, e(0x25), 0xFCF8_FF80, (az.speakers & 0x7F) as u32 | 1 << 17, 0x7_007F));
    for &ix in &DESCRIPTORS {
        // AUDIO_DESCRIPTORn is format code n + 1
        v.push(w(3, e(ix), 0, az.descriptors[(ix - 0x28) as usize], !0));
    }
    v.push(w(3, e(0x38), !1, 1, 1)); // RESPONSE_HBR.HBR_CAPABLE = 1 (DP SST: no bandwidth limit)
    v.push(w(3, e(0x37), 0xFFFF_FF00, az.video_lipsync as u32, 0xFF)); // RESPONSE_LIPSYNC.VIDEO_LIPSYNC
    v.push(w(3, e(0x37), 0xFFFF_00FF, (az.audio_lipsync as u32) << 8, 0xFF00)); // .AUDIO_LIPSYNC
    for (k, &x) in az.sink_info.iter().enumerate() {
        v.push(w(3, e(0x3A + k as u32), 0, x, !0)); // SINK_INFO0-8
    }
    v.push(w(3, e(0x54), !1, 0, 0x8000_0001)); // CLOCK_GATING_DISABLE = 0
    // 4: dce_aud_az_enable
    v.push(w(4, e(0x54), 0x7FFF_FFFE, 0x8000_0001, 0x8000_0001)); // AUDIO_ENABLED = 1 with CLOCK_GATING_DISABLE
    v.push(w(4, e(0x54), !1, 0, 0x8000_0001)); // CLOCK_GATING_DISABLE = 0
    // 5: enc3_se_dp_audio_enable: the DP audio packets
    v.push(w(5, b(0x155C0), !1, 1, 1)); // DIG0_AFMT_CNTL.AFMT_AUDIO_CLOCK_EN = 1
    v.push(w(5, b(0x157C4), 0, 0x8000, 0xFF_FFFF)); // DP_SEC_AUD_N = 0x8000 (REG_SET)
    v.push(w(5, b(0x157D4), 0, 1, 1)); // DP_SEC_TIMESTAMP.DP_SEC_TIMESTAMP_MODE = 1 auto (REG_SET)
    v.push(w(5, b(0x1551C), 0xFFFF_FFCE, 1, 0x31)); // afmt31_poweron: AFMT_MEM_PWR_DIS = 1, _FORCE = 0
    v.push(w(5, b(0x15508), 0xFBFF_FFFF, 0x400_0000, 0xBB7F_FFFF)); // AFMT_60958_CS_UPDATE = 1
    v.push(w(5, b(0x154D4), 0xEFFF_FFFE, 0, 0x1000_0001)); // AFMT_AUDIO_LAYOUT_OVRD = 0, AFMT_60958_OSF_OVRD = 0
    v.push(w(5, b(0x1550C), 0xFFFF_FF7F, 0x80, 0xFFFF_FF7F)); // AFMT_AUDIO_INFO_UPDATE = 1 (a strobe)
    v.push(w(5, b(0x154E0), 0xCFFF_FFFF, 0, 0x3000_0000)); // AFMT_60958_CS_CLOCK_ACCURACY = 0
    v.push(w(5, b(0x157AC), 0xFFFF_FFEF, 0x10, !0)); // DP_SEC_CNTL.DP_SEC_ASP_ENABLE = 1
    v.push(w(5, b(0x157AC), 0xFFFF_EEFF, 0x1100, !0)); // DP_SEC_ATP_ENABLE = 1, DP_SEC_AIP_ENABLE = 1
    v.push(w(5, b(0x157AC), !1, 1, !0)); // DP_SEC_STREAM_ENABLE = 1, after the others
    // 6: afmt3_audio_mute_control(false)
    v.push(w(6, b(0x1551C), 0xFFFF_FFCE, 1, 0x31)); // afmt31_poweron
    v.push(w(6, b(0x15508), !1, 1, 0xBB7F_FFFF)); // AFMT_AUDIO_SAMPLE_SEND = 1
    v
}

/// The undo in Linux's order for audio off (`link_hwss_dio.c:210-228`, `dce_audio.c:368-391`): samples muted,
/// packets off, the AFMT, the endpoint, last the audio DTO, then the endpoint index. The firmware's values
/// (saved-5.json, 04.10.2026) written whole.
pub const UNDO: [Write; OFF_WRITES] = [
    w(0, b(0x15508), 0, 0x0400_0800, 0xBB7F_FFFF), // AFMT_AUDIO_PACKET_CONTROL
    w(0, b(0x157AC), 0, 0, !0),                    // DP_SEC_CNTL
    w(0, b(0x155C0), 0, 0x100, 0xFFFF_FEFF),       // DIG0_AFMT_CNTL
    w(0, b(0x157D4), 0, 0, !0),                    // DP_SEC_TIMESTAMP
    w(0, b(0x157C4), 0, 0x8000, !0),               // DP_SEC_AUD_N
    w(0, b(0x154E0), 0, 0, !0),                    // AFMT_60958_0
    w(0, b(0x1550C), 0, 0, 0xFFFF_FF7F),           // AFMT_INFOFRAME_CONTROL0
    w(0, b(0x154D4), 0, 0, !0),                    // AFMT_AUDIO_PACKET_CONTROL2
    w(0, b(0x15514), 0, 0, !0),                    // AFMT_AUDIO_SRC_CONTROL
    w(0, b(0x1551C), 0, 0x130, 0xFFFF_FCFF),       // AFMT_MEM_PWR
    w(0, e(0x54), !0, 1, 1),                       // HOT_PLUG_CONTROL: CLOCK_GATING_DISABLE = 1
    w(0, e(0x54), 0x7FFF_FFFF, 0, 0x8000_0000),    // AUDIO_ENABLED = 0
    w(0, e(0x25), 0, 0, 0xFCFF_FFFF),              // CHANNEL_SPEAKER
    w(0, e(0x28), 0, 0x0701_0701, !0),             // AUDIO_DESCRIPTOR0
    w(0, e(0x29), 0, 0, !0),
    w(0, e(0x2A), 0, 0, !0),
    w(0, e(0x2B), 0, 0, !0),
    w(0, e(0x2C), 0, 0, !0),
    w(0, e(0x2D), 0, 0, !0),
    w(0, e(0x2E), 0, 0, !0),
    w(0, e(0x2F), 0, 0, !0),
    w(0, e(0x31), 0, 0, !0),
    w(0, e(0x32), 0, 0, !0),
    w(0, e(0x33), 0, 0, !0),
    w(0, e(0x35), 0, 0, !0),
    w(0, e(0x38), 0, 1, !0), // RESPONSE_HBR
    w(0, e(0x37), 0, 0, !0), // RESPONSE_LIPSYNC
    w(0, e(0x3A), 0, 0, !0), // SINK_INFO0-8
    w(0, e(0x3B), 0, 0, !0),
    w(0, e(0x3C), 0, 0, !0),
    w(0, e(0x3D), 0, 0, !0),
    w(0, e(0x3E), 0, 0, !0),
    w(0, e(0x3F), 0, 0, !0),
    w(0, e(0x40), 0, 0, !0),
    w(0, e(0x41), 0, 0, !0),
    w(0, e(0x42), 0, 0, !0),
    w(0, e(0x54), 0, 0, 0xFFFF_FFEF),   // HOT_PLUG_CONTROL as saved
    w(0, b(0x5AC), 0, 0x30, !0),        // DCCG_AUDIO_DTO_SOURCE: DTO_SEL back first
    w(0, b(0x5B8), 0, 0, !0),           // DCCG_AUDIO_DTO1_PHASE
    w(0, b(0x5BC), 0, 1, !0),           // DCCG_AUDIO_DTO1_MODULE
    w(0, b(EP_INDEX), 0, 0, !0),        // the endpoint index as saved
];

/// A register's name for a refusal (V1 `dpa_names`).
pub fn name(loc: Loc) -> &'static str {
    match loc {
        Loc::Bar(0x5AC) => "DCCG_AUDIO_DTO_SOURCE",
        Loc::Bar(0x5B8) => "DCCG_AUDIO_DTO1_PHASE",
        Loc::Bar(0x5BC) => "DCCG_AUDIO_DTO1_MODULE",
        Loc::Bar(EP_INDEX) => "the endpoint index",
        Loc::Bar(0x154D4) => "AFMT_AUDIO_PACKET_CONTROL2",
        Loc::Bar(0x154E0) => "AFMT_60958_0",
        Loc::Bar(0x15508) => "AFMT_AUDIO_PACKET_CONTROL",
        Loc::Bar(0x1550C) => "AFMT_INFOFRAME_CONTROL0",
        Loc::Bar(0x15514) => "AFMT_AUDIO_SRC_CONTROL",
        Loc::Bar(0x1551C) => "AFMT_MEM_PWR",
        Loc::Bar(0x155C0) => "DIG0_AFMT_CNTL",
        Loc::Bar(0x157AC) => "DP_SEC_CNTL",
        Loc::Bar(0x157C4) => "DP_SEC_AUD_N",
        Loc::Bar(0x157D4) => "DP_SEC_TIMESTAMP",
        Loc::Ep(0x25) => "endpoint CHANNEL_SPEAKER",
        Loc::Ep(0x28..=0x35) => "endpoint AUDIO_DESCRIPTOR",
        Loc::Ep(0x37) => "endpoint RESPONSE_LIPSYNC",
        Loc::Ep(0x38) => "endpoint RESPONSE_HBR",
        Loc::Ep(0x3A..=0x42) => "endpoint SINK_INFO",
        Loc::Ep(0x54) => "endpoint HOT_PLUG_CONTROL",
        _ => "a register",
    }
}

fn hex(loc: Loc) -> String {
    match loc {
        Loc::Bar(x) | Loc::Ep(x) => format!("{x:X}"),
    }
}

/// What the display side's audio is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Off,
    /// As `on` leaves it: the audio clock from DTO1, samples sent, endpoint 0 enabled.
    On,
    /// No display engine to ask, or the segment bases not confirmed (gates 1 and 2).
    Absent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum On {
    Written { writes: u32 },
    AlreadyOn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Off {
    pub writes: u32,
    /// Writes that did not read back (counted; the rest was still written).
    pub mismatched: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnError {
    NoEngine,
    /// A gate failed: nothing written.
    Gate { gate: Gate, read: u32 },
    /// A write did not read back: stopped there and undone (`undo` is the undo's own result).
    Surprise { stage: u8, nth: u32, loc: Loc, wrote: u32, read: u32, undo: Result<Off, OffError> },
    /// A register access failed half way (device gone): undone as far as possible.
    Access { stage: u8, nth: u32, loc: Loc, error: RegError, undo: Result<Off, OffError> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OffError {
    NoEngine,
    /// Gate 1 or 2 failed: nothing undone.
    NotConfirmed { gate: Gate, read: u32 },
    /// An access failed mid-way (`writes` were made before it).
    Access { writes: u32, loc: Loc, error: RegError },
}

impl On {
    /// The log line, as V1 wrote it.
    pub fn line(&self) -> String {
        match self {
            On::Written { writes } => format!("HDMI audio: display side on (DIG0, DP SST, {writes} writes)"),
            On::AlreadyOn => "HDMI audio: display side on already (DIG0, DP SST), nothing written".into(),
        }
    }
}

impl Off {
    pub fn line(&self) -> String {
        if self.mismatched == 0 {
            format!("HDMI audio: display side off ({} writes)", self.writes)
        } else {
            format!("HDMI audio: display side off ({} writes), {} did not read back", self.writes, self.mismatched)
        }
    }
}

impl OffError {
    pub fn line(&self) -> String {
        match self {
            OffError::NoEngine => "HDMI audio: no display engine".into(),
            OffError::NotConfirmed { gate, read } => {
                format!("HDMI audio: display side not confirmed, nothing undone: {} (read {read:X})", gate.name)
            }
            OffError::Access { writes, loc, error } => {
                format!("HDMI audio: display side off stopped after {writes} writes: {} at {} ({error:?})", name(*loc), hex(*loc))
            }
        }
    }
}

impl OnError {
    /// The short why (V1's rbx text).
    pub fn why(&self) -> String {
        match self {
            OnError::NoEngine => "HDMI audio: no display engine".into(),
            OnError::Gate { gate, .. } => format!("HDMI audio: display side not ready: {}", gate.name),
            OnError::Surprise { loc, .. } | OnError::Access { loc, .. } => format!("HDMI audio: display side refused: {}", name(*loc)),
        }
    }
    /// The log line (the undo's own line follows it for a surprise).
    pub fn line(&self) -> String {
        match self {
            OnError::NoEngine => self.why(),
            OnError::Gate { gate, read } => format!("HDMI audio: display side not ready: {} (read {read:X}), nothing written", gate.name),
            OnError::Surprise { stage, nth, loc, wrote, read, .. } => format!(
                "HDMI audio: display side refused: {} (stage {stage}, write {nth} at {}: wrote {wrote:X}, read {read:X})",
                name(*loc),
                hex(*loc)
            ),
            OnError::Access { stage, nth, loc, error, .. } => {
                format!("HDMI audio: display side refused: {} (stage {stage}, write {nth} at {}: {error:?})", name(*loc), hex(*loc))
            }
        }
    }
    pub fn undo(&self) -> Option<&Result<Off, OffError>> {
        match self {
            OnError::Surprise { undo, .. } | OnError::Access { undo, .. } => Some(undo),
            _ => None,
        }
    }
}

/// Reads a register; an endpoint one through the pair, the index as found put back (V1 `dpa_get`).
pub fn get<R: Regs>(r: &mut R, loc: Loc) -> Result<u32, RegError> {
    match loc {
        Loc::Bar(x) => r.read(x),
        Loc::Ep(ix) => r.with_pair(|r| {
            let old = r.read(EP_INDEX)?;
            r.write(EP_INDEX, ix).or_else(tolerate)?;
            let v = r.read(EP_DATA);
            let back = r.write(EP_INDEX, old).or_else(tolerate);
            let v = v?;
            back.map(|_| v)
        }),
    }
}

/// Writes a register (V1 `dpa_put`). The owner's own read-back compare is left to the caller's masked one.
pub fn put<R: Regs>(r: &mut R, loc: Loc, v: u32) -> Result<(), RegError> {
    match loc {
        Loc::Bar(x) => r.write(x, v).or_else(tolerate),
        Loc::Ep(ix) => r.with_pair(|r| {
            let old = r.read(EP_INDEX)?;
            r.write(EP_INDEX, ix).or_else(tolerate)?;
            let res = r.write(EP_DATA, v).or_else(tolerate);
            let back = r.write(EP_INDEX, old).or_else(tolerate);
            res.and(back)
        }),
    }
}

fn tolerate(e: RegError) -> Result<(), RegError> {
    match e {
        RegError::ReadBack { .. } => Ok(()),
        e => Err(e),
    }
}

/// `Ok(None)` all passed; `Ok(Some(..))` the first that failed and what it read.
fn check<R: Regs>(r: &mut R, gates: &[Gate]) -> Result<Option<(Gate, u32)>, RegError> {
    for g in gates {
        let v = get(r, g.loc)?;
        if ((v & g.mask) == g.want) == g.differ {
            return Ok(Some((*g, v)));
        }
    }
    Ok(None)
}

/// The engine is there: the first gate's register can be read.
fn present<R: Regs>(r: &mut R) -> bool {
    r.read(0xE318).is_ok()
}

/// V1 `dpa_is_on`.
fn is_on<R: Regs>(r: &mut R) -> Result<bool, RegError> {
    if get(r, b(0x5AC))? & 0x70 != 0x10 || get(r, b(0x15508))? & 1 == 0 {
        return Ok(false);
    }
    Ok(get(r, e(0x54))? >> 31 == 1)
}

/// The display side's audio (V1 `dp_audio_state`): only reads, the endpoint index put back.
pub fn state<R: Regs>(r: &mut R) -> State {
    if !present(r) {
        return State::Absent;
    }
    match check(r, &GATES[..2]) {
        Ok(None) => {}
        _ => return State::Absent,
    }
    match is_on(r) {
        Ok(true) => State::On,
        Ok(false) => State::Off,
        Err(_) => State::Absent,
    }
}

/// Whether a monitor shows a picture on the DP link the audio rides on (SN-11: switching HDMI sound on at boot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sense {
    /// No display engine, or the segment bases not confirmed (as [`State::Absent`]).
    Absent,
    /// The first display path gate that failed (gates 5-10: DIG0, front end, link trained, stream, OTG): no
    /// monitor lit on that link.
    Dark { gate: Gate, read: u32 },
    /// DIG0 in DP SST mode, link trained, video stream on, OTG0 running: a monitor is there.
    Lit,
}

/// The display path gates of [`GATES`] (5-10): what Linux's DC has done for a connected sink before it enables
/// the stream's audio and reports the pin's monitor as present.
pub const LINK_GATES: core::ops::Range<usize> = 4..10;

/// [`Sense`] by reads only (no index select, no write): the gates `on` checks first for the link, without the
/// audio ones, so a monitor is sensed before anything is written. Linux reports a pin's monitor present when a
/// connector with an enabled stream holds its audio instance (linux-6.6/drivers/gpu/drm/amd/display/amdgpu_dm/
/// amdgpu_dm.c:932-952, set at the stream's commit :8490-8501; patch_hdmi.c:1827-1834 takes it as
/// `monitor_present`).
pub fn sense<R: Regs>(r: &mut R) -> Sense {
    if !present(r) || !matches!(check(r, &GATES[..2]), Ok(None)) {
        return Sense::Absent;
    }
    match check(r, &GATES[LINK_GATES]) {
        Ok(None) => Sense::Lit,
        Ok(Some((gate, read))) => Sense::Dark { gate, read },
        Err(_) => Sense::Absent,
    }
}

/// Every write of `list`; `stop` at the first surprise. -> (writes made, mismatches, first surprise, error).
struct Applied {
    writes: u32,
    mismatched: u32,
    surprise: Option<(Write, u32, u32, u32)>,
    error: Option<(Write, u32, RegError)>,
}

fn apply<R: Regs>(r: &mut R, list: &[Write], stop: bool) -> Applied {
    let mut a = Applied { writes: 0, mismatched: 0, surprise: None, error: None };
    for (i, wr) in list.iter().enumerate() {
        let nth = i as u32 + 1;
        let old = match get(r, wr.loc) {
            Ok(v) => v,
            Err(e) => {
                a.error = Some((*wr, nth, e));
                return a;
            }
        };
        let new = (old & wr.and) | wr.or;
        if let Err(e) = put(r, wr.loc, new) {
            a.error = Some((*wr, nth, e));
            return a;
        }
        a.writes += 1;
        let back = match get(r, wr.loc) {
            Ok(v) => v,
            Err(e) => {
                a.error = Some((*wr, nth, e));
                return a;
            }
        };
        if (back ^ new) & wr.check != 0 {
            a.mismatched += 1;
            if a.surprise.is_none() {
                a.surprise = Some((*wr, nth, new, back));
            }
            if stop {
                return a;
            }
        }
    }
    a
}

/// V1 `dp_audio_on` for the sink `az`: gates, then the 48 writes; already on: nothing written. A surprise or a
/// failed access is undone at once.
pub fn on<R: Regs>(r: &mut R, az: &AzConfig) -> Result<On, OnError> {
    if !present(r) {
        return Err(OnError::NoEngine);
    }
    match check(r, &GATES) {
        Ok(None) => {}
        Ok(Some((gate, read))) => return Err(OnError::Gate { gate, read }),
        Err(_) => return Err(OnError::NoEngine),
    }
    match is_on(r) {
        Ok(true) => return Ok(On::AlreadyOn),
        Ok(false) => {}
        Err(_) => return Err(OnError::NoEngine),
    }
    let list = on_writes(az);
    let a = apply(r, &list, true);
    if let Some((wr, nth, wrote, read)) = a.surprise {
        return Err(OnError::Surprise { stage: wr.stage, nth, loc: wr.loc, wrote, read, undo: off(r) });
    }
    if let Some((wr, nth, error)) = a.error {
        return Err(OnError::Access { stage: wr.stage, nth, loc: wr.loc, error, undo: off(r) });
    }
    Ok(On::Written { writes: a.writes })
}

/// V1 `dp_audio_off`: the firmware's values back. Needs only gates 1 and 2 (not the link: the picture may have
/// changed since). A write that does not read back is counted and the rest still written. No waits: also the
/// Quiesce path (interrupts off).
pub fn off<R: Regs>(r: &mut R) -> Result<Off, OffError> {
    if !present(r) {
        return Err(OffError::NoEngine);
    }
    match check(r, &GATES[..2]) {
        Ok(None) => {}
        Ok(Some((gate, read))) => return Err(OffError::NotConfirmed { gate, read }),
        Err(_) => return Err(OffError::NoEngine),
    }
    let a = apply(r, &UNDO, false);
    if let Some((wr, _, error)) = a.error {
        return Err(OffError::Access { writes: a.writes, loc: wr.loc, error });
    }
    Ok(Off { writes: a.writes, mismatched: a.mismatched })
}

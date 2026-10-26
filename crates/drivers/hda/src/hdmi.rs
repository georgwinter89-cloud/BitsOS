//! SN-5: sound over HDMI on the Mini-PC. The display side (`hdacodec::dpaudio`: 14 gates, 48 writes, 41-write
//! undo) through `amdgpu::dcn::DcnRegs`, the sink from the monitor's EDID (`hdacodec::eld`, generic without
//! one), then the GPU's HD Audio controller (1002:1640) from enumerate-only to a streaming controller whose DP
//! audio pin (3, AZ endpoint 0 -> DIG0) is playable. `hdmi_off` and Quiesce (Reload, reboot, power off) put the
//! display side back to the firmware's values.
//!
//! QEMU has no display engine: `DcnRegs::attached()` is `None`, every call answers "no display
//! engine" and nothing is written (ktest `sn.dpaudio.absent`).

use crate::ctrl::DP_AUDIO_PIN;
use crate::{Hda, Kind, controllers};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu::dcn::{self, DcnError, DcnRegs};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use hdacodec::dpaudio::{self, EP_INDEX, Off, OffError, On, OnError, RegError, Regs, State};
use hdacodec::eld::{AzConfig, EdidError, Sink};
use ksync::{IrqSpinLock, Mutex, SpinLock};

/// The AZ endpoint index/data pair (0xE118/0xE11C): `DcnRegs`' one lock for every user (`dcn::AZ_ENDPOINT_PAIR`,
/// interrupts off while held; INT-W8 moved SN-5's own lock there).
static PAIR: &IrqSpinLock<()> = &dcn::AZ_ENDPOINT_PAIR;
/// Whether Quiesce must undo, the last Quiesce's note, the writes made.
static LATCH: Latch = Latch::new();
static SINK: SpinLock<Option<Sink>> = SpinLock::named("hda.hdmi.sink", None);
/// One switch at a time (it may sleep: the GPU controller's restart).
static SERIAL: Mutex<()> = Mutex::named("hda.hdmi", ());
/// Bumped when the HDMI output appears or goes (the mixer re-resolves its target).
static GEN: AtomicU32 = AtomicU32::new(0);
/// Quiesce does not wait for the pair longer than this many tries: a holder on another CPU is done with one
/// access within microseconds; one that never lets go (a stopped CPU) is not waited for.
const PAIR_TRIES: u32 = 1 << 16;

/// What the glue needs of the display owner: `DcnRegs` on the hardware, a register image in host tests.
pub trait Raw {
    fn read(&self, byte: u32) -> Result<u32, DcnError>;
    fn write(&self, byte: u32, v: u32) -> Result<(), DcnError>;
}

impl Raw for DcnRegs {
    fn read(&self, byte: u32) -> Result<u32, DcnError> {
        DcnRegs::read(self, byte)
    }
    fn write(&self, byte: u32, v: u32) -> Result<(), DcnError> {
        DcnRegs::write(self, byte, v)
    }
}

/// The undo's bookkeeping. `armed`: display registers may differ from the firmware's because of this kernel, so
/// Quiesce (Reload, reboot, power off) must undo. Set before the first write, cleared only when nothing was
/// written or an undo restored every register.
pub(crate) struct Latch {
    armed: AtomicBool,
    /// Quiesce cannot log: bit 31 = a note, 30 = refused, 29 = the pair was busy, 15:8 = writes, 7:0 = mismatched.
    note: AtomicU32,
    /// Writes handed to the owner (refused ones included) that can change what the display engine does: the
    /// absent-device and boot-switch ktests check it stays. Not the endpoint index selects, see `selects`.
    write_calls: AtomicU64,
    /// SN-13: `EP_INDEX` writes made while holding the endpoint pair. `dpaudio::get`/`put` select the endpoint
    /// register and put the index back under the same hold, so a plain state read writes it twice; the endpoint
    /// register itself is written through `EP_DATA`, which counts in `write_calls`.
    selects: AtomicU64,
}

impl Latch {
    pub(crate) const fn new() -> Self {
        Latch { armed: AtomicBool::new(false), note: AtomicU32::new(0), write_calls: AtomicU64::new(0), selects: AtomicU64::new(0) }
    }
    pub(crate) fn armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }
    pub(crate) fn write_calls(&self) -> u64 {
        self.write_calls.load(Ordering::Acquire)
    }
    pub(crate) fn selects(&self) -> u64 {
        self.selects.load(Ordering::Acquire)
    }
    pub(crate) fn note(&self) -> Option<QuiesceNote> {
        let n = self.note.load(Ordering::Acquire);
        (n >> 31 == 1).then_some(QuiesceNote {
            refused: n >> 30 & 1 == 1,
            pair_busy: n >> 29 & 1 == 1,
            writes: n >> 8 & 0xFF,
            mismatched: n & 0xFF,
        })
    }
}

/// The last Quiesce's undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuiesceNote {
    /// The undo stopped (writes = the ones made before).
    pub refused: bool,
    /// It stopped because another holder kept the endpoint pair: endpoint registers not undone.
    pub pair_busy: bool,
    pub writes: u32,
    pub mismatched: u32,
}

pub(crate) struct Bus<'a, A: Raw + ?Sized> {
    raw: &'a A,
    latch: &'a Latch,
    pair: &'a IrqSpinLock<()>,
    quiesce: bool,
    /// Inside `with_pair` (nested holds count).
    depth: u32,
}

impl<A: Raw + ?Sized> Bus<'_, A> {
    fn paired<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, RegError>) -> Result<T, RegError> {
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }
}

fn map(e: DcnError) -> RegError {
    match e {
        DcnError::NotAttached => RegError::NotAttached,
        DcnError::OutOfRange => RegError::OutOfRange,
        DcnError::ReadBackMismatch { read, .. } => RegError::ReadBack { read },
    }
}

impl<A: Raw + ?Sized> Regs for Bus<'_, A> {
    fn read(&mut self, byte: u32) -> Result<u32, RegError> {
        self.raw.read(byte).map_err(map)
    }
    fn write(&mut self, byte: u32, v: u32) -> Result<(), RegError> {
        // An index select under the pair is not a display write (SN-13); one outside it is, as is every other.
        let n = if byte == EP_INDEX && self.depth > 0 { &self.latch.selects } else { &self.latch.write_calls };
        n.fetch_add(1, Ordering::AcqRel);
        self.raw.write(byte, v).map_err(map)
    }
    fn with_pair<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, RegError>) -> Result<T, RegError> {
        let pair = self.pair;
        if !self.quiesce {
            let _g = pair.lock();
            return self.paired(f);
        }
        // Quiesce never waits for long: a busy pair leaves the endpoint register alone (`PairBusy`), so the undo
        // cannot interleave with a holder that is half-way through an access (index written, data not yet).
        for _ in 0..PAIR_TRIES {
            if let Some(g) = pair.try_lock() {
                let r = self.paired(f);
                drop(g);
                return r;
            }
            core::hint::spin_loop();
        }
        Err(RegError::PairBusy)
    }
}

/// The display side's steps over one owner, with the undo bookkeeping (host-tested on a register image).
pub(crate) struct Ctx<'a, A: Raw + ?Sized> {
    pub(crate) raw: &'a A,
    pub(crate) latch: &'a Latch,
    pub(crate) pair: &'a IrqSpinLock<()>,
}

impl<'a, A: Raw + ?Sized> Ctx<'a, A> {
    pub(crate) fn bus(&self, quiesce: bool) -> Bus<'a, A> {
        Bus { raw: self.raw, latch: self.latch, pair: self.pair, quiesce, depth: 0 }
    }

    /// `dpaudio::on`, armed before its first write: a Quiesce during the writes, or after an undo that did not
    /// restore everything, still undoes.
    pub(crate) fn on(&self, az: &AzConfig) -> Result<On, OnError> {
        let before = self.latch.armed.swap(true, Ordering::AcqRel);
        let r = dpaudio::on(&mut self.bus(false), az);
        let armed = match &r {
            Ok(_) => true,
            // Refused before any write: as it was.
            Err(OnError::NoEngine | OnError::Gate { .. }) => before,
            // Written, then undone at once: disarmed only if the undo put every register back.
            Err(e) => !matches!(e.undo(), Some(Ok(o)) if o.mismatched == 0),
        };
        self.latch.armed.store(armed, Ordering::Release);
        r
    }

    /// `dpaudio::off`; disarmed only when every register read back as the firmware's.
    pub(crate) fn off(&self) -> Result<Off, OffError> {
        let r = dpaudio::off(&mut self.bus(false));
        if matches!(&r, Ok(o) if o.mismatched == 0) {
            self.latch.armed.store(false, Ordering::Release);
        }
        r
    }

    /// From Quiesce (interrupts off, maybe the panic path): the undo if armed. No waits, no klog: the result goes
    /// into the note.
    pub(crate) fn quiesce(&self) {
        if !self.latch.armed() {
            return;
        }
        let note = match dpaudio::off(&mut self.bus(true)) {
            Ok(o) => {
                if o.mismatched == 0 {
                    self.latch.armed.store(false, Ordering::Release);
                }
                1 << 31 | o.writes.min(0xFF) << 8 | o.mismatched.min(0xFF)
            }
            Err(OffError::Access { writes, error, .. }) => {
                1 << 31 | 1 << 30 | ((error == RegError::PairBusy) as u32) << 29 | writes.min(0xFF) << 8
            }
            Err(_) => 1 << 31 | 1 << 30,
        };
        self.latch.note.store(note, Ordering::Release);
    }
}

fn ctx(regs: &DcnRegs) -> Ctx<'_, DcnRegs> {
    Ctx { raw: regs, latch: &LATCH, pair: PAIR }
}

/// Display writes handed to `DcnRegs` since boot: every write except the endpoint index selects made under the
/// endpoint pair (those are [`index_selects`]).
pub fn write_calls() -> u64 {
    LATCH.write_calls()
}

/// `EP_INDEX` selects (and their put-backs) handed to `DcnRegs` under the endpoint pair since boot (SN-13).
pub fn index_selects() -> u64 {
    LATCH.selects()
}

/// Changes when the HDMI output appears or goes.
pub fn outputs_gen() -> u32 {
    GEN.load(Ordering::Acquire)
}

/// The monitor's EDID (from whoever reads it: DP AUX or the firmware's EDID, see notes). Without one the
/// display side announces a generic stereo LPCM sink.
pub fn set_sink_edid(edid: &[u8]) -> Result<(), EdidError> {
    match Sink::from_edid(edid) {
        Ok(s) => {
            let name = String::from_utf8_lossy(&s.name).into_owned();
            klog::event!(Info, "snd", "dpaudio.sink", source = "edid", manufacturer = klog::Hex(s.manufacturer_id as u64),
                product = klog::Hex(s.product_id as u64), name = name.as_str(), sads = s.sads.len(), speakers = klog::Hex(s.speakers as u64),
                lpcm = s.has_lpcm(), bad_checksum = s.bad_checksum);
            *SINK.lock() = Some(s);
            Ok(())
        }
        Err(e) => {
            klog::event!(Warn, "snd", "dpaudio.edid_refused", error = format!("{e:?}"), bytes = edid.len(); "keeping the sink as it was");
            Err(e)
        }
    }
}

fn az() -> Result<AzConfig, String> {
    let s = SINK.lock().clone();
    let s = match s {
        Some(s) => s,
        None => {
            klog::event!(Warn, "snd", "dpaudio.sink", source = "generic"; "no EDID of the monitor: a generic stereo LPCM sink (32/44.1/48 kHz, 16 bit)");
            Sink::generic()
        }
    };
    if !s.has_lpcm() {
        klog::event!(Warn, "snd", "dpaudio.sink_no_lpcm", sads = s.sads.len(); "the monitor's EDID offers no LPCM: no HDMI sound");
        return Err("HDMI audio: the monitor takes no LPCM".into());
    }
    Ok(AzConfig::from_sink(&s))
}

/// The display side's audio (only reads; the endpoint index is selected and put back).
pub fn display_state() -> State {
    match DcnRegs::attached() {
        Some(regs) => dpaudio::state(&mut ctx(regs).bus(false)),
        None => State::Absent,
    }
}

/// [`display_state`] without waiting (SN-10: the `snd` tool, `/sys/bitsos/snd_output`): None while `hdmi_on` or
/// `hdmi_off` runs, so a reader never selects the endpoint index between their writes.
pub fn display_state_now() -> Option<State> {
    let _s = SERIAL.try_lock()?;
    Some(display_state())
}

/// SN-11: is a monitor lit on the DP link HDMI sound rides on? Reads only (no index select); waits while
/// `hdmi_on`/`hdmi_off` runs (they are bounded). Thread context: the first call attaches the display engine.
pub fn monitor() -> dpaudio::Sense {
    let _s = SERIAL.lock();
    match DcnRegs::attached() {
        Some(regs) => dpaudio::sense(&mut ctx(regs).bus(false)),
        None => dpaudio::Sense::Absent,
    }
}

/// Whether the sink came from a monitor's EDID ([`set_sink_edid`]); false: `hdmi_on` would announce the
/// generic stereo LPCM sink.
pub fn sink_known() -> bool {
    SINK.lock().is_some()
}

fn log_off(r: &Result<Off, OffError>) {
    match r {
        Ok(o) => klog::event!(Info, "snd", "dpaudio.off", writes = o.writes, mismatched = o.mismatched, text = o.line().as_str()),
        Err(OffError::NoEngine) => klog::event!(Info, "snd", "dpaudio.no_engine"; "no display engine: nothing undone"),
        Err(e) => klog::event!(Warn, "snd", "dpaudio.off_refused", text = e.line().as_str()),
    }
}

/// The display side on (V1 `dp_audio_on`) for the current sink. One log line per outcome.
pub fn display_on() -> Result<On, String> {
    let Some(regs) = DcnRegs::attached() else {
        klog::event!(Info, "snd", "dpaudio.no_engine"; "no display engine (QEMU, or no GPU attach): nothing written");
        return Err(OnError::NoEngine.why());
    };
    let az = az()?;
    let r = ctx(regs).on(&az);
    match &r {
        Ok(o @ On::Written { writes }) => klog::event!(Warn, "snd", "dpaudio.on", writes = *writes, text = o.line().as_str()),
        Ok(o @ On::AlreadyOn) => klog::event!(Info, "snd", "dpaudio.on_already", text = o.line().as_str()),
        Err(OnError::NoEngine) => klog::event!(Info, "snd", "dpaudio.no_engine"; "no display engine: nothing written"),
        Err(e @ OnError::Gate { read, .. }) => klog::event!(Warn, "snd", "dpaudio.gate", read = klog::Hex(*read as u64), text = e.line().as_str()),
        Err(e) => {
            klog::event!(Error, "snd", "dpaudio.refused", text = e.line().as_str(); "a write did not read back: undone at once");
            if let Some(u) = e.undo() {
                log_off(u);
            }
            if LATCH.armed() {
                klog::event!(Error, "snd", "dpaudio.undo_pending"; "the undo did not restore every register: Quiesce (Reload, reboot, power off) undoes again");
            }
        }
    }
    r.map_err(|e| e.why())
}

/// The display side back to the firmware's values (V1 `dp_audio_off`).
pub fn display_off() -> Result<Off, String> {
    let Some(regs) = DcnRegs::attached() else {
        klog::event!(Info, "snd", "dpaudio.no_engine"; "no display engine: nothing undone");
        return Err(OffError::NoEngine.line());
    };
    let r = ctx(regs).off();
    log_off(&r);
    r.map_err(|e| e.line())
}

/// The GPU's HD Audio controller (1002:xxxx).
pub fn gpu_controller() -> Option<Arc<Hda>> {
    controllers().into_iter().find(|d| d.ctrl.kind() == Kind::AtiHdmi)
}

/// HDMI sound on: the display side, then the GPU controller streaming with its DP audio pin playable. Thread
/// context; may sleep (the controller's restart). Undone again if the controller does not come up.
pub fn hdmi_on() -> Result<(), String> {
    let _s = SERIAL.lock();
    let d = gpu_controller().ok_or_else(|| String::from("HDMI audio: no GPU HD Audio function"))?;
    display_on()?;
    if d.ctrl.read_only() || d.ctrl.state() != crate::State::Up {
        d.ctrl.set_streaming(true);
        if let Err(e) = d.restart() {
            klog::event!(Error, "snd", "hdmi.failed", ctrl = d.ctrl.name(), error = e.as_str(); "the GPU controller did not come up: undone");
            back_to_read_only(&d);
            let _ = display_off();
            return Err(e);
        }
    }
    let out = d.ctrl.outputs().into_iter().find(|o| o.id.pin == DP_AUDIO_PIN);
    let Some(out) = out.filter(|o| o.playable) else {
        klog::event!(Error, "snd", "hdmi.failed", ctrl = d.ctrl.name(), pin = klog::Hex(DP_AUDIO_PIN as u64); "no playable DP audio pin: undone");
        back_to_read_only(&d);
        let _ = display_off();
        return Err("HDMI audio: the GPU codec has no DP audio pin 3".into());
    };
    GEN.fetch_add(1, Ordering::AcqRel);
    klog::event!(Info, "snd", "hdmi.ready", ctrl = d.ctrl.name(), cad = out.id.cad, pin = klog::Hex(out.id.pin as u64), snoop = d.ctrl.snooped(),
        plugged = match out.plugged { Some(true) => "yes", Some(false) => "no", None => "unknown" });
    Ok(())
}

fn back_to_read_only(d: &Arc<Hda>) {
    d.ctrl.set_streaming(false);
    if let Err(e) = d.restart() {
        klog::event!(Warn, "snd", "hdmi.read_only_failed", ctrl = d.ctrl.name(), error = e.as_str());
    }
}

/// HDMI sound off (a driver stop): the GPU controller back to enumerate-only (streams gone, bus master off,
/// in reset), then the display side's undo.
pub fn hdmi_off() -> Result<Off, String> {
    let _s = SERIAL.lock();
    if let Some(d) = gpu_controller()
        && !d.ctrl.read_only()
    {
        back_to_read_only(&d);
        GEN.fetch_add(1, Ordering::AcqRel);
        klog::event!(Info, "snd", "hdmi.stopped", ctrl = d.ctrl.name());
    }
    display_off()
}

/// From the GPU controller's Quiesce (interrupts off, maybe the panic path): the undo if this kernel's writes may
/// still be in the display registers. No waits, no klog: the result is kept for [`quiesce_note`].
pub(crate) fn quiesce_display() {
    // `attached_now`: Quiesce never maps or logs; an engine nobody attached was never written either.
    if let Some(regs) = DcnRegs::attached_now() {
        ctx(regs).quiesce();
    }
}

/// The last Quiesce's undo, if any (ktests, the console after a Reload).
pub fn quiesce_note() -> Option<QuiesceNote> {
    LATCH.note()
}

/// The endpoint registers and DP/AFMT registers the undo writes, as they read now ((where, value)). For the
/// Mini-PC's undo check.
pub fn undo_registers() -> Result<Vec<(dpaudio::Loc, u32)>, String> {
    let regs = DcnRegs::attached().ok_or_else(|| OnError::NoEngine.why())?;
    let mut bus = ctx(regs).bus(false);
    dpaudio::UNDO
        .iter()
        .map(|w| dpaudio::get(&mut bus, w.loc).map(|v| (w.loc, v)).map_err(|e| format!("{:?}: {e:?}", w.loc)))
        .collect()
}

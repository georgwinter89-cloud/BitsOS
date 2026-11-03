//! Quiesce (Reload, reboot, power off, the panic path): the streams stopped, the codecs shut down, the chip
//! stopped (SN-7, SN-12). Lock-free, bounded, no klog or allocation; the note is logged at the next bring-up.

use super::*;

/// Quiesce note bit: an engine was not seen stopped.
const NOTE_STUCK: u8 = 0x40;
/// Quiesce note bits 4:2: what the codecs' shutdown did ([`CodecStop`]).
const NOTE_CODEC_SHIFT: u8 = 2;
const NOTE_CODEC: u8 = 0x1C;
/// Quiesce's bounds for the codec shutdown: per answer (a codec answers within a frame, ~21 us) and for the
/// D3 wait of each function group (the list's own timeout is capped to this).
const QUIESCE_ANSWER_US: u64 = 1000;
const QUIESCE_WAIT_US: u64 = 10_000;

/// What Quiesce's codec shutdown did (SN-12: Linux's `snd_hda_codec_shutdown` before the controller's stop).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecStop {
    /// Nothing sent: the controller was not up, never streamed (the read-only GPU controller) or has no codec.
    None = 0,
    /// Every codec's list sent and answered, D3 reported.
    Done = 1,
    /// The link or the codec list was held (a CPU stopped inside a command, a waiter): nothing sent.
    Busy = 2,
    /// A verb was not answered within its bound: the rest of that codec's list was skipped.
    NoAnswer = 3,
    /// Every verb answered, but a function group did not report D3 within the bound.
    NotD3 = 4,
    /// Every verb answered; the codec keeps no power state (its group never shows D3 as set: QEMU's hda codecs).
    NoPowerStates = 5,
}

impl CodecStop {
    fn from(v: u8) -> CodecStop {
        match v {
            1 => CodecStop::Done,
            2 => CodecStop::Busy,
            3 => CodecStop::NoAnswer,
            4 => CodecStop::NotD3,
            5 => CodecStop::NoPowerStates,
            _ => CodecStop::None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            CodecStop::None => "none",
            CodecStop::Done => "d3",
            CodecStop::Busy => "busy",
            CodecStop::NoAnswer => "no_answer",
            CodecStop::NotD3 => "not_d3",
            CodecStop::NoPowerStates => "no_power_states",
        }
    }
}

/// One controller's Quiesce result (the panic path's report, ktests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuiesceResult {
    pub was: State,
    /// A DMA engine was not seen stopped within 1 ms (only bus master off held it).
    pub stuck: bool,
    pub codec: CodecStop,
}

impl<H: Hw> Controller<H> {
    /// Stop for Reload, reboot and power off, in the order of Linux's PCI shutdown of the controller
    /// (`hda_intel.c` azx_shutdown -> azx_stop_chip, *without* the link reset): every stream's DMA stopped and
    /// waited for, the command rings off, interrupts and unsolicited responses off, the position buffer off,
    /// then MSI and bus master off. SN-7: the link stays out of reset. Entering it right after a live HDMI
    /// stream stopped, with the display side's audio still on, preceded an AMD sync flood at the next reset on
    /// the Mini-PC; Linux never resets the link at shutdown, and bus master off already keeps every engine
    /// from memory (the next bring-up resets the link before it uses any buffer). Lock-free, bounded (1 ms per
    /// wait), no klog; the event comes from a note later.
    pub fn quiesce(&self) -> QuiesceResult {
        self.quiesce_with(None, false)
    }

    /// [`Controller::quiesce`]; `clock` replaces the controller's clock and waits (the panic path passes its own
    /// TSC clock: the kernel clock's sequence lock may be held by a stopped CPU). SN-12, Linux's order: the streams' DMA stopped and waited for
    /// (`snd_pcm_suspend_all`), the codecs' shutdown verbs with the command rings still running (every converter
    /// off its stream tag, the codec in D3: `snd_hda_codec_shutdown`), then the chip (`azx_stop_chip`).
    /// `terminal`: the kernel never goes on after this (the real panic path, before halt or reset): the link and
    /// codec locks taken for the verbs are kept, never released, because a release may wake a waiter through the
    /// scheduler (SN-12 fixer). The ktest of the panic path passes false, it brings the controller up again.
    pub(crate) fn quiesce_with(&self, clock: Option<&(dyn Fn() -> u64 + Sync)>, terminal: bool) -> QuiesceResult {
        let hw = &Timed { regs: &self.hw, clock };
        // Pessimistic until the streams have answered: a close on another CPU that sees Quiesced in between keeps
        // its buffers (the engine is not yet seen stopped and bus master is still on).
        self.quiesce_stuck.store(true, Ordering::Release);
        let was = self.state.swap(State::Quiesced as u8, Ordering::AcqRel);
        self.irq_driven.store(false, Ordering::Release);
        let mut stuck = false;
        let mut codec = CodecStop::None;
        if hw.rd32(GCTL) != !0 {
            let total = self.gcap.total();
            let streams = link::stop_streams(hw, total);
            if State::from(was) == State::Up && !self.read_only() {
                codec = self.codec_shutdown(hw, terminal);
            }
            let rings = link::stop_cmd_io(hw);
            // SN-13 (w25): Linux's `azx_int_clear` (`hdac_controller.c` stop_chip) also clears the codecs' state
            // change bits; the stream, ring and controller status are cleared above.
            hw.wr16(STATESTS, STATESTS_MASK);
            stuck = !(streams && rings);
            hw.wr32(DPLBASE, 0);
            hw.wr32(DPUBASE, 0);
            hw.wr32(GCTL, hw.rd32(GCTL) & !GCTL_UNSOL);
        }
        hw.pci_off();
        self.quiesce_stuck.store(stuck, Ordering::Release);
        let flag = if stuck { NOTE_STUCK } else { 0 };
        let note = 0x80 | flag | (codec as u8) << NOTE_CODEC_SHIFT | was;
        self.quiesce_last.store(note, Ordering::Release);
        self.quiesce_note.store(note, Ordering::Release);
        QuiesceResult { was: State::from(was), stuck, codec }
    }

    /// Every codec's shutdown list over the link, without waiting for a lock or allocating: the link and the codec
    /// list are taken only if free and nobody waits for them (a waiter would have to be woken through the
    /// scheduler, which the panic path must not call). Every answer and wait bounded. `terminal`: both guards are
    /// forgotten instead of dropped (no release, no wake, no preemption point; see [`Controller::quiesce_with`]).
    /// Left over on the panic path: a CPU stopped between a waiter's priority record and its queueing (a few
    /// instructions) makes `try_lock` report the take to the scheduler; accepted, the window is not visible here.
    fn codec_shutdown<X: Hw + ?Sized>(&self, hw: &X, terminal: bool) -> CodecStop {
        if self.link.is_locked() || self.link.waiters() != 0 || self.codecs.is_locked() {
            return CodecStop::Busy;
        }
        let Some(mut g) = self.link.try_lock() else { return CodecStop::Busy };
        let Some(codecs) = self.codecs.try_lock() else {
            if terminal {
                core::mem::forget(g);
            }
            return CodecStop::Busy;
        };
        let mut result = CodecStop::None;
        if let Some(l) = g.as_mut() {
            for c in codecs.iter() {
                let r = Self::send_quiet(hw, l, c.cad, &c.shutdown);
                // The worst result counts: NoAnswer over NotD3 over NoPowerStates over Done.
                result = match (result, r) {
                    (CodecStop::NoAnswer, _) | (_, CodecStop::NoAnswer) => CodecStop::NoAnswer,
                    (CodecStop::NotD3, _) | (_, CodecStop::NotD3) => CodecStop::NotD3,
                    (CodecStop::NoPowerStates, _) | (_, CodecStop::NoPowerStates) => CodecStop::NoPowerStates,
                    _ => r,
                };
            }
        }
        if terminal {
            core::mem::forget(codecs);
            core::mem::forget(g);
        }
        result
    }

    /// One codec's list with Quiesce's bounds; stops at the first unanswered verb.
    fn send_quiet<X: Hw + ?Sized>(hw: &X, l: &mut Link<H::Mem>, cad: u8, list: &[Action]) -> CodecStop {
        let mut result = CodecStop::Done;
        for a in list {
            match *a {
                Action::Set { nid, verb } => {
                    if l.cmd_quiet(hw, cad, nid, verb, QUIESCE_ANSWER_US).is_err() {
                        return CodecStop::NoAnswer;
                    }
                }
                Action::WaitFor { nid, verb, mask, value, timeout_ms } => {
                    let limit = (timeout_ms as u64 * 1000).min(QUIESCE_WAIT_US);
                    let end = hw.now_ns().saturating_add(limit * 1000);
                    loop {
                        let Ok(v) = l.cmd_quiet(hw, cad, nid, verb, QUIESCE_ANSWER_US) else { return CodecStop::NoAnswer };
                        if v & mask == value {
                            break;
                        }
                        // GET_POWER_STATE: bits 3:0 the state last set, 7:4 the actual one. A codec that does not show
                        // the D3 just set keeps no power states: nothing to wait for.
                        if verb == hdacodec::verb::GET_POWER_STATE && (v & 0xF) != (value >> 4 & 0xF) {
                            result = CodecStop::NoPowerStates;
                            break;
                        }
                        if hw.now_ns() >= end {
                            result = CodecStop::NotD3;
                            break;
                        }
                        hw.delay_us(100);
                    }
                }
                Action::UpdateBits { nid, get, .. } => {
                    let Ok(v) = l.cmd_quiet(hw, cad, nid, get, QUIESCE_ANSWER_US) else { return CodecStop::NoAnswer };
                    if let Some(set) = a.update_verb(v)
                        && l.cmd_quiet(hw, cad, nid, set, QUIESCE_ANSWER_US).is_err()
                    {
                        return CodecStop::NoAnswer;
                    }
                }
            }
        }
        result
    }

    /// Holds the link lock as a CPU stopped inside a command would (host tests).
    #[cfg(test)]
    pub(crate) fn hold_link(&self) -> ksync::MutexGuard<'_, Option<Link<H::Mem>>> {
        self.link.lock()
    }

    /// The link and codec-list locks (host tests of the terminal panic path: still held afterwards).
    #[cfg(test)]
    pub(crate) fn locks_held(&self) -> (bool, bool) {
        (self.link.is_locked(), self.codecs.is_locked())
    }

    /// The last Quiesce's result (kept after its note is logged by the service thread or the next bring-up):
    /// ktests assert on it instead of the log line (the serial log may lose its tail when QEMU exits).
    pub fn last_quiesce(&self) -> Option<QuiesceResult> {
        decode_note(self.quiesce_last.load(Ordering::Acquire))
    }

    /// Quiesced (by [`Controller::quiesce`]): the stream calls do not touch the registers any more (SN-7: the
    /// mixer winding down after the reboot's Quiesce must not reach the device before the reset).
    pub(super) fn quiesced(&self) -> bool {
        self.state() == State::Quiesced
    }

    pub(super) fn log_quiesce_note(&self) {
        let Some(r) = decode_note(self.quiesce_note.swap(0, Ordering::AcqRel)) else { return };
        let (was, codec) = (r.was.name(), r.codec.name());
        match note_kind(&r) {
            NoteKind::Stuck => klog::event!(Warn, "snd", "hda.quiesced", ctrl = self.name.as_str(), was = was, stuck = true, codec = codec;
                "a DMA engine was not seen stopped within 1 ms: only bus master off held it"),
            NoteKind::CodecPartial => klog::event!(Warn, "snd", "hda.quiesced", ctrl = self.name.as_str(), was = was, stuck = false, codec = codec;
                "the codec's shutdown (stream tags cleared, D3) did not go through in full"),
            NoteKind::Fine => klog::event!(Info, "snd", "hda.quiesced", ctrl = self.name.as_str(), was = was, stuck = false, codec = codec),
        }
    }
}

/// The Quiesce note word back as a result (None: no note).
fn decode_note(n: u8) -> Option<QuiesceResult> {
    (n & 0x80 != 0).then(|| QuiesceResult {
        was: State::from(n & 0x3),
        stuck: n & NOTE_STUCK != 0,
        codec: CodecStop::from((n & NOTE_CODEC) >> NOTE_CODEC_SHIFT),
    })
}

/// Which `hda.quiesced` event a result gets: the first two are Warn, the last Info.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoteKind {
    Stuck,
    CodecPartial,
    Fine,
}

pub(crate) fn note_kind(r: &QuiesceResult) -> NoteKind {
    if r.stuck {
        NoteKind::Stuck
    } else if matches!(r.codec, CodecStop::Busy | CodecStop::NoAnswer | CodecStop::NotD3) {
        NoteKind::CodecPartial
    } else {
        NoteKind::Fine
    }
}

/// `regs` with another clock: the panic path's Quiesce keeps the controller's registers but not the kernel clock.
/// `clock` None: `regs`' own clock and waits.
struct Timed<'a, R: Hw> {
    regs: &'a R,
    clock: Option<&'a (dyn Fn() -> u64 + Sync)>,
}

impl<R: Hw> Hw for Timed<'_, R> {
    type Mem = R::Mem;
    fn rd8(&self, off: usize) -> u8 {
        self.regs.rd8(off)
    }
    fn rd16(&self, off: usize) -> u16 {
        self.regs.rd16(off)
    }
    fn rd32(&self, off: usize) -> u32 {
        self.regs.rd32(off)
    }
    fn wr8(&self, off: usize, v: u8) {
        self.regs.wr8(off, v)
    }
    fn wr16(&self, off: usize, v: u16) {
        self.regs.wr16(off, v)
    }
    fn wr32(&self, off: usize, v: u32) {
        self.regs.wr32(off, v)
    }
    fn now_ns(&self) -> u64 {
        match self.clock {
            Some(now) => now(),
            None => self.regs.now_ns(),
        }
    }
    fn delay_us(&self, us: u64) {
        match self.clock {
            Some(now) => {
                let end = now().saturating_add(us.saturating_mul(1000));
                while now() < end {
                    core::hint::spin_loop();
                }
            }
            None => self.regs.delay_us(us),
        }
    }
    fn alloc(&self, len: usize, usage: DmaUse, limit32: bool, snoop: bool) -> Result<Self::Mem, (AllocError, String)> {
        self.regs.alloc(len, usage, limit32, snoop)
    }
    fn enable(&self, dma: bool) -> Result<(), String> {
        self.regs.enable(dma)
    }
    fn pci_off(&self) {
        self.regs.pci_off()
    }
}

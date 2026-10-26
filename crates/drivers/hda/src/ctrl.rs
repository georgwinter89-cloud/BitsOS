//! One HD Audio controller: bring-up (stop what a Reload left running, PCI quirks, link reset, command rings,
//! codecs read with `hdacodec`), outputs, playback streams for SN-3 (open, write, start/stop, period callback,
//! position, delay), unsolicited jack events, the hard-IRQ acknowledge and Quiesce. Generic over `Hw`, so host
//! tests run it against `fake.rs`. Logs every state change, error, retry, timeout and fallback (B6).

use crate::format as fmtw;
use crate::hw::{AllocError, DmaUse, Hw, Mem};
use crate::link::{self, CmdError, Link, Mode, Unsol};
use crate::regs::*;
use crate::stream::{self, Layout, Position};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use devmodel::audio::{Output as Port, PcmFormat};
use hdacodec::dump::Dump;
use hdacodec::model::Codec;
use hdacodec::path::{self, OutputKind};
use hdacodec::setup::{self, Action};
use hdacodec::{NoResponse, VerbSource};
use ksync::{Mutex, SpinLock};

/// Which controller family: decides the PCI quirks, snooping, the position rule and whether we stream at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Intel and QEMU's intel-hda: TCSEL cleared, snooped, LPIB (Linux `POS_FIX_AUTO` picks posbuf; see notes).
    Intel,
    /// AMD FCH (1022:15E3): `AZX_DCAPS_PRESET_AMD_SB`: no TCSEL, ATI snoop byte, FIFO position fix (32 frames).
    AmdFch,
    /// AMD/ATI GPU HDMI/DP audio (1002:xxxx): enumerated only (immediate registers, no bus master, back into
    /// reset) until SN-5's display side runs; then [`Controller::set_streaming`] makes it a no-snoop controller
    /// like the others.
    AtiHdmi,
    Generic,
}

impl Kind {
    pub fn of(vendor: u16, device: u16) -> Kind {
        match (vendor, device) {
            (VENDOR_INTEL, _) => Kind::Intel,
            (VENDOR_AMD, _) => Kind::AmdFch,
            (VENDOR_ATI, _) => Kind::AtiHdmi,
            _ => Kind::Generic,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Intel => "intel",
            Kind::AmdFch => "amd_fch",
            Kind::AtiHdmi => "ati_hdmi",
            Kind::Generic => "generic",
        }
    }
    /// Starts enumerate-only (the GPU controller, until SN-5's display side is on).
    pub fn enumerate_only(self) -> bool {
        self == Kind::AtiHdmi
    }
    /// Frames the AMD FIFO holds ahead of LPIB (Linux `AMD_FIFO_SIZE`, `hda_intel.c:844`), else none known.
    pub fn fifo_frames(self) -> Option<u32> {
        (self == Kind::AmdFch).then_some(32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Down = 0,
    Up = 1,
    Quiesced = 2,
    Failed = 3,
}

impl State {
    fn from(v: u8) -> State {
        match v {
            1 => State::Up,
            2 => State::Quiesced,
            3 => State::Failed,
            _ => State::Down,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            State::Down => "down",
            State::Up => "up",
            State::Quiesced => "quiesced",
            State::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdaError {
    /// Not up, quiesced, or an enumerate-only controller.
    NotUp,
    /// HDMI/DP: needs the display owner (SN-5).
    Refused,
    NoSuchOutput,
    NoSuchStream,
    BadFormat,
    /// The converter plays another stream, or every output descriptor is taken.
    Busy,
    NoMemory,
    /// The codec did not answer while the path was set up.
    Codec,
}

impl HdaError {
    pub fn dev(self) -> devmodel::DevError {
        use devmodel::DevError as D;
        match self {
            HdaError::NotUp => D::Gone,
            HdaError::Refused | HdaError::Busy => D::Refused,
            HdaError::NoSuchOutput | HdaError::NoSuchStream | HdaError::BadFormat => D::InvalidArgument,
            HdaError::NoMemory => D::NoMemory,
            HdaError::Codec => D::Io,
        }
    }
}

/// An output of this controller: codec address and pin node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutputId {
    pub cad: u8,
    pub pin: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInfo {
    pub id: OutputId,
    pub port: Port,
    pub kind: OutputKind,
    pub converter: u8,
    pub formats: Vec<PcmFormat>,
    /// Plug state if the pin can sense it.
    pub plugged: Option<bool>,
    /// Can be opened here (HDMI/DP: only the DP audio pin, only while the display side sends audio, SN-5).
    pub playable: bool,
}

/// A stream handle: generation in the high bits, output descriptor number in the low byte.
pub type StreamId = u32;

/// Called from the controller's service thread (`hda/<n>`) after each period with the frames played so far.
pub type PeriodFn = Arc<dyn Fn(StreamId, u64) + Send + Sync>;
/// Called from the service thread on a plug change.
pub type JackFn = Arc<dyn Fn(OutputId, bool) + Send + Sync>;

pub struct CodecEntry {
    pub cad: u8,
    pub model: Codec,
    pub outputs: Vec<path::Output>,
    /// The read walk as taken at bring-up (`hdacodec` dump format).
    pub dump: Dump,
    /// (tag, pin) for unsolicited plug events.
    pub tags: Vec<(u8, u8)>,
    /// Microphones and line inputs with their ADC paths (SN-9).
    pub inputs: Vec<hdacodec::input::Input>,
}

struct Slot<M: Mem> {
    id: StreamId,
    /// An input (capture) descriptor: `pos.played` counts bytes the DMA wrote, `written` bytes read (SN-9).
    input: bool,
    overruns: u64,
    out: OutputId,
    converter: u8,
    fmt: PcmFormat,
    layout: Layout,
    /// Held while the descriptor may read it.
    _bdl: M,
    data: M,
    pos: Position,
    written: u64,
    running: bool,
    periods_seen: u64,
    callback: Option<PeriodFn>,
    underruns: u64,
    errors: u8,
}

/// Bring-up switches (CONFIG keys in the kernel).
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// `hda_snoop=off`: no-snoop DMA with UC/WC buffers even where snooping works (tests the UC/WC path).
    pub force_no_snoop: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { force_no_snoop: false }
    }
}

const JACK_POLL_NS: u64 = 1_000_000_000;

mod capture;
pub use capture::InputInfo;

/// The ATI codec pin of AZ endpoint 0 (`atihdmi_port2pin`: port * 2 + 3), the one SN-5's display side feeds to
/// DIG0 (the DP SST link to the Mini-PC's DP->HDMI converter).
pub const DP_AUDIO_PIN: u8 = 3;

pub struct Controller<H: Hw> {
    name: String,
    kind: Kind,
    pub hw: H,
    gcap: Gcap,
    state: AtomicU8,
    snoop: AtomicBool,
    link: Mutex<Option<Link<H::Mem>>>,
    codecs: SpinLock<Vec<Arc<CodecEntry>>>,
    slots: Mutex<Vec<Option<Slot<H::Mem>>>>,
    generation: AtomicU32,
    jacks: SpinLock<Vec<(OutputId, Option<bool>)>>,
    jack_fns: SpinLock<Vec<JackFn>>,
    next_jack_poll: AtomicU64,
    /// Interrupt causes seen by the hard-IRQ hook since the service thread last looked (bit = INTSTS bit).
    pending: AtomicU32,
    irqs: AtomicU64,
    /// Completion interrupts are armed (MSI): BDL entries carry IOC, INTCTL has the streams.
    irq_driven: AtomicBool,
    /// Quiesce does not log (it may run on the panic path): bit 7 = note present, low bits = state before.
    quiesce_note: AtomicU8,
    /// DMA buffers an engine may still use (a stop that was not confirmed): held until the next confirmed link
    /// reset, which resets every DMA engine (HDA spec 3.3.7 CRST), so they are never freed under a live engine.
    kept: SpinLock<Vec<H::Mem>>,
    /// The GPU controller streams (SN-5: its display side sends audio); always true for the other kinds.
    streaming: AtomicBool,
}

/// The codec's verbs through the link (the bring-up's `VerbSource`).
struct LinkSource<'a, H: Hw> {
    hw: &'a H,
    link: &'a mut Link<H::Mem>,
    cad: u8,
}

impl<H: Hw> VerbSource for LinkSource<'_, H> {
    fn get(&mut self, nid: u8, verb: u32) -> Result<u32, NoResponse> {
        self.link.cmd(self.hw, self.cad, nid, verb).map_err(|_| NoResponse)
    }
}

impl<H: Hw> Controller<H> {
    /// Reads GCAP; touches nothing else.
    pub fn new(name: String, kind: Kind, hw: H, cfg: Config) -> Controller<H> {
        let gcap = Gcap::parse(hw.rd16(GCAP));
        Controller {
            name,
            kind,
            hw,
            gcap,
            state: AtomicU8::new(State::Down as u8),
            // Snooped everywhere unless `hda_snoop=off`: Linux 6.6 takes 1002:1640 by the generic ATI class entry
            // (`hda_intel.c:2735-2738`, `AZX_DCAPS_PRESET_ATI_HDMI`, without `_NS`), so the GPU controller snoops too.
            snoop: AtomicBool::new(!cfg.force_no_snoop),
            link: Mutex::named("hda.link", None),
            codecs: SpinLock::named("hda.codecs", Vec::new()),
            slots: Mutex::named("hda.slots", Vec::new()),
            generation: AtomicU32::new(0),
            jacks: SpinLock::named("hda.jacks", Vec::new()),
            jack_fns: SpinLock::named("hda.jack_fns", Vec::new()),
            next_jack_poll: AtomicU64::new(0),
            pending: AtomicU32::new(0),
            irqs: AtomicU64::new(0),
            irq_driven: AtomicBool::new(false),
            quiesce_note: AtomicU8::new(0),
            kept: SpinLock::named("hda.kept", Vec::new()),
            streaming: AtomicBool::new(!kind.enumerate_only()),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }
    /// Only enumerated (immediate commands, no bus master, no streams): the GPU controller before SN-5's
    /// display side is on.
    pub fn read_only(&self) -> bool {
        !self.streaming.load(Ordering::Acquire)
    }
    /// The GPU controller may stream from the next bring-up on (`on`), or goes back to enumerate-only. The
    /// caller restarts it (`Hda::restart`); the other kinds always stream.
    pub fn set_streaming(&self, on: bool) {
        if self.kind.enumerate_only() {
            self.streaming.store(on, Ordering::Release);
        }
    }
    pub fn gcap(&self) -> Gcap {
        self.gcap
    }
    pub fn state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
    }
    pub fn snooped(&self) -> bool {
        self.snoop.load(Ordering::Acquire)
    }
    pub fn link_mode(&self) -> Option<Mode> {
        self.link.lock().as_ref().map(|l| l.mode)
    }
    pub fn irq_count(&self) -> u64 {
        self.irqs.load(Ordering::Acquire)
    }
    pub fn irq_driven(&self) -> bool {
        self.irq_driven.load(Ordering::Acquire)
    }
    /// Output slots (tags 1..=15 per direction); input slots follow them.
    fn nout(&self) -> usize {
        self.gcap.oss.min(TAG_MAX as usize)
    }
    /// The descriptor of slot `n`: outputs at `gcap.output`, inputs from 0 (inputs come first).
    fn desc_of(&self, n: usize) -> usize {
        if n < self.nout() { self.gcap.output(n) } else { n - self.nout() }
    }
    pub fn codecs(&self) -> Vec<Arc<CodecEntry>> {
        self.codecs.lock().clone()
    }
    /// (timeouts, strays) of the command link.
    pub fn link_stats(&self) -> (u64, u64) {
        self.link.lock().as_ref().map_or((0, 0), |l| (l.timeouts, l.strays))
    }

    /// Brings the controller up from whatever state it is in. Thread context; may sleep.
    pub fn bring_up(&self) -> Result<(), String> {
        let r = self.bring_up_inner();
        match &r {
            Ok(()) => self.state.store(State::Up as u8, Ordering::Release),
            Err(e) => {
                self.state.store(State::Failed as u8, Ordering::Release);
                klog::event!(Error, "snd", "hda.bring_up_failed", ctrl = self.name.as_str(), error = e.as_str());
            }
        }
        r
    }

    fn bring_up_inner(&self) -> Result<(), String> {
        self.log_quiesce_note();
        let hw = &self.hw;
        let total = self.gcap.total();
        if hw.rd32(GCTL) == !0 {
            return Err("controller reads all ones".into());
        }
        // MSI and bus master off first: on a restart of a running controller they are still on (probe has
        // done this already). A Reload or the firmware may have left streams and rings running: stop them.
        hw.pci_off();
        let was_running = (0..total).filter(|&i| hw.rd8(sd(i) + SD_CTL) & SD_CTL_RUN != 0).count();
        let rings_running = hw.rd8(CORBCTL) & CORBCTL_RUN != 0 || hw.rd8(RIRBCTL) & RIRBCTL_DMA != 0;
        let stopped = link::stop_all(hw, total);
        if was_running > 0 || rings_running {
            klog::event!(Warn, "snd", "hda.stale_dma", ctrl = self.name.as_str(), streams = was_running, rings = rings_running, stopped = stopped;
                "streams or command rings were running before bring-up (Reload, firmware): stopped");
        }
        if !stopped {
            klog::event!(Warn, "snd", "hda.stop_stuck", ctrl = self.name.as_str(); "an engine did not stop: its buffers wait for the link reset");
        }
        self.irq_driven.store(false, Ordering::Release);
        // Earlier bring-ups' buffers wait for the link reset below (enable turns bus master on before it).
        let old: Vec<Slot<H::Mem>> = self.slots.lock().drain(..).flatten().collect();
        let old_rings = self.link.lock().take().and_then(|mut l| l.take_rings());
        {
            let mut k = self.kept.lock();
            for s in old {
                k.push(s._bdl);
                k.push(s.data);
            }
            k.extend(old_rings);
        }
        self.pci_quirks();
        let dma = !self.read_only();
        hw.enable(dma)?;
        let mask = link::reset(hw, true)?;
        // The reset stopped every engine: now the kept buffers can go back to MM.
        let freed = core::mem::take(&mut *self.kept.lock());
        drop(freed);
        klog::event!(Info, "snd", "hda.reset", ctrl = self.name.as_str(), kind = self.kind.name(), codecs = klog::Hex(mask as u64),
            oss = self.gcap.oss, iss = self.gcap.iss, bss = self.gcap.bss, ok64 = self.gcap.ok64);
        let mut l = if dma { self.start_rings() } else { Link::immediate() };
        let mut codecs = Vec::new();
        for cad in (0..=hdacodec::CAD_LAST).filter(|c| mask & 1 << c != 0) {
            match self.read_codec(&mut l, cad) {
                Some(c) => codecs.push(Arc::new(c)),
                None => klog::event!(Warn, "snd", "hda.codec_silent", ctrl = self.name.as_str(), cad = cad; "reported in STATESTS but gave no vendor id"),
            }
        }
        if codecs.is_empty() {
            klog::event!(Warn, "snd", "hda.no_codec", ctrl = self.name.as_str(), mask = klog::Hex(mask as u64));
        }
        let mut jacks = Vec::new();
        for c in &codecs {
            for o in &c.outputs {
                let id = OutputId { cad: c.cad, pin: o.pin };
                let plugged = o.jack.and_then(|j| self.sense(&mut l, c.cad, &j, !dma));
                jacks.push((id, plugged));
            }
            if dma {
                for &(tag, pin) in &c.tags {
                    let Some(j) = c.outputs.iter().find(|o| o.pin == pin).and_then(|o| o.jack) else { continue };
                    if let Some(Action::Set { nid, verb }) = j.unsol_enable(tag) {
                        if let Err(e) = l.cmd(hw, c.cad, nid, verb) {
                            klog::event!(Warn, "snd", "hda.unsol_enable_failed", ctrl = self.name.as_str(), cad = c.cad, pin = klog::Hex(pin as u64), error = alloc::format!("{e:?}"));
                        }
                    }
                }
            }
        }
        for (id, p) in &jacks {
            klog::event!(Info, "snd", "hda.jack", ctrl = self.name.as_str(), cad = id.cad, pin = klog::Hex(id.pin as u64),
                plugged = match p { Some(true) => "yes", Some(false) => "no", None => "unknown" }, cause = "bring_up");
        }
        if !dma {
            // The GPU controller goes back into reset: nothing of it runs until SN-5's display side is on.
            hw.wr32(GCTL, hw.rd32(GCTL) & !GCTL_CRST);
        }
        *self.jacks.lock() = jacks;
        *self.codecs.lock() = codecs;
        *self.link.lock() = Some(l);
        let mut slots = self.slots.lock();
        slots.clear();
        // Outputs first, then inputs (SN-9); tags are per direction.
        slots.resize_with(self.nout() + self.gcap.iss.min(TAG_MAX as usize), || None);
        Ok(())
    }

    /// Linux `azx_init_pci` (`hda_intel.c:374`): Intel clears TCSEL; AMD FCH turns snooping on in its ATI byte.
    fn pci_quirks(&self) {
        match self.kind {
            Kind::Intel => self.hw.pci_modify32(PCI_TCSEL, 0x7, 0),
            Kind::AmdFch => {
                let set = if self.snooped() { ATI_ENABLE_SNOOP << 16 } else { 0 };
                self.hw.pci_modify32(PCI_ATI_MISC2 & !3, 0x7 << 16, set);
            }
            _ => {}
        }
    }

    fn start_rings(&self) -> Link<H::Mem> {
        let mem = match self.alloc(link::RINGS_BYTES, DmaUse::Control) {
            Ok(m) => m,
            Err(e) => {
                klog::event!(Warn, "snd", "hda.rings_fallback", ctrl = self.name.as_str(), error = e.as_str(); "no ring memory: immediate commands");
                return Link::immediate();
            }
        };
        let addr = mem.dev_addr();
        match Link::start_rings(&self.hw, mem) {
            Ok(mut l) => {
                // One read proves the rings move (a bridge without bus master leaves the CORB unread).
                match l.cmd(&self.hw, self.first_codec(), 0, hdacodec::verb::get_param(hdacodec::verb::PAR_VENDOR_ID)) {
                    Ok(_) => {
                        klog::event!(Info, "snd", "hda.rings", ctrl = self.name.as_str(), addr = klog::Hex(addr), snoop = self.snooped());
                        l
                    }
                    Err(e) => {
                        if let Some(m) = l.stop_rings(&self.hw) {
                            self.keep(m, "rings");
                        }
                        klog::event!(Warn, "snd", "hda.rings_fallback", ctrl = self.name.as_str(), error = alloc::format!("{e:?}");
                            "CORB/RIRB gave no answer: immediate commands");
                        Link::immediate()
                    }
                }
            }
            Err(e) => {
                klog::event!(Warn, "snd", "hda.rings_fallback", ctrl = self.name.as_str(), error = e.as_str(); "immediate commands");
                Link::immediate()
            }
        }
    }

    /// Holds a buffer whose engine did not confirm its stop until the next link reset.
    fn keep(&self, m: H::Mem, what: &str) {
        klog::event!(Warn, "snd", "hda.dma_kept", ctrl = self.name.as_str(), what = what, bytes = m.size();
            "the engine did not stop: buffer held until the next link reset");
        self.kept.lock().push(m);
    }

    /// Buffers held for a stuck engine (tests).
    pub fn kept_buffers(&self) -> usize {
        self.kept.lock().len()
    }

    fn first_codec(&self) -> u8 {
        // STATESTS was cleared by the reset; codec 0 is where every board puts its first codec.
        0
    }

    /// DMA memory with the snoop rule; UC/WC refused (MM-4 before MM-6) falls back to snooping where the
    /// controller can snoop (not the GPU controller).
    fn alloc(&self, len: usize, usage: DmaUse) -> Result<H::Mem, String> {
        let limit32 = !self.gcap.ok64;
        match self.hw.alloc(len, usage, limit32, self.snooped()) {
            Ok(m) => Ok(m),
            Err((AllocError::Unsupported, e)) if !self.snooped() => {
                klog::event!(Warn, "snd", "hda.snoop_fallback", ctrl = self.name.as_str(), error = e.as_str();
                    "uncached/write-combining DMA memory not available: snooped write-back memory instead");
                self.snoop.store(true, Ordering::Release);
                self.pci_quirks();
                self.hw.alloc(len, usage, limit32, true).map_err(|(_, e)| e)
            }
            Err((_, e)) => Err(e),
        }
    }

    /// One walk over the codec (`hdacodec::walk`), kept as a dump; the model and outputs are made from it.
    fn read_codec(&self, l: &mut Link<H::Mem>, cad: u8) -> Option<CodecEntry> {
        let t0 = self.hw.now_ns();
        let before = l.timeouts;
        let src = LinkSource { hw: &self.hw, link: l, cad };
        let dump = hdacodec::walk::dump_codec(src, cad, &[("format", "hdacodec-dump 1"), ("synthetic", "no"), ("ctrl", self.name.as_str())]);
        let model = Codec::read(&mut dump.source(cad), cad).ok()?;
        let outputs = path::outputs(&model);
        let mut tags = Vec::new();
        for o in outputs.iter().filter(|o| o.jack.is_some_and(|j| j.unsol)) {
            let tag = tags.len() as u8 + 1;
            if tag <= 0x3F {
                tags.push((tag, o.pin));
            }
        }
        klog::event!(Info, "snd", "hda.codec", ctrl = self.name.as_str(), cad = cad, vendor = klog::Hex(model.vendor_id as u64),
            revision = klog::Hex(model.revision_id as u64), groups = model.groups.len(), outputs = outputs.len(), reads = dump.lines().len(),
            missing = model.missing.len(), timeouts = l.timeouts - before, ms = (self.hw.now_ns() - t0) / 1_000_000);
        for o in &outputs {
            let chain: Vec<String> = o.path.iter().map(|h| format!("{:02x}", h.nid)).collect();
            klog::event!(Info, "snd", "hda.output", ctrl = self.name.as_str(), cad = cad, pin = klog::Hex(o.pin as u64),
                kind = kind_name(o.kind), hdmi = o.hdmi, dp = o.dp, path = chain.join("-").as_str(), jack = o.jack.is_some());
        }
        let inputs = hdacodec::input::inputs(&model);
        for i in &inputs {
            let chain: Vec<String> = i.path.iter().map(|h| format!("{:02x}", h.nid)).collect();
            klog::event!(Info, "snd", "hda.input", ctrl = self.name.as_str(), cad = cad, pin = klog::Hex(i.pin as u64),
                kind = if i.kind == hdacodec::input::InputKind::Mic { "mic" } else { "line" }, fixed = i.fixed, path = chain.join("-").as_str(),
                jack = i.jack.is_some());
        }
        Some(CodecEntry { cad, model, outputs, dump, tags, inputs })
    }

    /// Reads the plug state; `read_only`: no execute verb (the GPU controller is only read).
    fn sense(&self, l: &mut Link<H::Mem>, cad: u8, j: &path::JackSense, read_only: bool) -> Option<bool> {
        if let Some(Action::Set { nid, verb }) = j.before_read() {
            if read_only {
                return None;
            }
            l.cmd(&self.hw, cad, nid, verb).ok()?;
        }
        l.cmd(&self.hw, cad, j.pin, j.read_verb()).ok().map(path::JackSense::plugged)
    }

    /// Every output of every codec with its state.
    pub fn outputs(&self) -> Vec<OutputInfo> {
        let jacks = self.jacks.lock().clone();
        let mut v = Vec::new();
        for c in self.codecs() {
            for o in &c.outputs {
                let id = OutputId { cad: c.cad, pin: o.pin };
                let port = if o.kind == OutputKind::Digital { Port::Hdmi } else { Port::Jack };
                let cvt = o.converter();
                let caps = c.model.groups.iter().find(|g| g.nid == o.group).and_then(|g| g.widget(cvt)).map_or(0, |w| w.pcm.0);
                v.push(OutputInfo {
                    id,
                    port,
                    kind: o.kind,
                    converter: cvt,
                    formats: fmtw::formats(caps),
                    plugged: jacks.iter().find(|j| j.0 == id).and_then(|j| j.1),
                    playable: !self.read_only() && (port == Port::Jack || self.kind == Kind::AtiHdmi && o.pin == DP_AUDIO_PIN),
                });
            }
        }
        v
    }

    pub fn jack_present(&self, id: OutputId) -> Option<bool> {
        self.jacks.lock().iter().find(|j| j.0 == id).and_then(|j| j.1)
    }

    /// Plug changes are passed to `f` from the service thread.
    pub fn on_jack(&self, f: JackFn) {
        self.jack_fns.lock().push(f);
    }

    /// Opens a playback stream on `id` (not running yet): picks a free output descriptor, allocates the ring
    /// and its BDL, powers and sets up the path with `hdacodec`, programs the descriptor.
    pub fn open(&self, id: OutputId, fmt: PcmFormat, ring_frames: u32, period_frames: u32) -> Result<StreamId, HdaError> {
        if self.state() != State::Up || self.read_only() {
            return Err(HdaError::NotUp);
        }
        let info = self.outputs().into_iter().find(|o| o.id == id).ok_or(HdaError::NoSuchOutput)?;
        if !info.playable {
            return Err(HdaError::Refused);
        }
        let word = fmtw::format_word(fmt).ok_or(HdaError::BadFormat)?;
        if !info.formats.contains(&fmt) {
            return Err(HdaError::BadFormat);
        }
        let layout = Layout::new(fmtw::frame_bytes(fmt), ring_frames, period_frames).ok_or(HdaError::BadFormat)?;
        let codec = self.codecs().into_iter().find(|c| c.cad == id.cad).ok_or(HdaError::NoSuchOutput)?;
        let out = codec.outputs.iter().find(|o| o.pin == id.pin).ok_or(HdaError::NoSuchOutput)?.clone();
        let mut slots = self.slots.lock();
        if slots.iter().flatten().any(|s| s.out.cad == id.cad && s.converter == info.converter) {
            return Err(HdaError::Busy);
        }
        let held: u64 = slots.iter().flatten().map(|s| s.layout.ring_bytes as u64).sum();
        if layout.ring_bytes > stream::MAX_RING_BYTES || held + layout.ring_bytes as u64 > stream::MAX_RING_BYTES as u64 {
            klog::event!(Warn, "snd", "hda.open_failed", ctrl = self.name.as_str(), what = "ring_size", bytes = layout.ring_bytes,
                held = held, limit = stream::MAX_RING_BYTES);
            return Err(HdaError::NoMemory);
        }
        let n = slots[..self.nout().min(slots.len())].iter().position(Option::is_none).ok_or(HdaError::Busy)?;
        let desc = self.gcap.output(n);
        let tag = n as u8 + 1;
        let fail = |what: &str, e: &str| {
            klog::event!(Warn, "snd", "hda.open_failed", ctrl = self.name.as_str(), what = what, error = e);
            HdaError::NoMemory
        };
        let bdl = self.alloc(layout.bdl_bytes(), DmaUse::Control).map_err(|e| fail("bdl", &e))?;
        let data = self.alloc(layout.ring_bytes as usize, DmaUse::Data).map_err(|e| fail("ring", &e))?;
        let ioc = self.irq_driven();
        if !stream::write_bdl(&bdl, data.dev_addr(), &layout, ioc) {
            return Err(fail("bdl", "write"));
        }
        let st = setup::Stream { tag, channel: 0, format: word };
        let actions = setup::output_setup(&codec.model, &out, &st).map_err(|e| {
            klog::event!(Warn, "snd", "hda.setup_refused", ctrl = self.name.as_str(), pin = klog::Hex(id.pin as u64), error = alloc::format!("{e:?}"));
            HdaError::Codec
        })?;
        if let Err(e) = self.run_actions(id.cad, &actions) {
            // Part of the list may have gone through: no stream tag without a slot behind it.
            let _ = self.clear_converter(id.cad, info.converter);
            return Err(e);
        }
        if !stream::reset(&self.hw, desc) {
            klog::event!(Warn, "snd", "hda.stream_reset_slow", ctrl = self.name.as_str(), desc = desc);
        }
        stream::program(&self.hw, desc, tag, word, bdl.dev_addr(), &layout, !self.snooped());
        let g = self.generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1) & 0xFF_FFFF;
        let sid = g << 8 | n as u32;
        klog::event!(Info, "snd", "hda.open", ctrl = self.name.as_str(), stream = sid, desc = desc, tag = tag, cad = id.cad,
            pin = klog::Hex(id.pin as u64), rate = fmt.rate, bits = fmt.bits, ring_frames = layout.ring_frames(),
            period_frames = layout.period_frames(), periods = layout.periods, ioc = ioc, snoop = self.snooped());
        slots[n] = Some(Slot {
            id: sid,
            input: false,
            overruns: 0,
            out: id,
            converter: info.converter,
            fmt,
            layout,
            _bdl: bdl,
            data,
            pos: Position::default(),
            written: 0,
            running: false,
            periods_seen: 0,
            callback: None,
            underruns: 0,
            errors: 0,
        });
        Ok(sid)
    }

    /// Sends a setup list: `Set`, `WaitFor` (bounded by its timeout), `UpdateBits` (read-modify-write).
    fn run_actions(&self, cad: u8, list: &[Action]) -> Result<(), HdaError> {
        let mut g = self.link.lock();
        let l = g.as_mut().ok_or(HdaError::NotUp)?;
        let hw = &self.hw;
        let fail = |nid: u8, verb: u32, e: CmdError| {
            klog::event!(Warn, "snd", "hda.verb_failed", ctrl = self.name.as_str(), cad = cad, nid = klog::Hex(nid as u64),
                verb = klog::Hex(verb as u64), error = alloc::format!("{e:?}"));
            HdaError::Codec
        };
        for a in list {
            match *a {
                Action::Set { nid, verb } => {
                    l.cmd(hw, cad, nid, verb).map_err(|e| fail(nid, verb, e))?;
                }
                Action::WaitFor { nid, verb, mask, value, timeout_ms } => {
                    let end = hw.now_ns() + timeout_ms as u64 * 1_000_000;
                    loop {
                        let v = l.cmd(hw, cad, nid, verb).map_err(|e| fail(nid, verb, e))?;
                        if v & mask == value {
                            break;
                        }
                        if hw.now_ns() >= end {
                            klog::event!(Warn, "snd", "hda.wait_timeout", ctrl = self.name.as_str(), cad = cad, nid = klog::Hex(nid as u64),
                                verb = klog::Hex(verb as u64), answer = klog::Hex(v as u64), ms = timeout_ms; "going on anyway (V1 rule)");
                            break;
                        }
                        hw.pause_us(1000);
                    }
                }
                Action::UpdateBits { nid, get, .. } => {
                    let v = l.cmd(hw, cad, nid, get).map_err(|e| fail(nid, get, e))?;
                    if let Some(set) = a.update_verb(v) {
                        l.cmd(hw, cad, nid, set).map_err(|e| fail(nid, set, e))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// `f` on the open stream `sid`. `need_up`: refused once the controller is quiesced or failed (start, write);
    /// position, delay and close still answer so a program can wind down.
    fn with_slot<R>(&self, sid: StreamId, need_up: bool, f: impl FnOnce(&mut Slot<H::Mem>, usize) -> R) -> Result<R, HdaError> {
        let mut slots = self.slots.lock();
        if need_up && self.state() != State::Up {
            return Err(HdaError::NotUp);
        }
        let n = (sid & 0xFF) as usize;
        match slots.get_mut(n) {
            Some(Some(s)) if s.id == sid => Ok(f(s, self.desc_of(n))),
            _ => Err(HdaError::NoSuchStream),
        }
    }

    /// Copies whole frames into the ring at the write position; returns the bytes taken (0 when full).
    pub fn write(&self, sid: StreamId, data: &[u8]) -> Result<usize, HdaError> {
        self.with_slot(sid, true, |s, desc| {
            if s.running {
                Self::update_pos(&self.hw, s, desc);
            }
            let fb = s.layout.frame_bytes as u64;
            let ring = s.layout.ring_bytes as u64;
            if s.written < s.pos.played {
                s.written = s.pos.played;
            }
            let free = ring - (s.written - s.pos.played).min(ring);
            let n = (data.len() as u64).min(free) / fb * fb;
            let mut done = 0u64;
            while done < n {
                let at = (s.written + done) % ring;
                let chunk = (ring - at).min(n - done);
                s.data.copy_in(at as usize, &data[done as usize..(done + chunk) as usize]);
                done += chunk;
            }
            s.data.barrier();
            s.written += n;
            n as usize
        })
    }

    pub fn set_period_callback(&self, sid: StreamId, f: Option<PeriodFn>) -> Result<(), HdaError> {
        self.with_slot(sid, true, |s, _| s.callback = f)
    }

    pub fn start(&self, sid: StreamId) -> Result<(), HdaError> {
        let ioc = self.irq_driven();
        self.with_slot(sid, true, |s, desc| {
            if !s.running {
                stream::run(&self.hw, desc, true, ioc);
                s.running = true;
                klog::event!(Debug, "snd", "hda.start", ctrl = self.name.as_str(), stream = sid, written = s.written);
            }
        })
    }

    pub fn stop(&self, sid: StreamId) -> Result<(), HdaError> {
        self.with_slot(sid, false, |s, desc| {
            if s.running {
                Self::update_pos(&self.hw, s, desc);
                stream::run(&self.hw, desc, false, false);
                s.running = false;
                klog::event!(Debug, "snd", "hda.stop", ctrl = self.name.as_str(), stream = sid, played = s.pos.played / s.layout.frame_bytes as u64);
            }
        })
    }

    /// Stops and resets the descriptor, takes the converter off the stream, all under the slot lock (an open on
    /// the same converter waits for it). The ring goes back to MM only once the descriptor is seen stopped; else
    /// it is kept until the next link reset.
    pub fn close(&self, sid: StreamId) -> Result<(), HdaError> {
        let mut slots = self.slots.lock();
        let n = (sid & 0xFF) as usize;
        let Some(x) = slots.get_mut(n).filter(|x| x.as_ref().is_some_and(|s| s.id == sid)) else {
            return Err(HdaError::NoSuchStream);
        };
        let desc = self.desc_of(n);
        stream::run(&self.hw, desc, false, false);
        let reset_ok = stream::reset(&self.hw, desc);
        let ctl = self.hw.rd8(sd(desc) + SD_CTL);
        let running = ctl != 0xFF && ctl & SD_CTL_RUN != 0;
        let Some(s) = x.take() else { return Err(HdaError::NoSuchStream) };
        // A quiesced controller has its link in reset: no codec to talk to (bring-up sets the codec up anew).
        if self.state() == State::Up {
            if let Err(e) = self.clear_converter(s.out.cad, s.converter) {
                klog::event!(Warn, "snd", "hda.close_codec_failed", ctrl = self.name.as_str(), stream = sid,
                    converter = klog::Hex(s.converter as u64), error = alloc::format!("{e:?}"));
            }
        }
        drop(slots);
        klog::event!(Info, "snd", "hda.close", ctrl = self.name.as_str(), stream = sid, played = s.pos.played / s.layout.frame_bytes as u64,
            underruns = s.underruns, lpib_bogus = s.pos.bogus, reset = reset_ok);
        if running {
            klog::event!(Warn, "snd", "hda.close_stuck", ctrl = self.name.as_str(), stream = sid, desc = desc, ctl = klog::Hex(ctl as u64));
            self.keep(s._bdl, "bdl");
            self.keep(s.data, "ring");
        }
        Ok(())
    }

    /// Takes the converter off its stream tag (`SET_CHANNEL_STREAMID` 0).
    fn clear_converter(&self, cad: u8, converter: u8) -> Result<(), HdaError> {
        self.run_actions(cad, &[Action::Set { nid: converter, verb: hdacodec::verb::set(hdacodec::verb::SET_CHANNEL_STREAMID, 0) }])
    }

    /// Frames the hardware has played since the stream was opened.
    pub fn position(&self, sid: StreamId) -> Result<u64, HdaError> {
        self.with_slot(sid, false, |s, desc| {
            if s.running {
                Self::update_pos(&self.hw, s, desc);
            }
            s.pos.played / s.layout.frame_bytes as u64
        })
    }

    /// Frames between the write position and the speaker: queued in the ring plus the controller's FIFO
    /// (AMD: 32 frames, less right after start, Linux `azx_get_pos_fifo`; others: the descriptor's FIFOS).
    pub fn delay(&self, sid: StreamId) -> Result<u32, HdaError> {
        let fifo = self.kind.fifo_frames();
        self.with_slot(sid, false, |s, desc| {
            if s.running {
                Self::update_pos(&self.hw, s, desc);
            }
            let fb = s.layout.frame_bytes as u64;
            let queued = s.written.saturating_sub(s.pos.played) / fb;
            let hw = match fifo {
                Some(f) => (f as u64).min(s.pos.played / fb),
                None => {
                    let fifos = self.hw.rd16(sd(desc) + SD_FIFOS);
                    if fifos == 0xFFFF { 0 } else { (fifos as u64 + 1) / fb }
                }
            };
            (queued + hw).min(u32::MAX as u64) as u32
        })
    }

    /// (ring frames, period frames) of an open stream.
    pub fn stream_layout(&self, sid: StreamId) -> Result<(u32, u32), HdaError> {
        self.with_slot(sid, false, |s, _| (s.layout.ring_frames(), s.layout.period_frames()))
    }

    fn update_pos(hw: &H, s: &mut Slot<H::Mem>, desc: usize) -> u32 {
        let raw = hw.rd32(sd(desc) + SD_LPIB);
        s.pos.update(raw, s.layout.ring_bytes)
    }

    /// Completion interrupts on (MSI armed) or off (polled). Rings of streams opened before keep their IOC bits.
    pub fn set_irq_driven(&self, on: bool) {
        self.irq_driven.store(on, Ordering::Release);
        let outs: u32 = (0..self.gcap.oss).map(|n| 1u32 << self.gcap.output(n)).fold(0, |a, b| a | b);
        let streams = outs | (0..self.gcap.iss).map(|n| 1u32 << n).fold(0, |a, b| a | b);
        self.hw.wr32(INTCTL, if on { INT_GLOBAL_EN | INT_CTRL_EN | streams } else { 0 });
        klog::event!(Info, "snd", "hda.irq_mode", ctrl = self.name.as_str(), mode = if on { "msi" } else { "polled" });
    }

    /// The hard-IRQ hook: acknowledges every stream and the RIRB so the line goes quiet (an unacknowledged
    /// SD_STS would storm), notes the causes. Lock-free, no heap, no klog.
    pub fn irq_ack(&self) -> u32 {
        let hw = &self.hw;
        let mut seen = 0;
        for _ in 0..4 {
            let st = hw.rd32(INTSTS);
            if st == 0 || st == !0 {
                break;
            }
            for i in 0..self.gcap.total().min(30) {
                if st & 1 << i != 0 {
                    hw.wr8(sd(i) + SD_STS, SD_STS_MASK);
                }
            }
            if st & INT_CTRL_EN != 0 {
                hw.wr8(RIRBSTS, RIRBSTS_MASK);
            }
            seen |= st;
        }
        if seen != 0 {
            self.pending.fetch_or(seen, Ordering::AcqRel);
            self.irqs.fetch_add(1, Ordering::AcqRel);
        }
        seen
    }

    /// The service thread's round: unsolicited responses (jack events), positions, period callbacks, errors,
    /// polled jacks. Bounded: every lock is taken with a deadline, every verb has one.
    pub fn service(&self) {
        self.log_quiesce_note();
        if self.state() != State::Up {
            return;
        }
        self.pending.swap(0, Ordering::AcqRel);
        let mut unsol = Vec::new();
        if let Some(mut g) = self.link.lock_timeout(2_000_000) {
            if let Some(l) = g.as_mut() {
                l.drain(&self.hw, None);
                unsol = core::mem::take(&mut l.unsol);
            }
        }
        for u in unsol {
            self.on_unsol(u);
        }
        let now = self.hw.now_ns();
        if now >= self.next_jack_poll.load(Ordering::Acquire) {
            self.next_jack_poll.store(now + JACK_POLL_NS, Ordering::Release);
            self.poll_jacks();
        }
        let mut calls: Vec<(PeriodFn, StreamId, u64)> = Vec::new();
        if let Some(mut slots) = self.slots.lock_timeout(2_000_000) {
            for (n, s) in slots.iter_mut().enumerate().filter_map(|(n, s)| s.as_mut().map(|s| (n, s))) {
                if !s.running {
                    continue;
                }
                let desc = self.desc_of(n);
                Self::update_pos(&self.hw, s, desc);
                let sts = self.hw.rd8(sd(desc) + SD_STS);
                if sts != 0xFF && sts & (SD_STS_FIFOE | SD_STS_DESE) != 0 {
                    self.hw.wr8(sd(desc) + SD_STS, sts & (SD_STS_FIFOE | SD_STS_DESE));
                    if s.errors < 8 {
                        s.errors += 1;
                        klog::event!(Warn, "snd", "hda.stream_error", ctrl = self.name.as_str(), stream = s.id, fifo = sts & SD_STS_FIFOE != 0,
                            descriptor = sts & SD_STS_DESE != 0);
                    }
                }
                if !s.input && s.pos.played > s.written {
                    // Underrun: the ring is replaying old samples. Silence it and go on from the play position.
                    s.underruns += 1;
                    s.data.fill(0, s.layout.ring_bytes as usize, 0);
                    s.data.barrier();
                    s.written = s.pos.played;
                    if s.underruns == 1 || s.underruns.is_power_of_two() {
                        klog::event!(Warn, "snd", "hda.underrun", ctrl = self.name.as_str(), stream = s.id, count = s.underruns);
                    }
                }
                let periods = s.pos.played / s.layout.period_bytes as u64;
                if periods != s.periods_seen {
                    s.periods_seen = periods;
                    if let Some(f) = &s.callback {
                        calls.push((f.clone(), s.id, s.pos.played / s.layout.frame_bytes as u64));
                    }
                }
            }
        }
        for (f, sid, frames) in calls {
            f(sid, frames);
        }
    }

    /// How long the service thread may sleep: a quarter period of the shortest running stream, else 100 ms.
    pub fn service_interval_ns(&self) -> u64 {
        let Some(slots) = self.slots.try_lock() else { return 1_000_000 };
        slots
            .iter()
            .flatten()
            .filter(|s| s.running)
            .map(|s| s.layout.period_frames() as u64 * 1_000_000_000 / s.fmt.rate.max(1) as u64 / 4)
            .min()
            .unwrap_or(100_000_000)
            .clamp(500_000, 100_000_000)
    }

    fn on_unsol(&self, u: Unsol) {
        let Some(c) = self.codecs().into_iter().find(|c| c.cad == u.cad) else {
            klog::event!(Debug, "snd", "hda.unsol_unknown", ctrl = self.name.as_str(), cad = u.cad, res = klog::Hex(u.res as u64));
            return;
        };
        let Some(&(_, pin)) = c.tags.iter().find(|t| t.0 == u.tag()) else {
            klog::event!(Debug, "snd", "hda.unsol_unknown", ctrl = self.name.as_str(), cad = u.cad, res = klog::Hex(u.res as u64));
            return;
        };
        self.recheck_jack(&c, pin, "unsol");
    }

    fn poll_jacks(&self) {
        if self.read_only() {
            return;
        }
        for c in self.codecs() {
            for o in c.outputs.iter().filter(|o| o.jack.is_some_and(|j| !j.unsol)) {
                self.recheck_jack(&c, o.pin, "poll");
            }
        }
    }

    fn recheck_jack(&self, c: &CodecEntry, pin: u8, cause: &str) {
        let Some(j) = c.outputs.iter().find(|o| o.pin == pin).and_then(|o| o.jack) else { return };
        let now = {
            let Some(mut g) = self.link.lock_timeout(2_000_000) else { return };
            let Some(l) = g.as_mut() else { return };
            self.sense(l, c.cad, &j, false)
        };
        let id = OutputId { cad: c.cad, pin };
        let changed = {
            let mut jacks = self.jacks.lock();
            match jacks.iter_mut().find(|x| x.0 == id) {
                Some(x) if x.1 != now => {
                    x.1 = now;
                    true
                }
                _ => false,
            }
        };
        if changed && let Some(p) = now {
            klog::event!(Info, "snd", "hda.jack", ctrl = self.name.as_str(), cad = c.cad, pin = klog::Hex(pin as u64),
                plugged = if p { "yes" } else { "no" }, cause = cause);
            let fns = self.jack_fns.lock().clone();
            for f in fns {
                f(id, p);
            }
        }
    }

    /// Sends one verb (ktests, the dump tool). Thread context.
    pub fn verb(&self, cad: u8, nid: u8, verb: u32) -> Result<u32, CmdError> {
        let mut g = self.link.lock();
        let l = g.as_mut().ok_or(CmdError::Bad)?;
        l.cmd(&self.hw, cad, nid, verb)
    }

    /// Stop for Reload, reboot and panic: every stream and the rings stopped, interrupts off, the link in
    /// reset, then MSI and bus master off. Lock-free, bounded, no klog; the event comes from a note later.
    pub fn quiesce(&self) {
        let was = self.state.swap(State::Quiesced as u8, Ordering::AcqRel);
        self.irq_driven.store(false, Ordering::Release);
        let hw = &self.hw;
        if hw.rd32(GCTL) != !0 {
            link::stop_all(hw, self.gcap.total());
            hw.wr32(GCTL, hw.rd32(GCTL) & !(GCTL_CRST | GCTL_UNSOL));
        }
        hw.pci_off();
        self.quiesce_note.store(0x80 | was, Ordering::Release);
    }

    fn log_quiesce_note(&self) {
        let n = self.quiesce_note.swap(0, Ordering::AcqRel);
        if n & 0x80 != 0 {
            klog::event!(Info, "snd", "hda.quiesced", ctrl = self.name.as_str(), was = State::from(n & 0x7F).name());
        }
    }
}

pub fn kind_name(k: OutputKind) -> &'static str {
    match k {
        OutputKind::LineOut => "line_out",
        OutputKind::Speaker => "speaker",
        OutputKind::Headphone => "headphone",
        OutputKind::Digital => "digital",
    }
}

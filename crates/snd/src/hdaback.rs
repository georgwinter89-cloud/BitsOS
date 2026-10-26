//! The mixer's back end on `hda` (same lane): endpoints from `Hda::pick`, one 48 kHz 16-bit stereo stream
//! per open, the period callback wakes the mixer, plug events (and the HDMI output appearing or going, SN-5)
//! re-route `Target::Auto` and `Target::Prefer`.

use crate::backend::{Backend, Endpoint, OutError, Port, Stream, Target, WakeFn};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::audio::PcmFormat;
use hda::hw::Hw;
use hda::{Controller, Hda, HdaError, OutputId, StreamId};

const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };

/// What holds a controller: the kernel's `Hda` (MMIO), or a bare `Controller` over the register-level fake in
/// host tests, so the error translation below is tested on the real driver code.
pub(crate) trait Holder: Send + Sync + 'static {
    type H: Hw;
    fn ctrl(&self) -> &Controller<Self::H>;
}

impl Holder for Hda {
    type H = hda::MmioHw;
    fn ctrl(&self) -> &Controller<hda::MmioHw> {
        &self.ctrl
    }
}

impl<H: Hw + Send + Sync + 'static> Holder for Controller<H> {
    type H = H;
    fn ctrl(&self) -> &Controller<H> {
        self
    }
}

pub(crate) fn map(e: HdaError) -> OutError {
    match e {
        HdaError::Busy => OutError::Busy,
        HdaError::NotUp => OutError::Gone,
        HdaError::Refused => OutError::Refused,
        HdaError::NoSuchOutput | HdaError::NoSuchStream => OutError::NoSuch,
        _ => OutError::Other,
    }
}

pub struct HdaBackend {
    jack: Arc<AtomicBool>,
    kick: WakeFn,
    registered: Vec<String>,
    hdmi_gen: u32,
}

/// One controller's outputs as `Target::Auto` weighs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cand {
    /// Its jack pick is a plugged headphone.
    pub headphone: bool,
    pub jack: bool,
    /// A playable HDMI output and whether its monitor is there.
    pub hdmi: Option<Option<bool>>,
}

/// `Target::Auto` over the controllers in probe order -> (index, port). Headphones win (Linux's
/// `hda_generic.c` headphone automute); then HDMI with its monitor sensed; then the analog outputs; then HDMI.
pub fn auto_pick(c: &[Cand]) -> Option<(usize, Port)> {
    let first = |f: &dyn Fn(&Cand) -> bool| c.iter().position(f);
    if let Some(i) = first(&|x| x.headphone) {
        return Some((i, Port::Jack));
    }
    if let Some(i) = first(&|x| x.hdmi == Some(Some(true))) {
        return Some((i, Port::Hdmi));
    }
    if let Some(i) = first(&|x| x.jack) {
        return Some((i, Port::Jack));
    }
    first(&|x| x.hdmi.is_some()).map(|i| (i, Port::Hdmi))
}

impl HdaBackend {
    pub fn new(kick: WakeFn) -> HdaBackend {
        HdaBackend { jack: Arc::new(AtomicBool::new(false)), kick, registered: Vec::new(), hdmi_gen: hda::hdmi::outputs_gen() }
    }

    /// Controllers probe after the mixer may have started: plug callbacks are added as they appear.
    fn controllers(&mut self) -> Vec<Arc<Hda>> {
        let list = hda::controllers();
        for d in &list {
            let name = d.ctrl.name();
            if d.ctrl.read_only() || self.registered.iter().any(|r| r == name) {
                continue;
            }
            self.registered.push(name.into());
            let (flag, kick) = (self.jack.clone(), self.kick.clone());
            d.ctrl.on_jack(Arc::new(move |_, _| {
                flag.store(true, Ordering::Release);
                kick();
            }));
        }
        list
    }
}

/// hda's pick for a mixer port (USB is not hda's).
fn pick(d: &Hda, p: Port) -> Option<OutputId> {
    d.pick(p.hw()?)
}

fn endpoint(d: &Hda, port: Port, o: OutputId) -> Endpoint {
    Endpoint { dev: d.ctrl.name().into(), port, cad: o.cad, pin: o.pin }
}

fn cand(d: &Hda) -> Cand {
    let jack = pick(d, Port::Jack);
    let headphone = jack.is_some_and(|id| {
        d.ctrl.outputs().iter().any(|o| o.id == id && o.kind == hdacodec::path::OutputKind::Headphone && o.plugged == Some(true))
    });
    let hdmi = pick(d, Port::Hdmi).map(|id| d.ctrl.jack_present(id));
    Cand { headphone, jack: jack.is_some(), hdmi }
}

fn auto(list: &[Arc<Hda>]) -> Option<Endpoint> {
    let cands: Vec<Cand> = list.iter().map(|d| cand(d)).collect();
    let (i, port) = auto_pick(&cands)?;
    pick(&list[i], port).map(|o| endpoint(&list[i], port, o))
}

impl Backend for HdaBackend {
    fn resolve(&mut self, t: &Target) -> Option<Endpoint> {
        let list = self.controllers();
        match t {
            Target::Auto => auto(&list),
            Target::Prefer(port) => list.iter().find_map(|d| pick(d, *port).map(|o| endpoint(d, *port, o))).or_else(|| auto(&list)),
            Target::Fixed { dev, port } => list.iter().find(|d| d.ctrl.name() == dev).and_then(|d| pick(d, *port).map(|o| endpoint(d, *port, o))),
        }
    }

    fn endpoints(&mut self) -> Vec<Endpoint> {
        let mut v = Vec::new();
        for d in self.controllers() {
            for port in [Port::Jack, Port::Hdmi] {
                if let Some(o) = pick(&d, port) {
                    v.push(endpoint(&d, port, o));
                }
            }
        }
        v
    }

    fn open(&mut self, e: &Endpoint, ring: u32, period: u32, wake: WakeFn, _now: u64) -> Result<Box<dyn Stream>, OutError> {
        let d = self.controllers().into_iter().find(|d| d.ctrl.name() == e.dev).ok_or(OutError::NoSuch)?;
        open_on(d, e, ring, period, wake)
    }

    fn take_jack_events(&mut self) -> bool {
        let g = hda::hdmi::outputs_gen();
        let hdmi = g != self.hdmi_gen;
        self.hdmi_gen = g;
        self.jack.swap(false, Ordering::AcqRel) | hdmi
    }
}

/// Opens one 48 kHz stream for `e` on `d` with the period callback kicking the mixer.
pub(crate) fn open_on<D: Holder>(d: Arc<D>, e: &Endpoint, ring: u32, period: u32, wake: WakeFn) -> Result<Box<dyn Stream>, OutError> {
    let c = d.ctrl();
    let sid = c.open(OutputId { cad: e.cad, pin: e.pin }, F48, ring, period).map_err(map)?;
    let setup = c.set_period_callback(sid, Some(Arc::new(move |_, _| wake()))).and_then(|_| c.stream_layout(sid));
    match setup {
        Ok((ring_frames, _)) => Ok(Box::new(HdaStream { d, sid, ring_frames, bytes: Vec::new() })),
        Err(x) => {
            let _ = c.close(sid);
            Err(map(x))
        }
    }
}

struct HdaStream<D: Holder> {
    d: Arc<D>,
    sid: StreamId,
    ring_frames: u32,
    bytes: Vec<u8>,
}

impl<D: Holder> Stream for HdaStream<D> {
    fn write(&mut self, frames: &[[i16; 2]]) -> Result<usize, OutError> {
        self.bytes.clear();
        for f in frames {
            self.bytes.extend_from_slice(&f[0].to_le_bytes());
            self.bytes.extend_from_slice(&f[1].to_le_bytes());
        }
        self.d.ctrl().write(self.sid, &self.bytes).map(|b| b / 4).map_err(map)
    }
    fn start(&mut self) -> Result<(), OutError> {
        self.d.ctrl().start(self.sid).map_err(map)
    }
    fn stop(&mut self) {
        let _ = self.d.ctrl().stop(self.sid);
    }
    fn position(&mut self, _now: u64) -> Result<u64, OutError> {
        self.d.ctrl().position(self.sid).map_err(map)
    }
    fn ring_frames(&self) -> u32 {
        self.ring_frames
    }
    fn close(self: Box<Self>) {
        let _ = self.d.ctrl().set_period_callback(self.sid, None);
        let _ = self.d.ctrl().close(self.sid);
    }
}

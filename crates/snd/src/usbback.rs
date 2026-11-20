//! The mixer's USB back end (SN-8b) on `usb-audio`, and the kernel mixer's back end `SysBackend`: `hda` plus
//! USB. A USB headset that is plugged in wins `Target::Auto` like a plugged headphone (it is the newest output
//! someone chose to wear); unplugged, its stream ends `Gone`, the mixer drops it and `Auto` falls back to what
//! hda offers. Outputs coming or going kick the mixer (`usb_audio::on_change`), so it re-routes at once.

use crate::backend::{Backend, Endpoint, OutError, Port, Stream, Target, WakeFn};
use crate::hdaback::HdaBackend;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use usb_core::UsbError;

pub(crate) fn map(e: UsbError) -> OutError {
    match e {
        UsbError::Gone | UsbError::NotRunning => OutError::Gone,
        UsbError::Busy => OutError::Busy,
        _ => OutError::Other,
    }
}

pub struct UsbBackend {
    changed: Arc<AtomicBool>,
    /// Outputs that failed to open since the last plug change (key, interface): `Auto` passes over them.
    failed: Vec<(String, u8)>,
    seen: u64,
}

fn endpoint(o: &usb_audio::OutputInfo) -> Endpoint {
    Endpoint { dev: o.key.clone(), port: Port::Usb, cad: o.control, pin: o.iface }
}

impl UsbBackend {
    pub fn new(kick: WakeFn) -> UsbBackend {
        let changed = Arc::new(AtomicBool::new(false));
        let w = Arc::downgrade(&changed);
        usb_audio::on_change(Arc::new(move || match w.upgrade() {
            Some(f) => {
                f.store(true, Ordering::Release);
                kick();
                true
            }
            None => false,
        }));
        UsbBackend { changed, failed: Vec::new(), seen: usb_audio::generation() }
    }

    fn sync_gen(&mut self) {
        let g = usb_audio::generation();
        if g != self.seen {
            self.seen = g;
            self.failed.clear();
        }
    }

    /// Playable outputs, headsets before a board's jacks, stereo first; without the ones that just failed.
    fn usable(&mut self) -> Vec<usb_audio::OutputInfo> {
        self.sync_gen();
        usb_audio::outputs().into_iter().filter(|o| !self.failed.iter().any(|f| f.0 == o.key && f.1 == o.iface)).collect()
    }
}

/// `UsbBackend`'s pick over `list` (also the report page's, which has no back end of its own).
pub(crate) fn usb_resolve(list: &[usb_audio::OutputInfo], t: &Target) -> Option<Endpoint> {
    match t {
        Target::Fixed { dev, port: Port::Usb } => list.iter().find(|o| &o.key == dev).map(endpoint),
        Target::Fixed { .. } => None,
        // GP-SOUND AU-5: `jack` reaches here only when hda has no jack (the Gaming-PC): the board's USB jacks
        // then, the speaker first. Without board jacks (every UAC1 device, the Mini-PC) it is `Auto` as before.
        Target::Prefer(Port::Jack) => list.iter().find(|o| o.board).or(list.first()).map(endpoint),
        _ => list.first().map(endpoint),
    }
}

/// `Pair`'s order of asking its two back ends.
pub(crate) fn pair_resolve(t: &Target, first: &mut dyn FnMut(&Target) -> Option<Endpoint>, second: &mut dyn FnMut(&Target) -> Option<Endpoint>)
    -> Option<Endpoint>
{
    match t {
        Target::Auto | Target::Prefer(Port::Usb) => first(t).or_else(|| second(&Target::Auto)),
        Target::Prefer(Port::Jack) => second(t).or_else(|| first(t)),
        Target::Prefer(_) => second(t).or_else(|| first(&Target::Auto)),
        Target::Fixed { .. } => first(t).or_else(|| second(t)),
    }
}

impl Backend for UsbBackend {
    fn resolve(&mut self, t: &Target) -> Option<Endpoint> {
        usb_resolve(&self.usable(), t)
    }

    fn endpoints(&mut self) -> Vec<Endpoint> {
        usb_audio::outputs().iter().map(endpoint).collect()
    }

    fn open(&mut self, e: &Endpoint, ring: u32, period: u32, wake: WakeFn, _now: u64) -> Result<Box<dyn Stream>, OutError> {
        match usb_audio::open_playback(&e.dev, e.pin, ring as usize, period, wake) {
            Ok(p) => Ok(Box::new(UsbStream(p))),
            Err(x) => {
                self.sync_gen();
                self.failed.push((e.dev.clone(), e.pin));
                Err(map(x))
            }
        }
    }

    fn take_jack_events(&mut self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }
}

pub(crate) struct UsbStream(pub(crate) usb_audio::Playback);

impl Stream for UsbStream {
    fn write(&mut self, frames: &[[i16; 2]]) -> Result<usize, OutError> {
        self.0.write(frames).map_err(map)
    }
    fn start(&mut self) -> Result<(), OutError> {
        self.0.start().map_err(map)
    }
    fn stop(&mut self) {
        self.0.stop()
    }
    fn position(&mut self, _now: u64) -> Result<u64, OutError> {
        self.0.position().map_err(map)
    }
    fn ring_frames(&self) -> u32 {
        self.0.ring_frames() as u32
    }
    fn close(self: Box<Self>) {
        self.0.close()
    }
}

/// Two back ends as one: `first` is asked first for `Auto` and `Prefer(Usb)`, `Fixed` goes to the one that
/// has it, `Prefer(Jack | Hdmi)` to `second` (then `first` if `second` has nothing at all: `Auto` for hdmi, `jack`
/// itself for jack, which `UsbBackend` takes as the board's USB jacks).
pub struct Pair<A: Backend, B: Backend> {
    pub first: A,
    pub second: B,
}

/// The kernel mixer's back end: USB headsets first, then hda.
pub type SysBackend = Pair<UsbBackend, HdaBackend>;

pub fn sys_backend(kick: WakeFn) -> SysBackend {
    Pair { first: UsbBackend::new(kick.clone()), second: HdaBackend::new(kick) }
}

impl<A: Backend, B: Backend> Backend for Pair<A, B> {
    fn resolve(&mut self, t: &Target) -> Option<Endpoint> {
        let (a, b) = (&mut self.first, &mut self.second);
        pair_resolve(t, &mut |x| a.resolve(x), &mut |x| b.resolve(x))
    }

    fn endpoints(&mut self) -> Vec<Endpoint> {
        let mut v = self.first.endpoints();
        v.extend(self.second.endpoints());
        v
    }

    fn open(&mut self, e: &Endpoint, ring: u32, period: u32, wake: WakeFn, now: u64) -> Result<Box<dyn Stream>, OutError> {
        if self.first.endpoints().contains(e) {
            self.first.open(e, ring, period, wake, now)
        } else {
            self.second.open(e, ring, period, wake, now)
        }
    }

    fn take_jack_events(&mut self) -> bool {
        let a = self.first.take_jack_events();
        let b = self.second.take_jack_events();
        a | b
    }
}

/// The master switch reaches the USB feature units' mute too (both channels off = muted). Thread context
/// (control transfers); never under `MASTER`. `switch` reads the current switch: usb-audio reads it under its
/// own mute lock, so two writers that race end with the hardware as the switch is now, whatever their order.
pub fn master_switch(switch: &dyn Fn() -> [bool; 2]) {
    usb_audio::set_hw_mute(&|| {
        let sw = switch();
        !sw[0] && !sw[1]
    });
}

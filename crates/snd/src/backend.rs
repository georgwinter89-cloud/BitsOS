//! What the mixer needs from the hardware: endpoints to open, streams of 48 kHz 16-bit stereo frames with a
//! play position. `hdaback` implements it on `hda`, `usbback` on `usb-audio`; host tests on a fake; `NullStream` is the time-driven
//! fallback when no output can be opened (programs keep their clock, nothing is heard).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Where an endpoint plays. `devmodel::audio::Output` knows only the controller ports; USB is the mixer's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    Jack,
    Hdmi,
    /// A USB audio device (SN-8b): `Endpoint::dev` is its port path (`usb:xhci0-1`), `cad` its AudioControl
    /// interface, `pin` its streaming interface.
    Usb,
}

impl Port {
    /// The controller port hda knows (None for USB).
    pub fn hw(self) -> Option<devmodel::audio::Output> {
        match self {
            Port::Jack => Some(devmodel::audio::Output::Jack),
            Port::Hdmi => Some(devmodel::audio::Output::Hdmi),
            Port::Usb => None,
        }
    }
}

/// An output the mixer can play to: a controller (`hda1`), its port, and the codec pin the back end picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub dev: String,
    pub port: Port,
    pub cad: u8,
    pub pin: u8,
}

impl Endpoint {
    pub fn name(&self) -> String {
        let port = match self.port {
            Port::Jack => "jack",
            Port::Hdmi => "hdmi",
            Port::Usb => "usb",
        };
        alloc::format!("{}/{}/{}:{:#x}", self.dev, port, self.cad, self.pin)
    }
}

/// Where the mixer should play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Follows plug state: a plugged headphone; else a playable HDMI output whose monitor is there (SN-5);
    /// else the first analog output (line out, else speaker); else any playable HDMI output.
    Auto,
    /// This port where one is playable (CONFIG/`set_output` `jack` or `hdmi`), else as `Auto`.
    Prefer(Port),
    /// This controller and port (under QEMU a second intel-hda); stays even when nothing plays there.
    Fixed { dev: String, port: Port },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutError {
    /// The converter plays another stream (speaker and headphone of the ALC269 share one).
    Busy,
    /// Controller quiesced, failed or gone.
    Gone,
    /// Refused by a rule (HDMI while the display side sends no audio, SN-5).
    Refused,
    NoSuch,
    Other,
}

/// Called from the back end's thread when a period completed: wakes the mixer.
pub type WakeFn = Arc<dyn Fn() + Send + Sync>;

pub trait Stream: Send {
    /// Takes up to `frames.len()` frames at the write position; returns how many.
    fn write(&mut self, frames: &[[i16; 2]]) -> Result<usize, OutError>;
    fn start(&mut self) -> Result<(), OutError>;
    fn stop(&mut self);
    /// Frames played since open.
    fn position(&mut self, now_ns: u64) -> Result<u64, OutError>;
    fn ring_frames(&self) -> u32;
    fn close(self: Box<Self>);
}

pub trait Backend: Send {
    /// The endpoint `t` means now (None: nothing playable there).
    fn resolve(&mut self, t: &Target) -> Option<Endpoint>;
    /// Every playable endpoint (status, tests).
    fn endpoints(&mut self) -> Vec<Endpoint>;
    fn open(&mut self, e: &Endpoint, ring: u32, period: u32, wake: WakeFn, now_ns: u64) -> Result<Box<dyn Stream>, OutError>;
    /// Whether a plug state changed since the last call.
    fn take_jack_events(&mut self) -> bool;
}

/// Plays into nothing at 48 kHz by the clock: the fallback without an output.
pub struct NullStream {
    ring: u32,
    start: Option<u64>,
    played_before: u64,
    written: u64,
}

impl NullStream {
    pub fn new(ring: u32) -> NullStream {
        NullStream { ring, start: None, played_before: 0, written: 0 }
    }
    fn played(&self, now: u64) -> u64 {
        let run = self.start.map_or(0, |s| (now.saturating_sub(s) as u128 * 48_000 / 1_000_000_000) as u64);
        self.played_before + run
    }
}

/// Null streams know the time only through `position`; `write` uses the last one seen.
pub struct NullOut {
    s: NullStream,
    now: u64,
}

impl NullOut {
    pub fn boxed(ring: u32, now: u64) -> Box<dyn Stream> {
        Box::new(NullOut { s: NullStream::new(ring), now })
    }
}

impl Stream for NullOut {
    fn write(&mut self, frames: &[[i16; 2]]) -> Result<usize, OutError> {
        let played = self.s.played(self.now);
        if self.s.written < played {
            self.s.written = played;
        }
        let free = self.s.ring as u64 - (self.s.written - played).min(self.s.ring as u64);
        let n = (frames.len() as u64).min(free);
        self.s.written += n;
        Ok(n as usize)
    }
    fn start(&mut self) -> Result<(), OutError> {
        if self.s.start.is_none() {
            self.s.start = Some(self.now);
        }
        Ok(())
    }
    fn stop(&mut self) {
        self.s.played_before = self.s.played(self.now);
        self.s.start = None;
    }
    fn position(&mut self, now_ns: u64) -> Result<u64, OutError> {
        self.now = self.now.max(now_ns);
        Ok(self.s.played(self.now))
    }
    fn ring_frames(&self) -> u32 {
        self.s.ring
    }
    fn close(self: Box<Self>) {}
}

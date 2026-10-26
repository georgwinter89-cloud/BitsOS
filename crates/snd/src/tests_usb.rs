//! The USB back end's stream (`usb_audio::Playback` over a fake isochronous pipe) and the hda + USB `Pair` in
//! the real mixer core: a headset plugged in while a client plays takes over (make before break), its packets
//! carry the signal in order; unplugged (requests end `Gone`) the mixer goes back to the jack without a stuck
//! client; plugged again it comes back. The jack is the clock-driven null stream under a jack endpoint.
extern crate std;

use crate::backend::{Backend, Endpoint, NullOut, OutError, Port, Stream, Target, WakeFn};
use crate::client::{self, State};
use crate::mixer::{Config, Core, Shared};
use crate::testsig::{self, frames};
use crate::usbback::{Pair, UsbStream};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use devmodel::audio::PcmFormat;
use std::collections::VecDeque;
use std::sync::Mutex;
use usb_audio::stream::{self, Layout, Pipe};
use usb_core::{Buffer, IsoDone, IsoRequest, IsoResult, UsbError, XferMem, XferResult};

const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };
const HP: &str = "hda0/jack/0:0x15";
const USB: &str = "usb:xhci0-1/usb/0:0x1";

struct Mem(Mutex<Vec<u8>>);
impl XferMem for Mem {
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    fn read(&self, off: usize, out: &mut [u8]) -> bool {
        let m = self.0.lock().unwrap();
        m.get(off..off + out.len()).map(|s| out.copy_from_slice(s)).is_some()
    }
    fn write(&self, off: usize, data: &[u8]) -> bool {
        let mut m = self.0.lock().unwrap();
        m.get_mut(off..off + data.len()).map(|s| s.copy_from_slice(data)).is_some()
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[derive(Default)]
struct PipeState {
    window: usize,
    queued: AtomicUsize,
    pending: Mutex<VecDeque<(IsoRequest, IsoDone)>>,
    sent: Mutex<Vec<u8>>,
}

#[derive(Clone)]
struct FakePipe(Arc<PipeState>);

impl Pipe for FakePipe {
    fn room(&self) -> usize {
        self.0.window - self.0.queued.load(Ordering::Acquire)
    }
    fn queued(&self) -> usize {
        self.0.queued.load(Ordering::Acquire)
    }
    fn window(&self) -> usize {
        self.0.window
    }
    fn max_packet(&self) -> usize {
        288
    }
    fn interval_ns(&self) -> u64 {
        1_000_000
    }
    fn queue(&self, req: IsoRequest, done: IsoDone) -> Result<(), UsbError> {
        assert!(req.packets.len() <= self.room());
        self.0.queued.fetch_add(req.packets.len(), Ordering::AcqRel);
        self.0.pending.lock().unwrap().push_back((req, done));
        Ok(())
    }
}

impl FakePipe {
    /// The bus sends the oldest request (`error` None) or ends all with `error`.
    fn complete(&self, all: bool, error: Option<UsbError>) {
        loop {
            let Some((req, done)) = self.0.pending.lock().unwrap().pop_front() else { return };
            let mut res = Vec::new();
            for p in &req.packets {
                let mut b = alloc::vec![0; p.len];
                req.buf.read(p.offset, &mut b);
                if error.is_none() {
                    self.0.sent.lock().unwrap().extend_from_slice(&b);
                }
                res.push(XferResult::ok(p.len));
            }
            self.0.queued.fetch_sub(req.packets.len(), Ordering::AcqRel);
            done(IsoResult { packets: res, error });
            if !all {
                return;
            }
        }
    }
}

fn hp() -> Endpoint {
    Endpoint { dev: "hda0".into(), port: Port::Jack, cad: 0, pin: 0x15 }
}
fn usb() -> Endpoint {
    Endpoint { dev: "usb:xhci0-1".into(), port: Port::Usb, cad: 0, pin: 1 }
}

struct Jack;
impl Backend for Jack {
    fn resolve(&mut self, _: &Target) -> Option<Endpoint> {
        Some(hp())
    }
    fn endpoints(&mut self) -> Vec<Endpoint> {
        alloc::vec![hp()]
    }
    fn open(&mut self, _: &Endpoint, ring: u32, _: u32, _: WakeFn, now: u64) -> Result<Box<dyn Stream>, OutError> {
        Ok(NullOut::boxed(ring, now))
    }
    fn take_jack_events(&mut self) -> bool {
        false
    }
}

#[derive(Clone, Default)]
struct Plug {
    present: Arc<AtomicBool>,
    changed: Arc<AtomicBool>,
    pipes: Arc<Mutex<Vec<FakePipe>>>,
}

struct UsbFake(Plug);
impl Backend for UsbFake {
    fn resolve(&mut self, t: &Target) -> Option<Endpoint> {
        let here = self.0.present.load(Ordering::Acquire);
        match t {
            Target::Fixed { port, .. } if *port != Port::Usb => None,
            _ => here.then(usb),
        }
    }
    fn endpoints(&mut self) -> Vec<Endpoint> {
        if self.0.present.load(Ordering::Acquire) { alloc::vec![usb()] } else { Vec::new() }
    }
    fn open(&mut self, _: &Endpoint, ring: u32, period: u32, wake: WakeFn, _: u64) -> Result<Box<dyn Stream>, OutError> {
        let (window_ns, req) = stream::sizing(period, 1_000_000);
        let pipe = FakePipe(Arc::new(PipeState { window: (window_ns / 1_000_000) as usize, ..Default::default() }));
        self.0.pipes.lock().unwrap().push(pipe.clone());
        let alloc = |n: usize| -> Result<Buffer, UsbError> { Ok(Arc::new(Mem(Mutex::new(alloc::vec![0; n])))) };
        let p = usb_audio::Playback::open(Box::new(pipe), &alloc, Layout { channels: 2, subframe: 2 }, ring as usize, req, wake)
            .map_err(crate::usbback::map)?;
        Ok(Box::new(UsbStream(p)))
    }
    fn take_jack_events(&mut self) -> bool {
        self.0.changed.swap(false, Ordering::AcqRel)
    }
}

/// Right channel of `testsig::c`: frame number + 1. Checks each pipe's frames run in order without a repeat.
fn check_order(bytes: &[u8]) -> usize {
    let r: Vec<u16> = bytes.chunks_exact(4).map(|f| u16::from_le_bytes([f[2], f[3]])).filter(|&x| x != 0).collect();
    for w in r.windows(2) {
        // Frame 65535 carries 0 (filtered out with the silence): the counter steps over it.
        let want = if w[0] == 0xFFFF { 1 } else { w[0].wrapping_add(1) };
        assert_eq!(w[1], want, "a gap or repeat on the USB stream");
    }
    r.len()
}

#[test]
fn a_headset_takes_over_and_gives_back_on_unplug() {
    let plug = Plug::default();
    let sh = Arc::new(Shared::new());
    let mut core = Core::new(Pair { first: UsbFake(plug.clone()), second: Jack }, sh.clone(), Config::latency_ms(40));
    let mut now = 1_000_000_000u64;
    let mut tick = 0u64;
    let mut ms = |core: &mut Core<Pair<UsbFake, Jack>>, n: usize| {
        for _ in 0..n {
            now += 1_000_000;
            tick += 1;
            sh.test_now.store(now, Ordering::Release);
            if tick % 4 == 0 {
                let p = plug.pipes.lock().unwrap().last().cloned();
                if let Some(p) = p.filter(|_| plug.present.load(Ordering::Acquire)) {
                    p.complete(false, None);
                }
            }
            core.step(now);
        }
    };
    let x = client::open(&sh, F48, 480_000, "test").unwrap();
    assert_eq!(x.write_frames(&frames(testsig::c, 0, 480_000)), Ok(480_000));
    x.start();
    ms(&mut core, 300);
    assert_eq!(sh.status().endpoint.as_deref(), Some(HP));
    // Plugged in: the mixer hears of it and moves.
    plug.present.store(true, Ordering::Release);
    plug.changed.store(true, Ordering::Release);
    ms(&mut core, 600);
    assert_eq!(sh.status().endpoint.as_deref(), Some(USB));
    let first = plug.pipes.lock().unwrap()[0].clone();
    let n = check_order(&first.0.sent.lock().unwrap());
    assert!(n > 20_000, "the headset played {n} frames in 600 ms");
    // Pulled: queued requests end Gone, the output list changes.
    let before = x.position();
    plug.present.store(false, Ordering::Release);
    first.complete(true, Some(UsbError::Gone));
    plug.changed.store(true, Ordering::Release);
    ms(&mut core, 300);
    assert_eq!(sh.status().endpoint.as_deref(), Some(HP), "back on the jack");
    assert_eq!(x.state(), State::Running);
    assert!(x.position() > before + 9_600, "the client is not stuck: {} -> {}", before, x.position());
    // Plugged again: back on the headset, a fresh stream.
    plug.present.store(true, Ordering::Release);
    plug.changed.store(true, Ordering::Release);
    ms(&mut core, 400);
    assert_eq!(sh.status().endpoint.as_deref(), Some(USB));
    let pipes = plug.pipes.lock().unwrap().clone();
    assert_eq!(pipes.len(), 2);
    assert!(check_order(&pipes[1].0.sent.lock().unwrap()) > 10_000);
    assert_eq!(sh.status().switches, 3);
}

#[test]
fn pair_routes_targets() {
    let plug = Plug::default();
    let mut p = Pair { first: UsbFake(plug.clone()), second: Jack };
    assert_eq!(p.resolve(&Target::Auto), Some(hp()));
    assert_eq!(p.resolve(&Target::Prefer(Port::Usb)), Some(hp()), "no headset: auto");
    plug.present.store(true, Ordering::Release);
    assert_eq!(p.resolve(&Target::Auto), Some(usb()), "a headset wins auto");
    assert_eq!(p.resolve(&Target::Prefer(Port::Jack)), Some(hp()), "the jack when asked for");
    assert_eq!(p.resolve(&Target::Fixed { dev: "hda0".into(), port: Port::Jack }), Some(hp()));
    assert_eq!(p.endpoints(), alloc::vec![usb(), hp()]);
    assert!(!p.take_jack_events());
    plug.changed.store(true, Ordering::Release);
    assert!(p.take_jack_events());
    assert_eq!(crate::parse_output("usb"), Some(Target::Prefer(Port::Usb)));
    assert_eq!(usb().name(), USB);
}

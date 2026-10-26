//! The real `hdaback` translation over the register-level fake controller of `hda` (feature `fake`): a
//! Quiesce mid-stream makes the driver answer NotUp, the mixer falls back to the null output without a panic
//! and comes back on the controller after the next bring-up.
extern crate std;

use crate::backend::{Backend, Endpoint, OutError, Stream, Target, WakeFn};
use crate::client::{self, State};
use crate::hdaback::open_on;
use crate::mixer::{Config, Core, Shared};
use crate::testsig::{self, frames};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::backend::Port;
use devmodel::audio::PcmFormat;
use hda::fake::{FakeHda, FakeHw};
use hda::{Controller, Kind};

const ALC269: &str = include_str!("../../hdacodec/fixtures/alc269-synthetic.txt");
const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };
/// The first output stream descriptor of the QEMU-like GCAP 0x4401 (4 input engines first).
const OUT0: usize = 4;
const HP: &str = "hda0/jack/0:0x15";

struct OnFake(Arc<Controller<FakeHw>>);

fn hp() -> Endpoint {
    Endpoint { dev: "hda0".into(), port: Port::Jack, cad: 0, pin: 0x15 }
}

impl Backend for OnFake {
    fn resolve(&mut self, _: &Target) -> Option<Endpoint> {
        Some(hp())
    }
    fn endpoints(&mut self) -> Vec<Endpoint> {
        alloc::vec![hp()]
    }
    fn open(&mut self, e: &Endpoint, ring: u32, period: u32, wake: WakeFn, _: u64) -> Result<Box<dyn Stream>, OutError> {
        open_on(self.0.clone(), e, ring, period, wake)
    }
    fn take_jack_events(&mut self) -> bool {
        false
    }
}

struct Rig {
    f: Arc<FakeHda>,
    c: Arc<Controller<FakeHw>>,
    sh: Arc<Shared>,
    core: Core<OnFake>,
    now: u64,
}

impl Rig {
    fn new() -> Rig {
        let f = FakeHda::new(0x4401);
        f.add_codec(0, ALC269);
        let c = Arc::new(Controller::new("hda0".into(), Kind::Intel, FakeHw(f.clone()), hda::Config::default()));
        c.bring_up().unwrap();
        let sh = Arc::new(Shared::new());
        let core = Core::new(OnFake(c.clone()), sh.clone(), Config::latency_ms(40));
        Rig { f, c, sh, core, now: 1_000_000_000 }
    }
    /// 1 ms steps: the fake plays 48 frames, the driver's service round runs, then the mixer.
    fn ms(&mut self, ms: usize) {
        for _ in 0..ms {
            self.f.advance(48 * 4);
            self.c.service();
            self.now += 1_000_000;
            self.sh.test_now.store(self.now, core::sync::atomic::Ordering::Release);
            self.core.step(self.now);
        }
    }
}

#[test]
fn quiesce_mid_stream_falls_back_to_null_and_recovers() {
    let mut r = Rig::new();
    let x = client::open(&r.sh, F48, 96000, "test").unwrap();
    assert_eq!(x.write_frames(&frames(testsig::c, 0, 90000)), Ok(90000));
    x.start();
    r.ms(200);
    assert_eq!(r.sh.status().endpoint.as_deref(), Some(HP));
    assert!(r.sh.real_output());
    let sink = r.f.sd(OUT0).sink;
    assert!(sink.chunks(4).any(|b| b != [0; 4]), "nothing reached the fake controller");
    // Quiesce (as before a Reload): start/write/set_period_callback answer NotUp -> OutError::Gone.
    r.c.quiesce();
    r.ms(300);
    let st = r.sh.status();
    assert_eq!(st.endpoint, None, "still on the quiesced controller");
    assert!(!r.sh.real_output());
    assert_eq!(x.state(), State::Running);
    let p = x.position();
    r.ms(200);
    assert!(x.position() > p + 4800, "the null output does not keep time");
    // Up again: the retry (once a second) opens the controller again.
    r.c.bring_up().unwrap();
    r.ms(1500);
    assert_eq!(r.sh.status().endpoint.as_deref(), Some(HP));
    assert!(r.sh.real_output());
    assert!(r.sh.status().opens >= 2);
    drop(x);
    r.ms(2500);
    assert_eq!(r.sh.status().streams, 0, "the idle output did not close");
}

const AA01: &str = include_str!("../../hdacodec/fixtures/aa01-synthetic.txt");

/// SN-5: `Target::Auto` weighs headphones, then HDMI with its monitor, then analog, then HDMI.
#[test]
fn auto_policy() {
    use crate::hdaback::{Cand, auto_pick};
    let jack = Cand { headphone: false, jack: true, hdmi: None };
    let hp = Cand { headphone: true, ..jack };
    let hdmi = |sensed| Cand { headphone: false, jack: false, hdmi: Some(sensed) };
    assert_eq!(auto_pick(&[]), None);
    assert_eq!(auto_pick(&[jack]), Some((0, Port::Jack)));
    assert_eq!(auto_pick(&[hdmi(Some(true)), jack]), Some((0, Port::Hdmi)), "monitor there, nothing in the jack: HDMI");
    assert_eq!(auto_pick(&[hdmi(Some(true)), hp]), Some((1, Port::Jack)), "headphones win");
    assert_eq!(auto_pick(&[hdmi(None), jack]), Some((1, Port::Jack)), "HDMI without a sensed monitor: analog first");
    assert_eq!(auto_pick(&[hdmi(Some(false))]), Some((0, Port::Hdmi)), "nothing else: HDMI");
    assert_eq!(crate::parse_output("hdmi"), Some(Target::Prefer(Port::Hdmi)));
    assert_eq!(crate::parse_output("jack"), Some(Target::Prefer(Port::Jack)));
    assert_eq!(crate::parse_output("auto"), Some(Target::Auto));
    assert_eq!(crate::parse_output("HDMI"), None);
}

/// The output switch's refusals: only the three words (CONFIG trims before), and `hdmi` without HDMI sound
/// refuses before the mixer is touched (no GPU HD Audio function on the host, as in QEMU).
#[test]
fn output_switch_refusals() {
    for bad in ["", " ", "hdmi ", " jack", "Auto", "jack\n", "hdmi0", "speaker", "auto,hdmi"] {
        assert_eq!(crate::parse_output(bad), None, "{bad:?}");
    }
    assert_eq!(crate::parse_output(" hdmi\t".trim()), Some(Target::Prefer(Port::Hdmi)));
    let e = crate::set_output("bogus").unwrap_err();
    assert!(e.contains("is not auto, jack, hdmi or usb"), "{e}");
    // Host tests cannot start the mixer thread: the mixer is started first (SN-10 fixer), so HDMI sound is never
    // turned on for a switch that cannot happen. On the kernel without a GPU function it is hdmi_on's refusal.
    let e = crate::set_output("hdmi").unwrap_err();
    assert!(e == "NoMixer" || e == "HDMI audio: no GPU HD Audio function", "{e}");
    assert_eq!(hda::hdmi::write_calls(), 0, "nothing reached the display registers");
    assert_eq!(hda::hdmi::index_selects(), 0, "no endpoint index was selected either");
}

/// Jack and the GPU controller over the fake: HDMI appears (the display side on) while a client plays -> the
/// mixer moves to pin 3 of the GPU controller (make before break), HDMI goes again -> back on the jack, no panic.
struct TwoFake {
    jack: Arc<Controller<FakeHw>>,
    gpu: Arc<Controller<FakeHw>>,
    changed: Arc<core::sync::atomic::AtomicBool>,
}

fn hdmi_ep() -> Endpoint {
    Endpoint { dev: "hda1".into(), port: Port::Hdmi, cad: 0, pin: hda::DP_AUDIO_PIN }
}

impl Backend for TwoFake {
    fn resolve(&mut self, t: &Target) -> Option<Endpoint> {
        let hdmi = !self.gpu.read_only() && self.gpu.outputs().iter().any(|o| o.playable);
        match t {
            Target::Prefer(Port::Hdmi) if hdmi => Some(hdmi_ep()),
            _ => Some(hp()),
        }
    }
    fn endpoints(&mut self) -> Vec<Endpoint> {
        alloc::vec![hp(), hdmi_ep()]
    }
    fn open(&mut self, e: &Endpoint, ring: u32, period: u32, wake: WakeFn, _: u64) -> Result<Box<dyn Stream>, OutError> {
        if e.dev == "hda1" { open_on(self.gpu.clone(), e, ring, period, wake) } else { open_on(self.jack.clone(), e, ring, period, wake) }
    }
    fn take_jack_events(&mut self) -> bool {
        self.changed.swap(false, core::sync::atomic::Ordering::AcqRel)
    }
}

#[test]
fn switch_to_hdmi_and_back() {
    use core::sync::atomic::{AtomicBool, Ordering};
    let fj = FakeHda::new(0x4401);
    fj.add_codec(0, ALC269);
    let jack = Arc::new(Controller::new("hda0".into(), Kind::Intel, FakeHw(fj.clone()), hda::Config::default()));
    jack.bring_up().unwrap();
    let fg = FakeHda::new(0x4401);
    fg.add_codec(0, AA01);
    let gpu = Arc::new(Controller::new("hda1".into(), Kind::AtiHdmi, FakeHw(fg.clone()), hda::Config::default()));
    gpu.bring_up().unwrap();
    let changed = Arc::new(AtomicBool::new(false));
    let sh = Arc::new(Shared::new());
    let mut core = Core::new(TwoFake { jack: jack.clone(), gpu: gpu.clone(), changed: changed.clone() }, sh.clone(), Config::latency_ms(40));
    sh.set_target(Target::Prefer(Port::Hdmi));
    let mut now = 1_000_000_000u64;
    let mut ms = |core: &mut Core<TwoFake>, n: usize| {
        for _ in 0..n {
            fj.advance(48 * 4);
            fg.advance(48 * 4);
            jack.service();
            gpu.service();
            now += 1_000_000;
            sh.test_now.store(now, Ordering::Release);
            core.step(now);
        }
    };
    let x = client::open(&sh, F48, 192000, "test").unwrap();
    assert_eq!(x.write_frames(&frames(testsig::c, 0, 190000)), Ok(190000));
    x.start();
    ms(&mut core, 200);
    assert_eq!(sh.status().endpoint.as_deref(), Some(HP), "HDMI not there yet: Prefer falls back");
    // The display side on: the GPU controller streams, the mixer hears of it.
    gpu.set_streaming(true);
    gpu.bring_up().unwrap();
    changed.store(true, Ordering::Release);
    ms(&mut core, 300);
    assert_eq!(sh.status().endpoint.as_deref(), Some("hda1/hdmi/0:0x3"));
    assert!(fg.sd(OUT0).sink.chunks(4).any(|b| b != [0; 4]), "nothing reached the GPU controller");
    // HDMI off again (hdmi_off: back to enumerate-only): the stream is gone, the mixer goes back to the jack.
    gpu.set_streaming(false);
    gpu.bring_up().unwrap();
    changed.store(true, Ordering::Release);
    ms(&mut core, 1500);
    assert_eq!(sh.status().endpoint.as_deref(), Some(HP));
    assert!(sh.real_output());
    assert_eq!(x.state(), State::Running);
}

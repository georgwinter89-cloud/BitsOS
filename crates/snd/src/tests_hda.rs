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

/// The controller and its name (the endpoint's `dev`).
struct OnFake(Arc<Controller<FakeHw>>);

fn hp_on(dev: &str) -> Endpoint {
    Endpoint { dev: dev.into(), port: Port::Jack, cad: 0, pin: 0x15 }
}

fn hp() -> Endpoint {
    Endpoint { dev: "hda0".into(), port: Port::Jack, cad: 0, pin: 0x15 }
}

impl Backend for OnFake {
    fn resolve(&mut self, _: &Target) -> Option<Endpoint> {
        Some(hp_on(self.0.name()))
    }
    fn endpoints(&mut self) -> Vec<Endpoint> {
        alloc::vec![hp_on(self.0.name())]
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
        Rig::named("hda0")
    }
    /// A controller of its own name: the klog checks find its lines among the other tests'.
    fn named(name: &str) -> Rig {
        let f = FakeHda::new(0x4401);
        f.add_codec(0, ALC269);
        let c = Arc::new(Controller::new(name.into(), Kind::Intel, FakeHw(f.clone()), hda::Config::default()));
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

/// SN-13 (w25): with no client the mixer closes its output after `idle_ns` and the controller's stream with it
/// (open_streams 0, the descriptor stopped); the controller itself stays up (Linux with CachyOS's power_save=0 on
/// mains, PipeWire's idle suspend closes the PCM only), so the next client opens without a restart.
#[test]
fn idle_mixer_leaves_no_stream_open_on_the_controller() {
    let mut r = Rig::new();
    let x = client::open(&r.sh, F48, 9600, "video").unwrap();
    assert_eq!(x.write(&[0u8; 9600 * 4]), Ok(9600 * 4));
    x.start();
    r.ms(100);
    assert_eq!(r.c.open_streams(), 1);
    assert!(r.f.sd(OUT0).running());
    drop(x);
    r.ms(2600);
    assert_eq!(r.sh.status().streams, 0);
    assert_eq!(r.c.open_streams(), 0, "a stream left open on the controller");
    assert!(!r.f.sd(OUT0).running(), "stream DMA with no client");
    assert_eq!(r.c.state(), hda::State::Up, "the controller stays up");
    let y = client::open(&r.sh, F48, 9600, "next").unwrap();
    assert_eq!(y.write(&[0u8; 4800 * 4]), Ok(4800 * 4));
    y.start();
    r.ms(30);
    assert_eq!(r.c.open_streams(), 1, "reopened at once");
}

/// SN-7: the reboot's Quiesce order (`devmodel::quiesce_all`, newest first): the mixer's flag, then hda's
/// controller. The mixer's next round (during the reset's sleeps) closes its output without one register
/// access, nothing opens or starts again, the stream DMA stays stopped, and the link is left out of reset.
#[test]
fn reboot_order_closes_without_touching_the_controller() {
    use devmodel::Quiesce;
    let mut r = Rig::new();
    // Silence, as the muted video streams it.
    let x = client::open(&r.sh, F48, 96000, "video").unwrap();
    assert_eq!(x.write(&[0u8; 360_000]), Ok(360_000));
    x.start();
    r.ms(200);
    assert_eq!(r.sh.status().endpoint.as_deref(), Some(HP));
    assert!(r.f.sd(OUT0).running());
    let q = crate::thread::MixQuiesce(Arc::downgrade(&r.sh));
    assert_eq!(q.name(), "snd/mix");
    q.quiesce();
    r.c.quiesce();
    assert!(!r.f.sd(OUT0).running(), "stream DMA after Quiesce");
    let n = r.f.accesses.load(core::sync::atomic::Ordering::Acquire);
    // The thread's next wake: the flag, every output closed.
    assert!(crate::thread::stop_seen(&mut r.core, &r.sh));
    assert_eq!(r.sh.status().streams, 0, "an output still open");
    assert_eq!(r.f.accesses.load(core::sync::atomic::Ordering::Acquire), n, "a register access after Quiesce");
    assert!(!r.f.sd(OUT0).running());
    assert_ne!(r.f.reg32(hda::regs::GCTL) & hda::regs::GCTL_CRST, 0);
    drop(x);
}

/// The klog lines of controller `ctrl` named `names` since `seq`.
fn hda_lines(seq: u64, ctrl: &str, names: &[&str]) -> Vec<alloc::string::String> {
    let tag = alloc::format!("ctrl={ctrl} ");
    klog::read_since(seq, 1 << 20)
        .into_iter()
        .filter(|r| names.contains(&r.name) && alloc::format!("{} ", r.text).contains(&tag))
        .map(|r| r.render())
        .collect()
}

/// SN-13 (w25): after Quiesce the mixer's last round (the reboot's reset sleeps) closes its output without one log
/// line from hda (the console mirror's traffic in the reset window preceded the floods, desk 28.10.); before it, a
/// close logs `hda.close` as always (the control: the check sees the lines when they are there).
#[test]
fn closes_after_quiesce_log_nothing() {
    const QUIET: &[&str] = &["hda.close", "hda.stop", "hda.close_stuck", "hda.dma_kept"];
    klog::heap_ready();
    // The control: an idle close on a controller that is up logs `hda.close`.
    let mut a = Rig::named("hda-sn13-up");
    let seq = klog::next_seq();
    let x = client::open(&a.sh, F48, 9600, "video").unwrap();
    assert_eq!(x.write(&[0u8; 9600 * 4]), Ok(9600 * 4));
    x.start();
    a.ms(100);
    drop(x);
    a.ms(2600);
    assert_eq!(a.c.open_streams(), 0);
    let up = hda_lines(seq, "hda-sn13-up", &["hda.close"]);
    assert_eq!(up.len(), 1, "{up:?}");
    // Quiesced under a playing output: the mixer's closes log nothing.
    let mut r = Rig::named("hda-sn13-q");
    let x = client::open(&r.sh, F48, 96000, "video").unwrap();
    assert_eq!(x.write(&[0u8; 360_000]), Ok(360_000));
    x.start();
    r.ms(200);
    assert!(r.f.sd(OUT0).running());
    r.c.quiesce();
    let seq = klog::next_seq();
    drop(x);
    r.ms(3000);
    assert_eq!(r.sh.status().streams, 0, "an output still open");
    assert_eq!(r.c.open_streams(), 0, "a stream left open on the controller");
    let quiet = hda_lines(seq, "hda-sn13-q", QUIET);
    assert!(quiet.is_empty(), "lines after Quiesce: {quiet:?}");
}

/// SN-7: Quiesce without the mixer's flag first (a Quiesce from elsewhere): the mixer keeps retrying its
/// target, every retry is refused before a register access, the stream never runs again.
#[test]
fn quiesced_controller_is_never_restarted_by_the_mixer() {
    let mut r = Rig::new();
    let x = client::open(&r.sh, F48, 96000, "video").unwrap();
    assert_eq!(x.write(&[0u8; 360_000]), Ok(360_000));
    x.start();
    r.ms(200);
    assert!(r.f.sd(OUT0).running());
    r.c.quiesce();
    let n = r.f.accesses.load(core::sync::atomic::Ordering::Acquire);
    r.ms(3000);
    assert!(!r.f.sd(OUT0).running(), "the mixer started the stream again");
    assert_eq!(r.f.accesses.load(core::sync::atomic::Ordering::Acquire), n, "a register access after Quiesce");
    assert_eq!(r.sh.status().endpoint, None);
    drop(x);
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

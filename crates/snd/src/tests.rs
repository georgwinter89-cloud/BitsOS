//! Mixer core on a fake back end and a fake clock (1 ms steps, 48 frames per ms played). The fake stream
//! models QEMU's hda-codec: frames counted as played pass a delay line of `delay` frames before they reach
//! the sink, and a stop loses what is in it (QEMU 2048 frames, the AMD FIFO 32).
extern crate std;

use crate::backend::{Backend, Endpoint, OutError, Stream, Target, WakeFn};
use crate::client::{self, Playback, SndError, State, UNITY};
use crate::mixer::{Config, Core, LATE_WINDOW_NS, Shared, start_together};
use crate::testsig::{self, frames};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::backend::Port;
use devmodel::audio::PcmFormat;
use std::collections::VecDeque;
use std::sync::Mutex;

const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };

struct Sim {
    ep: Endpoint,
    conv: (String, u8),
    ring: usize,
    queue: VecDeque<[i16; 2]>,
    played: u64,
    running: bool,
    closed: bool,
    fifo: VecDeque<[i16; 2]>,
}

#[derive(Default)]
struct Hw {
    sims: Vec<Sim>,
    sinks: Vec<(String, Vec<[i16; 2]>)>,
    delay: usize,
    gone: bool,
    none: bool,
    hp: bool,
    jack: bool,
    /// The streams' positions stop (a controller that stopped counting).
    stall: bool,
    /// Only this controller's streams stop counting.
    stall_dev: Option<String>,
}

impl Hw {
    fn advance(&mut self, n: usize) {
        if self.stall {
            return;
        }
        let stalled = self.stall_dev.clone();
        for s in self.sims.iter_mut().filter(|s| s.running && !s.closed && stalled.as_deref() != Some(s.ep.dev.as_str())) {
            let name = s.ep.name();
            let i = match self.sinks.iter().position(|x| x.0 == name) {
                Some(i) => i,
                None => {
                    self.sinks.push((name, Vec::new()));
                    self.sinks.len() - 1
                }
            };
            for _ in 0..n {
                s.fifo.push_back(s.queue.pop_front().unwrap_or([0, 0]));
                s.played += 1;
                if s.fifo.len() > self.delay {
                    let f = s.fifo.pop_front().unwrap();
                    self.sinks[i].1.push(f);
                }
            }
        }
    }
    fn open_count(&self) -> usize {
        self.sims.iter().filter(|s| !s.closed).count()
    }
}

type HwRef = Arc<Mutex<Hw>>;

struct Fake(HwRef);

fn ep(dev: &str, pin: u8) -> Endpoint {
    Endpoint { dev: dev.into(), port: Port::Jack, cad: 0, pin }
}

impl Backend for Fake {
    fn resolve(&mut self, t: &Target) -> Option<Endpoint> {
        let hw = self.0.lock().unwrap();
        if hw.none {
            return None;
        }
        let dev0 = ep("dev0", if hw.hp { 0x15 } else { 0x14 });
        match t {
            Target::Auto => Some(dev0),
            Target::Fixed { dev, port: Port::Jack } if dev == "dev0" => Some(dev0),
            Target::Fixed { dev, port: Port::Jack } if dev == "dev1" => Some(ep("dev1", 3)),
            _ => None,
        }
    }
    fn endpoints(&mut self) -> Vec<Endpoint> {
        alloc::vec![ep("dev0", 0x14), ep("dev0", 0x15), ep("dev1", 3)]
    }
    fn open(&mut self, e: &Endpoint, ring: u32, _: u32, _: WakeFn, _: u64) -> Result<Box<dyn Stream>, OutError> {
        let mut hw = self.0.lock().unwrap();
        if hw.gone {
            return Err(OutError::Gone);
        }
        let conv = (e.dev.clone(), 2);
        if hw.sims.iter().any(|s| !s.closed && s.conv == conv) {
            return Err(OutError::Busy);
        }
        hw.sims.push(Sim { ep: e.clone(), conv, ring: ring as usize, queue: VecDeque::new(), played: 0, running: false, closed: false, fifo: VecDeque::new() });
        let id = hw.sims.len() - 1;
        Ok(Box::new(FakeStream(self.0.clone(), id)))
    }
    fn take_jack_events(&mut self) -> bool {
        core::mem::take(&mut self.0.lock().unwrap().jack)
    }
}

struct FakeStream(HwRef, usize);

impl Stream for FakeStream {
    fn write(&mut self, f: &[[i16; 2]]) -> Result<usize, OutError> {
        let mut hw = self.0.lock().unwrap();
        if hw.gone {
            return Err(OutError::Gone);
        }
        let s = &mut hw.sims[self.1];
        let n = f.len().min(s.ring - s.queue.len());
        s.queue.extend(&f[..n]);
        Ok(n)
    }
    fn start(&mut self) -> Result<(), OutError> {
        let mut hw = self.0.lock().unwrap();
        if hw.gone {
            return Err(OutError::Gone);
        }
        hw.sims[self.1].running = true;
        Ok(())
    }
    fn stop(&mut self) {
        let s = &mut self.0.lock().unwrap().sims[self.1];
        s.running = false;
        s.fifo.clear();
    }
    fn position(&mut self, _: u64) -> Result<u64, OutError> {
        Ok(self.0.lock().unwrap().sims[self.1].played)
    }
    fn ring_frames(&self) -> u32 {
        self.0.lock().unwrap().sims[self.1].ring as u32
    }
    fn close(self: Box<Self>) {
        let s = &mut self.0.lock().unwrap().sims[self.1];
        s.closed = true;
        s.running = false;
        s.fifo.clear();
    }
}

struct T {
    hw: HwRef,
    sh: Arc<Shared>,
    core: Core<Fake>,
    now: u64,
}

impl T {
    fn new(delay: usize) -> T {
        T::with_latency(delay, 40)
    }
    fn with_latency(delay: usize, ms: u32) -> T {
        let hw: HwRef = Arc::new(Mutex::new(Hw { delay, ..Default::default() }));
        let sh = Arc::new(Shared::new());
        let core = Core::new(Fake(hw.clone()), sh.clone(), Config::latency_ms(ms));
        T { hw, sh, core, now: 1_000_000_000 }
    }
    fn ms(&mut self, ms: usize) {
        for _ in 0..ms {
            self.hw.lock().unwrap().advance(48);
            self.now += 1_000_000;
            self.sh.test_now.store(self.now, core::sync::atomic::Ordering::Release);
            self.core.step(self.now);
        }
    }
    fn until(&mut self, limit_ms: usize, f: impl Fn() -> bool) -> usize {
        for i in 0..limit_ms {
            if f() {
                return i;
            }
            self.ms(1);
        }
        panic!("condition not met in {limit_ms} ms");
    }
    fn open(&self, fmt: PcmFormat, ring: u32) -> Playback {
        client::open(&self.sh, fmt, ring, "test").unwrap()
    }
    fn sink(&self, name: &str) -> Vec<[i16; 2]> {
        self.hw.lock().unwrap().sinks.iter().find(|s| s.0 == name).map(|s| s.1.clone()).unwrap_or_default()
    }
}

fn sound(v: &[[i16; 2]]) -> Vec<[i16; 2]> {
    v.iter().copied().filter(|f| *f != [0, 0]).collect()
}

const D0: &str = "dev0/jack/0:0x14";
const D0HP: &str = "dev0/jack/0:0x15";
const D1: &str = "dev1/jack/0:0x3";

#[test]
fn two_clients_exact_and_saturating() {
    let mut t = T::new(0);
    let (x, y) = (t.open(F48, 48000), t.open(F48, 48000));
    y.set_volume([24576, 24576]);
    assert_eq!(x.write_frames(&frames(testsig::a, 0, 9600)), Ok(9600));
    assert_eq!(y.write_frames(&frames(testsig::b, 0, 9600)), Ok(9600));
    t.ms(5); // the mixer has seen them (prepared): nothing opens yet
    assert_eq!(t.hw.lock().unwrap().open_count(), 0);
    start_together(&t.sh, &[&x, &y]);
    x.start_drain();
    y.start_drain();
    t.until(1000, || x.state() == State::Drained && y.state() == State::Drained);
    let s = sound(&t.sink(D0));
    assert_eq!(s.len(), 9600);
    for (i, f) in s.iter().enumerate() {
        assert_eq!(*f, testsig::mixed(i as u64, UNITY, 24576, [UNITY; 2]), "frame {i}");
    }
    assert!(s.iter().any(|f| f[0] == i16::MAX) && s.iter().any(|f| f[0] == i16::MIN), "no saturation seen");
    assert_eq!((x.position(), y.position()), (9600, 9600));
    assert_eq!(x.underruns().0, 0);
}

#[test]
fn master_volume_and_ramp() {
    let mut t = T::new(0);
    t.sh.set_master([16384, 8192]);
    let x = t.open(F48, 48000);
    x.write_frames(&frames(testsig::a, 0, 4800)).unwrap();
    x.start();
    x.start_drain();
    t.until(1000, || x.state() == State::Drained);
    let s = t.sink(D0);
    for (i, f) in s[..4800].iter().enumerate() {
        assert_eq!(*f, testsig::mixed(i as u64, UNITY, 0, [16384, 8192]), "frame {i}");
    }
    // A volume change ramps: from 10000 to 0 over at most 128 frames, never a jump.
    let mut t = T::new(0);
    let x = t.open(F48, 48000);
    x.write_frames(&alloc::vec![[10000, 10000]; 20000]).unwrap();
    x.start();
    t.ms(100);
    x.set_volume([0, 0]);
    t.ms(200);
    let s: Vec<i16> = t.sink(D0).iter().map(|f| f[0]).collect();
    let first = s.iter().position(|&v| v == 10000).unwrap();
    let down = first + s[first..].iter().position(|&v| v < 10000).unwrap();
    let zero = down + s[down..].iter().position(|&v| v == 0).unwrap();
    assert!(zero - down <= 128, "ramp of {} frames", zero - down);
    assert!(s[down - 1..=zero].windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] <= 10000 / 128 + 1), "not a smooth ramp");
}

#[test]
fn client_underrun_is_silence_and_counted() {
    let mut t = T::new(0);
    let x = t.open(F48, 48000);
    x.write_frames(&frames(testsig::c, 0, 960)).unwrap();
    x.start();
    t.ms(100);
    let (n, silent) = x.underruns();
    assert_eq!(n, 1);
    assert!(silent > 0);
    assert_eq!(sound(&t.sink(D0)).len(), 960);
    x.write_frames(&frames(testsig::c, 960, 960)).unwrap();
    t.ms(100);
    assert_eq!(x.underruns().0, 2);
    let s = sound(&t.sink(D0));
    assert_eq!(s, frames(testsig::c, 0, 1920));
    assert_eq!(x.state(), State::Running);
}

#[test]
fn hw_underrun_counted_no_replay() {
    let mut t = T::new(0);
    let x = t.open(F48, 48000);
    x.write_frames(&frames(testsig::c, 0, 24000)).unwrap();
    x.start();
    t.ms(50);
    t.hw.lock().unwrap().advance(48 * 200); // the mixer thread starved for 200 ms
    x.start_drain();
    t.until(2000, || x.state() == State::Drained);
    assert!(t.sh.status().hw_underruns >= 1);
    assert_eq!(sound(&t.sink(D0)), frames(testsig::c, 0, 24000), "a frame lost or replayed");
}

fn counters(v: &[[i16; 2]]) -> Vec<u64> {
    v.iter().filter(|f| **f != [0, 0]).map(|f| f[1] as u16 as u64).collect()
}

fn check_sine(v: &[[i16; 2]]) {
    for f in v.iter().filter(|f| **f != [0, 0]) {
        assert_eq!(*f, testsig::c(f[1] as u16 as u64 - 1), "sine and counter disagree");
    }
}

#[test]
fn switch_make_before_break_is_lossless() {
    let mut t = T::new(2048); // QEMU's hda-codec buffer
    t.sh.set_target(Target::Fixed { dev: "dev0".into(), port: Port::Jack });
    let x = t.open(F48, 60000);
    x.write_frames(&frames(testsig::c, 0, 30000)).unwrap();
    x.start();
    t.ms(200);
    t.sh.set_target(Target::Fixed { dev: "dev1".into(), port: Port::Jack });
    t.ms(200);
    assert_eq!(t.sh.status().endpoint.as_deref(), Some(D1));
    t.sh.set_target(Target::Fixed { dev: "dev0".into(), port: Port::Jack });
    x.start_drain();
    t.until(2000, || x.state() == State::Drained);
    t.ms(3000);
    let (a, b) = (t.sink(D0), t.sink(D1));
    check_sine(&a);
    check_sine(&b);
    let (ca, cb) = (counters(&a), counters(&b));
    assert!(ca.windows(2).all(|w| w[1] > w[0]) && cb.windows(2).all(|w| w[1] > w[0]), "out of order inside one output");
    let mut all: Vec<u64> = ca.iter().chain(&cb).copied().collect();
    all.sort();
    assert_eq!(all, (1..=30000).collect::<Vec<u64>>(), "lost or doubled frames across the switches");
    assert!(!cb.is_empty() && ca.len() > cb.len());
    let st = t.sh.status();
    assert_eq!((st.switches, st.streams), (2, 0));
    assert_eq!(t.hw.lock().unwrap().open_count(), 0);
}

#[test]
fn switch_on_one_converter_breaks_before_make() {
    let mut t = T::new(32); // the AMD FIFO
    let x = t.open(F48, 60000);
    x.write_frames(&frames(testsig::c, 0, 20000)).unwrap();
    x.start();
    t.ms(150);
    {
        let mut hw = t.hw.lock().unwrap();
        hw.hp = true;
        hw.jack = true;
    }
    t.ms(10);
    assert_eq!(t.sh.status().endpoint.as_deref(), Some(D0HP));
    x.start_drain();
    t.until(2000, || x.state() == State::Drained);
    t.ms(3000); // the FIFO's last frames reach the sink while the output retires
    let mut all: Vec<u64> = counters(&t.sink(D0)).into_iter().chain(counters(&t.sink(D0HP))).collect();
    let n = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), n, "frames doubled");
    assert!(20000 - n <= 32, "{} frames lost (more than the FIFO)", 20000 - n);
    assert_eq!(t.sh.status().switches, 1);
}

#[test]
fn lost_output_falls_back_to_null_and_comes_back() {
    let mut t = T::new(0);
    let x = t.open(F48, 96000);
    x.write_frames(&frames(testsig::c, 0, 60000)).unwrap();
    x.start();
    t.ms(200);
    t.hw.lock().unwrap().gone = true;
    t.ms(300);
    let st = t.sh.status();
    assert_eq!(st.endpoint, None);
    assert!(!t.sh.real_output());
    let p = x.position();
    t.ms(200);
    assert!(x.position() > p + 4800, "the null output does not keep time");
    t.hw.lock().unwrap().gone = false;
    t.ms(1200);
    assert_eq!(t.sh.status().endpoint.as_deref(), Some(D0));
    assert!(t.sh.status().opens >= 2);
}

#[test]
fn idle_output_closes_and_reopens() {
    let mut t = T::new(0);
    let x = t.open(F48, 4800);
    x.write_frames(&frames(testsig::c, 0, 4800)).unwrap();
    x.start_drain();
    t.until(1000, || x.state() == State::Drained);
    assert_eq!(t.sh.status().streams, 1);
    t.ms(2600);
    assert_eq!(t.sh.status().streams, 0);
    assert_eq!(t.hw.lock().unwrap().open_count(), 0);
    drop(x);
    let y = t.open(F48, 4800);
    y.write_frames(&frames(testsig::c, 0, 480)).unwrap();
    y.start();
    t.ms(20);
    assert_eq!(t.sh.status().streams, 1);
    assert_eq!(t.sh.status().opens, 2);
    t.ms(5);
    assert_eq!(t.sh.status().clients, 1, "the closed client is still listed");
}

/// SN-13 (w25): an idle output whose position stopped is closed after `retire_ns` instead of staying open (and
/// its controller streaming) with no client.
#[test]
fn idle_output_with_a_stalled_position_closes_anyway() {
    let mut t = T::new(0);
    let x = t.open(F48, 4800);
    x.write_frames(&frames(testsig::c, 0, 960)).unwrap();
    x.start_drain();
    t.until(1000, || x.state() == State::Drained);
    drop(x);
    t.hw.lock().unwrap().stall = true;
    let cfg = Config::latency_ms(40);
    t.ms((cfg.idle_ns / 1_000_000) as usize + 5);
    assert_eq!(t.sh.status().streams, 1, "retiring: it waits for its rest to play");
    assert_eq!(t.hw.lock().unwrap().open_count(), 1);
    t.ms((cfg.retire_ns() / 1_000_000) as usize + 2);
    assert_eq!(t.sh.status().streams, 0);
    assert_eq!(t.hw.lock().unwrap().open_count(), 0, "closed although the position never moved");
    assert!(cfg.retire_ns() < 1_000_000_000, "bound {} ns", cfg.retire_ns());
}

/// SN-13 fixer: `hdmi-off` moves the target, then waits `leave_bound_ns` for the mixer to leave the controller.
/// With the old controller's position stopped, the retiring output's own deadline closes it inside that wait.
#[test]
fn a_stalled_output_leaves_its_controller_within_the_hdmi_off_bound() {
    let mut t = T::new(0);
    t.sh.set_target(Target::Fixed { dev: "dev1".into(), port: Port::Jack });
    let x = t.open(F48, 60000);
    x.write_frames(&frames(testsig::c, 0, 30000)).unwrap();
    x.start();
    t.ms(200);
    assert_eq!(t.sh.status().endpoint.as_deref(), Some(D1));
    t.hw.lock().unwrap().stall_dev = Some("dev1".into());
    t.sh.set_target(Target::Fixed { dev: "dev0".into(), port: Port::Jack });
    let cfg = Config::latency_ms(40);
    let bound_ms = (crate::output::leave_bound_ns(&cfg) / 1_000_000) as usize;
    let sh = t.sh.clone();
    let waited = t.until(bound_ms, || !crate::output::on_controller(&sh.status(), "dev1"));
    assert!(waited as u64 * 1_000_000 >= cfg.retire_ns(), "left after {waited} ms: before the deadline, so not stalled");
    assert_eq!(t.sh.status().endpoint.as_deref(), Some(D0));
    assert_eq!(t.hw.lock().unwrap().open_count(), 1, "only the new output open");
    drop(x);
}

/// SN-13 fixer: a retiring output whose position counts closes when its rest has played (the `until` path), well
/// before `retire_ns`, at every latency; nothing of its rest is cut off.
#[test]
fn a_normal_drain_closes_before_the_retire_deadline() {
    for ms in [5u32, 40, 200] {
        let mut t = T::with_latency(0, ms);
        let cfg = Config::latency_ms(ms);
        t.sh.set_target(Target::Fixed { dev: "dev0".into(), port: Port::Jack });
        let x = t.open(F48, 60000);
        x.write_frames(&frames(testsig::c, 0, 30000)).unwrap();
        x.start();
        t.ms(200);
        assert_eq!(t.sh.status().endpoint.as_deref(), Some(D0), "{ms} ms");
        t.sh.set_target(Target::Fixed { dev: "dev1".into(), port: Port::Jack });
        let sh = t.sh.clone();
        t.until(50, || sh.status().endpoint.as_deref() == Some(D1));
        let waited = t.until(2000, || sh.status().streams == 1) as u64 * 1_000_000;
        let drain = (cfg.lead as u64 + cfg.drain_margin as u64 + 2 * cfg.period as u64) * 1_000_000_000 / 48_000;
        assert!(waited <= drain + 5_000_000, "{ms} ms: retired after {waited} ns, a drain takes {drain} ns");
        assert!(waited + cfg.period_ns() < cfg.retire_ns(), "{ms} ms: closed by the deadline, not by its rest");
        x.start_drain();
        t.until(3000, || x.state() == State::Drained);
        t.ms(500);
        let mut all: Vec<u64> = counters(&t.sink(D0)).into_iter().chain(counters(&t.sink(D1))).collect();
        all.sort();
        assert_eq!(all, (1..=30000).collect::<Vec<u64>>(), "{ms} ms: the retiring output's rest was cut off");
    }
}

/// SN-13 fixer: the retire deadline grows with the latency and always leaves a whole drain twice over.
#[test]
fn retire_deadline_follows_the_latency() {
    let mut last = 0;
    for ms in [5u32, 10, 20, 40, 100, 200, 500] {
        let c = Config::latency_ms(ms);
        let drain = (c.lead as u64 + c.drain_margin as u64) * 1_000_000_000 / 48_000;
        assert!(c.retire_ns() >= 2 * drain + 4 * c.period_ns(), "{ms} ms");
        assert!(c.retire_ns() >= last, "{ms} ms");
        last = c.retire_ns();
    }
    assert!(Config::latency_ms(500).retire_ns() > 1_000_000_000, "the high-latency bound is above 1 s");
}

#[test]
fn release_closes_without_waiting_for_idle() {
    let mut t = T::new(0);
    let x = t.open(F48, 4800);
    x.write_frames(&frames(testsig::c, 0, 960)).unwrap();
    x.start_drain();
    t.until(1000, || x.state() == State::Drained);
    t.sh.release.store(true, core::sync::atomic::Ordering::Release);
    t.ms(200); // lead + margin, not the 2 s idle time
    assert_eq!(t.sh.status().streams, 0);
    assert_eq!(sound(&t.sink(D0)), frames(testsig::c, 0, 960));
}

#[test]
fn many_clients() {
    let mut t = T::new(0);
    let list: Vec<Playback> = (0..40).map(|_| t.open(F48, 4800)).collect();
    for p in &list {
        p.write_frames(&alloc::vec![[100, -100]; 4800]).unwrap();
    }
    let refs: Vec<&Playback> = list.iter().collect();
    start_together(&t.sh, &refs);
    t.ms(300);
    let s = sound(&t.sink(D0));
    assert_eq!(s.len(), 4800);
    assert!(s.iter().all(|f| *f == [4000, -4000]));
}

#[test]
fn formats_and_mono() {
    let t = T::new(0);
    let bad = |fmt: PcmFormat, ring: u32| client::open(&t.sh, fmt, ring, "bad").err();
    assert_eq!(bad(PcmFormat { rate: 48000, channels: 2, bits: 24 }, 100), Some(SndError::BadFormat));
    assert_eq!(bad(PcmFormat { rate: 48000, channels: 3, bits: 16 }, 100), Some(SndError::BadFormat));
    assert_eq!(bad(PcmFormat { rate: 12345, channels: 2, bits: 16 }, 100), Some(SndError::BadFormat));
    assert_eq!(bad(F48, 0), Some(SndError::BadSize));
    assert_eq!(bad(F48, u32::MAX), Some(SndError::BadSize));
    let mut t = T::new(0);
    let m = t.open(PcmFormat { rate: 48000, channels: 1, bits: 16 }, 100);
    assert_eq!(m.write(&[0xE8, 0x03, 0x18, 0xFC, 0x01]), Ok(4)); // 1000, -1000; the odd byte is not a frame
    m.start();
    t.ms(20);
    assert_eq!(sound(&t.sink(D0))[..2], [[1000, 1000], [-1000, -1000]]);
}

#[test]
fn resampled_client_in_the_mix() {
    let mut t = T::new(0);
    let x = t.open(PcmFormat { rate: 44100, channels: 2, bits: 16 }, 44100);
    x.write_frames(&frames(testsig::tone441, 0, 44100)).unwrap();
    x.start();
    t.ms(100);
    // Ring + half the filter + the output's lead (1920 frames at 48 kHz = 1764 at 44.1 kHz).
    let want = x.queued() as u64 + 32 + 1764;
    assert!(x.delay().abs_diff(want) <= 50, "delay {} want about {want}", x.delay());
    x.start_drain();
    t.until(3000, || x.state() == State::Drained);
    assert_eq!(x.position(), 44100);
    let s = sound(&t.sink(D0));
    assert!((s.len() as i64 - 48000).abs() <= 64, "{} frames", s.len());
    let peak = s[1000..47000].iter().map(|f| f[0].unsigned_abs()).max().unwrap();
    assert!((15900..=16100).contains(&peak), "peak {peak}");
}

#[test]
fn drain_waits_until_played() {
    let mut t = T::new(0);
    let x = t.open(F48, 4800);
    x.write_frames(&frames(testsig::c, 0, 4800)).unwrap();
    x.start_drain();
    let ms = t.until(1000, || x.state() == State::Drained);
    assert!(ms >= 100, "drained after {ms} ms: before 4800 frames could play");
    assert_eq!(sound(&t.sink(D0)).len(), 4800);
}

#[test]
fn without_any_output_the_clock_runs() {
    let mut t = T::new(0);
    t.hw.lock().unwrap().none = true;
    let x = t.open(F48, 48000);
    x.write_frames(&frames(testsig::c, 0, 48000)).unwrap();
    x.start();
    t.ms(500);
    let p = x.position();
    assert!((p as i64 - 24000).abs() < 3000, "position {p} after 500 ms");
    assert_eq!(t.sh.status().endpoint, None);
    assert!(x.delay() >= 48000 - p);
}

#[test]
fn fast_switch_back_cuts_the_retiring_output() {
    let mut t = T::new(0);
    t.sh.set_target(Target::Fixed { dev: "dev0".into(), port: Port::Jack });
    let x = t.open(F48, 60000);
    x.write_frames(&frames(testsig::c, 0, 20000)).unwrap();
    x.start();
    t.ms(100);
    t.sh.set_target(Target::Fixed { dev: "dev1".into(), port: Port::Jack });
    t.ms(20); // dev0 still plays its lead
    t.sh.set_target(Target::Fixed { dev: "dev0".into(), port: Port::Jack });
    x.start_drain();
    t.until(2000, || x.state() == State::Drained);
    t.ms(3000);
    let mut all: Vec<u64> = counters(&t.sink(D0)).into_iter().chain(counters(&t.sink(D1))).collect();
    let n = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), n, "frames doubled");
    // Only dev0's unplayed lead at the cut may be missing.
    assert!(20000 - n <= 1920, "{} lost", 20000 - n);
    assert_eq!(t.sh.status().switches, 2);
    assert_eq!(t.hw.lock().unwrap().open_count(), 0);
}

#[test]
fn latency_config() {
    let c = Config::latency_ms(40);
    assert_eq!((c.period, c.lead, c.ring), (480, 1920, 2880));
    let c = Config::latency_ms(20);
    assert_eq!((c.period, c.lead), (256, 1024));
    let c = Config::latency_ms(0);
    assert_eq!((c.period, c.lead), (64, 256));
    assert!(Config::latency_ms(10_000).lead <= 4 * 6016);
    for ms in [1, 7, 33, 40, 99, 500] {
        let c = Config::latency_ms(ms);
        assert!(c.period % 32 == 0 && c.lead == 4 * c.period && c.floor == 2 * c.period && c.ring == c.lead + 2 * c.period);
    }
}

#[test]
fn stop_on_underrun_is_xrun_until_prepared() {
    let mut t = T::new(0);
    let x = t.open(F48, 4800);
    x.set_stop_on_underrun(true);
    t.sh.test_now.store(t.now, core::sync::atomic::Ordering::Release);
    x.write_frames(&frames(testsig::c, 0, 960)).unwrap();
    x.start();
    t.ms(100);
    assert_eq!(x.state(), State::Xrun);
    assert_eq!(x.underruns().0, 1);
    // SN-12: the xrun carries what the desk needs to tell a late writer from a late mixer.
    let i = t.core.last_xrun().expect("xrun reported");
    assert_eq!((i.id, i.period, i.ring, i.avail, i.consumed), (x.id(), 480, 4800, 4800, 960));
    assert!(i.got < i.need, "{i:?}");
    let at = i.since_write.expect("it wrote");
    assert!(at >= 1_000_000 && at <= 100_000_000 && at % 1_000_000 == 0, "{i:?}");
    assert_eq!((i.late, i.late_max), (0, 0), "1 ms steps never pass the 5 ms wake-up");
    assert_eq!(sound(&t.sink(D0)).len(), 960, "what it had was played");
    assert_eq!(x.write_frames(&[[1, 1]]), Err(SndError::Xrun));
    assert_eq!(x.wait_space(10, 1_000_000), Err(SndError::Xrun));
    t.ms(50);
    assert_eq!(x.underruns().0, 1, "an xrun client is not mixed again");
    // ALSA prepare: back to prepared, hw_ptr at 0, usable again.
    x.drop_pending();
    assert_eq!(x.state(), State::Prepared);
    assert_eq!(x.position(), 0);
    assert_eq!(x.write_frames(&frames(testsig::c, 960, 480)), Ok(480));
    x.start();
    t.ms(5);
    assert_eq!(x.state(), State::Running);
    assert!(x.position() > 0);
    // Drain from xrun ends at once.
    t.ms(100);
    assert_eq!(x.state(), State::Xrun);
    assert!(x.drain(1_000_000));
}

#[test]
fn xrun_reports_how_late_the_mixer_woke() {
    let mut t = T::new(0);
    let x = t.open(F48, 4800);
    x.set_stop_on_underrun(true);
    t.sh.test_now.store(t.now, core::sync::atomic::Ordering::Release);
    assert_eq!(x.write_frames(&frames(testsig::c, 0, 4800)), Ok(4800));
    x.start();
    t.ms(1);
    assert_eq!(x.state(), State::Running);
    // The mixer asked to run again in 5 ms (half a 10 ms period) but runs 50 ms later; the xrun that follows
    // when the ring runs dry reports that 45 ms as the worst lateness since the previous report.
    t.hw.lock().unwrap().advance(48 * 50);
    t.now += 50_000_000;
    t.sh.test_now.store(t.now, core::sync::atomic::Ordering::Release);
    t.core.step(t.now);
    t.until(200, || x.state() == State::Xrun);
    let i = t.core.last_xrun().expect("xrun reported");
    assert_eq!((i.late, i.late_max), (0, 45_000_000), "{i:?}");
    assert!(i.since_write.is_some_and(|w| w > 51_000_000), "{i:?}");
    // The maximum restarts after each report.
    x.drop_pending();
    assert_eq!(x.write_frames(&frames(testsig::c, 0, 480)), Ok(480));
    x.start();
    t.ms(100);
    let j = t.core.last_xrun().expect("second xrun");
    assert_eq!((j.late, j.late_max), (0, 0), "{j:?}");
}

#[test]
fn early_wakes_are_not_late_and_old_spikes_age_out() {
    let mut t = T::new(0);
    let x = t.open(F48, 48_000);
    assert_eq!(x.write_frames(&frames(testsig::c, 0, 48_000)), Ok(48_000));
    x.start();
    t.ms(1);
    assert_eq!(x.state(), State::Running);
    let at = |t: &mut T, ms: u64| {
        t.hw.lock().unwrap().advance(48 * ms as usize);
        t.now += ms * 1_000_000;
        t.sh.test_now.store(t.now, core::sync::atomic::Ordering::Release);
        t.core.step(t.now)
    };
    // The thread sleeps until 5 ms from now; a kick wakes it after 1 ms: not late.
    t.core.sleeping_until(t.now + 5_000_000);
    let sleep = at(&mut t, 1);
    assert_eq!(t.core.lateness(), (0, 0));
    // The next wake comes 15 ms after the one that step asked for.
    at(&mut t, sleep / 1_000_000 + 15);
    assert_eq!(t.core.lateness(), (15_000_000, 15_000_000));
    // On time for two windows: the 15 ms spike no longer counts.
    for _ in 0..(2 * LATE_WINDOW_NS / 1_000_000 / 4 + 1) {
        at(&mut t, 4);
    }
    assert_eq!(x.state(), State::Running);
    assert_eq!(t.core.lateness(), (0, 0));
}

#[test]
fn state_calls_follow_alsa() {
    let mut t = T::new(0);
    let x = t.open(F48, 960);
    assert_eq!(x.pause(true), Err(SndError::BadState), "pause while prepared");
    assert_eq!(x.write_frames(&frames(testsig::c, 0, 2000)), Ok(960));
    x.start();
    t.ms(1);
    assert_eq!(x.pause(true), Ok(()));
    assert_eq!(x.pause(true), Err(SndError::BadState));
    t.ms(10);
    let full = x.avail();
    // Paused: the wait runs to its deadline (Ok(false)), it does not return at once with an error.
    assert_eq!(x.wait_space(x.ring_frames(), 2_000_000), Ok(full >= x.ring_frames()));
    assert_eq!(x.write_frames(&[[1, 1]]).is_ok(), true);
    assert_eq!(x.pause(false), Ok(()));
    assert_eq!(x.pause(false), Err(SndError::BadState));
    t.ms(30);
    assert_eq!(x.wait_space(480, 1_000_000), Ok(true));
    // Drop restarts the position; a drop while draining turns it prepared (not overwritten by the mixer).
    assert!(x.position() > 0);
    x.write_frames(&frames(testsig::c, 0, 480)).unwrap();
    x.start_drain();
    assert_eq!(x.write_frames(&[[1, 1]]), Err(SndError::BadState));
    assert_eq!(x.wait_space(1, 1_000_000), Err(SndError::BadState));
    x.drop_pending();
    assert_eq!((x.state(), x.position(), x.queued()), (State::Prepared, 0, 0));
    t.ms(50);
    assert_eq!(x.state(), State::Prepared, "the mixer moved a dropped client");
    // Drained -> dropped -> started again plays the new data (no stale drain end).
    x.write_frames(&frames(testsig::c, 0, 480)).unwrap();
    assert!({ x.start_drain(); true });
    t.until(500, || x.state() == State::Drained);
    x.drop_pending();
    x.write_frames(&frames(testsig::c, 0, 480)).unwrap();
    x.start();
    t.ms(20);
    assert_eq!((x.state(), x.position()), (State::Running, 480));
}

#[test]
fn rounding_has_no_dc_bias() {
    // At a gain that is not a power of two, floor would bias every sample by about -0.5 LSB on average;
    // round to nearest keeps the mean error near 0.
    let mut t = T::new(0);
    let x = t.open(F48, 48000);
    x.set_volume([20000, 20000]);
    let input: Vec<[i16; 2]> = (0..4800).map(|i| [((i % 201) as i16) - 100, 0]).collect();
    x.write_frames(&input).unwrap();
    x.start();
    x.start_drain();
    t.until(1000, || x.state() == State::Drained);
    let s = t.sink(D0);
    let exact = |v: i16| v as f64 * 20000.0 / 32768.0;
    let err: f64 = s[..4800].iter().zip(&input).map(|(o, i)| o[0] as f64 - exact(i[0])).sum::<f64>() / 4800.0;
    assert!(err.abs() < 0.05, "mean error {err} LSB");
}

/// SN-7: the mixer tops its output up only as far as the running clients have frames (down to `floor`), so a
/// writer that keeps less than the lead queued plays on; the XRUN comes once the output would drop below the
/// floor, and what the client had is still played whole.
#[test]
fn writer_below_the_lead_is_not_starved() {
    let mut t = T::new(0);
    let x = t.open(F48, 8192);
    x.set_stop_on_underrun(true);
    let c = t.core.config();
    let keep = (c.floor + c.lead) as u64 / 2 + 240;
    let mut at = 0;
    for _ in 0..2000 {
        while x.delay() < keep {
            assert_eq!(x.write_frames(&frames(testsig::c, at, 240)), Ok(240));
            at += 240;
            x.start();
        }
        t.ms(1);
    }
    assert_eq!((x.state(), x.underruns().0), (State::Running, 0), "kept {keep} of a {} lead", c.lead);
    // It stops writing: the XRUN comes only after the queue fell to the floor; nothing it wrote is lost.
    t.ms(((keep - c.floor as u64) / 48 / 2) as usize);
    assert_eq!(x.state(), State::Running, "XRUN a whole lead early");
    t.ms(200);
    assert_eq!((x.state(), x.underruns().0), (State::Xrun, 1));
    assert_eq!(sound(&t.sink(D0)).len(), at as usize, "every written frame was played");
}

/// The output queue's count and its time are one word (SN-7 fixer): at the step's instant `queued_now` gives
/// the count back exactly, then falls at the output rate and stops at 0; nothing queued is 0 at any time.
#[test]
fn queued_now_is_one_consistent_reading() {
    use core::sync::atomic::Ordering;
    let sh = Shared::new();
    for q in [0u64, 1, 479, 480, 1920, 4097, 1 << 20] {
        let at = 7_000_000_123;
        sh.set_queued(at, q);
        sh.test_now.store(at, Ordering::Release);
        assert_eq!(sh.queued_now(), q, "q {q} at the step");
        sh.test_now.store(at + 10_000_000, Ordering::Release);
        assert_eq!(sh.queued_now(), q.saturating_sub(480), "q {q} 10 ms later");
        sh.test_now.store(at + 100_000_000_000, Ordering::Release);
        assert_eq!(sh.queued_now(), 0, "q {q} long after");
    }
    sh.set_queued(0, 0);
    sh.test_now.store(1, Ordering::Release);
    assert_eq!(sh.queued_now(), 0);
}

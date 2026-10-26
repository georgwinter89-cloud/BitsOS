//! In-kernel tests (SN-3). `sn.mix.null`, `sn.mix.client_churn` and `sn.mix.state` are `Safe` (no sound: a
//! private mixer on the null output, and the kernel mixer's state without a client); `sn.mix.two`, `.switch`,
//! `.resample` play through the kernel mixer (`Audible`) into QEMU's WAV files, checked by `host/tests/sn/mixcheck.py`
//! (`host/tests/sn/30-mix.sh`). Every test leaves no output open (`release`), so they rerun in one boot;
//! `sn.mix.leave_running` (QemuOnly) leaves one playing for the Reload scenario.

use crate::backend::{Backend, Endpoint, OutError, Stream, Target, WakeFn};
use crate::client::{State, UNITY};
use crate::thread::Mixer;
use crate::{testsig, Config};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use crate::backend::Port;
use devmodel::audio::PcmFormat;
use ktest::{kassert, ktest, TestResult};

/// Generous: QEMU timing is never judged (CONTRACTS §4).
pub(crate) const SLACK_NS: u64 = 20_000_000_000;
const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };
const F441: PcmFormat = PcmFormat { rate: 44100, channels: 2, bits: 16 };

pub(crate) struct NoOutputs;
impl Backend for NoOutputs {
    fn resolve(&mut self, _: &Target) -> Option<Endpoint> {
        None
    }
    fn endpoints(&mut self) -> Vec<Endpoint> {
        Vec::new()
    }
    fn open(&mut self, _: &Endpoint, _: u32, _: u32, _: WakeFn, _: u64) -> Result<Box<dyn Stream>, OutError> {
        Err(OutError::NoSuch)
    }
    fn take_jack_events(&mut self) -> bool {
        false
    }
}

pub(crate) fn err<E: core::fmt::Debug>(what: &str) -> impl FnOnce(E) -> String + '_ {
    move |e| format!("{what}: {e:?}")
}

fn thread_class(id: Option<u64>) -> Option<sched::Class> {
    let id = id?;
    sched::threads().into_iter().find(|t| t.0 == id).map(|t| t.2)
}

pub(crate) fn bytes(f: &[[i16; 2]]) -> Vec<u8> {
    f.iter().flat_map(|x| [x[0].to_le_bytes(), x[1].to_le_bytes()]).flatten().collect()
}

pub(crate) fn wait(cond: impl Fn() -> bool, ns: u64) -> bool {
    let end = time::now_ns() + ns;
    while !cond() {
        if time::now_ns() > end {
            return false;
        }
        sched::sleep_ms(5);
    }
    true
}

ktest!("sn.mix.null", Safe, null);
/// A private mixer thread on the null output (by the clock): two clients (44.1 and 48 kHz) drain completely,
/// a starved one counts its underrun, the thread is KernelRt. Touches no hardware.
fn null() -> TestResult {
    let m = Mixer::spawn("snd/mix-test", Config::latency_ms(40), |_| NoOutputs).map_err(err("spawn"))?;
    let r = (|| -> TestResult {
        kassert!(thread_class(m.thread_id()) == Some(sched::Class::KernelRt), "mixer thread class {:?}", thread_class(m.thread_id()));
        let x = m.open(F441, 44100, "ktest.441").map_err(err("open 44.1"))?;
        let y = m.open(F48, 48000, "ktest.48").map_err(err("open 48"))?;
        let nx = 13230; // 0.3 s
        let ny = 14400;
        kassert!(x.write(&bytes(&testsig::frames(testsig::tone441, 0, nx))) == Ok(nx * 4));
        kassert!(y.write(&bytes(&testsig::frames(testsig::a, 0, ny))) == Ok(ny * 4));
        m.start_together(&[&x, &y]);
        y.start_drain();
        kassert!(x.drain(SLACK_NS) && y.drain(SLACK_NS), "drain: {} {}", x.state().name(), y.state().name());
        kassert!(x.position() == nx as u64 && y.position() == ny as u64, "positions {} {}", x.position(), y.position());
        kassert!(x.underruns().0 == 0 && y.underruns().0 == 0, "underruns {:?} {:?}", x.underruns(), y.underruns());
        let z = m.open(F48, 4800, "ktest.starved").map_err(err("open starved"))?;
        let _ = z.write(&bytes(&testsig::frames(testsig::a, 0, 480)));
        z.start();
        kassert!(wait(|| z.underruns().0 >= 1, SLACK_NS), "no underrun counted");
        kassert!(z.underruns().1 > 0 && z.state() == State::Running);
        let st = m.status();
        kassert!(st.endpoint.is_none() && st.mixed >= (ny as u64), "status {st:?}");
        klog::event!(Info, "ktest", "mix.null", mixed = st.mixed, played = st.played, hw_underruns = st.hw_underruns, starved = z.underruns().0,
            silence = z.underruns().1);
        Ok(())
    })();
    m.stop();
    r
}

ktest!("sn.mix.client_churn", Safe, client_churn);
/// R1 and hostile use on a private mixer (null output, no hardware): 200 clients of all formats opened,
/// written, some started, paused, dropped, volume-changed and closed leave no client and no heap behind;
/// a bad open is refused; calls on a client whose mixer stopped are harmless.
fn client_churn() -> TestResult {
    use crate::client::SndError;
    let m = Mixer::spawn("snd/mix-churn", Config::latency_ms(40), |_| NoOutputs).map_err(err("spawn"))?;
    let heap0 = mm_core::HEAP.allocated_bytes();
    let r = (|| -> TestResult {
        let rates = [8000, 11025, 22050, 32000, 44100, 48000, 96000];
        for i in 0..200u32 {
            let fmt = PcmFormat { rate: rates[i as usize % rates.len()], channels: 1 + (i % 2) as u8, bits: 16 };
            let x = m.open(fmt, 48000, "ktest.churn").map_err(err("open"))?;
            let ch = fmt.channels as usize;
            let data: Vec<u8> = (0..960 * ch).flat_map(|k| ((k as i16).wrapping_mul(37)).to_le_bytes()).collect();
            kassert!(x.write(&data) == Ok(data.len()), "write {i}");
            x.set_volume([u32::MAX, i * 100]);
            kassert!(x.volume()[0] == UNITY, "volume not clamped");
            match i % 4 {
                0 => {}
                1 => x.start(),
                2 => {
                    x.start();
                    kassert!(x.pause(true).is_ok());
                }
                _ => {
                    x.start();
                    x.drop_pending();
                    kassert!(x.position() == 0 && x.state() == State::Prepared);
                }
            }
        }
        kassert!(wait(|| m.status().clients == 0, SLACK_NS), "clients left: {}", m.status().clients);
        // Bad requests are refused without a trace.
        kassert!(m.open(PcmFormat { rate: 12345, channels: 2, bits: 16 }, 100, "bad").err() == Some(SndError::BadFormat));
        kassert!(m.open(F48, u32::MAX, "huge").err() == Some(SndError::BadSize));
        kassert!(m.open(F48, 0, "empty").err() == Some(SndError::BadSize));
        // 200 rings of 192 KB would be 38 MB if any leaked; other threads' noise stays far below 4 MiB.
        let heap1 = mm_core::HEAP.allocated_bytes();
        kassert!(heap1 < heap0 + (4 << 20), "heap grew by {} bytes", heap1.saturating_sub(heap0));
        klog::event!(Info, "ktest", "mix.churn", clients = 200u64, heap_delta = heap1 as i64 - heap0 as i64, mixed = m.status().mixed);
        Ok(())
    })();
    // A client whose mixer stopped: every call answers, nothing blocks past its deadline.
    let q = m.open(F48, 4800, "ktest.orphan").ok();
    m.stop();
    if let (Ok(()), Some(q)) = (&r, q) {
        kassert!(q.write(&[0; 16]).is_ok());
        q.set_volume([0, 0]);
        q.start();
        kassert!(!q.drain(10_000_000), "drained without a mixer");
        kassert!(q.write(&[0; 4]) == Err(SndError::BadState));
        kassert!(m.open(F48, 100, "late").err() == Some(SndError::Closed));
    }
    r
}

ktest!("sn.mix.state", Safe, state);
/// The kernel mixer exists, its thread is KernelRt, and what it would play to (no client: nothing opens).
fn state() -> TestResult {
    let m = crate::mixer().map_err(err("mixer"))?;
    kassert!(thread_class(m.thread_id()) == Some(sched::Class::KernelRt));
    let st = m.status();
    let cfg = m.config();
    let outs: Vec<String> = hda::controllers()
        .iter()
        .flat_map(|d| [Port::Jack, Port::Hdmi].into_iter().filter_map(move |p| p.hw().and_then(|h| d.pick(h)).map(|o| format!("{}/{p:?}/{:#x}", d.ctrl.name(), o.pin))))
        .collect();
    klog::event!(Info, "ktest", "mix.state", target = format!("{:?}", m.target()), endpoint = st.endpoint.as_deref().unwrap_or("none"),
        streams = st.streams as u64, clients = st.clients as u64, lead = cfg.lead, period = cfg.period, ring = cfg.ring,
        playable = outs.join(",").as_str(), hw_underruns = st.hw_underruns, switches = st.switches);
    Ok(())
}

pub(crate) fn players() -> Result<Vec<String>, TestResult> {
    let v: Vec<String> = hda::controllers().iter().filter(|d| d.pick(devmodel::audio::Output::Jack).is_some()).map(|d| d.ctrl.name().into()).collect();
    if v.is_empty() {
        return Err(ktest::skip("no controller with a playable analog output"));
    }
    Ok(v)
}

pub(crate) fn fixed(dev: &str) -> Target {
    Target::Fixed { dev: dev.into(), port: Port::Jack }
}

/// The client `sn.mix.leave_running` left (closed by the next mixer test).
static LEFT: ksync::SpinLock<Vec<crate::Playback>> = ksync::SpinLock::named("snd.ktest_left", Vec::new());

fn release_left() {
    let old = core::mem::take(&mut *LEFT.lock());
    drop(old);
}

/// Plays through the kernel mixer, then releases its output; the target goes back to Auto.
pub(crate) fn with_mixer(target: Target, f: impl FnOnce(&Mixer) -> TestResult) -> TestResult {
    release_left();
    let m = crate::mixer().map_err(err("mixer"))?;
    kassert!(m.release(SLACK_NS), "an output was still open before the test");
    m.set_target(target);
    m.set_master([UNITY, UNITY]);
    let r = f(&m);
    let released = m.release(SLACK_NS);
    m.set_target(Target::Auto);
    r?;
    kassert!(released, "output not closed after the test");
    Ok(())
}

ktest!("sn.mix.two", Audible, two);
/// Two 48 kHz clients started together on the first player: `a` at 0 dB, `b` at 0.75: the WAV must hold
/// `testsig::mixed` sample-exactly, saturated where the sum exceeds 16 bit (`mixcheck.py two`).
fn two() -> TestResult {
    let p = match players() {
        Ok(p) => p,
        Err(r) => return r,
    };
    with_mixer(fixed(&p[0]), |m| {
        let n = 48000;
        let x = m.open(F48, n as u32, "ktest.a").map_err(err("open a"))?;
        let y = m.open(F48, n as u32, "ktest.b").map_err(err("open b"))?;
        y.set_volume([24576, 24576]);
        kassert!(x.write(&bytes(&testsig::frames(testsig::a, 0, n))) == Ok(n * 4));
        kassert!(y.write(&bytes(&testsig::frames(testsig::b, 0, n))) == Ok(n * 4));
        m.start_together(&[&x, &y]);
        y.start_drain();
        kassert!(x.drain(SLACK_NS) && y.drain(SLACK_NS), "drain: {} {}", x.state().name(), y.state().name());
        let st = m.status();
        kassert!(x.underruns().0 == 0 && y.underruns().0 == 0, "client underruns {:?} {:?}", x.underruns(), y.underruns());
        klog::event!(Info, "ktest", "mix.two", endpoint = st.endpoint.as_deref().unwrap_or("none"), frames = n as u64, hw_underruns = st.hw_underruns);
        kassert!(st.endpoint.is_some_and(|e| e.starts_with(p[0].as_str())), "not on {}", p[0]);
        Ok(())
    })
}

ktest!("sn.mix.switch", Audible, switch);
/// One 48 kHz client (`testsig::c`, 1.2 s) switched first -> second controller at a third, back at two
/// thirds: across both WAVs every frame exactly once, in order (`mixcheck.py switch`).
fn switch() -> TestResult {
    let p = match players() {
        Ok(p) => p,
        Err(r) => return r,
    };
    if p.len() < 2 {
        return ktest::skip("needs two controllers with a playable output (QEMU: two intel-hda)");
    }
    with_mixer(fixed(&p[0]), |m| {
        let n = 57600;
        let x = m.open(F48, n as u32, "ktest.switch").map_err(err("open"))?;
        kassert!(x.write(&bytes(&testsig::frames(testsig::c, 0, n))) == Ok(n * 4));
        let s0 = m.status().switches;
        x.start();
        for (k, dev) in [(1, &p[1]), (2, &p[0])] {
            kassert!(wait(|| x.position() >= (n * k / 3) as u64, SLACK_NS), "client stuck at {}", x.position());
            m.set_target(fixed(dev));
            let on = |st: &crate::Status| st.endpoint.as_deref().is_some_and(|e| e.starts_with(dev.as_str()));
            kassert!(wait(|| { let st = m.status(); st.switches >= s0 + k as u64 && on(&st) }, SLACK_NS), "switch {k} to {dev} did not happen");
            let st = m.status();
            kassert!(st.endpoint.as_deref().is_some_and(|e| e.starts_with(dev.as_str())), "switch {k}: on {:?}", st.endpoint);
            klog::event!(Info, "ktest", "mix.switched", to = dev.as_str(), at = x.position());
        }
        kassert!(x.drain(SLACK_NS), "drain: {}", x.state().name());
        kassert!(x.underruns().0 == 0, "client underruns {:?}", x.underruns());
        klog::event!(Info, "ktest", "mix.switch", frames = n as u64, switches = m.status().switches - s0, hw_underruns = m.status().hw_underruns);
        Ok(())
    })
}

ktest!("sn.mix.resample", Audible, resample);
/// A 44.1 kHz client (1 kHz sine, 1 s) on the first player: the WAV holds a 48 kHz 1 kHz sine of the same
/// amplitude with low distortion (`mixcheck.py resample`).
fn resample() -> TestResult {
    let p = match players() {
        Ok(p) => p,
        Err(r) => return r,
    };
    with_mixer(fixed(&p[0]), |m| {
        let n = 44100;
        let x = m.open(F441, n as u32, "ktest.441").map_err(err("open"))?;
        kassert!(x.write(&bytes(&testsig::frames(testsig::tone441, 0, n))) == Ok(n * 4));
        x.start();
        kassert!(x.drain(SLACK_NS), "drain: {}", x.state().name());
        kassert!(x.position() == n as u64 && x.underruns().0 == 0, "position {} underruns {:?}", x.position(), x.underruns());
        klog::event!(Info, "ktest", "mix.resample", frames = n as u64, endpoint = m.status().endpoint.as_deref().unwrap_or("none"));
        Ok(())
    })
}

ktest!("sn.mix.leave_running", QemuOnly, leave_running);
/// A client playing 10 s through the kernel mixer, left running for the Reload scenario
/// (`host/tests/sn/20-hda-reload.sh`): the old kernel's Quiesce stops hda under the running mixer.
fn leave_running() -> TestResult {
    release_left();
    if let Err(r) = players() {
        return r;
    }
    let m = crate::mixer().map_err(err("mixer"))?;
    kassert!(m.release(SLACK_NS), "an output was still open before the test");
    m.set_target(Target::Auto);
    let n = 480_000;
    let x = m.open(F48, n as u32, "ktest.left").map_err(err("open"))?;
    kassert!(x.write(&bytes(&testsig::frames(testsig::c, 0, n))) == Ok(n * 4));
    x.start();
    kassert!(wait(|| m.status().endpoint.is_some(), SLACK_NS), "no real output opened");
    klog::event!(Info, "ktest", "mix.left_running", endpoint = m.status().endpoint.as_deref().unwrap_or("none"));
    LEFT.lock().push(x);
    Ok(())
}

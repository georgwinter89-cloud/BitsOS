//! SN-9: what `capture` records from. `SysInputs` (the kernel's): a USB headset's microphone first (the device
//! someone chose to wear, as `usbback` does for playback), else an hda input picked as Linux's auto-mic would
//! (`hda_generic.c` `check_auto_mic_availability`: a plugged mic jack, else the built-in mic; then a mic jack
//! without a plug sense, line in, an unplugged mic jack). The input is picked at open and kept until close.
//! `FakeInputs` feeds a known pattern (ktests, host tests: QEMU has no USB microphone and hda-duplex's line in
//! records only what the audiodev gives).

use crate::capture::{Inputs, Source, Wake};
use crate::client::SndError;
use crate::hdaback::Holder;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use devmodel::audio::PcmFormat;
use hda::{HdaError, OutputId, StreamId};
use hdacodec::input::InputKind;
use ksync::SpinLock;
use usb_core::UsbError;

const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };
/// Frames per copy from an hda ring (stack buffer).
const CHUNK: usize = 256;

pub struct SysInputs;

impl Inputs for SysInputs {
    fn open(&self, ring: u32, period: u32, wake: Wake) -> Result<Box<dyn Source>, SndError> {
        let mut busy = false;
        for i in usb_audio::inputs() {
            // The USB ring holds twice the ALSA buffer, so ALSA sees its own overrun (avail >= buffer) first.
            match usb_audio::open_capture(&i.key, i.iface, ring as usize * 2, Some(wake.clone())) {
                Ok(h) => return Ok(Box::new(UsbSource { name: format!("{}/{}", i.key, i.iface), h })),
                Err(UsbError::Busy) => busy = true,
                Err(e) => klog::event!(Warn, "snd", "cap.usb_failed", key = i.key.as_str(), interface = i.iface, error = e.as_str()),
            }
        }
        for d in hda::controllers().into_iter().filter(|d| !d.ctrl.read_only()) {
            match open_hda(&d, ring, period, wake.clone()) {
                Ok(s) => return Ok(s),
                Err(SndError::Busy) => busy = true,
                Err(_) => {}
            }
        }
        klog::event!(Info, "snd", "cap.no_input", busy = busy);
        Err(if busy { SndError::Busy } else { SndError::NoInput })
    }
}

/// Lower is better (see the module doc); None: not an input we record from.
fn rank(kind: InputKind, fixed: bool, plugged: Option<bool>) -> u8 {
    match (kind, fixed, plugged) {
        (InputKind::Mic, false, Some(true)) => 0,
        (InputKind::Mic, true, _) => 1,
        (InputKind::Mic, false, None) => 2,
        (InputKind::LineIn, _, _) => 3,
        (InputKind::Mic, false, Some(false)) => 4,
    }
}

/// The best input of one controller, opened.
pub(crate) fn open_hda<T: Holder>(d: &Arc<T>, ring: u32, period: u32, wake: Wake) -> Result<Box<dyn Source>, SndError> {
    let c = d.ctrl();
    let mut ins: alloc::vec::Vec<(u8, OutputId)> =
        c.inputs().iter().filter(|i| i.formats.contains(&F48)).map(|i| (rank(i.kind, i.fixed, c.input_plugged(i.id)), i.id)).collect();
    ins.sort();
    let mut busy = false;
    for (r, id) in ins {
        // Twice the ALSA buffer, as for USB: ALSA's own overrun (avail >= stop_threshold) comes first.
        match HdaSource::open(d, id, ring.saturating_mul(2), period, wake.clone()) {
            Ok(s) => {
                klog::event!(Info, "snd", "cap.hda_input", ctrl = c.name(), cad = id.cad, pin = klog::Hex(id.pin as u64), rank = r);
                return Ok(Box::new(s));
            }
            Err(HdaError::Busy) => busy = true,
            Err(e) => klog::event!(Warn, "snd", "cap.hda_failed", ctrl = c.name(), pin = klog::Hex(id.pin as u64), error = format!("{e:?}").as_str()),
        }
    }
    Err(if busy { SndError::Busy } else { SndError::NoInput })
}

struct UsbSource {
    name: String,
    h: usb_audio::CaptureHandle,
}

impl Source for UsbSource {
    fn name(&self) -> String {
        self.name.clone()
    }
    /// The alternate setting stays from open to close; start and stop only fill or stop the bus queue (the
    /// requests in flight still land), so a prepared, paused or dropped stream does not keep recording.
    fn start(&self) -> Result<(), SndError> {
        self.h.cap.start().map_err(|_| SndError::Closed)
    }
    fn stop(&self) {
        self.h.cap.stop();
    }
    fn read(&self, out: &mut [[i16; 2]]) -> Result<usize, SndError> {
        self.h.cap.read(out).map_err(|_| SndError::Closed)
    }
    fn available(&self) -> usize {
        self.h.cap.available()
    }
    fn overruns(&self) -> u64 {
        self.h.cap.counts().1
    }
    fn gone(&self) -> bool {
        self.h.cap.gone()
    }
    fn close(self: Box<Self>) {
        self.h.close();
    }
}

/// What the period callback last saw (read under `snd.pcm` without touching `hda.slots`).
struct HdaSeen {
    avail: AtomicU32,
    overruns: AtomicU64,
    gone: AtomicBool,
}

pub(crate) struct HdaSource<T: Holder> {
    d: Arc<T>,
    sid: StreamId,
    name: String,
    seen: Arc<HdaSeen>,
    fifo: u32,
}

impl<T: Holder> HdaSource<T> {
    fn open(d: &Arc<T>, id: OutputId, ring: u32, period: u32, wake: Wake) -> Result<HdaSource<T>, HdaError> {
        let c = d.ctrl();
        let sid = c.open_input(id, F48, ring, period)?;
        let seen = Arc::new(HdaSeen { avail: AtomicU32::new(0), overruns: AtomicU64::new(0), gone: AtomicBool::new(false) });
        let (w, s): (Weak<T>, Arc<HdaSeen>) = (Arc::downgrade(d), seen.clone());
        // The service thread calls it with no hda lock held.
        let cb: hda::PeriodFn = Arc::new(move |sid, _| {
            if let Some(d) = w.upgrade() {
                refresh(d.ctrl(), sid, &s);
            }
            wake();
        });
        if let Err(e) = c.set_period_callback(sid, Some(cb)) {
            let _ = c.close(sid);
            return Err(e);
        }
        Ok(HdaSource { d: d.clone(), sid, name: format!("{}/{}:{:#x}", c.name(), id.cad, id.pin), seen, fifo: c.kind().fifo_frames().unwrap_or(0) })
    }
}

fn refresh<H: hda::hw::Hw>(c: &hda::Controller<H>, sid: StreamId, s: &HdaSeen) {
    match (c.readable(sid), c.overruns(sid)) {
        (Ok(a), Ok(o)) => {
            s.avail.store(a, Ordering::Release);
            s.overruns.store(o, Ordering::Release);
        }
        _ => s.gone.store(true, Ordering::Release),
    }
}

impl<T: Holder> Source for HdaSource<T> {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn start(&self) -> Result<(), SndError> {
        self.d.ctrl().start(self.sid).map_err(|_| SndError::Closed)
    }
    fn stop(&self) {
        let _ = self.d.ctrl().stop(self.sid);
        refresh(self.d.ctrl(), self.sid, &self.seen);
    }
    fn read(&self, out: &mut [[i16; 2]]) -> Result<usize, SndError> {
        let c = self.d.ctrl();
        let mut buf = [0u8; CHUNK * 4];
        let mut done = 0;
        while done < out.len() {
            let want = (out.len() - done).min(CHUNK) * 4;
            let n = match c.read(self.sid, &mut buf[..want]) {
                Ok(n) => n / 4,
                Err(_) => {
                    self.seen.gone.store(true, Ordering::Release);
                    break;
                }
            };
            for (k, f) in buf[..n * 4].chunks_exact(4).enumerate() {
                out[done + k] = [i16::from_le_bytes([f[0], f[1]]), i16::from_le_bytes([f[2], f[3]])];
            }
            done += n;
            if n * 4 < want {
                break;
            }
        }
        refresh(c, self.sid, &self.seen);
        if done == 0 && self.gone() {
            return Err(SndError::Closed);
        }
        Ok(done)
    }
    fn available(&self) -> usize {
        self.seen.avail.load(Ordering::Acquire) as usize
    }
    fn overruns(&self) -> u64 {
        // hda counts lost rings; one lost ring is a ring of frames.
        self.seen.overruns.load(Ordering::Acquire)
    }
    fn gone(&self) -> bool {
        self.seen.gone.load(Ordering::Acquire) || self.d.ctrl().state() != hda::State::Up
    }
    fn latency(&self) -> u32 {
        self.fifo
    }
    fn close(self: Box<Self>) {
        let _ = self.d.ctrl().close(self.sid);
    }
}

/// The fake's frame `i`: left counts up, right is its complement (any lost or repeated frame shows).
pub fn pattern(i: u64) -> [i16; 2] {
    let l = i as u16 as i16;
    [l, !l]
}

/// A fake input: frames pushed by the test, or made by the clock at 48 kHz while running (`clocked`).
pub struct Fake {
    q: SpinLock<VecDeque<[i16; 2]>>,
    cap: AtomicU32,
    overruns: AtomicU64,
    running: AtomicBool,
    gone: AtomicBool,
    open: AtomicBool,
    wake: SpinLock<Option<Wake>>,
    clock: Option<fn() -> u64>,
    /// (start of the current run, frames made in earlier runs + this one so far)
    run: SpinLock<(u64, u64, u64)>,
    pub opens: AtomicU32,
}

impl Fake {
    /// `clock`: make frames by time (ktests: `time::now_ns`); None: only `push`.
    pub fn new(clock: Option<fn() -> u64>) -> Arc<Fake> {
        Arc::new(Fake {
            q: SpinLock::named("snd.cap.fake", VecDeque::new()),
            cap: AtomicU32::new(0),
            overruns: AtomicU64::new(0),
            running: AtomicBool::new(false),
            gone: AtomicBool::new(false),
            open: AtomicBool::new(false),
            wake: SpinLock::named("snd.cap.fake_wake", None),
            clock,
            run: SpinLock::named("snd.cap.fake_run", (0, 0, 0)),
            opens: AtomicU32::new(0),
        })
    }

    /// The device records `frames` (kept whether running or not, as a USB microphone does).
    pub fn push(&self, frames: &[[i16; 2]]) {
        self.store(frames.iter().copied());
        self.kick();
    }

    fn store(&self, frames: impl Iterator<Item = [i16; 2]>) {
        let cap = self.cap.load(Ordering::Acquire).max(1) as usize;
        let mut q = self.q.lock();
        for f in frames {
            if q.len() >= cap {
                q.pop_front();
                self.overruns.fetch_add(1, Ordering::AcqRel);
            }
            if q.try_reserve(1).is_err() {
                break;
            }
            q.push_back(f);
        }
    }

    fn kick(&self) {
        let w = self.wake.lock().clone();
        if let Some(w) = w {
            w();
        }
    }

    pub fn unplug(&self) {
        self.gone.store(true, Ordering::Release);
        self.kick();
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// Clocked: the frames due by now (pattern numbering continues across runs).
    fn pump(&self) {
        let Some(now) = self.clock.map(|c| c()) else { return };
        if !self.running.load(Ordering::Acquire) || self.gone.load(Ordering::Acquire) {
            return;
        }
        let (from, to) = {
            let mut r = self.run.lock();
            let due = r.1 + ((now.saturating_sub(r.0)) as u128 * 48_000 / 1_000_000_000) as u64;
            let from = r.2;
            if due <= from {
                return;
            }
            // Never more than a ring at once (a long stall loses the old frames, as a device would).
            let cap = self.cap.load(Ordering::Acquire).max(1) as u64;
            let start = from.max(due.saturating_sub(cap));
            if start > from {
                self.overruns.fetch_add(start - from, Ordering::AcqRel);
            }
            r.2 = due;
            (start, due)
        };
        self.store((from..to).map(pattern));
    }
}

pub struct FakeInputs(pub Arc<Fake>);

struct FakeSource(Arc<Fake>);

impl Inputs for FakeInputs {
    fn open(&self, ring: u32, _period: u32, wake: Wake) -> Result<Box<dyn Source>, SndError> {
        let f = &self.0;
        if f.gone.load(Ordering::Acquire) {
            return Err(SndError::NoInput);
        }
        if f.open.swap(true, Ordering::AcqRel) {
            return Err(SndError::Busy);
        }
        f.cap.store(ring.saturating_mul(2), Ordering::Release);
        *f.wake.lock() = Some(wake);
        f.opens.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(FakeSource(f.clone())))
    }
}

impl Source for FakeSource {
    fn name(&self) -> String {
        "fake".into()
    }
    fn start(&self) -> Result<(), SndError> {
        let f = &self.0;
        if f.gone.load(Ordering::Acquire) {
            return Err(SndError::Closed);
        }
        if !f.running.swap(true, Ordering::AcqRel) {
            let now = f.clock.map_or(0, |c| c());
            let mut r = f.run.lock();
            r.0 = now;
            r.1 = r.2;
        }
        Ok(())
    }
    fn stop(&self) {
        self.0.pump();
        self.0.running.store(false, Ordering::Release);
    }
    fn read(&self, out: &mut [[i16; 2]]) -> Result<usize, SndError> {
        self.0.pump();
        let mut q = self.0.q.lock();
        let n = out.len().min(q.len());
        for (o, f) in out.iter_mut().zip(q.drain(..n)) {
            *o = f;
        }
        if n == 0 && self.0.gone.load(Ordering::Acquire) {
            return Err(SndError::Closed);
        }
        Ok(n)
    }
    fn available(&self) -> usize {
        self.0.pump();
        self.0.q.lock().len()
    }
    fn overruns(&self) -> u64 {
        self.0.overruns.load(Ordering::Acquire)
    }
    fn gone(&self) -> bool {
        self.0.gone.load(Ordering::Acquire)
    }
    fn close(self: Box<Self>) {
        let f = &self.0;
        f.running.store(false, Ordering::Release);
        *f.wake.lock() = None;
        f.q.lock().clear();
        f.open.store(false, Ordering::Release);
    }
}

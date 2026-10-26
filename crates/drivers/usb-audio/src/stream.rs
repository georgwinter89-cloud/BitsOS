//! Playback and capture over an isochronous pipe (Linux `endpoint.c`: `prepare_outbound_urb`,
//! `retire_outbound_urb`, `snd_complete_urb`; `pcm.c` `prepare_playback_urb`). At 48 kHz every service
//! interval (125 us x 2^k) carries a whole number of frames (6 per microframe, 48 per frame), so packets are of
//! one size and synchronous/adaptive endpoints need no feedback.
//!
//! Playback: the mixer writes 16-bit stereo frames into a ring (`write` never blocks); the pump moves whole
//! packets from the ring into requests of `req` packets while the pipe has room; each completion counts its
//! frames as played (the position), pumps again and wakes the mixer. The pump runs in the mixer's thread
//! (`write`, `start`) and in the controller's completion thread; a flag, not a lock, keeps it single (queue
//! may grow a ring, which allocates). Closing only stops the pump: what is queued plays out within the window,
//! no command is sent from the mixer's thread. A request ending `Gone` (unplug) marks the stream gone: the
//! mixer's next `position` sees it and switches back to another output.
//!
//! Capture: requests of `req` packets of the endpoint's size are kept queued; completed packets (short ones
//! are normal) are decoded into a ring of stereo frames (mono duplicated); a full ring drops the oldest.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::SpinLock;
use usb_core::{Buffer, IsoDone, IsoPipe, IsoRequest, IsoResult, UsbError};

klog::counter!(PLAYED, "usb_audio.frames_played");
klog::counter!(MISSED, "usb_audio.packets_missed");
klog::counter!(BUS_EMPTY, "usb_audio.bus_empty");
klog::counter!(CAPTURED, "usb_audio.frames_captured");
klog::counter!(OVERRUNS, "usb_audio.capture_overruns");

pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// What a stream needs of an isochronous pipe (`IsoPipe`; host tests fake it).
pub trait Pipe: Send + Sync {
    fn room(&self) -> usize;
    fn queued(&self) -> usize;
    fn window(&self) -> usize;
    fn max_packet(&self) -> usize;
    fn interval_ns(&self) -> u64;
    fn queue(&self, req: IsoRequest, done: IsoDone) -> Result<(), UsbError>;
}

impl Pipe for IsoPipe {
    fn room(&self) -> usize {
        IsoPipe::room(self)
    }
    fn queued(&self) -> usize {
        IsoPipe::queued(self)
    }
    fn window(&self) -> usize {
        IsoPipe::window(self)
    }
    fn max_packet(&self) -> usize {
        IsoPipe::max_packet(self)
    }
    fn interval_ns(&self) -> u64 {
        IsoPipe::interval_ns(self)
    }
    fn queue(&self, req: IsoRequest, done: IsoDone) -> Result<(), UsbError> {
        IsoPipe::queue(self, req, done)
    }
}

/// Sample layout on the bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub channels: u8,
    pub subframe: u8,
}

impl Layout {
    pub fn frame_bytes(self) -> usize {
        self.channels as usize * self.subframe as usize
    }
    fn put(self, f: [i16; 2], out: &mut Vec<u8>) {
        let mut one = |s: i16| {
            if self.subframe == 3 {
                let v = (s as i32) << 8;
                out.extend_from_slice(&v.to_le_bytes()[..3]);
            } else {
                out.extend_from_slice(&s.to_le_bytes());
            }
        };
        if self.channels == 1 {
            one(((f[0] as i32 + f[1] as i32) >> 1) as i16);
        } else {
            one(f[0]);
            one(f[1]);
        }
    }
    fn get(self, b: &[u8]) -> [i16; 2] {
        let s = |i: usize| -> i16 {
            let o = i * self.subframe as usize;
            if self.subframe == 3 { i16::from_le_bytes([b[o + 1], b[o + 2]]) } else { i16::from_le_bytes([b[o], b[o + 1]]) }
        };
        if self.channels == 1 { [s(0); 2] } else { [s(0), s(1)] }
    }
}

/// Frames per service interval at 48 kHz (6 per 125 us).
pub fn frames_per_packet(interval_ns: u64) -> Option<usize> {
    let f = interval_ns.checked_mul(6)? / 125_000;
    (f > 0 && interval_ns % 125_000 == 0).then_some(f as usize)
}

/// Window and request size from the mixer's period (frames at 48 kHz): half the mixer's lead (4 periods) on
/// the bus, requests of a fifth of that, at least one packet each.
pub fn sizing(period: u32, interval_ns: u64) -> (u64, usize) {
    let window_ns = (2 * period as u64 * 1_000_000_000 / 48_000).max(2 * interval_ns).max(2_000_000);
    let packets = window_ns.div_ceil(interval_ns.max(1)) as usize;
    (window_ns, (packets / 5).max(1))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub requests: u64,
    pub missed: u64,
    pub bus_empty: u64,
    pub errors: u64,
}

struct Common {
    pipe: Box<dyn Pipe>,
    bufs: Vec<Buffer>,
    next: AtomicUsize,
    req: usize,
    fpp: usize,
    lay: Layout,
    running: AtomicBool,
    closed: AtomicBool,
    gone: AtomicBool,
    pumping: AtomicBool,
    again: AtomicBool,
    scratch: SpinLock<Vec<u8>>,
    stats: SpinLock<Stats>,
    wake: Option<Wake>,
}

impl Common {
    fn new(pipe: Box<dyn Pipe>, alloc: &dyn Fn(usize) -> Result<Buffer, UsbError>, req: usize, lay: Layout, wake: Option<Wake>, out: bool)
        -> Result<Common, UsbError>
    {
        let fpp = frames_per_packet(pipe.interval_ns()).ok_or(UsbError::Invalid("service interval not a multiple of 125 us"))?;
        let pkt = if out { fpp * lay.frame_bytes() } else { pipe.max_packet() };
        if pkt == 0 || pkt > pipe.max_packet() {
            return Err(UsbError::Invalid("48 kHz packet larger than the endpoint's maximum"));
        }
        let req = req.clamp(1, pipe.window().max(1));
        // Requests complete in order and at most `window` packets are queued: this many never reuses one in flight.
        let n = pipe.window().div_ceil(req) + 2;
        let mut bufs = Vec::new();
        bufs.try_reserve(n).map_err(|_| UsbError::NoMemory)?;
        for _ in 0..n {
            bufs.push(alloc(req * pkt)?);
        }
        let mut scratch = Vec::new();
        scratch.try_reserve(req * pkt).map_err(|_| UsbError::NoMemory)?;
        Ok(Common {
            pipe, bufs, next: AtomicUsize::new(0), req, fpp, lay,
            running: AtomicBool::new(false), closed: AtomicBool::new(false), gone: AtomicBool::new(false),
            pumping: AtomicBool::new(false), again: AtomicBool::new(false),
            scratch: SpinLock::named("usb_audio.scratch", scratch), stats: SpinLock::named("usb_audio.stats", Stats::default()), wake,
        })
    }

    fn buf(&self) -> Buffer {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.bufs.len();
        self.bufs[i].clone()
    }

    fn live(&self) -> bool {
        self.running.load(Ordering::Acquire) && !self.closed.load(Ordering::Acquire) && !self.gone.load(Ordering::Acquire)
    }

    /// Runs `body` as the only pumper; a pump asked for meanwhile runs it again.
    fn single(&self, mut body: impl FnMut()) {
        loop {
            if self.pumping.swap(true, Ordering::AcqRel) {
                self.again.store(true, Ordering::Release);
                return;
            }
            self.again.store(false, Ordering::Release);
            body();
            self.pumping.store(false, Ordering::Release);
            if !self.again.swap(false, Ordering::AcqRel) {
                return;
            }
        }
    }

    fn failed(&self, e: UsbError) {
        if e == UsbError::Gone || e == UsbError::NotRunning {
            if !self.gone.swap(true, Ordering::AcqRel) {
                klog::event!(Info, "usb_audio", "uac.stream_gone", error = e.as_str());
            }
        } else {
            let n = {
                let mut s = self.stats.lock();
                s.errors += 1;
                s.errors
            };
            if n.is_power_of_two() {
                klog::event!(Warn, "usb_audio", "uac.queue_failed", error = e.as_str(), count = n);
            }
        }
    }

    fn result(&self, r: &IsoResult) {
        let missed = r.failed() as u64;
        let mut s = self.stats.lock();
        s.requests += 1;
        s.missed += missed;
        drop(s);
        if missed > 0 {
            MISSED.add(missed);
        }
        if let Some(e) = r.error {
            if e != UsbError::Cancelled {
                self.failed(e);
            }
        }
    }

    fn kick(&self) {
        if let Some(w) = &self.wake {
            w()
        }
    }
}

struct OutInner {
    c: Common,
    ring: SpinLock<VecDeque<[i16; 2]>>,
    cap: usize,
    played: AtomicU64,
}

/// A playback stream (see the module doc).
pub struct Playback(Arc<OutInner>);

impl Playback {
    /// `ring` frames of ring, requests of `req` packets, buffers from `alloc`.
    pub fn open(pipe: Box<dyn Pipe>, alloc: &dyn Fn(usize) -> Result<Buffer, UsbError>, lay: Layout, ring: usize, req: usize, wake: Wake)
        -> Result<Playback, UsbError>
    {
        let c = Common::new(pipe, alloc, req, lay, Some(wake), true)?;
        let cap = ring.max(c.fpp * c.req);
        let mut q = VecDeque::new();
        q.try_reserve(cap).map_err(|_| UsbError::NoMemory)?;
        Ok(Playback(Arc::new(OutInner { c, ring: SpinLock::named("usb_audio.ring", q), cap, played: AtomicU64::new(0) })))
    }

    pub fn ring_frames(&self) -> usize {
        self.0.cap
    }

    fn check(&self) -> Result<(), UsbError> {
        if self.0.c.gone.load(Ordering::Acquire) { Err(UsbError::Gone) } else { Ok(()) }
    }

    /// Takes frames up to the ring's room; never blocks.
    pub fn write(&self, frames: &[[i16; 2]]) -> Result<usize, UsbError> {
        self.check()?;
        let n = {
            let mut q = self.0.ring.lock();
            let n = frames.len().min(self.0.cap - q.len().min(self.0.cap));
            q.extend(frames[..n].iter().copied());
            n
        };
        pump(&self.0);
        Ok(n)
    }

    pub fn start(&self) -> Result<(), UsbError> {
        self.check()?;
        self.0.c.running.store(true, Ordering::Release);
        pump(&self.0);
        Ok(())
    }

    /// Stops feeding the bus (what is queued plays out); the ring keeps its frames.
    pub fn stop(&self) {
        self.0.c.running.store(false, Ordering::Release);
    }

    /// Frames whose packets the bus has sent (lost ones count: their time passed).
    pub fn position(&self) -> Result<u64, UsbError> {
        self.check()?;
        Ok(self.0.played.load(Ordering::Acquire))
    }

    pub fn close(&self) {
        self.0.c.closed.store(true, Ordering::Release);
        self.0.c.running.store(false, Ordering::Release);
        self.0.ring.lock().clear();
    }

    pub fn stats(&self) -> Stats {
        *self.0.c.stats.lock()
    }
    pub fn queued_packets(&self) -> usize {
        self.0.c.pipe.queued()
    }
}

fn pump(o: &Arc<OutInner>) {
    let c = &o.c;
    c.single(|| {
        while c.live() {
            let room = c.pipe.room();
            let busy = c.pipe.queued() > 0;
            let pkt = c.fpp * c.lay.frame_bytes();
            let mut s = c.scratch.lock();
            s.clear();
            // Counted and taken under one hold of the ring: `close` may empty it from the mixer's thread.
            let k = {
                let mut q = o.ring.lock();
                let have = q.len() / c.fpp;
                let k = c.req.min(room).min(have);
                if k == 0 || (k < c.req && room >= c.req && busy) {
                    // Wait for a whole request while the bus still has some; send what there is when it runs dry.
                    0
                } else {
                    for f in q.drain(..k * c.fpp) {
                        c.lay.put(f, &mut s);
                    }
                    k
                }
            };
            if k == 0 {
                break;
            }
            let frames = k * c.fpp;
            let buf = c.buf();
            let ok = buf.write(0, &s);
            drop(s);
            if !ok {
                c.failed(UsbError::Invalid("buffer too small"));
                break;
            }
            let o2 = o.clone();
            let done: IsoDone = Box::new(move |r: IsoResult| {
                o2.played.fetch_add(frames as u64, Ordering::AcqRel);
                PLAYED.add(frames as u64);
                o2.c.result(&r);
                pump(&o2);
                if o2.c.live() && o2.c.pipe.queued() == 0 {
                    let n = {
                        let mut s = o2.c.stats.lock();
                        s.bus_empty += 1;
                        s.bus_empty
                    };
                    BUS_EMPTY.inc();
                    if n.is_power_of_two() {
                        klog::event!(Debug, "usb_audio", "uac.bus_empty", count = n; "nothing queued on the bus: the mixer was late");
                    }
                }
                o2.c.kick();
            });
            if let Err(e) = c.pipe.queue(IsoRequest::even(buf, k, pkt), done) {
                // The frames of a refused request are lost; their time is not (the position goes on).
                o.played.fetch_add(frames as u64, Ordering::AcqRel);
                c.failed(e);
                c.kick();
                break;
            }
        }
    });
}

struct InInner {
    c: Common,
    ring: SpinLock<VecDeque<[i16; 2]>>,
    cap: usize,
    captured: AtomicU64,
    overruns: AtomicU64,
}

/// A capture stream (see the module doc).
pub struct Capture(Arc<InInner>);

impl Capture {
    pub fn open(pipe: Box<dyn Pipe>, alloc: &dyn Fn(usize) -> Result<Buffer, UsbError>, lay: Layout, ring: usize, req: usize, wake: Option<Wake>)
        -> Result<Capture, UsbError>
    {
        let c = Common::new(pipe, alloc, req, lay, wake, false)?;
        let cap = ring.max(c.fpp * c.req);
        let mut q = VecDeque::new();
        q.try_reserve(cap).map_err(|_| UsbError::NoMemory)?;
        Ok(Capture(Arc::new(InInner { c, ring: SpinLock::named("usb_audio.cring", q), cap, captured: AtomicU64::new(0), overruns: AtomicU64::new(0) })))
    }

    pub fn start(&self) -> Result<(), UsbError> {
        if self.0.c.gone.load(Ordering::Acquire) {
            return Err(UsbError::Gone);
        }
        self.0.c.running.store(true, Ordering::Release);
        fill(&self.0);
        Ok(())
    }

    /// Takes captured frames (stereo; a mono microphone on both); never blocks.
    pub fn read(&self, out: &mut [[i16; 2]]) -> Result<usize, UsbError> {
        let mut q = self.0.ring.lock();
        let n = out.len().min(q.len());
        for (o, f) in out.iter_mut().zip(q.drain(..n)) {
            *o = f;
        }
        drop(q);
        if n == 0 && self.0.c.gone.load(Ordering::Acquire) {
            return Err(UsbError::Gone);
        }
        Ok(n)
    }

    pub fn available(&self) -> usize {
        self.0.ring.lock().len()
    }
    /// The device went away (a request ended `Gone`); what is in the ring can still be read (SN-9).
    pub fn gone(&self) -> bool {
        self.0.c.gone.load(Ordering::Acquire)
    }
    /// Frames the device sent since start, and frames dropped because the ring was full.
    pub fn counts(&self) -> (u64, u64) {
        (self.0.captured.load(Ordering::Acquire), self.0.overruns.load(Ordering::Acquire))
    }
    pub fn stop(&self) {
        self.0.c.running.store(false, Ordering::Release);
    }
    pub fn close(&self) {
        self.0.c.closed.store(true, Ordering::Release);
        self.0.c.running.store(false, Ordering::Release);
    }
    pub fn stats(&self) -> Stats {
        *self.0.c.stats.lock()
    }
}

fn fill(i: &Arc<InInner>) {
    let c = &i.c;
    c.single(|| {
        while c.live() && c.pipe.room() >= c.req {
            let len = c.pipe.max_packet();
            let buf = c.buf();
            let (i2, b2) = (i.clone(), buf.clone());
            let done: IsoDone = Box::new(move |r: IsoResult| {
                i2.c.result(&r);
                take_in(&i2, &b2, len, &r);
                fill(&i2);
                i2.c.kick();
            });
            if let Err(e) = c.pipe.queue(IsoRequest::even(buf, c.req, len), done) {
                c.failed(e);
                break;
            }
        }
    });
}

/// Decodes the packets of a completed IN request into the ring (completion context: no allocation, the ring
/// was reserved at open).
fn take_in(i: &InInner, buf: &Buffer, len: usize, r: &IsoResult) {
    let fb = i.c.lay.frame_bytes();
    let mut s = i.c.scratch.lock();
    for (k, p) in r.packets.iter().enumerate() {
        if p.error.is_some() || p.actual == 0 {
            continue;
        }
        let n = p.actual.min(len) / fb * fb;
        s.clear();
        s.resize(n, 0);
        if !buf.read(k * len, &mut s[..n]) {
            continue;
        }
        let mut q = i.ring.lock();
        for f in s[..n].chunks_exact(fb) {
            if q.len() >= i.cap {
                q.pop_front();
                i.overruns.fetch_add(1, Ordering::AcqRel);
                OVERRUNS.inc();
            }
            q.push_back(i.c.lay.get(f));
        }
        drop(q);
        let frames = (n / fb) as u64;
        i.captured.fetch_add(frames, Ordering::AcqRel);
        CAPTURED.add(frames);
    }
}

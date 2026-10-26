//! In-kernel tests of SN-8a under QEMU with `-device usb-audio` (full speed UAC1, EP 1 OUT isochronous, 48 kHz
//! 16-bit stereo: 192 bytes per 1 ms frame) and a wav audiodev (`host/tests/us/70-isoch.sh`). The test sets the
//! streaming interface's alternate setting itself (no audio class driver involved) and streams a known pattern
//! through an isochronous pipe, refilled from its completion callbacks the way a class driver would: per audio
//! frame i, left = (i mod 48) * 512 - 12288, right = i mod 2^16. Timing is never judged here.

use crate::driver::controllers;
use crate::hc::MS;
use crate::isoch::{ISO_ERRORS, ISO_MISSED};
use alloc::boxed::Box;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;
use ktest::{kassert, ktest, TestResult};
use usb_core::{registry, Buffer, Device, EpKind, IsoOpts, IsoPipe, IsoRequest, IsoResult, UsbError};

/// Packets per request and the pipe's window (ms of audio queued).
const PER_REQ: usize = 4;
const WINDOW_MS: u64 = 32;
const FRAME_BYTES: usize = 4;

/// The pattern's audio frame `i` (see the module doc), little endian.
pub fn pattern_frame(i: u64) -> [u8; 4] {
    let l = ((i % 48) as i16) * 512 - 12288;
    let r = (i & 0xFFFF) as u16;
    let (a, b) = (l.to_le_bytes(), r.to_le_bytes());
    [a[0], a[1], b[0], b[1]]
}

/// An unbound device with an audio streaming interface (01/02) whose alternate setting has an isochronous OUT
/// endpoint: (device, interface, alternate setting, endpoint address, packet size).
fn find_audio() -> Option<(Arc<Device>, u8, u8, u8, usize)> {
    registry::devices().into_iter().find_map(|d| {
        if !registry::bindings_of(d.id).is_empty() {
            return None;
        }
        let cfg = d.config.as_ref()?;
        let (f, e) = cfg.interfaces.iter().filter(|f| (f.class, f.subclass) == (1, 2)).find_map(|f| {
            f.endpoints.iter().find(|e| e.kind() == EpKind::Isoch && !e.is_in()).map(|e| (f.clone(), *e))
        })?;
        Some((d.clone(), f.number, f.alt, e.address, e.max_packet() as usize))
    })
}

struct Stream {
    pipe: IsoPipe,
    bufs: Vec<Buffer>,
    seq: AtomicU64,
    filling: AtomicBool,
    stop: AtomicBool,
    packet: usize,
    requests: AtomicU64,
    packets: AtomicU64,
    bad: AtomicU64,
    gone: AtomicU64,
    first_bad: SpinLock<Option<IsoResult>>,
}

impl Stream {
    /// Queues requests while the window has room (from the test thread at the start, then from callbacks). One
    /// context fills at a time (a flag, not a lock: queueing may allocate ring segments); whoever finds the flag
    /// taken leaves it to the filler, which looks again after letting go.
    fn refill(self: &Arc<Self>) {
        loop {
            if self.filling.swap(true, Ordering::AcqRel) {
                return;
            }
            self.fill_some();
            self.filling.store(false, Ordering::Release);
            if self.stop.load(Ordering::Acquire) || self.pipe.room() < PER_REQ {
                return;
            }
        }
    }

    fn fill_some(self: &Arc<Self>) {
        while !self.stop.load(Ordering::Acquire) && self.pipe.room() >= PER_REQ {
            let seq = self.seq.load(Ordering::Acquire);
            let buf = self.bufs[(seq as usize) % self.bufs.len()].clone();
            let frames = (self.packet / FRAME_BYTES) as u64;
            let mut bytes = Vec::with_capacity(PER_REQ * self.packet);
            for i in 0..PER_REQ as u64 * frames {
                bytes.extend_from_slice(&pattern_frame(seq * PER_REQ as u64 * frames + i));
            }
            if !buf.write(0, &bytes) {
                return;
            }
            let me = self.clone();
            let req = IsoRequest::even(buf, PER_REQ, self.packet);
            match self.pipe.queue(req, Box::new(move |r| me.done(r))) {
                Ok(()) => self.seq.store(seq + 1, Ordering::Release),
                Err(e) => {
                    klog::event!(Debug, "usb", "ktest.iso_queue_refused", error = e.as_str());
                    return;
                }
            }
        }
    }

    fn done(self: &Arc<Self>, r: IsoResult) {
        self.requests.fetch_add(1, Ordering::AcqRel);
        if r.error == Some(UsbError::Gone) {
            self.gone.fetch_add(1, Ordering::AcqRel);
            return;
        }
        let bad = r.packets.iter().filter(|p| p.error.is_some() || p.actual != self.packet).count() as u64;
        self.packets.fetch_add(r.packets.len() as u64 - bad, Ordering::AcqRel);
        if bad > 0 || r.error.is_some() {
            self.bad.fetch_add(bad.max(1), Ordering::AcqRel);
            let mut f = self.first_bad.lock();
            if f.is_none() {
                *f = Some(r);
            }
        }
        self.refill();
    }
}

fn settled() -> bool {
    let deadline = time::now_ns().saturating_add(15_000 * MS);
    while time::now_ns() < deadline {
        if controllers().iter().all(|c| c.state() != crate::hc::State::Running || c.settled()) {
            return true;
        }
        sched::sleep_ns(20 * MS);
    }
    false
}

fn until(ns: u64, f: impl Fn() -> bool) -> bool {
    let deadline = time::now_ns().saturating_add(ns);
    while time::now_ns() < deadline {
        if f() {
            return true;
        }
        sched::sleep_ns(5 * MS);
    }
    f()
}

/// Sets the alternate setting and starts streaming; `None` when no usb-audio is there.
fn start() -> Result<Option<(Arc<Device>, u8, Arc<Stream>)>, alloc::string::String> {
    if !settled() {
        return Err("hotplug did not settle".into());
    }
    let Some((d, iface, alt, ep, packet)) = find_audio() else { return Ok(None) };
    d.set_interface(iface, alt).map_err(|e| format!("SET_INTERFACE {iface}/{alt}: {e:?}"))?;
    let pipe = d.iso_pipe(ep, IsoOpts::ms(WINDOW_MS)).map_err(|e| format!("pipe: {e:?}"))?;
    // QEMU's usb-audio takes 192 bytes per ms (48 kHz stereo 16 bit) even where its descriptor allows more.
    let packet = packet.min(192);
    let pool = pipe.window() / PER_REQ + 2;
    let mut bufs = Vec::new();
    for _ in 0..pool {
        bufs.push(d.alloc_buffer(PER_REQ * packet).map_err(|e| format!("buffer: {e:?}"))?);
    }
    let st = Arc::new(Stream {
        pipe, bufs, seq: AtomicU64::new(0), filling: AtomicBool::new(false), stop: AtomicBool::new(false), packet,
        requests: AtomicU64::new(0), packets: AtomicU64::new(0), bad: AtomicU64::new(0), gone: AtomicU64::new(0),
        first_bad: SpinLock::named("ktest.iso_bad", None),
    });
    st.refill();
    klog::event!(Info, "usb", "ktest.iso_started", device = d.path(), interface = iface, alt = alt, ep = klog::Hex(ep as u64),
        packet = packet, window = st.pipe.window());
    Ok(Some((d, iface, st)))
}

/// Streams the pattern for 3 s: every packet completes whole, no missed service interval, no error; then the
/// queue drains, stop leaves nothing queued and alternate setting 0 drops the endpoint.
fn stream() -> TestResult {
    let (missed0, errors0) = (ISO_MISSED.get(), ISO_ERRORS.get());
    let Some((d, iface, st)) = start()? else {
        return ktest::skip("no unbound usb-audio (scenario host/tests/us/70-isoch.sh)");
    };
    sched::sleep_ns(3000 * MS);
    st.stop.store(true, Ordering::Release);
    let drained = until(1000 * MS, || st.pipe.queued() == 0);
    let left = st.pipe.stop().map_err(|e| format!("stop: {e:?}"))?;
    d.set_interface(iface, 0).map_err(|e| format!("alternate setting 0: {e:?}"))?;
    let (packets, bad) = (st.packets.load(Ordering::Acquire), st.bad.load(Ordering::Acquire));
    let frames = st.seq.load(Ordering::Acquire) * (PER_REQ * st.packet / FRAME_BYTES) as u64;
    klog::event!(Info, "usb", "ktest.iso_stream", device = d.path(), packets = packets, bad = bad, frames = frames,
        requests = st.requests.load(Ordering::Acquire), missed = ISO_MISSED.get() - missed0, errors = ISO_ERRORS.get() - errors0);
    kassert!(bad == 0, "{bad} bad packets, first {:?}", *st.first_bad.lock());
    kassert!(packets >= 100, "only {packets} packets completed in 3 s");
    kassert!(ISO_MISSED.get() == missed0 && ISO_ERRORS.get() == errors0, "missed service or errors under QEMU");
    kassert!(drained && left == 0, "queue did not drain ({} queued, {left} cancelled by stop)", st.pipe.queued());
    Ok(())
}
ktest!("us.isoch.stream", QemuOnly, stream);

/// With `host/tests/us/70-isoch.sh` (usb_iso_test=on): the script pulls the usb-audio while it streams; every
/// queued request ends (`Gone`), nothing stays queued, the controller runs on. Then the script plugs a usb-audio
/// in again and it streams like the first.
fn unplug() -> TestResult {
    if !pci::registry::opted_in("usb_iso_test") {
        return ktest::skip("scenario only: host/tests/us/70-isoch.sh boots with usb_iso_test=on and pulls the device");
    }
    let Some((d, _iface, st)) = start()? else { return Err("usb-audio expected".into()) };
    kassert!(until(5000 * MS, || st.packets.load(Ordering::Acquire) >= 200), "not streaming");
    klog::event!(Info, "usb", "ktest.iso_streaming", device = d.path(); "pull the device now");
    kassert!(until(60_000 * MS, || d.is_gone()), "device not gone within 60 s");
    kassert!(until(2000 * MS, || st.pipe.queued() == 0), "{} packets still queued after the unplug", st.pipe.queued());
    st.stop.store(true, Ordering::Release);
    kassert!(controllers().iter().all(|c| c.state() == crate::hc::State::Running), "a controller stopped");
    klog::event!(Info, "usb", "ktest.iso_unplug_done", packets = st.packets.load(Ordering::Acquire),
        gone = st.gone.load(Ordering::Acquire), bad = st.bad.load(Ordering::Acquire));
    // Plugged in again (QMP device_add): the new device's alternate setting and pipe work as the first time.
    klog::event!(Info, "usb", "ktest.iso_replug"; "plug the device in again");
    kassert!(until(60_000 * MS, || settled() && find_audio().is_some()), "no usb-audio again within 60 s");
    let Some((d, iface, st)) = start()? else { return Err("usb-audio vanished".into()) };
    kassert!(until(5000 * MS, || st.packets.load(Ordering::Acquire) >= 200), "not streaming after the replug");
    st.stop.store(true, Ordering::Release);
    kassert!(until(1000 * MS, || st.pipe.queued() == 0), "queue did not drain after the replug");
    d.set_interface(iface, 0).map_err(|e| format!("alternate setting 0 after the replug: {e:?}"))?;
    let bad = st.bad.load(Ordering::Acquire);
    kassert!(bad == 0, "{bad} bad packets after the replug, first {:?}", *st.first_bad.lock());
    klog::event!(Info, "usb", "ktest.iso_replug_done", device = d.path(), packets = st.packets.load(Ordering::Acquire));
    Ok(())
}
ktest!("us.isoch.unplug", QemuOnly, unplug);

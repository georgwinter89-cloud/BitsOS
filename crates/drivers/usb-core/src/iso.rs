//! Isochronous transfers for class drivers (SN-8a; USB audio first). An `IsoPipe` is opened on an isochronous
//! endpoint of the interface's current alternate setting (`Device::set_interface` first) and takes requests of
//! one buffer with any number of packets, one per service interval (Linux `iso_frame_desc`): each packet has
//! its own offset and length, at most `IsoPipe::max_packet` bytes (the endpoint's Max ESIT Payload). Requests go
//! out back to back, as soon as possible after the ones queued before (xHCI SIA), or from a chosen bus frame.
//! The pipe keeps a window: at most `window()` packets queued at a time, so a driver keeps "N ms" in flight by
//! queueing whenever `room()` allows (from its completion callback, or from its own thread).
//!
//! A request ends once: `on_done` gets the length and status of every packet (an OUT packet that went out has
//! `actual` = its length; an IN packet may be short, which is no error; a missed service interval is
//! `Transfer(23)`, Missed Service, with nothing moved) and, if the request as a whole was ended (device gone,
//! `stop`, alternate setting changed), that error in `IsoResult::error`. Callbacks of one controller run one at a
//! time in the order the controller finished the requests, outside every host lock, in the host's completion
//! context (a Normal thread): they must not sleep or wait; they may queue more.

use crate::desc::Endpoint;
use crate::host::UsbError;
use crate::speed::Speed;
use crate::transfer::{Buffer, XferResult};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::SpinLock;

/// One packet of a request: `len` bytes at `offset` of the request's buffer (0 is a zero-length packet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IsoPacket {
    pub offset: usize,
    pub len: usize,
}

/// Packets in consecutive service intervals, all in one buffer from `Device::alloc_buffer`.
#[derive(Clone)]
pub struct IsoRequest {
    pub buf: Buffer,
    pub packets: Vec<IsoPacket>,
    /// The bus frame (1 ms, 11 bits as in SOF) of the first packet; `None`: as soon as possible after what is
    /// queued (what audio uses). A frame the host cannot schedule any more (or not yet) falls back to ASAP.
    pub frame: Option<u16>,
}

impl IsoRequest {
    /// `n` packets of `len` bytes, back to back from the buffer's start, ASAP.
    pub fn even(buf: Buffer, n: usize, len: usize) -> Self {
        IsoRequest { buf, packets: (0..n).map(|i| IsoPacket { offset: i * len, len }).collect(), frame: None }
    }
    /// Every packet lies inside the buffer.
    pub fn in_bounds(&self) -> bool {
        let size = self.buf.len();
        self.packets.iter().all(|p| p.offset.checked_add(p.len).is_some_and(|e| e <= size))
    }
}

/// How a request ended: one result per packet in order, and the error that ended the whole request, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsoResult {
    pub packets: Vec<XferResult>,
    pub error: Option<UsbError>,
}

impl IsoResult {
    /// Bytes moved by all packets.
    pub fn bytes(&self) -> usize {
        self.packets.iter().map(|p| p.actual).sum()
    }
    /// Packets that ended with an error.
    pub fn failed(&self) -> usize {
        self.packets.iter().filter(|p| p.error.is_some()).count()
    }
}

/// Runs once when the request ends (host completion context: must not sleep or wait, may queue more).
pub type IsoDone = Box<dyn FnOnce(IsoResult) + Send>;

static NEXT_ISO: AtomicU64 = AtomicU64::new(1);

/// The shared state of one request between the pipe and the host.
pub struct IsoUrb {
    pub id: u64,
    packets: usize,
    done: AtomicBool,
    on_done: SpinLock<Option<IsoDone>>,
}

impl IsoUrb {
    pub fn new(packets: usize, on_done: IsoDone) -> Arc<IsoUrb> {
        Arc::new(IsoUrb {
            id: NEXT_ISO.fetch_add(1, Ordering::Relaxed),
            packets,
            done: AtomicBool::new(false),
            on_done: SpinLock::named("usb.iso_urb", Some(on_done)),
        })
    }
    pub fn packets(&self) -> usize {
        self.packets
    }
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
    /// Host side: the first completion wins (later ones return false) and runs the callback. Never called with a
    /// host lock held.
    pub fn complete(&self, r: IsoResult) -> bool {
        if self.done.swap(true, Ordering::AcqRel) {
            return false;
        }
        let cb = self.on_done.lock().take();
        if let Some(f) = cb {
            f(r);
        }
        true
    }
}

impl core::fmt::Debug for IsoUrb {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "IsoUrb({} packets {} done {})", self.id, self.packets, self.is_done())
    }
}

/// What a pipe keeps queued: `window_ns` of service intervals (at least one packet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IsoOpts {
    pub window_ns: u64,
}

impl IsoOpts {
    pub fn ms(n: u64) -> Self {
        IsoOpts { window_ns: n.saturating_mul(1_000_000) }
    }
}

/// The service interval of an isochronous endpoint in ns: 2^(bInterval-1) frames at full speed, microframes at
/// high and super speed (USB 2.0 9.6.6, USB 3.2 9.6.6; bInterval 1..16).
pub fn service_interval_ns(speed: Speed, ep: &Endpoint) -> u64 {
    let units = 1u64 << (ep.interval.clamp(1, 16) - 1);
    match speed {
        Speed::Low | Speed::Full => units * 1_000_000,
        _ => units * 125_000,
    }
}

/// Bytes an isochronous endpoint moves per service interval (Max ESIT Payload: packet size x transactions per
/// microframe at high speed; SuperSpeed: the companion's wBytesPerInterval, else packet x burst x mult).
pub fn max_service_payload(speed: Speed, ep: &Endpoint) -> usize {
    let mps = ep.max_packet() as usize;
    match (speed, ep.companion) {
        (Speed::High, _) => mps * (ep.hs_mult() as usize + 1),
        (Speed::Super | Speed::SuperPlus, Some(c)) => {
            let full = mps * (c.max_burst as usize + 1) * ((c.attributes & 3) as usize + 1);
            if c.bytes_per_interval == 0 { full } else { (c.bytes_per_interval as usize).min(full) }
        }
        _ => mps,
    }
}

/// An open isochronous pipe (see the module doc). Dropping it does not stop what is queued: `stop()` does, and
/// so do the device going away and `Device::set_interface`.
pub struct IsoPipe {
    pub(crate) dev: Arc<crate::device::Device>,
    pub(crate) ep: Endpoint,
    pub(crate) window: usize,
    pub(crate) queued: Arc<AtomicUsize>,
}

impl IsoPipe {
    pub fn endpoint(&self) -> u8 {
        self.ep.address
    }
    pub fn descriptor(&self) -> Endpoint {
        self.ep
    }
    pub fn interval_ns(&self) -> u64 {
        service_interval_ns(self.dev.speed(), &self.ep)
    }
    /// Most bytes one packet may carry.
    pub fn max_packet(&self) -> usize {
        max_service_payload(self.dev.speed(), &self.ep)
    }
    /// Packets the pipe keeps queued at most.
    pub fn window(&self) -> usize {
        self.window
    }
    /// Packets queued and not yet completed.
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
    /// Packets that may be queued now.
    pub fn room(&self) -> usize {
        self.window.saturating_sub(self.queued())
    }

    /// Queues a request behind the ones already queued; returns at once and never sleeps (callable from a
    /// completion callback). `Busy` if it does not fit the window now; `Invalid` if it never can (more packets
    /// than the window). On an error `on_done` never runs.
    pub fn queue(&self, req: IsoRequest, on_done: IsoDone) -> Result<(), UsbError> {
        if self.dev.is_gone() {
            return Err(UsbError::Gone);
        }
        let n = req.packets.len();
        if n == 0 || !req.in_bounds() {
            return Err(UsbError::Invalid("isochronous request without packets or outside its buffer"));
        }
        let max = self.max_packet();
        if req.packets.iter().any(|p| p.len > max) {
            return Err(UsbError::Invalid("isochronous packet longer than the endpoint's service interval payload"));
        }
        let w = self.window;
        if n > w {
            return Err(UsbError::Invalid("isochronous request longer than the pipe's window"));
        }
        self.queued.try_update(Ordering::AcqRel, Ordering::Acquire, |q| (q + n <= w).then_some(q + n)).map_err(|_| UsbError::Busy)?;
        let q = self.queued.clone();
        let urb = IsoUrb::new(n, Box::new(move |r| {
            q.fetch_sub(n, Ordering::AcqRel);
            on_done(r)
        }));
        let r = self.dev.host.submit_iso(self.dev.slot, self.ep.address, req, urb);
        if r.is_err() {
            self.queued.fetch_sub(n, Ordering::AcqRel);
        }
        r
    }

    /// Stops the endpoint and ends everything queued (`Cancelled`; Stop Endpoint and the dequeue pointer past
    /// it); the pipe can queue again afterwards. Returns the packets ended.
    pub fn stop(&self) -> Result<usize, UsbError> {
        if self.dev.is_gone() {
            return Ok(0);
        }
        self.dev.host.abort_endpoint(self.dev.slot, self.ep.address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::SsCompanion;
    use alloc::vec;
    use std::sync::Mutex as StdMutex;

    fn ep(mps_raw: u16, interval: u8, companion: Option<SsCompanion>) -> Endpoint {
        Endpoint { address: 0x01, attributes: 0x09, max_packet_raw: mps_raw, interval, companion }
    }

    #[test]
    fn intervals_and_payloads() {
        // Georg's JBL Quantum 800 game stream: full speed, 0x0120 (288 bytes), bInterval 1 = 1 ms.
        let jbl = ep(0x0120, 1, None);
        assert_eq!(service_interval_ns(Speed::Full, &jbl), 1_000_000);
        assert_eq!(max_service_payload(Speed::Full, &jbl), 288);
        // High speed, 3 transactions of 1024 per microframe, every 4th microframe.
        let hs = ep(0x1400, 3, None);
        assert_eq!(service_interval_ns(Speed::High, &hs), 500_000);
        assert_eq!(max_service_payload(Speed::High, &hs), 3 * 1024);
        // SuperSpeed: the companion limits the payload; 0 there means the full burst x mult.
        let c = SsCompanion { max_burst: 3, attributes: 1, bytes_per_interval: 5000 };
        assert_eq!(max_service_payload(Speed::Super, &ep(1024, 1, Some(c))), 5000);
        let c0 = SsCompanion { bytes_per_interval: 0, ..c };
        assert_eq!(max_service_payload(Speed::Super, &ep(1024, 1, Some(c0))), 1024 * 4 * 2);
        assert_eq!(service_interval_ns(Speed::Full, &ep(64, 0, None)), 1_000_000, "bInterval 0 taken as 1");
    }

    #[test]
    fn urb_completes_once_and_results_add_up() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let s = seen.clone();
        let u = IsoUrb::new(2, Box::new(move |r| s.lock().unwrap().push(r)));
        let r = IsoResult { packets: vec![XferResult::ok(192), XferResult::failed(UsbError::Transfer(23), 0)], error: None };
        assert_eq!((r.bytes(), r.failed()), (192, 1));
        assert!(u.complete(r.clone()));
        assert!(!u.complete(IsoResult { packets: vec![], error: Some(UsbError::Gone) }));
        assert!(u.is_done());
        assert_eq!(*seen.lock().unwrap(), vec![r]);
    }
}

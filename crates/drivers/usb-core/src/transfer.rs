//! The transfer API class drivers use for bulk and interrupt endpoints (US-3; spec usb.md 3.5, 3.6).
//!
//! - **Buffers** come from the device's host (`Device::alloc_buffer`): DMA memory within the controller's address
//!   limit, possibly made of several pieces. A class driver reads and writes it through `XferMem`; it never sees a
//!   device address, and a host accepts only buffers it made itself (`as_any` lets it check), so safe code cannot
//!   point a controller at arbitrary memory. The host keeps its own reference while the controller may still touch
//!   the memory, so dropping a `Buffer` early never frees memory under a running transfer.
//! - **Transfers** are queued per endpoint, any number at once (R1): `Device::submit` returns a `Transfer` at once;
//!   its `Urb` completes in the host's event context (callback) and wakes waiters. Completion runs in the
//!   controller's event thread (polled now, MSI-X later wakes the same thread) or in a waiter draining for it,
//!   never in a hard IRQ, so callbacks may take spin locks and submit further transfers, but must not sleep or
//!   wait. For one controller, callbacks and pipe sinks run one at a time and in the order the controller
//!   reported their transfers (a pipe's `Stopped` comes after its last report).
//! - **Timeouts**: `Transfer::wait(timeout)` cancels the transfer when the time is up (xHCI: Stop Endpoint, then
//!   the ring's dequeue pointer moved past it or its TRBs turned into No-Ops, so nothing stale stays queued). The
//!   result carries the bytes that moved before the stop.
//! - **Errors**: a STALL or transaction error halts the endpoint; transfers queued behind wait until the driver
//!   calls `Device::clear_halt` (toggle back to 0 on both sides, then they run) or `Device::abort` (all cancelled).
//! - **Interrupt IN pipes** (`Device::interrupt_pipe`): the host keeps `depth` transfers queued and hands every
//!   report to a sink; errors are repaired by the host (V1's give-up rule after 50 in a row).

use crate::host::{Sink, UsbError};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};

/// DMA memory a transfer reads from or writes into, as the CPU sees it. Made by a host (`Host::alloc_buffer`).
pub trait XferMem: Send + Sync + Any {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Copies `out.len()` bytes from offset `off`; false if that range is outside the buffer.
    fn read(&self, off: usize, out: &mut [u8]) -> bool;
    /// Copies `data` to offset `off`; false if that range is outside the buffer.
    fn write(&self, off: usize, data: &[u8]) -> bool;
    /// For the host that made it: lets it find its own type again (foreign buffers are refused).
    fn as_any(&self) -> &dyn Any;
}

pub type Buffer = Arc<dyn XferMem>;

/// One transfer: `len` bytes at `offset` of `buf` (IN: the most the device may send; OUT: what goes out; 0 = a
/// zero-length packet). An OUT transfer that is a whole number of max-size packets ends without a zero-length
/// packet; a class whose protocol wants one there (Linux `URB_ZERO_PACKET`: CDC ACM/ECM, not BOT) submits a
/// `len` 0 request right behind it on the same endpoint (`Endpoint::max_packet` tells the size).
#[derive(Clone)]
pub struct Request {
    pub buf: Buffer,
    pub offset: usize,
    pub len: usize,
}

impl Request {
    pub fn new(buf: Buffer, offset: usize, len: usize) -> Self {
        Request { buf, offset, len }
    }
    /// The whole buffer.
    pub fn whole(buf: Buffer) -> Self {
        let len = buf.len();
        Request { buf, offset: 0, len }
    }
    /// The range is inside the buffer.
    pub fn in_bounds(&self) -> bool {
        self.offset.checked_add(self.len).is_some_and(|e| e <= self.buf.len())
    }
}

/// How a transfer ended: the bytes that moved, and the error if it did not complete. A short IN transfer is no
/// error (`actual` < the length asked for); a failed one may still have moved data (BOT: "data before a stall
/// counts").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XferResult {
    pub actual: usize,
    pub error: Option<UsbError>,
}

impl XferResult {
    pub fn ok(actual: usize) -> Self {
        XferResult { actual, error: None }
    }
    pub fn failed(error: UsbError, actual: usize) -> Self {
        XferResult { actual, error: Some(error) }
    }
    /// `Ok(actual)` or the error.
    pub fn status(&self) -> Result<usize, UsbError> {
        match self.error {
            None => Ok(self.actual),
            Some(e) => Err(e),
        }
    }
}

/// Runs once when the transfer ends, in the host's event context (must not sleep or wait).
pub type OnDone = Box<dyn FnOnce(&XferResult) + Send>;

static NEXT_URB: AtomicU64 = AtomicU64::new(1);

/// How long `Transfer::wait`/`cancel` give a host's late completion before ending the transfer themselves.
const FINISH_GRACE_NS: u64 = 20_000_000;

/// The shared state of one transfer between the class driver and the host.
pub struct Urb {
    pub id: u64,
    done: AtomicBool,
    result: SpinLock<Option<XferResult>>,
    on_done: SpinLock<Option<OnDone>>,
    wq: WaitQueue,
}

impl Urb {
    pub fn new(on_done: Option<OnDone>) -> Arc<Urb> {
        Arc::new(Urb {
            id: NEXT_URB.fetch_add(1, Ordering::Relaxed),
            done: AtomicBool::new(false),
            result: SpinLock::named("usb.urb", None),
            on_done: SpinLock::named("usb.urb", on_done),
            wq: WaitQueue::new(),
        })
    }
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
    pub fn result(&self) -> Option<XferResult> {
        *self.result.lock()
    }
    /// Host side: the first completion wins (later ones return false); stores the result, wakes waiters and runs
    /// the callback. Never called with a host lock held.
    pub fn complete(&self, r: XferResult) -> bool {
        {
            let mut g = self.result.lock();
            if g.is_some() {
                return false;
            }
            *g = Some(r);
        }
        self.done.store(true, Ordering::Release);
        self.wq.wake_all();
        let cb = self.on_done.lock().take();
        if let Some(f) = cb {
            f(&r);
        }
        true
    }
    /// Where waiters sleep (the host's `wait` uses it).
    pub fn wait_queue(&self) -> &WaitQueue {
        &self.wq
    }
}

impl core::fmt::Debug for Urb {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Urb({} {:?})", self.id, self.result())
    }
}

/// An interrupt IN pipe: `len` bytes per transfer (at most 64 KiB), `depth` transfers kept queued (at least 1;
/// more keep the endpoint busy while a report is being handed on, which fast devices such as 1 ms gamepads need).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipeOpts {
    pub len: usize,
    pub depth: usize,
    /// The bInterval this pipe wants instead of the descriptor's, in the endpoint's own encoding (IN-2b: 1 ms
    /// gamepad polling). None: the descriptor's. A host that cannot apply it keeps the old interval.
    pub interval: Option<u8>,
}

impl PipeOpts {
    pub fn new(len: usize, depth: usize) -> Self {
        PipeOpts { len, depth, interval: None }
    }
    pub fn with_interval(self, b: u8) -> Self {
        PipeOpts { interval: Some(b), ..self }
    }
}

/// What a pipe delivers goes to a `Sink` (`host::PipeEvent`); this is how the driver stops it again.
pub struct PipeHandle {
    pub(crate) dev: Arc<crate::device::Device>,
    pub(crate) ep: u8,
}

impl PipeHandle {
    pub fn endpoint(&self) -> u8 {
        self.ep
    }
    /// Stops the pipe: queued transfers cancelled, the sink gets `Stopped(Cancelled)` once.
    pub fn stop(&self) -> Result<(), UsbError> {
        if self.dev.is_gone() {
            return Ok(());
        }
        self.dev.host.stop_pipe(self.dev.slot, self.ep)
    }
}

/// A queued transfer.
pub struct Transfer {
    pub(crate) dev: Arc<crate::device::Device>,
    pub(crate) ep: u8,
    pub(crate) urb: Arc<Urb>,
}

impl Transfer {
    pub fn urb(&self) -> &Arc<Urb> {
        &self.urb
    }
    pub fn endpoint(&self) -> u8 {
        self.ep
    }
    pub fn is_done(&self) -> bool {
        self.urb.is_done()
    }
    pub fn result(&self) -> Option<XferResult> {
        self.urb.result()
    }
    /// Waits at most `timeout_ns`. When the time is up the transfer is cancelled (Stop Endpoint and skip) and
    /// ends with `Timeout`, unless it completed in the meantime. Never waits without a deadline (B4).
    pub fn wait(&self, timeout_ns: u64) -> XferResult {
        if !self.urb.is_done() && !self.dev.host.wait(self.dev.slot, &self.urb, timeout_ns) {
            self.dev.host.cancel(self.dev.slot, self.ep, &self.urb, UsbError::Timeout);
        }
        self.finish()
    }
    /// Cancels the transfer now (it ends with `Cancelled` unless it completed first).
    pub fn cancel(&self) -> XferResult {
        if !self.urb.is_done() {
            self.dev.host.cancel(self.dev.slot, self.ep, &self.urb, UsbError::Cancelled);
        }
        self.finish()
    }
    fn finish(&self) -> XferResult {
        // A host's cancel always completes the URB; this only guards against a host that did not. A result still on
        // its way (the host's event context about to deliver it) gets a short grace first, so a transfer that did
        // complete is never reported as timed out.
        if !self.urb.is_done() && !self.dev.host.wait(self.dev.slot, &self.urb, FINISH_GRACE_NS) {
            self.urb.complete(XferResult::failed(UsbError::Timeout, 0));
        }
        self.urb.result().unwrap_or(XferResult::failed(UsbError::Timeout, 0))
    }
}

/// The sink type of interrupt pipes, re-exported here for class drivers.
pub type PipeSink = Sink;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::sync::atomic::AtomicUsize;

    struct Mem(SpinLock<Vec<u8>>);
    impl XferMem for Mem {
        fn len(&self) -> usize {
            self.0.lock().len()
        }
        fn read(&self, off: usize, out: &mut [u8]) -> bool {
            let v = self.0.lock();
            match v.get(off..off.saturating_add(out.len())) {
                Some(s) => {
                    out.copy_from_slice(s);
                    true
                }
                None => false,
            }
        }
        fn write(&self, off: usize, data: &[u8]) -> bool {
            let mut v = self.0.lock();
            match v.get_mut(off..off.saturating_add(data.len())) {
                Some(s) => {
                    s.copy_from_slice(data);
                    true
                }
                None => false,
            }
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn first_completion_wins_and_the_callback_runs_once() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let u = Urb::new(Some(Box::new(|r: &XferResult| {
            assert_eq!(*r, XferResult::ok(12));
            CALLS.fetch_add(1, Ordering::Relaxed);
        })));
        assert!(!u.is_done() && u.result().is_none());
        assert!(u.complete(XferResult::ok(12)));
        assert!(!u.complete(XferResult::failed(UsbError::Timeout, 0)), "a late cancel does not overwrite");
        assert_eq!((u.is_done(), u.result()), (true, Some(XferResult::ok(12))));
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
        assert!(Urb::new(None).id != u.id);
    }

    #[test]
    fn results_and_ranges() {
        assert_eq!(XferResult::ok(5).status(), Ok(5));
        let f = XferResult::failed(UsbError::Stall, 3);
        assert_eq!((f.status(), f.actual), (Err(UsbError::Stall), 3), "bytes before a stall are kept");
        let b: Buffer = Arc::new(Mem(SpinLock::new(alloc::vec![0u8; 64])));
        assert!(Request::whole(b.clone()).in_bounds());
        assert!(Request::new(b.clone(), 60, 4).in_bounds());
        assert!(!Request::new(b.clone(), 60, 5).in_bounds());
        assert!(!Request::new(b.clone(), usize::MAX, 2).in_bounds(), "no overflow");
        assert!(Request::new(b, 64, 0).in_bounds(), "zero-length at the end");
        assert_eq!(UsbError::Cancelled.as_str(), "cancelled");
    }
}

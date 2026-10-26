//! A device the core enumerated: where it sits, its descriptors, the configuration that was set, and the way to
//! its endpoints through its host controller. Class drivers get an `Arc<Device>`.

use crate::desc::{Configuration, DeviceDescriptor, EpKind};
use crate::host::{Data, HcSlot, Host, PipeEvent, Sink, TimedSink, UsbError};
use crate::transfer::{Buffer, OnDone, PipeHandle, PipeOpts, Request, Transfer, Urb, XferResult};
use alloc::vec;
use alloc::vec::Vec;
use crate::place::{DeviceId, Place};
use crate::setup::Setup;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::Mutex;

pub struct Device {
    pub id: DeviceId,
    pub(crate) host: Arc<dyn Host>,
    pub slot: HcSlot,
    /// The USB address the controller assigned.
    pub address: u8,
    pub place: Place,
    pub desc: DeviceDescriptor,
    /// The configuration that was set (all devices with one get their first; `None` if they have none).
    pub config: Option<Configuration>,
    pub(crate) gone: AtomicBool,
    /// Held while its interfaces are offered to drivers: enumeration and a driver registering at the same time
    /// must not probe one interface twice.
    pub(crate) bind_lock: Mutex<()>,
    /// Interfaces switched to another alternate setting (`alt.rs`).
    pub(crate) alts: crate::alt::Alts,
}

impl Device {
    pub fn host(&self) -> &Arc<dyn Host> {
        &self.host
    }
    pub fn host_name(&self) -> &str {
        self.host.name()
    }
    pub fn speed(&self) -> crate::speed::Speed {
        self.place.speed
    }
    pub fn vendor(&self) -> u16 {
        self.desc.vendor
    }
    pub fn product(&self) -> u16 {
        self.desc.product
    }
    /// "xhci0-3.1.4": controller and port path, for logs.
    pub fn path(&self) -> String {
        format!("{}-{}", self.host.name(), self.place.path())
    }
    /// True once the device was detached; every transfer then fails with `Gone`.
    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    pub fn control(&self, setup: Setup, data: Data<'_>) -> Result<usize, UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        self.host.control(self.slot, setup, data)
    }
    pub fn control_in(&self, setup: Setup, buf: &mut [u8]) -> Result<usize, UsbError> {
        self.control(setup, Data::In(buf))
    }
    pub fn control_out(&self, setup: Setup, data: &[u8]) -> Result<usize, UsbError> {
        let d = if data.is_empty() { Data::None } else { Data::Out(data) };
        self.control(setup, d)
    }
    /// Polls interrupt IN endpoint `ep` with one `len`-byte transfer queued until the device goes (hubs).
    pub fn interrupt_in(&self, ep: u8, len: usize, sink: Sink) -> Result<(), UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        self.host.interrupt_in(self.slot, ep, PipeOpts::new(len, 1), sink)
    }

    // ---- transfers on bulk and interrupt endpoints (US-3; module `transfer`)

    /// An interrupt IN pipe with `opts.depth` transfers queued; reports and errors go to `sink` (event context).
    /// Stop it with the handle (or it ends when the device goes).
    pub fn interrupt_pipe(self: &Arc<Self>, ep: u8, opts: PipeOpts, sink: Sink) -> Result<PipeHandle, UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        if opts.depth == 0 {
            return Err(UsbError::Invalid("a pipe needs at least one queued transfer"));
        }
        self.host.interrupt_in(self.slot, ep, opts, sink)?;
        Ok(PipeHandle { dev: self.clone(), ep })
    }

    /// Like `interrupt_pipe`, each event with the time the controller's interrupt for it arrived (IN-6 input
    /// stamps). A host that does not track it stamps when the event reaches the sink (`Host::now_ns`).
    pub fn interrupt_pipe_at(self: &Arc<Self>, ep: u8, opts: PipeOpts, sink: TimedSink) -> Result<PipeHandle, UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        if opts.depth == 0 {
            return Err(UsbError::Invalid("a pipe needs at least one queued transfer"));
        }
        match self.host.interrupt_in_at(self.slot, ep, opts, &sink) {
            Some(r) => r?,
            None => {
                // Weak: the host keeps the sink, the sink must not keep the host.
                let clock = Arc::downgrade(&self.host);
                let at = move |e: PipeEvent<'_>| sink(e, clock.upgrade().map_or(0, |h| h.now_ns()));
                self.host.interrupt_in(self.slot, ep, opts, Arc::new(at))?
            }
        }
        Ok(PipeHandle { dev: self.clone(), ep })
    }

    /// Zeroed DMA memory for transfers to and from this device.
    pub fn alloc_buffer(&self, len: usize) -> Result<Buffer, UsbError> {
        self.host.alloc_buffer(len)
    }

    /// Queues a transfer on bulk or interrupt endpoint `ep` (address; direction from bit 7) behind the ones already
    /// queued. Returns at once; the transfer runs while the caller does other things.
    pub fn submit(self: &Arc<Self>, ep: u8, req: Request) -> Result<Transfer, UsbError> {
        self.submit_urb(ep, req, None)
    }

    /// Like `submit`, and `on_done` runs when it ends (event context: must not sleep or wait, may submit more).
    pub fn submit_then(self: &Arc<Self>, ep: u8, req: Request, on_done: OnDone) -> Result<Transfer, UsbError> {
        self.submit_urb(ep, req, Some(on_done))
    }

    fn submit_urb(self: &Arc<Self>, ep: u8, req: Request, on_done: Option<OnDone>) -> Result<Transfer, UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        if !req.in_bounds() {
            return Err(UsbError::Invalid("transfer range outside its buffer"));
        }
        if !self.endpoint(ep).is_some_and(|e| matches!(e.kind(), EpKind::Bulk | EpKind::Interrupt)) {
            return Err(UsbError::Invalid("not a bulk or interrupt endpoint of this configuration"));
        }
        let urb = Urb::new(on_done);
        self.host.submit(self.slot, ep, req, urb.clone())?;
        Ok(Transfer { dev: self.clone(), ep, urb })
    }

    /// Queues a transfer and waits for it at most `timeout_ns` (cancelled when the time is up).
    pub fn transfer(self: &Arc<Self>, ep: u8, req: Request, timeout_ns: u64) -> XferResult {
        match self.submit(ep, req) {
            Ok(t) => t.wait(timeout_ns),
            Err(e) => XferResult::failed(e, 0),
        }
    }

    /// Sends `data` on OUT endpoint `ep` (copied into a fresh buffer); returns the bytes sent.
    pub fn out_bytes(self: &Arc<Self>, ep: u8, data: &[u8], timeout_ns: u64) -> Result<usize, UsbError> {
        let buf = self.alloc_buffer(data.len().max(1))?;
        if !buf.write(0, data) {
            return Err(UsbError::NoMemory);
        }
        self.transfer(ep, Request::new(buf, 0, data.len()), timeout_ns).status()
    }

    /// Reads at most `len` bytes from IN endpoint `ep` (one transfer; short is fine).
    pub fn in_bytes(self: &Arc<Self>, ep: u8, len: usize, timeout_ns: u64) -> Result<Vec<u8>, UsbError> {
        let buf = self.alloc_buffer(len.max(1))?;
        let n = self.transfer(ep, Request::new(buf.clone(), 0, len), timeout_ns).status()?;
        let mut v = vec![0u8; n];
        if !buf.read(0, &mut v) {
            return Err(UsbError::NoMemory);
        }
        Ok(v)
    }

    /// After a STALL (or to start a clean exchange): CLEAR_FEATURE(ENDPOINT_HALT) to the device (its toggle goes
    /// to 0), then the host side back to toggle 0 too (Reset Endpoint, or Stop + Configure Endpoint when it is not
    /// halted; spec usb.md 3.6). The device first, as Linux `usb_clear_halt` does: transfers queued behind a
    /// failed one run again as soon as the host side restarts, so the device must be ready by then. If the
    /// device refuses the request, the host side is left as it is (Linux: "don't un-halt except on success").
    /// On an endpoint that is not halted, a transfer the reset catches after it has moved data cannot go on
    /// (the controller forgets its place in it): it ends with `Cancelled` and the bytes it moved, and the ones
    /// behind it run. Normally the queue is empty or halted here (after a STALL, or after `abort`).
    pub fn clear_halt(&self, ep: u8) -> Result<(), UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        if ep & 0x0F == 0 {
            return Err(UsbError::Invalid("endpoint 0 has no halt to clear"));
        }
        self.control(crate::setup::Setup::clear_halt(ep), Data::None)?;
        self.host.reset_endpoint(self.slot, ep)
    }

    /// Cancels every transfer queued on `ep` (each ends with `Cancelled`); returns how many there were.
    pub fn abort(&self, ep: u8) -> Result<usize, UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        self.host.abort_endpoint(self.slot, ep)
    }
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Device({} {} {:04x}:{:04x})", self.id, self.path(), self.desc.vendor, self.desc.product)
    }
}

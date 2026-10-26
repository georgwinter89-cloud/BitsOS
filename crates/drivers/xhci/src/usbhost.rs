//! The controller as a `usb_core::Host`: what the core's enumeration, hub driver and class drivers call.

use crate::hc::{Controller, Env};
use alloc::sync::Arc;
use usb_core::{Buffer, Data, Endpoint, HcSlot, Host, HubParams, IsoRequest, IsoUrb, PipeEvent, PipeOpts, Place, Request, Setup, Sink, TimedSink, Urb, UsbError};

impl<E: Env> Host for Controller<E> {
    fn name(&self) -> &str {
        &self.name
    }
    fn now_ns(&self) -> u64 {
        self.env.now_ns()
    }
    fn sleep_ns(&self, ns: u64) {
        self.env.sleep_ns(ns)
    }
    fn address(&self, place: &Place, mps0: u16, deferred: bool) -> Result<(HcSlot, u8), UsbError> {
        self.address_device(place, mps0, deferred)
    }
    fn set_address(&self, slot: HcSlot) -> Result<u8, UsbError> {
        Controller::set_address(self, slot)
    }
    fn set_mps0(&self, slot: HcSlot, mps0: u16) -> Result<(), UsbError> {
        self.evaluate_mps0(slot, mps0)
    }
    fn control(&self, slot: HcSlot, setup: Setup, data: Data<'_>) -> Result<usize, UsbError> {
        self.control_transfer(slot, setup, data)
    }
    fn configure(&self, slot: HcSlot, endpoints: &[Endpoint]) -> Result<(), UsbError> {
        self.configure_eps(slot, endpoints)
    }
    fn set_hub(&self, slot: HcSlot, hub: HubParams) -> Result<(), UsbError> {
        self.configure_hub(slot, hub)
    }
    fn interrupt_in(&self, slot: HcSlot, ep: u8, opts: PipeOpts, sink: Sink) -> Result<(), UsbError> {
        self.start_pipe(slot, ep, opts, Arc::new(move |e: PipeEvent<'_>, _at: u64| sink(e)))
    }
    fn interrupt_in_at(&self, slot: HcSlot, ep: u8, opts: PipeOpts, sink: &TimedSink) -> Option<Result<(), UsbError>> {
        Some(self.start_pipe(slot, ep, opts, sink.clone()))
    }
    fn stop_pipe(&self, slot: HcSlot, ep: u8) -> Result<(), UsbError> {
        Controller::stop_pipe(self, slot, ep)
    }
    fn alloc_buffer(&self, len: usize) -> Result<Buffer, UsbError> {
        self.alloc_xfer_buffer(len, false)
    }
    fn submit(&self, slot: HcSlot, ep: u8, req: Request, urb: Arc<Urb>) -> Result<(), UsbError> {
        Controller::submit(self, slot, ep, req, urb)
    }
    fn wait(&self, slot: HcSlot, urb: &Urb, timeout_ns: u64) -> bool {
        self.wait_urb(slot, urb, timeout_ns)
    }
    fn cancel(&self, slot: HcSlot, ep: u8, urb: &Arc<Urb>, why: UsbError) {
        self.cancel_urb(slot, ep, urb, why)
    }
    fn reset_endpoint(&self, slot: HcSlot, ep: u8) -> Result<(), UsbError> {
        Controller::reset_endpoint(self, slot, ep)
    }
    fn abort_endpoint(&self, slot: HcSlot, ep: u8) -> Result<usize, UsbError> {
        Controller::abort_endpoint(self, slot, ep)
    }
    fn set_interface(&self, slot: HcSlot, drop: &[Endpoint], add: &[Endpoint]) -> Result<(), UsbError> {
        self.set_interface_eps(slot, drop, add)
    }
    fn submit_iso(&self, slot: HcSlot, ep: u8, req: IsoRequest, urb: Arc<IsoUrb>) -> Result<(), UsbError> {
        Controller::submit_iso(self, slot, ep, req, urb)
    }
    fn release(&self, slot: HcSlot) {
        self.release_slot(slot)
    }
    fn kick(&self) {
        Controller::kick(self)
    }
}

//! US-R: what the controller adds to the recovery of a wedged device. `look_ep` answers "did this endpoint really
//! stand still?" with what the controller itself shows, after handing on whatever it already wrote to the event
//! ring (`usb_core::Device::still_moving`). `reset_slot` is the slot's part of a device reset in place (Linux
//! `xhci_discover_or_reset_device`, linux-6.6/drivers/usb/host/xhci.c:3643): after the port reset, Reset Device
//! keeps the slot and its address-0 state; Address Device (`set_address`) and Configure Endpoint follow.

use crate::bulk::dci_of;
use crate::hc::{Controller, Env, Source};
use crate::trb::{cc, Trb};
use alloc::vec::Vec;
use usb_core::{EpLook, HcSlot, UsbError};

klog::counter!(SLOT_RESETS, "usb.xhci.slot_resets");

/// A look never waits for long behind a slot operation (a control transfer may hold it for seconds): the dequeue
/// pointer is left out then.
const LOOK_GATE_NS: u64 = 1_000_000;

impl<E: Env> Controller<E> {
    pub(crate) fn look_ep(&self, h: HcSlot, ep: u8) -> Option<EpLook> {
        self.process_events(Source::Waiter);
        let e = self.entry(h).ok()?;
        let dci = dci_of(ep);
        let events = e.xfer.lock().ep_ring(dci)?.events;
        let dequeue = e.op.lock_timeout(LOOK_GATE_NS).and_then(|op| {
            let oc = self.ctx_out(&op.out);
            Some((oc.get(dci as usize, 2)? as u64 & !0xF) | (oc.get(dci as usize, 3)? as u64) << 32)
        });
        Some(EpLook { events, dequeue: dequeue.unwrap_or(0) })
    }

    /// `usb_core::Host::reset_slot`. Only after Reset Device has answered are the rings and buffers free: if it
    /// does not answer, everything stays as it was and the transfers stay queued for the next try.
    pub(crate) fn reset_slot(&self, h: HcSlot) -> Result<(), UsbError> {
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        match self.cmd(Trb::reset_device(e.id)) {
            // Already in the Default state (an earlier try got that far; Linux takes this as no error too).
            Ok(_) | Err(UsbError::Command(cc::CONTEXT_STATE)) => {}
            Err(err) => return Err(err),
        }
        let mut ended = Vec::new();
        e.xfer.lock().forget_endpoints(UsbError::Cancelled, &mut ended);
        self.defer(ended);
        drop(op);
        e.wq.wake_all();
        self.deliver_pending();
        SLOT_RESETS.inc();
        klog::event!(Debug, "usb", "xhci.slot_reset", ctrl = self.name.as_str(), slot = e.id, root_port = e.root_port);
        Ok(())
    }
}

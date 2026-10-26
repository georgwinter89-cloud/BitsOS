//! The hotplug worker's part for isochronous endpoints (SN-8a): an error that may halt one (STALL, Babble, USB
//! Transaction, Split Transaction; Linux `xhci_requires_manual_halt_cleanup`) sets `IsoEp::check`. The worker
//! looks: halted -> Reset Endpoint + Set TR Dequeue to the next live TD, then rung again. A refused command is
//! tried again in later rounds (the endpoint counts as halted meanwhile, no doorbell); after `ISO_REPAIR_LIMIT`
//! refusals in a row the repair is given up as for interrupt pipes: everything queued ends with the error and the
//! endpoint refuses requests until its alternate setting is set again.

use crate::ctx::ep_state;
use crate::hc::{Controller, Env};
use crate::pipe::PIPE_ERROR_LIMIT;
use crate::trb::Trb;
use crate::xfer::code_err;
use alloc::vec::Vec;

klog::counter!(ISO_REPAIRS, "usb.xhci.iso_repairs");

/// Refused repairs in a row before the endpoint is given up (the same bound as interrupt pipes).
pub(crate) const ISO_REPAIR_LIMIT: u32 = PIPE_ERROR_LIMIT;

impl<E: Env> Controller<E> {
    pub(crate) fn repair_isoch(&self) {
        let entries: Vec<_> = self.slots.lock().iter().flatten().cloned().collect();
        for e in entries {
            let todo: Vec<(u8, u8)> = {
                let mut x = e.xfer.lock();
                x.eps.iter_mut().enumerate().filter_map(|(d, r)| {
                    let iso = r.as_mut()?.iso.as_mut()?;
                    core::mem::take(&mut iso.check).then_some((d as u8, iso.check_code))
                }).collect()
            };
            for (dci, code) in todo {
                // Device gone or being released: its requests end with it, nothing to repair.
                let Ok(op) = self.lock_op(&e) else { continue };
                if self.ep_state(&op, dci) != ep_state::HALTED {
                    if let Some(iso) = e.xfer.lock().ep_ring(dci).and_then(|r| r.iso.as_mut()) {
                        iso.repair_fails = 0;
                    }
                    continue;
                }
                let target = {
                    let mut x = e.xfer.lock();
                    let Some(r) = x.ep_ring(dci) else { continue };
                    r.busy = true;
                    r.next_live()
                };
                let res = self.cmd(Trb::reset_endpoint(e.id, dci)).and_then(|_| self.cmd(Trb::set_tr_dequeue(e.id, dci, target.0, target.1)));
                let mut given_up = None;
                if let Some(r) = e.xfer.lock().ep_ring(dci) {
                    match (&res, r.iso.as_mut()) {
                        (Ok(_), Some(iso)) => {
                            iso.repair_fails = 0;
                            r.retire_leading();
                            r.halted = false;
                        }
                        (Err(_), Some(iso)) => {
                            iso.repair_fails += 1;
                            if iso.repair_fails >= ISO_REPAIR_LIMIT {
                                let err = code_err(code);
                                iso.dead = Some(err);
                                given_up = Some(err);
                            } else {
                                iso.check = true;
                                iso.check_code = code;
                            }
                            r.halted = true;
                        }
                        _ => {}
                    }
                }
                if let Some(err) = given_up {
                    self.abort_locked(&e, &op, dci, err);
                }
                drop(op);
                self.unbusy(&e, dci);
                ISO_REPAIRS.inc();
                match (&res, given_up) {
                    (Ok(_), _) => klog::event!(Warn, "usb", "xhci.iso_repaired", ctrl = self.name.as_str(), slot = e.id, dci = dci,
                        code = code; "an isochronous endpoint halted after an error; reset"),
                    (Err(x), None) => {
                        klog::event!(Warn, "usb", "xhci.iso_repair_retry", ctrl = self.name.as_str(), slot = e.id, dci = dci,
                            code = code, error = x.as_str(); "resetting a halted isochronous endpoint failed; trying again");
                        self.kick();
                    }
                    (Err(x), Some(_)) => klog::event!(Error, "usb", "xhci.iso_given_up", ctrl = self.name.as_str(), slot = e.id,
                        dci = dci, code = code, errors = ISO_REPAIR_LIMIT, error = x.as_str();
                        "a halted isochronous endpoint does not reset; its requests end until the alternate setting is set again"),
                }
            }
        }
        self.deliver_pending();
    }
}

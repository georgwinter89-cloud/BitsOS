//! Alternate settings (SN-8a): SET_INTERFACE's host side (`usb_core::Host::set_interface`, Linux
//! `usb_hcd_alloc_bandwidth` + `xhci_check_bandwidth`). One Configure Endpoint drops the old setting's endpoints
//! and adds the new one's with fresh rings; a DCI in both is dropped and added (its context replaced). Context
//! Entries = the highest DCI still configured. A refusal (Bandwidth Error 8, Resource Error 7) changes nothing.

use crate::ctx::ep_context;
use crate::hc::{Controller, Env, SEGMENT_BYTES};
use crate::mem::DmaMem;
use crate::ring::ProducerRing;
use crate::trb::Trb;
use alloc::vec;
use alloc::vec::Vec;
use usb_core::{Endpoint, EpKind, HcSlot, UsbError};

/// Endpoints a Configure Endpoint can carry (no control endpoints, a real DCI, a packet size).
fn usable(ep: &Endpoint) -> bool {
    ep.kind() != EpKind::Control && (2..=31).contains(&ep.dci()) && ep.max_packet() > 0
}

/// Context Entries after a change: the highest DCI added, or configured before and not dropped.
pub(crate) fn context_entries(have: &[u8], drop: u32, add: u32) -> u8 {
    (1..32u8).rev().find(|&d| add & 1 << d != 0 || (have.contains(&d) && drop & 1 << d == 0)).unwrap_or(1)
}

impl<E: Env> Controller<E> {
    pub(crate) fn set_interface_eps(&self, h: HcSlot, old: &[Endpoint], add: &[Endpoint]) -> Result<(), UsbError> {
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        let have = e.xfer.lock().configured();
        let drops = old.iter().filter(|ep| usable(ep) && have.contains(&ep.dci())).fold(0u32, |a, ep| a | 1 << ep.dci());
        let mut new: Vec<(Endpoint, ProducerRing<E::Mem>)> = Vec::new();
        for ep in add.iter().filter(|ep| usable(ep)) {
            if new.iter().any(|(x, _)| x.dci() == ep.dci()) {
                continue;
            }
            new.push((*ep, ProducerRing::new(vec![self.alloc_mem(SEGMENT_BYTES, 64)?]).map_err(|_| UsbError::NoMemory)?));
        }
        let add_flags = new.iter().fold(1u32, |a, (ep, _)| a | 1 << ep.dci());
        let entries = context_entries(&have, drops, add_flags);
        let mut s = self.slot_from_output(&op)?;
        s[0] = (s[0] & !(0x1F << 27)) | (entries as u32) << 27;
        let ic = self.ctx_in(&op.inp);
        let mut ok = op.inp.zero() && ic.set_flags(drops, add_flags) && ic.set_all(0, &s);
        for (ep, ring) in &new {
            ok &= ic.set_all(ep.dci() as usize, &ep_context(e.speed, ep, ring.base()));
        }
        if !ok {
            return Err(UsbError::Invalid("context memory"));
        }
        if let Err(err) = self.cmd(Trb::configure_endpoint(op.inp.dev_addr(), e.id)) {
            klog::event!(Warn, "usb", "xhci.alt_refused", ctrl = self.name.as_str(), slot = e.id, drop = klog::Hex(drops as u64),
                add = klog::Hex(add_flags as u64), error = err.as_str(); "the controller keeps the old alternate setting");
            return Err(err);
        }
        {
            let mut x = e.xfer.lock();
            let mut ended = Vec::new();
            for d in (2..32u8).filter(|d| drops & 1 << d != 0) {
                if let Some(mut r) = x.eps[d as usize].take() {
                    r.end_all(UsbError::Invalid("endpoint dropped by a new alternate setting"), &mut ended);
                }
            }
            self.defer(ended);
            for (ep, ring) in new {
                x.set_endpoint(ep, ring);
            }
        }
        drop(op);
        self.deliver_pending();
        klog::event!(Debug, "usb", "xhci.alt_set", ctrl = self.name.as_str(), slot = e.id, drop = klog::Hex(drops as u64),
            add = klog::Hex(add_flags as u64), entries = entries);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_entries_follow_what_is_left() {
        // EP1 IN (DCI 3) stays, EP2 OUT (DCI 4) is dropped, EP6 OUT (DCI 12) added: 12.
        assert_eq!(context_entries(&[3, 4], 1 << 4, 1 | 1 << 12), 12);
        // Back to alternate setting 0: only DCI 3 left.
        assert_eq!(context_entries(&[3, 12], 1 << 12, 1), 3);
        // Nothing left: endpoint 0 only.
        assert_eq!(context_entries(&[4], 1 << 4, 1), 1);
        // Dropped and added again (new packet size): still counted.
        assert_eq!(context_entries(&[4], 1 << 4, 1 | 1 << 4), 4);
    }
}

//! The fake controller's isochronous endpoints (host tests of SN-8a): one TD per service interval, run at once on
//! the doorbell unless `hold` (a controller waiting for the TD's frame). Each TD is checked (Isoch TRB first, the
//! rest Normal, CH/IOC/ISP, TD Size, TLBPC) and recorded with SIA/Frame ID, BEI and its OUT data; IN TDs get
//! bytes of `in_byte`. A script decides what happens to the next TDs: done, missed with or without a Missed
//! Service event, short, an error (optionally halting the endpoint). A ring that runs empty after work gives one
//! Ring Underrun (OUT) or Overrun (IN) event.

use crate::fake::Model;
use crate::fake_usb::ep_state;
use crate::trb::{cc, ty, Trb, BEI, CHAIN, IOC, ISP, SIA};
use std::collections::VecDeque;
use std::format;
use std::vec;
use std::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsoAct {
    Done,
    /// Passed without moving data; `event`: a Missed Service event names the TD.
    Missed { event: bool },
    /// An IN TD gets only this many bytes.
    Short(usize),
    /// A Transfer Event with this code on the TD's first TRB; `halt`: the endpoint halts.
    Error { code: u8, halt: bool },
}

/// A TD the model ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsoSeen {
    pub dci: usize,
    pub len: usize,
    pub sia: bool,
    pub frame: u16,
    pub tlbpc: u32,
    pub bei: bool,
    pub data: Vec<u8>,
}

#[derive(Default)]
pub struct IsoWorld {
    pub hold: bool,
    pub script: VecDeque<IsoAct>,
    pub seen: Vec<IsoSeen>,
    /// Byte every IN TD's data is made of (then incremented).
    pub in_byte: u8,
    /// (slot, DCI) whose ring ran empty after work (one event each time).
    empty_sent: Vec<(u8, usize)>,
}

impl Model {
    fn check_iso_td(&mut self, slot: u8, dci: usize, trbs: &[(u64, Trb)], mps: u32, dir_in: bool) -> IsoSeen {
        let total: usize = trbs.iter().map(|(_, t)| (t.status & 0x1_FFFF) as usize).sum();
        let packets = total.div_ceil(mps.max(1) as usize).max(1);
        let first = trbs[0].1;
        let mut e = Vec::new();
        if first.trb_type() != ty::ISOCH {
            e.push(format!("first TRB type {}", first.trb_type()));
        }
        if trbs[1..].iter().any(|(_, t)| t.trb_type() != ty::NORMAL) {
            e.push("a later TRB of an isochronous TD is not Normal".into());
        }
        let tlbpc = (first.control >> 16) & 0xF;
        if tlbpc != (packets - 1) as u32 {
            e.push(format!("TLBPC {tlbpc} for {packets} packets"));
        }
        let mut done = 0usize;
        for (i, (_, t)) in trbs.iter().enumerate() {
            let last = i + 1 == trbs.len();
            done += (t.status & 0x1_FFFF) as usize;
            let want = if last { 0 } else { (packets - done / mps.max(1) as usize).min(31) };
            if (t.status >> 17) & 0x1F != want as u32 {
                e.push(format!("TD Size {} want {want}", (t.status >> 17) & 0x1F));
            }
            if (t.control & IOC != 0) != last || (t.control & CHAIN != 0) == last {
                e.push(format!("IOC/CH flags {:#x}", t.control));
            }
            if (t.control & ISP != 0) != dir_in {
                e.push(format!("ISP {}", t.control & ISP != 0));
            }
        }
        for x in e {
            self.usb.errors.push(format!("slot {slot} DCI {dci} isochronous TD at {:#x}: {x}", trbs[0].0));
        }
        let mut data = Vec::new();
        if !dir_in {
            for (_, t) in trbs {
                let mut b = vec![0u8; (t.status & 0x1_FFFF) as usize];
                self.read_mem(t.param, &mut b);
                data.extend_from_slice(&b);
            }
        }
        let last = trbs[trbs.len() - 1].1;
        IsoSeen { dci, len: total, sia: first.control & SIA != 0, frame: ((first.control >> 20) & 0x7FF) as u16, tlbpc,
            bei: last.control & BEI != 0, data }
    }

    /// Runs the TDs of an isochronous endpoint (see the module doc).
    pub(crate) fn run_iso(&mut self, slot: u8, dci: usize) {
        let dir_in = dci % 2 == 1;
        loop {
            let Some(ep) = self.usb.slots.get(&slot).map(|s| s.eps[dci]) else { return };
            if ep.state != ep_state::RUNNING || self.usb.iso.hold {
                return;
            }
            if self.event_room() < 3 {
                self.usb.waiting_for_events = true;
                return;
            }
            let Some((trbs, after)) = self.next_td(slot, dci) else {
                if !self.usb.iso.empty_sent.contains(&(slot, dci)) && self.usb.iso.seen.iter().any(|s| s.dci == dci) {
                    self.usb.iso.empty_sent.push((slot, dci));
                    let code = if dir_in { cc::RING_OVERRUN } else { cc::RING_UNDERRUN };
                    self.transfer_event(slot, dci as u8, 0, code, 0);
                }
                return;
            };
            self.usb.iso.empty_sent.retain(|x| *x != (slot, dci));
            let seen = self.check_iso_td(slot, dci, &trbs, ep.mps, dir_in);
            let total = seen.len;
            self.usb.iso.seen.push(seen);
            let act = self.usb.iso.script.pop_front().unwrap_or(IsoAct::Done);
            let fs = self.usb.slots.get_mut(&slot).unwrap();
            fs.eps[dci].deq = after.deq;
            fs.eps[dci].cycle = after.cycle;
            let (first, _) = trbs[0];
            let (last, _) = trbs[trbs.len() - 1];
            match act {
                IsoAct::Missed { event: true } => self.transfer_event(slot, dci as u8, first, cc::MISSED_SERVICE, total as u32),
                IsoAct::Missed { event: false } => {}
                IsoAct::Error { code, halt } => {
                    self.transfer_event(slot, dci as u8, first, code, trbs[0].1.status & 0x1_FFFF);
                    if halt {
                        self.halt(slot, dci);
                        return;
                    }
                }
                IsoAct::Done | IsoAct::Short(_) if !dir_in => self.transfer_event(slot, dci as u8, last, cc::SUCCESS, 0),
                IsoAct::Done | IsoAct::Short(_) => {
                    let n = if let IsoAct::Short(n) = act { n.min(total) } else { total };
                    let b = self.usb.iso.in_byte;
                    self.usb.iso.in_byte = b.wrapping_add(1);
                    let (mut off, mut short_at) = (0usize, None);
                    for (at, t) in &trbs {
                        let len = (t.status & 0x1_FFFF) as usize;
                        let k = len.min(n - off);
                        self.write_mem(t.param, &vec![b; k]);
                        off += k;
                        if k < len {
                            short_at = Some((*at, (len - k) as u32));
                            break;
                        }
                    }
                    match short_at {
                        Some((at, residual)) => {
                            self.transfer_event(slot, dci as u8, at, cc::SHORT_PACKET, residual);
                            if at != last && self.usb.spurious_success {
                                self.transfer_event(slot, dci as u8, last, cc::SUCCESS, 0);
                            }
                        }
                        None => self.transfer_event(slot, dci as u8, last, cc::SUCCESS, 0),
                    }
                }
            }
        }
    }
}

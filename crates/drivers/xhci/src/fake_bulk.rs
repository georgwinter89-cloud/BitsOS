//! The fake controller's bulk and interrupt endpoints (host tests of US-3): whole TDs of chained Normal TRBs,
//! Transfer No-Ops passed silently, IN data from the device filling TRB after TRB (a short packet ends the TD with
//! an event at that TRB; AMD mode reports it as Success with a residual), OUT data gathered and handed to the
//! device, NAK = the TD stays, STALL = Transfer Event with Stall and the endpoint halted. Data toggles are kept on
//! both sides and a mismatch is recorded. Driver mistakes (a TRB across 64 KiB, a wrong TD Size, CH/IOC/ISP
//! flags) go to `World::errors`.

use crate::fake::Model;
use crate::fake_dev::Token;
use crate::fake_usb::{ep_state, FEp};
use crate::trb::{cc, ty, Trb, CHAIN, IOC, ISP};
use std::format;
use std::vec;
use std::vec::Vec;

impl Model {
    /// The next TD on an endpoint: its TRBs (address, TRB) and the endpoint state after it; No-Ops at the head
    /// are passed (counted). `None` if no whole TD is handed over yet.
    pub(crate) fn next_td(&mut self, slot: u8, dci: usize) -> Option<(Vec<(u64, Trb)>, FEp)> {
        let mut ep = self.usb.slots.get(&slot)?.eps[dci];
        loop {
            let (at, t, next) = self.peek(ep)?;
            if t.trb_type() != ty::TRANSFER_NOOP {
                break;
            }
            self.usb.noops += 1;
            ep = next;
            self.usb.slots.get_mut(&slot)?.eps[dci] = FEp { state: ep_state::RUNNING, ..next };
            let _ = at;
            ep.state = ep_state::RUNNING;
        }
        let mut trbs = Vec::new();
        let mut cur = ep;
        loop {
            let (at, t, next) = self.peek(cur)?;
            trbs.push((at, t));
            cur = next;
            if t.control & CHAIN == 0 {
                return Some((trbs, cur));
            }
            if trbs.len() > 4096 {
                self.usb.errors.push(format!("slot {slot} DCI {dci}: chain without end"));
                return None;
            }
        }
    }

    fn check_td(&mut self, slot: u8, dci: usize, trbs: &[(u64, Trb)], mps: u32, dir_in: bool) {
        let total: u64 = trbs.iter().map(|(_, t)| (t.status & 0x1_FFFF) as u64).sum();
        let packets = total.div_ceil(mps.max(1) as u64);
        let mut done = 0u64;
        for (i, (at, t)) in trbs.iter().enumerate() {
            let len = (t.status & 0x1_FFFF) as u64;
            let last = i + 1 == trbs.len();
            let mut e = Vec::new();
            // xHCI 4.11.7.1: a TD going on past a Link TRB has whole packets in front of it.
            if i > 0 && trbs[i - 1].0 + 16 != *at && !done.is_multiple_of(mps.max(1) as u64) {
                e.push(format!("{done} bytes in front of a Link TRB are not whole packets of {mps}"));
            }
            if t.trb_type() != ty::NORMAL {
                e.push(format!("TRB type {}", t.trb_type()));
            }
            if len > 0 && t.param / 0x1_0000 != (t.param + len - 1) / 0x1_0000 {
                e.push(format!("buffer {:#x}+{len:#x} crosses 64 KiB", t.param));
            }
            done += len;
            let want = if last { 0 } else { (packets - done / mps.max(1) as u64).min(31) };
            if (t.status >> 17) & 0x1F != want as u32 {
                e.push(format!("TD Size {} want {want}", (t.status >> 17) & 0x1F));
            }
            if (t.control & IOC != 0) != last || (t.control & CHAIN != 0) == last {
                e.push(format!("IOC/CH flags {:#x}", t.control));
            }
            if (t.control & ISP != 0) != dir_in {
                e.push(format!("ISP {}", t.control & ISP != 0));
            }
            for x in e {
                self.usb.errors.push(format!("slot {slot} DCI {dci} TRB {at:#x}: {x}"));
            }
        }
    }

    fn toggle_step(&mut self, slot: u8, dci: usize, path: &[u8], ep_addr: u8, bytes: usize, mps: u32) {
        let packets = bytes.div_ceil(mps.max(1) as usize).max(1);
        let host = self.usb.slots[&slot].eps[dci].toggle;
        let Some(d) = self.dev(path) else { return };
        let dev = d.toggles.get(&ep_addr).copied().unwrap_or(false);
        d.toggles.insert(ep_addr, dev ^ (packets % 2 == 1));
        if host != dev {
            self.usb.errors.push(format!("slot {slot} EP {ep_addr:#x}: data toggle host {host} device {dev}"));
        }
        let e = &mut self.usb.slots.get_mut(&slot).unwrap().eps[dci];
        e.toggle = host ^ (packets % 2 == 1);
    }

    /// The endpoint's dequeue pointer moves past a TD (its toggle and state stay).
    fn advance(&mut self, slot: u8, dci: usize, after: FEp) {
        let e = &mut self.usb.slots.get_mut(&slot).unwrap().eps[dci];
        e.deq = after.deq;
        e.cycle = after.cycle;
    }

    /// Runs the TDs of a bulk or interrupt endpoint until it NAKs, stalls or has none.
    pub(crate) fn run_ep(&mut self, slot: u8, dci: usize) {
        for _ in 0..100_000 {
            let Some(fs) = self.usb.slots.get(&slot) else { return };
            let ep = fs.eps[dci];
            if ep.state != ep_state::RUNNING || ep.ep_type == 0 || ep.ep_type == 4 {
                return;
            }
            if ep.ep_type == 1 || ep.ep_type == 5 {
                return self.run_iso(slot, dci);
            }
            let path = fs.path.clone();
            // Room for this TD's events (a short packet may bring two): else wait for software (ERDP write).
            if self.event_room() < 3 {
                self.usb.waiting_for_events = true;
                return;
            }
            let Some((trbs, after)) = self.next_td(slot, dci) else { return };
            let dir_in = dci % 2 == 1;
            let ep_addr = (dci / 2) as u8 | if dir_in { 0x80 } else { 0 };
            let mps = ep.mps;
            self.check_td(slot, dci, &trbs, mps, dir_in);
            let is_interrupt = ep.ep_type == 3 || ep.ep_type == 7;
            if self.dev(&path).is_none() {
                self.transfer_event(slot, dci as u8, trbs[0].0, cc::USB_TRANSACTION, trbs[0].1.status & 0x1_FFFF);
                self.halt(slot, dci);
                return;
            }
            let token = if dir_in {
                let Some(dev) = self.dev(&path) else { return };
                dev.token_in(ep_addr, is_interrupt)
            } else {
                let mut bytes = Vec::new();
                for (_, t) in &trbs {
                    let mut b = vec![0u8; (t.status & 0x1_FFFF) as usize];
                    self.read_mem(t.param, &mut b);
                    bytes.extend_from_slice(&b);
                }
                let Some(dev) = self.dev(&path) else { return };
                match dev.token_out(ep_addr, &bytes) {
                    Token::Data(_) => Token::Data(bytes),
                    t => t,
                }
            };
            match token {
                Token::Nak => return,
                Token::Stall => {
                    self.transfer_event(slot, dci as u8, trbs[0].0, cc::STALL, trbs[0].1.status & 0x1_FFFF);
                    self.halt(slot, dci);
                    return;
                }
                Token::Data(d) if !dir_in => {
                    self.toggle_step(slot, dci, &path, ep_addr, d.len(), mps);
                    self.advance(slot, dci, after);
                    let (last, _) = trbs[trbs.len() - 1];
                    self.transfer_event(slot, dci as u8, last, cc::SUCCESS, 0);
                }
                Token::Data(d) => {
                    let total: usize = trbs.iter().map(|(_, t)| (t.status & 0x1_FFFF) as usize).sum();
                    let n = d.len().min(total);
                    if d.len() > total {
                        let rest = d[total..].to_vec();
                        if let Some(dev) = self.dev(&path) {
                            dev.give_back(ep_addr, rest);
                        }
                    }
                    self.toggle_step(slot, dci, &path, ep_addr, n, mps);
                    self.advance(slot, dci, after);
                    let mut off = 0usize;
                    let mut short_at = None;
                    for (at, t) in &trbs {
                        let len = (t.status & 0x1_FFFF) as usize;
                        let k = len.min(n - off);
                        self.write_mem(t.param, &d[off..off + k]);
                        off += k;
                        if k < len {
                            short_at = Some((*at, (len - k) as u32));
                            break;
                        }
                    }
                    let (last, _) = trbs[trbs.len() - 1];
                    match short_at {
                        Some((at, residual)) => {
                            let code = if self.usb.short_as_success { cc::SUCCESS } else { cc::SHORT_PACKET };
                            self.transfer_event(slot, dci as u8, at, code, residual);
                            if self.usb.spurious_success && at != last {
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

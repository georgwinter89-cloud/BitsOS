//! The USB side of the fake controller (host tests): devices on root ports and behind hubs, device slots with
//! output contexts, the commands US-2 uses (Enable/Disable Slot, Address Device, Evaluate Context, Configure
//! Endpoint, Reset/Stop Endpoint, Set TR Dequeue), endpoint 0 control transfers and interrupt IN endpoints. A
//! device answers with the endpoint 0 packet size it really has: with a wrong size programmed it delivers one
//! packet and the transfer ends short, as on the bus. Mistakes of the driver are recorded in `World::errors`.

use crate::fake::Model;
pub use crate::fake_dev::FakeDev;
use crate::trb::{cc, ty, Trb, LINK_TC};
use std::collections::{BTreeMap, VecDeque};
use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;
use usb_core::Speed;

#[derive(Clone, Copy, Default)]
pub struct FEp {
    pub deq: u64,
    pub cycle: bool,
    pub state: u32,
    pub ep_type: u32,
    /// Max packet size from the endpoint context (TD Size checks, data toggles).
    pub mps: u32,
    /// The host side's data toggle (reset by Reset Endpoint and by Configure Endpoint adding the endpoint).
    pub toggle: bool,
}

#[derive(Default)]
pub struct FSlot {
    pub path: Vec<u8>,
    pub out: u64,
    pub eps: [FEp; 32],
    pub addressed: bool,
    /// In the Default state after Address Device with BSR 1 (answers at address 0, no SET_ADDRESS yet).
    pub default_state: bool,
}

/// A test's action on the model at a chosen moment (`World::before_cmd`).
pub type ModelHook = std::boxed::Box<dyn FnOnce(&mut crate::fake::Model) + Send>;

#[derive(Default)]
pub struct World {
    pub root: BTreeMap<u8, FakeDev>,
    pub slots: BTreeMap<u8, FSlot>,
    next_address: u8,
    /// Driver mistakes the model saw (wrong context fields, bad flags).
    pub errors: Vec<String>,
    /// Endpoint 0 input contexts of every Evaluate Context, with the output context at that moment.
    pub evaluated: Vec<([u32; 8], [u32; 8])>,
    /// Root ports whose USB 3 link only comes up after a warm reset.
    pub stuck_link: Vec<u8>,
    /// Like `stuck_link`, but the link sits in SS.Inactive (only a warm reset helps; no point waiting).
    pub inactive_link: Vec<u8>,
    /// Every Address Device as (slot, BSR).
    pub addresses: Vec<(u8, bool)>,
    /// Reset Endpoint, Stop Endpoint and Set TR Dequeue fail with a Context State Error this many times.
    pub fail_ep_commands: usize,
    /// The next Enable Slot answers with this slot ID (a misbehaving controller), once.
    pub bogus_slot: Option<u8>,
    /// Root ports whose device shows CCS again only this long after HCRST (a late PHY), without an event.
    pub late: Vec<(u8, u64)>,
    late_at: Vec<(u8, u64)>,
    pub commands: Vec<u8>,
    /// (slot, DCI) of every host-side data toggle reset (Reset Endpoint, Configure Endpoint adding it).
    pub toggle_resets: Vec<(u8, u8)>,
    /// AMD: a short packet is reported as Success with a residual instead of Short Packet.
    pub short_as_success: bool,
    /// Some hosts also send Success for the last TRB after a short packet ended a TD early (Linux
    /// XHCI_SPURIOUS_SUCCESS).
    pub spurious_success: bool,
    /// Transfer No-Op TRBs the controller passed.
    pub noops: usize,
    /// Every Configure Endpoint as (drop flags, add flags).
    pub configures: Vec<(u32, u32)>,
    /// Completion codes for the next Configure Endpoints, in order (`SUCCESS`: carried out normally; any other code
    /// refuses the command and changes nothing, e.g. 8 = Bandwidth Error).
    pub configure_codes: VecDeque<u8>,
    /// A transfer waits until software advances ERDP (the event ring had no room for its event).
    pub waiting_for_events: bool,
    /// Runs on the model when the next command of this TRB type executes, before it (a device answering while
    /// a command of the driver is under way).
    pub before_cmd: Option<(u8, ModelHook)>,
    /// The next Stop Endpoint catches the TD at its dequeue pointer after this many bytes of its first TRB moved.
    pub stop_moved: Option<u32>,
    /// Isochronous endpoints (`fake_iso.rs`).
    pub iso: crate::fake_iso::IsoWorld,
}

/// The device at `path` (root port, then hub ports), reachable only through enabled hub ports.
fn walk<'a>(root: &'a mut BTreeMap<u8, FakeDev>, path: &[u8]) -> Option<&'a mut FakeDev> {
    let mut d = root.get_mut(path.first()?)?;
    for &p in &path[1..] {
        let h = d.hub.as_mut()?;
        let port = h.ports.get_mut(p as usize - 1)?;
        if !port.enabled {
            return None;
        }
        d = port.dev.as_mut()?;
    }
    Some(d)
}

pub mod ep_state {
    pub const RUNNING: u32 = 1;
    pub const HALTED: u32 = 2;
    pub const STOPPED: u32 = 3;
}

impl Model {
    fn csz(&self) -> u64 {
        if self.cfg.csz64 { 64 } else { 32 }
    }
    fn in_ctx(&mut self, input: u64, index: u64, dw: u64) -> u32 {
        let a = input + (index + 1) * self.csz() + 4 * dw;
        self.rd_mem32(a).unwrap_or(0)
    }
    fn out_set(&mut self, out: u64, index: u64, dw: u64, v: u32) {
        let a = out + index * self.csz() + 4 * dw;
        self.wr_mem32(a, v);
    }
    pub(crate) fn out_get(&mut self, out: u64, index: u64, dw: u64) -> u32 {
        let a = out + index * self.csz() + 4 * dw;
        self.rd_mem32(a).unwrap_or(0)
    }

    pub fn dev(&mut self, path: &[u8]) -> Option<&mut FakeDev> {
        walk(&mut self.usb.root, path)
    }
    fn is_usb3_port(&self, port: u8) -> bool {
        port as usize > self.cfg.ports as usize / 2
    }

    /// Plugs a device into a root port (CCS, CSC, speed; a USB 3 link comes up by itself unless stuck).
    pub fn attach_root(&mut self, port: u8, dev: FakeDev) {
        let i = port as usize - 1;
        let mut v = (self.portsc[i] & 0x3 << 14) | 1 << 9 | 1 | 1 << 17 | (dev.speed.xhci_psiv() as u32) << 10;
        if self.is_usb3_port(port) {
            v |= self.usb3_link(port);
        }
        self.portsc[i] = v;
        self.usb.root.insert(port, dev);
        if self.usbcmd & 1 != 0 {
            self.port_event(port);
        }
    }

    /// PORTSC bits of a USB 3 link that trained by itself (PED, U0), or one stuck in RxDetect / SS.Inactive.
    fn usb3_link(&self, port: u8) -> u32 {
        if self.usb.inactive_link.contains(&port) {
            6 << 5
        } else if self.usb.stuck_link.contains(&port) {
            5 << 5
        } else {
            2
        }
    }

    pub fn detach_root(&mut self, port: u8) -> Option<FakeDev> {
        let i = port as usize - 1;
        self.portsc[i] = (self.portsc[i] & (1 << 9 | 3 << 14)) | 1 << 17 | if self.portsc[i] & 2 != 0 { 1 << 18 } else { 0 };
        let d = self.usb.root.remove(&port);
        if self.usbcmd & 1 != 0 {
            self.port_event(port);
        }
        d
    }

    /// Plugs or pulls a device on a hub port (`hub` = the hub's path); the hub reports the change.
    pub fn hub_port(&mut self, hub: &[u8], port: u8, dev: Option<FakeDev>) -> Option<FakeDev> {
        let h = self.dev(hub)?.hub.as_mut()?;
        let p = &mut h.ports[port as usize - 1];
        let old = core::mem::replace(&mut p.dev, dev);
        if p.powered {
            p.change |= 1;
        }
        p.enabled = false;
        self.pump();
        old
    }

    /// The root port write rules: RW bits, RW1C change bits, PR / WPR actions; PED written as 1 is recorded.
    pub(crate) fn write_portsc(&mut self, i: usize, v: u32) {
        use crate::regs::portsc::*;
        if v & PED != 0 {
            self.ped_written = true;
        }
        let port = i as u8 + 1;
        let cur = self.portsc[i];
        let mut n = (cur & !KEEP & !CHANGES) | (v & KEEP) | (cur & CHANGES & !(v & CHANGES));
        let has_dev = self.usb.root.contains_key(&port);
        if v & PR != 0 && has_dev {
            n |= PED | PRC;
        }
        if v & WPR != 0 && has_dev {
            n = (n & !PLS_MASK) | PED | PRC | WRC;
            self.usb.stuck_link.retain(|&p| p != port);
            self.usb.inactive_link.retain(|&p| p != port);
        }
        self.portsc[i] = n;
        if v & (PR | WPR) != 0 && has_dev {
            self.port_event(port);
        }
    }

    /// After HCRST: devices stay connected, USB 2 ports disabled, USB 3 links up again (unless stuck).
    pub(crate) fn reset_ports(&mut self) {
        for i in 0..self.portsc.len() {
            let port = i as u8 + 1;
            let mut v = self.portsc[i] & (1 | 1 << 9 | 0xF << 10);
            if self.usb.root.contains_key(&port) && self.is_usb3_port(port) {
                v |= self.usb3_link(port);
            }
            if let Some(&(_, d)) = self.usb.late.iter().find(|(p, _)| *p == port) {
                self.usb.late_at.push((port, self.time + d));
                v &= !1;
            }
            self.portsc[i] = v;
        }
        self.usb.slots.clear();
    }

    /// Late devices show up again (called on every register access).
    pub(crate) fn usb_tick(&mut self) {
        let now = self.time;
        let due: Vec<u8> = self.usb.late_at.iter().filter(|(_, t)| now >= *t).map(|(p, _)| *p).collect();
        self.usb.late_at.retain(|(_, t)| now < *t);
        for p in due {
            self.portsc[p as usize - 1] |= 1;
        }
    }

    /// Executes a command TRB: (completion code, slot ID).
    pub(crate) fn exec_command(&mut self, t: &Trb) -> (u8, u8) {
        let slot = t.slot();
        if self.usb.before_cmd.as_ref().is_some_and(|(ty, _)| *ty == t.trb_type())
            && let Some((_, f)) = self.usb.before_cmd.take()
        {
            f(self);
        }
        self.usb.commands.push(t.trb_type());
        match t.trb_type() {
            ty::NOOP_COMMAND => (cc::SUCCESS, 0),
            ty::ENABLE_SLOT if self.usb.bogus_slot.is_some() => (cc::SUCCESS, self.usb.bogus_slot.take().unwrap()),
            ty::ENABLE_SLOT => match (1..=self.cfg.slots).find(|s| !self.usb.slots.contains_key(s)) {
                Some(s) => {
                    self.usb.slots.insert(s, FSlot::default());
                    (cc::SUCCESS, s)
                }
                None => (cc::NO_SLOTS, 0),
            },
            ty::DISABLE_SLOT => match self.usb.slots.remove(&slot) {
                Some(_) => (cc::SUCCESS, slot),
                None => (cc::SLOT_NOT_ENABLED, slot),
            },
            ty::ADDRESS_DEVICE => (self.address_device(slot, t.param, t.control & crate::trb::ADDRESS_BSR != 0), slot),
            ty::EVALUATE_CONTEXT => (self.evaluate(slot, t.param), slot),
            ty::CONFIGURE_ENDPOINT => (self.configure(slot, t.param), slot),
            ty::RESET_ENDPOINT | ty::STOP_ENDPOINT | ty::SET_TR_DEQUEUE => (self.ep_command(t), slot),
            _ => (cc::TRB_ERROR, slot),
        }
    }

    fn address_device(&mut self, slot: u8, input: u64, bsr: bool) -> u8 {
        self.usb.addresses.push((slot, bsr));
        let Some(out) = self.rd_mem64(self.dcbaap + slot as u64 * 8).filter(|&o| o != 0) else {
            self.usb.errors.push(format!("slot {slot}: no output context in the DCBAA"));
            return cc::TRB_ERROR;
        };
        let Some(was_default) = self.usb.slots.get(&slot).map(|s| s.default_state) else { return cc::SLOT_NOT_ENABLED };
        if bsr && was_default {
            self.usb.errors.push(format!("slot {slot}: Address Device with BSR 1 on a slot in the Default state"));
            return 19;
        }
        let (drop, add) = (self.rd_mem32(input).unwrap_or(0), self.rd_mem32(input + 4).unwrap_or(0));
        if drop != 0 || add != 3 {
            self.usb.errors.push(format!("Address Device flags {drop:#x}/{add:#x}"));
            return cc::TRB_ERROR;
        }
        let s: Vec<u32> = (0..4).map(|d| self.in_ctx(input, 0, d)).collect();
        let e: Vec<u32> = (0..5).map(|d| self.in_ctx(input, 1, d)).collect();
        let (route, psiv, root) = (s[0] & 0xF_FFFF, (s[0] >> 20) & 0xF, (s[1] >> 16) as u8);
        let mut path = vec![root];
        for i in 0..5 {
            let n = ((route >> (4 * i)) & 0xF) as u8;
            if n == 0 {
                break;
            }
            path.push(n);
        }
        let tt = (s[2] as u8, (s[2] >> 8) as u8);
        let expect_tt = self.expected_tt(&path);
        let Some(d) = self.dev(&path) else { return cc::USB_TRANSACTION };
        if d.fail_addresses > 0 {
            d.fail_addresses -= 1;
            return cc::USB_TRANSACTION;
        }
        if !bsr && !was_default && d.fail_direct_addresses > 0 {
            d.fail_direct_addresses -= 1;
            return cc::USB_TRANSACTION;
        }
        let (speed, real_mps0) = (d.speed, d.mps0);
        let mut errs = Vec::new();
        if psiv != speed.xhci_psiv() as u32 {
            errs.push(format!("{path:?}: slot speed {psiv}, device {}", speed.xhci_psiv()));
        }
        if speed.needs_tt() && tt != expect_tt {
            errs.push(format!("{path:?}: TT {tt:?}, expected {expect_tt:?}"));
        }
        // After BSR 1 and an Evaluate Context, the second Address Device names endpoint 0's real size.
        let mps_ok = e[1] >> 16 == speed.default_mps0() as u32 || (was_default && e[1] >> 16 == real_mps0 as u32);
        if !mps_ok || (e[1] >> 3) & 7 != 4 || (e[1] >> 1) & 3 != 3 || e[4] & 0xFFFF != 8 {
            errs.push(format!("{path:?}: EP0 context {:#x} {:#x}", e[1], e[4]));
        }
        if was_default {
            let fs = &self.usb.slots[&slot];
            let cur = fs.eps[1];
            if fs.path != path || (e[2] & !0xF) as u64 | (e[3] as u64) << 32 != cur.deq || (e[2] & 1 != 0) != cur.cycle {
                errs.push(format!("{path:?}: second Address Device: endpoint 0 dequeue {:#x}/{}, ring at {:#x}/{}", e[2], e[3], cur.deq, cur.cycle));
            }
        }
        self.usb.errors.extend(errs);
        let state = if bsr {
            1 << 27
        } else {
            self.usb.next_address = self.usb.next_address.wrapping_add(1).max(1);
            self.usb.next_address as u32 | 2 << 27
        };
        for (d, v) in s.iter().enumerate().take(3) {
            self.out_set(out, 0, d as u64, *v);
        }
        self.out_set(out, 0, 3, state);
        for (d, v) in e.iter().enumerate() {
            self.out_set(out, 1, d as u64, if d == 0 { (v & !7) | ep_state::RUNNING } else { *v });
        }
        let fs = self.usb.slots.get_mut(&slot).unwrap();
        fs.path = path;
        fs.out = out;
        fs.addressed = true;
        fs.default_state = bsr;
        fs.eps[1] = FEp { deq: (e[2] & !0xF) as u64 | (e[3] as u64) << 32, cycle: e[2] & 1 != 0, state: ep_state::RUNNING, ep_type: 4, ..Default::default() };
        cc::SUCCESS
    }

    /// The TT a device at `path` must name: the nearest high speed hub above a low/full speed device, as
    /// (hub slot, port); (0, 0) if none.
    fn expected_tt(&mut self, path: &[u8]) -> (u8, u8) {
        let mut best = (0u8, 0u8);
        for depth in 1..path.len() {
            let hub_path = &path[..depth];
            let hs = self.dev(hub_path).is_some_and(|h| h.speed == Speed::High);
            if hs {
                let slot = self.usb.slots.iter().find(|(_, s)| s.path == hub_path).map(|(&i, _)| i).unwrap_or(0);
                best = (slot, path[depth]);
            }
        }
        best
    }

    fn evaluate(&mut self, slot: u8, input: u64) -> u8 {
        let Some(out) = self.usb.slots.get(&slot).filter(|s| s.addressed).map(|s| s.out) else { return cc::SLOT_NOT_ENABLED };
        let (drop, add) = (self.rd_mem32(input).unwrap_or(0), self.rd_mem32(input + 4).unwrap_or(0));
        if drop != 0 || add & !3 != 0 {
            self.usb.errors.push(format!("Evaluate Context flags {drop:#x}/{add:#x}"));
            return cc::TRB_ERROR;
        }
        if add & 2 != 0 {
            let mut i = [0u32; 8];
            let mut o = [0u32; 8];
            for d in 0..8 {
                i[d] = self.in_ctx(input, 1, d as u64);
                o[d] = self.out_get(out, 1, d as u64);
            }
            self.usb.evaluated.push((i, o));
            self.out_set(out, 1, 1, (o[1] & 0xFFFF) | (i[1] & 0xFFFF_0000));
        }
        cc::SUCCESS
    }

    fn configure(&mut self, slot: u8, input: u64) -> u8 {
        let Some(out) = self.usb.slots.get(&slot).filter(|s| s.addressed).map(|s| s.out) else { return cc::SLOT_NOT_ENABLED };
        if self.usb.slots[&slot].default_state {
            self.usb.errors.push(format!("slot {slot}: Configure Endpoint in the Default state"));
            return 19;
        }
        let (drop, add) = (self.rd_mem32(input).unwrap_or(0), self.rd_mem32(input + 4).unwrap_or(0));
        self.usb.configures.push((drop, add));
        if drop & 3 != 0 || add & 3 != 1 {
            self.usb.errors.push(format!("Configure Endpoint flags {drop:#x}/{add:#x} (QEMU refuses those)"));
            return cc::TRB_ERROR;
        }
        if let Some(c) = self.usb.configure_codes.pop_front().filter(|&c| c != cc::SUCCESS) {
            return c;
        }
        let s: Vec<u32> = (0..3).map(|d| self.in_ctx(input, 0, d)).collect();
        let old0 = self.out_get(out, 0, 0);
        if s[0] & 0xF_FFFF != old0 & 0xF_FFFF {
            self.usb.errors.push(format!("slot {slot}: Configure Endpoint changed the route string"));
        }
        for (d, v) in s.iter().enumerate() {
            self.out_set(out, 0, d as u64, *v);
        }
        for dci in 2..32u32 {
            if drop & (1 << dci) != 0 {
                self.usb.slots.get_mut(&slot).unwrap().eps[dci as usize] = FEp::default();
                self.out_set(out, dci as u64, 0, 0);
            }
            if add & (1 << dci) != 0 {
                let e: Vec<u32> = (0..5).map(|d| self.in_ctx(input, dci as u64, d)).collect();
                // Periodic endpoints (isoch/interrupt, OUT 1/3, IN 5/7) need a Max ESIT Payload and an average TRB
                // length; Intel and AMD controllers refuse zero with a Parameter Error.
                let periodic = matches!((e[1] >> 3) & 7, 1 | 3 | 5 | 7);
                let esit = (e[4] >> 16) | (e[0] >> 24) << 16;
                if periodic && (esit == 0 || e[4] & 0xFFFF == 0) {
                    self.usb.errors.push(format!("slot {slot}: DCI {dci} periodic with ESIT payload {esit}, average {}", e[4] & 0xFFFF));
                    return 17;
                }
                for (d, v) in e.iter().enumerate() {
                    self.out_set(out, dci as u64, d as u64, if d == 0 { (v & !7) | ep_state::RUNNING } else { *v });
                }
                if (s[0] >> 27) < dci {
                    self.usb.errors.push(format!("slot {slot}: Context Entries {} below DCI {dci}", s[0] >> 27));
                }
                self.usb.slots.get_mut(&slot).unwrap().eps[dci as usize] = FEp {
                    deq: (e[2] & !0xF) as u64 | (e[3] as u64) << 32,
                    cycle: e[2] & 1 != 0,
                    state: ep_state::RUNNING,
                    ep_type: (e[1] >> 3) & 7,
                    mps: e[1] >> 16,
                    toggle: false,
                };
                self.usb.toggle_resets.push((slot, dci as u8));
            }
        }
        let st = self.out_get(out, 0, 3);
        self.out_set(out, 0, 3, (st & 0xFF) | 3 << 27);
        cc::SUCCESS
    }

    fn ep_command(&mut self, t: &Trb) -> u8 {
        let (slot, dci) = (t.slot(), t.endpoint() as usize);
        if self.usb.fail_ep_commands > 0 {
            self.usb.fail_ep_commands -= 1;
            return 19;
        }
        let Some(fs) = self.usb.slots.get_mut(&slot) else { return cc::SLOT_NOT_ENABLED };
        let out = fs.out;
        let ep = &mut fs.eps[dci];
        let mut stopped_at = None;
        let code = match t.trb_type() {
            ty::RESET_ENDPOINT if ep.state == ep_state::HALTED => {
                ep.state = ep_state::STOPPED;
                ep.toggle = false;
                self.usb.toggle_resets.push((slot, dci as u8));
                cc::SUCCESS
            }
            ty::STOP_ENDPOINT if ep.state == ep_state::RUNNING => {
                ep.state = ep_state::STOPPED;
                stopped_at = Some(*ep);
                cc::SUCCESS
            }
            ty::SET_TR_DEQUEUE if ep.state == ep_state::STOPPED => {
                ep.deq = t.param & !0xF;
                ep.cycle = t.param & 1 != 0;
                cc::SUCCESS
            }
            _ => 19, // Context State Error
        };
        let st = ep.state;
        let (deq, cycle) = (ep.deq, ep.cycle);
        let v = self.out_get(out, dci as u64, 0);
        self.out_set(out, dci as u64, 0, (v & !7) | st);
        // The controller keeps its dequeue pointer in the output context (xHCI 4.6.9, 4.6.10).
        self.out_set(out, dci as u64, 2, (deq as u32 & !0xF) | cycle as u32);
        self.out_set(out, dci as u64, 3, (deq >> 32) as u32);
        // Stopped inside a TD: a Transfer Event with Stopped names the TRB it stood at (nothing of it moved, or
        // `stop_moved` bytes).
        if let Some(ep) = stopped_at.filter(|_| dci > 1)
            && let Some((at, t, _)) = self.peek(ep)
        {
            let moved = self.usb.stop_moved.take().unwrap_or(0);
            self.transfer_event(slot, dci as u8, at, cc::STOPPED, (t.status & 0x1_FFFF).saturating_sub(moved));
        }
        code
    }

    /// The TRB at an endpoint's dequeue pointer if software handed it over, following Link TRBs.
    pub(crate) fn peek(&mut self, mut ep: FEp) -> Option<(u64, Trb, FEp)> {
        for _ in 0..64 {
            let t = self.rd_trb(ep.deq)?;
            if t.cycle() != ep.cycle {
                return None;
            }
            if t.trb_type() == ty::LINK {
                ep.deq = t.param;
                if t.control & LINK_TC != 0 {
                    ep.cycle = !ep.cycle;
                }
                continue;
            }
            let at = ep.deq;
            ep.deq += 16;
            return Some((at, t, ep));
        }
        None
    }

    pub(crate) fn transfer_event(&mut self, slot: u8, dci: u8, trb: u64, code: u8, residual: u32) {
        self.post(Trb {
            param: trb,
            status: (code as u32) << 24 | residual,
            control: (ty::TRANSFER_EVENT as u32) << 10 | (dci as u32) << 16 | (slot as u32) << 24,
        });
    }

    pub(crate) fn halt(&mut self, slot: u8, dci: usize) {
        let fs = self.usb.slots.get_mut(&slot).unwrap();
        fs.eps[dci].state = ep_state::HALTED;
        let out = fs.out;
        let v = self.out_get(out, dci as u64, 0);
        self.out_set(out, dci as u64, 0, (v & !7) | ep_state::HALTED);
    }

    pub(crate) fn doorbell_slot(&mut self, slot: u8, dci: u8) {
        if !self.running_pub() || !self.usb.slots.contains_key(&slot) {
            return;
        }
        // Ringing a stopped endpoint's doorbell runs it again (xHCI §4.8.3); a halted one stays halted.
        let fs = self.usb.slots.get_mut(&slot).unwrap();
        let out = fs.out;
        if let Some(ep) = fs.eps.get_mut(dci as usize).filter(|e| e.state == ep_state::STOPPED) {
            ep.state = ep_state::RUNNING;
            let v = self.out_get(out, dci as u64, 0);
            self.out_set(out, dci as u64, 0, (v & !7) | ep_state::RUNNING);
        }
        if dci == 1 {
            self.run_ep0(slot);
        }
        self.pump();
    }

    fn run_ep0(&mut self, slot: u8) {
        for _ in 0..1000 {
            let ep = self.usb.slots[&slot].eps[1];
            if ep.state != ep_state::RUNNING {
                return;
            }
            // A whole TD (setup, data?, status) must be there.
            let Some((sa, st, e1)) = self.peek(ep) else { return };
            if st.trb_type() != ty::SETUP {
                self.usb.errors.push(format!("slot {slot}: EP0 TD starts with TRB type {}", st.trb_type()));
                return;
            }
            let Some((a2, t2, e2)) = self.peek(e1) else { return };
            let (data, status) = if t2.trb_type() == ty::DATA {
                let Some((a3, t3, e3)) = self.peek(e2) else { return };
                (Some((a2, t2)), (a3, t3, e3))
            } else {
                (None, (a2, t2, e2))
            };
            let (stat_a, stat_t, end) = status;
            if stat_t.trb_type() != ty::STATUS {
                self.usb.errors.push(format!("slot {slot}: EP0 TD without a status stage"));
                return;
            }
            self.usb.slots.get_mut(&slot).unwrap().eps[1] = FEp { state: ep_state::RUNNING, ..end };
            let path = self.usb.slots[&slot].path.clone();
            let out = self.usb.slots[&slot].out;
            let programmed = self.out_get(out, 1, 1) >> 16;
            let mut payload = vec![0u8; data.map(|(_, t)| (t.status & 0x1_FFFF) as usize).unwrap_or(0)];
            if let Some((_, t)) = data.filter(|(_, t)| t.control & (1 << 16) == 0) {
                let mut b = payload.clone();
                self.read_mem(t.param, &mut b);
                payload = b;
            }
            let Some(dev) = self.dev(&path) else {
                self.transfer_event(slot, 1, sa, cc::USB_TRANSACTION, 0);
                self.halt(slot, 1);
                return;
            };
            if dev.hang {
                return;
            }
            let mps0 = dev.mps0 as u32;
            match dev.request(st.param, &payload) {
                Err(()) => {
                    let at = data.map(|(a, _)| a).unwrap_or(stat_a);
                    self.transfer_event(slot, 1, at, cc::STALL, 0);
                    self.halt(slot, 1);
                    return;
                }
                Ok(bytes) => {
                    if let Some((da, dt)) = data.filter(|(_, t)| t.control & (1 << 16) != 0) {
                        let want = (dt.status & 0x1_FFFF) as usize;
                        let mut n = want.min(bytes.len());
                        // A device whose real packet size is below the programmed one sends one short packet.
                        if programmed > mps0 && n > mps0 as usize {
                            n = mps0 as usize;
                        }
                        self.write_mem(dt.param, &bytes[..n]);
                        if n < want {
                            self.transfer_event(slot, 1, da, cc::SHORT_PACKET, (want - n) as u32);
                        }
                    }
                    self.transfer_event(slot, 1, stat_a, cc::SUCCESS, 0);
                }
            }
        }
    }

    /// Runs every bulk and interrupt endpoint that has work (`fake_bulk.rs`).
    pub fn pump(&mut self) {
        if !self.running_pub() {
            return;
        }
        let slots: Vec<u8> = self.usb.slots.keys().copied().collect();
        for slot in slots {
            for dci in 2..32 {
                self.run_ep(slot, dci);
            }
        }
    }
}

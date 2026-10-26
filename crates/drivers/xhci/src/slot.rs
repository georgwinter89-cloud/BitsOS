//! Device slots (xHCI 1.2 §4.3, §4.5; V1 `usbdev.asm`, `xhcictx.asm`): Enable Slot + Address Device with the slot
//! and endpoint 0 contexts, Evaluate Context for endpoint 0's real size (the **whole** endpoint 0 output context
//! copied, as Linux does), Configure Endpoint for a configuration's endpoints and for hub slots, Disable Slot.
//! A handle (`HcSlot`) carries the slot ID and the controller's epoch, so a handle from before a restart never
//! reaches a slot of the same number after it.

use crate::ctx::{self, ep0_context, ep_context, slot_dw0, slot_dw1, slot_dw2, Ctx, SLOT_HUB, SLOT_MTT};
use crate::hc::{CmdError, Controller, Env, State, SEGMENT_BYTES};
use crate::mem::DmaMem;
use crate::ring::ProducerRing;
use crate::trb::Trb;
use crate::xfer::Xfer;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{Mutex, MutexGuard, SpinLock, WaitQueue};
use usb_core::{Endpoint, EpKind, HcSlot, HubParams, Place, Speed, UsbError};

klog::counter!(SLOTS_ENABLED, "usb.xhci.slots_enabled");
klog::counter!(SLOTS_DISABLED, "usb.xhci.slots_disabled");
klog::counter!(SLOTS_LEAKED, "usb.xhci.slots_leaked");

/// Slot operations (commands, control transfers) of one device run one at a time; a caller waits at most this
/// long for its turn: a control transfer and the two commands of an endpoint restart, each with its abort.
pub const OP_GATE_NS: u64 = crate::xfer::CONTROL_NS + 2 * crate::hc::CMD_GATE_NS;

/// What only one operation at a time touches: the contexts and endpoint 0's data buffer.
pub(crate) struct SlotOp<M: DmaMem> {
    pub(crate) out: M,
    pub(crate) inp: M,
    pub(crate) data: Option<M>,
}

pub(crate) struct SlotEntry<M: DmaMem> {
    pub(crate) id: u8,
    pub(crate) epoch: u32,
    pub(crate) speed: Speed,
    pub(crate) root_port: u8,
    pub(crate) route: u32,
    pub(crate) op: Mutex<SlotOp<M>>,
    /// Rings and transfers in flight; the event thread takes it too.
    pub(crate) xfer: SpinLock<Xfer<M>>,
    pub(crate) wq: WaitQueue,
    pub(crate) gone: AtomicBool,
}

/// A slot as the controller keeps it (output context), for ktests and tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotInfo {
    pub id: u8,
    pub state: u8,
    pub address: u8,
    pub route: u32,
    pub speed: u8,
    pub root_port: u8,
    pub entries: u8,
    pub hub: bool,
    pub mtt: bool,
    pub ports: u8,
    pub tt_slot: u8,
    pub tt_port: u8,
    pub mps0: u16,
    pub ep0_state: u8,
    /// DCBAA entry == this slot's output context.
    pub dcbaa_ok: bool,
}

/// Slot states in the output slot context (dword 3 bits 31:27).
pub mod slot_state {
    pub const DISABLED: u8 = 0;
    pub const DEFAULT: u8 = 1;
    pub const ADDRESSED: u8 = 2;
    pub const CONFIGURED: u8 = 3;
}

pub(crate) fn cmd_err(e: CmdError) -> UsbError {
    match e {
        CmdError::NotRunning | CmdError::ControllerFailed => UsbError::NotRunning,
        CmdError::Busy => UsbError::Busy,
        CmdError::Timeout => UsbError::Timeout,
        CmdError::Ring(_) => UsbError::Invalid("command ring"),
        CmdError::Failed { code, .. } => UsbError::Command(code),
    }
}

impl<E: Env> Controller<E> {
    pub(crate) fn handle_of(e: &SlotEntry<E::Mem>) -> HcSlot {
        HcSlot((e.epoch as u64) << 8 | e.id as u64)
    }

    /// The live slot behind a handle: the controller runs, the epoch matches, the device is not gone.
    pub(crate) fn entry(&self, h: HcSlot) -> Result<Arc<SlotEntry<E::Mem>>, UsbError> {
        if self.state() != State::Running {
            return Err(UsbError::NotRunning);
        }
        let e = self.slots.lock().get((h.0 & 0xFF) as usize).cloned().flatten().ok_or(UsbError::Gone)?;
        if e.epoch != (h.0 >> 8) as u32 || e.gone.load(Ordering::Acquire) {
            return Err(UsbError::Gone);
        }
        Ok(e)
    }

    pub(crate) fn lock_op<'a>(&self, e: &'a SlotEntry<E::Mem>) -> Result<MutexGuard<'a, SlotOp<E::Mem>>, UsbError> {
        let g = e.op.lock_timeout(OP_GATE_NS).ok_or(UsbError::Busy)?;
        if e.gone.load(Ordering::Acquire) {
            return Err(UsbError::Gone);
        }
        Ok(g)
    }

    pub(crate) fn cmd(&self, t: Trb) -> Result<crate::hc::CmdCompletion, UsbError> {
        self.command(t).map_err(cmd_err)
    }

    pub(crate) fn alloc_mem(&self, len: usize, align: u64) -> Result<E::Mem, UsbError> {
        let limit = if self.caps.ac64 { u64::MAX } else { 1 << 32 };
        self.env.alloc(len, align, limit).ok_or(UsbError::NoMemory)
    }

    pub(crate) fn ctx_in<'a>(&self, m: &'a E::Mem) -> Ctx<'a, E::Mem> {
        Ctx { mem: m, csz: self.caps.csz, input: true }
    }
    pub(crate) fn ctx_out<'a>(&self, m: &'a E::Mem) -> Ctx<'a, E::Mem> {
        Ctx { mem: m, csz: self.caps.csz, input: false }
    }

    /// The supported-protocol entry covering a root port.
    pub fn protocol_of(&self, port: u8) -> Option<crate::regs::Protocol> {
        self.ext.protocols.iter().find(|p| p.covers(port)).cloned()
    }

    /// Enable Slot (the slot type of the root port's protocol), then Address Device for a device just reset at
    /// `place`, endpoint 0 at `mps0`. BSR 0: the controller sends SET_ADDRESS. BSR 1 (`deferred`): the slot only
    /// reaches the Default state and the device answers at address 0 until `set_address` (Linux's first step for
    /// devices that fail a SET_ADDRESS straight after the reset).
    pub(crate) fn address_device(&self, place: &Place, mps0: u16, deferred: bool) -> Result<(HcSlot, u8), UsbError> {
        let proto = self.protocol_of(place.root_port).ok_or(UsbError::Invalid("root port without a protocol"))?;
        let csz = self.caps.csz;
        let out = self.alloc_mem(csz.device_context_len(), 64)?;
        let inp = self.alloc_mem(csz.input_context_len(), 64)?;
        let ring = ProducerRing::new(vec![self.alloc_mem(SEGMENT_BYTES, 64)?]).map_err(|_| UsbError::NoMemory)?;
        let id = self.cmd(Trb::enable_slot(proto.slot_type))?.slot;
        // Slot 0 and IDs past the table are no slots; an ID in use belongs to a live device, whose slot a Disable
        // Slot would break (and `release_slot` of that device may be under way): none of them gets a command.
        let bad = match self.slots.lock().get(id as usize) {
            _ if id == 0 => Some("zero"),
            None => Some("past max slots"),
            Some(Some(_)) => Some("in use"),
            Some(None) => None,
        };
        if let Some(why) = bad {
            SLOTS_LEAKED.inc();
            klog::event!(Error, "usb", "xhci.bad_slot_id", ctrl = self.name.as_str(), slot = id, why = why;
                "Enable Slot returned a slot that cannot be used; left alone");
            return Err(UsbError::Invalid("the controller handed out a slot it does not have or that is in use"));
        }
        SLOTS_ENABLED.inc();
        let tt = place.tt.map(|t| ((t.hub.0 & 0xFF) as u8, t.port, t.multi)).unwrap_or((0, 0, false));
        let ic = self.ctx_in(&inp);
        let ok = ic.set_flags(0, 0b11)
            && ic.set_all(0, &[slot_dw0(place.route, place.speed, tt.2, false, 1), slot_dw1(place.root_port, 0), slot_dw2(tt.0, tt.1, 0), 0])
            && ic.set_all(1, &ep0_context(mps0, ring.base()));
        let entry = Arc::new(SlotEntry {
            id,
            epoch: self.epoch(),
            speed: place.speed,
            root_port: place.root_port,
            route: place.route,
            op: Mutex::named("xhci.slot_op", SlotOp { out, inp, data: None }),
            xfer: SpinLock::named("xhci.xfer", Xfer::new(ring)),
            wq: WaitQueue::new(),
            gone: AtomicBool::new(false),
        });
        let input = {
            let op = entry.op.lock();
            if !ok || !self.set_dcbaa(id, op.out.dev_addr()) {
                drop(op);
                self.drop_slot(&entry);
                return Err(UsbError::Invalid("context memory"));
            }
            op.inp.dev_addr()
        };
        self.slots.lock()[id as usize] = Some(entry.clone());
        if let Err(e) = self.cmd(Trb::address_device(input, id, deferred)) {
            self.drop_slot(&entry);
            return Err(e);
        }
        let op = entry.op.lock();
        let address = if deferred { 0 } else { self.ctx_out(&op.out).get(0, 3).map(|v| v as u8).unwrap_or(0) };
        Ok((Self::handle_of(&entry), address))
    }

    /// The second step after `address_device(.., deferred)`: Address Device with BSR 0 on the same slot. The
    /// input context is built afresh: the slot context as the controller keeps it, endpoint 0 with its size now
    /// (an Evaluate Context may have changed it) and its dequeue pointer where the ring stands after the
    /// transfers at address 0 (Linux `xhci_copy_ep0_dequeue_into_input_ctx`).
    pub(crate) fn set_address(&self, h: HcSlot) -> Result<u8, UsbError> {
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        let s = self.slot_from_output(&op)?;
        let mps0 = self.ctx_out(&op.out).get(1, 1).map(|v| (v >> 16) as u16).ok_or(UsbError::Invalid("context memory"))?;
        let (deq, cycle) = e.xfer.lock().ep0_dequeue().ok_or(UsbError::Invalid("no endpoint 0 ring"))?;
        let mut ep0 = ep0_context(mps0, deq);
        ep0[2] = (deq as u32 & !0xF) | cycle as u32;
        let ic = self.ctx_in(&op.inp);
        if !(op.inp.zero() && ic.set_flags(0, 0b11) && ic.set_all(0, &s) && ic.set_all(1, &ep0)) {
            return Err(UsbError::Invalid("context memory"));
        }
        self.cmd(Trb::address_device(op.inp.dev_addr(), e.id, false))?;
        Ok(self.ctx_out(&op.out).get(0, 3).map(|v| v as u8).unwrap_or(0))
    }

    /// Endpoint 0's real packet size: Evaluate Context with endpoint 0's output context copied whole into the
    /// input context (a controller taking more than the size must not rewind its dequeue pointer; Linux, V1
    /// `xhci_ctx_mps`).
    pub(crate) fn evaluate_mps0(&self, h: HcSlot, mps0: u16) -> Result<(), UsbError> {
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        let mut ep0 = self.ctx_out(&op.out).read8(1).ok_or(UsbError::Invalid("context memory"))?;
        ep0[1] = (ep0[1] & 0xFFFF) | (mps0 as u32) << 16;
        let ic = self.ctx_in(&op.inp);
        if !(op.inp.zero() && ic.set_flags(0, 0b10) && ic.set_all(1, &ep0)) {
            return Err(UsbError::Invalid("context memory"));
        }
        self.cmd(Trb::evaluate_context(op.inp.dev_addr(), e.id)).map(|_| ())
    }

    /// The input context's slot context as the controller keeps it (output), dword 3 cleared (V1
    /// `xhci_ctx_slot`): the start of every Configure Endpoint.
    pub(crate) fn slot_from_output(&self, op: &SlotOp<E::Mem>) -> Result<[u32; 4], UsbError> {
        let s = self.ctx_out(&op.out).read8(0).ok_or(UsbError::Invalid("context memory"))?;
        Ok([s[0], s[1], s[2], 0])
    }

    /// Configure Endpoint for a configuration's endpoints (control endpoints and zero-size ones are skipped);
    /// endpoints of an earlier configuration are dropped in the same command.
    pub(crate) fn configure_eps(&self, h: HcSlot, eps: &[Endpoint]) -> Result<(), UsbError> {
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        let mut new: Vec<(Endpoint, ProducerRing<E::Mem>)> = Vec::new();
        for ep in eps {
            let dci = ep.dci();
            if ep.kind() == EpKind::Control || !(2..=31).contains(&dci) || ep.max_packet() == 0 || new.iter().any(|(x, _)| x.dci() == dci) {
                continue;
            }
            let ring = ProducerRing::new(vec![self.alloc_mem(SEGMENT_BYTES, 64)?]).map_err(|_| UsbError::NoMemory)?;
            new.push((*ep, ring));
        }
        let old: Vec<u8> = e.xfer.lock().configured();
        let drop = old.iter().fold(0u32, |a, &d| a | 1 << d);
        let add = new.iter().fold(1u32, |a, (ep, _)| a | 1 << ep.dci());
        let entries = new.iter().map(|(ep, _)| ep.dci()).max().unwrap_or(1);
        let mut s = self.slot_from_output(&op)?;
        s[0] = (s[0] & !(0x1F << 27)) | (entries as u32) << 27;
        let ic = self.ctx_in(&op.inp);
        let mut ok = op.inp.zero() && ic.set_flags(drop, add) && ic.set_all(0, &s);
        for (ep, ring) in &new {
            ok &= ic.set_all(ep.dci() as usize, &ep_context(e.speed, ep, ring.base()));
        }
        if !ok {
            return Err(UsbError::Invalid("context memory"));
        }
        self.cmd(Trb::configure_endpoint(op.inp.dev_addr(), e.id))?;
        {
            let mut x = e.xfer.lock();
            let mut ended = Vec::new();
            x.drop_endpoints(&mut ended);
            self.defer(ended);
            for (ep, ring) in new {
                x.set_endpoint(ep, ring);
            }
        }
        self.deliver_pending();
        Ok(())
    }

    /// IN-2b: one configured endpoint again with `ep`'s settings (a pipe's interval wish): Configure Endpoint
    /// with only its DCI in the Drop and Add flags (xHCI 1.2 4.6.6; other endpoints untouched) and a fresh ring.
    /// The caller holds the slot operation and has checked that nothing is queued on it.
    pub(crate) fn set_interval(&self, e: &SlotEntry<E::Mem>, op: &SlotOp<E::Mem>, ep: Endpoint) -> Result<(), UsbError> {
        let dci = ep.dci();
        if !(2..=31).contains(&dci) {
            return Err(UsbError::Invalid("endpoint"));
        }
        let ring = ProducerRing::new(vec![self.alloc_mem(SEGMENT_BYTES, 64)?]).map_err(|_| UsbError::NoMemory)?;
        let s = self.slot_from_output(op)?;
        let ic = self.ctx_in(&op.inp);
        let ok = op.inp.zero() && ic.set_flags(1 << dci, 1 | 1 << dci) && ic.set_all(0, &s) && ic.set_all(dci as usize, &ep_context(e.speed, &ep, ring.base()));
        if !ok {
            return Err(UsbError::Invalid("context memory"));
        }
        self.cmd(Trb::configure_endpoint(op.inp.dev_addr(), e.id))?;
        let mut ended = Vec::new();
        e.xfer.lock().replace_endpoint(ep, ring, &mut ended);
        self.defer(ended);
        self.deliver_pending();
        klog::event!(Debug, "usb", "xfer.interval_set", ctrl = self.name.as_str(), slot = e.id, dci = dci, binterval = ep.interval);
        Ok(())
    }

    /// Makes the slot a hub's: Configure Endpoint with only the slot context added (xHCI 1.0+, Linux
    /// `xhci_update_hub_device`, V1 `usb_hub_slot`).
    pub(crate) fn configure_hub(&self, h: HcSlot, hub: HubParams) -> Result<(), UsbError> {
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        let mut s = self.slot_from_output(&op)?;
        s[0] = (s[0] | SLOT_HUB) & !SLOT_MTT | if hub.multi_tt { SLOT_MTT } else { 0 };
        s[1] = (s[1] & 0x00FF_FFFF) | (hub.ports as u32) << 24;
        if e.speed == Speed::High {
            s[2] = (s[2] & !(3 << 16)) | ((hub.tt_think_time & 3) as u32) << 16;
        }
        let ic = self.ctx_in(&op.inp);
        if !(op.inp.zero() && ic.set_flags(0, 1) && ic.set_all(0, &s)) {
            return Err(UsbError::Invalid("context memory"));
        }
        self.cmd(Trb::configure_endpoint(op.inp.dev_addr(), e.id)).map(|_| ())
    }

    /// The device is gone: pipes stopped, waiters woken, Disable Slot, DCBAA entry cleared, memory freed. If the
    /// controller still runs but does not confirm the Disable Slot, the memory is kept (it might still be used
    /// by the controller) and counted.
    pub(crate) fn release_slot(&self, h: HcSlot) {
        let id = (h.0 & 0xFF) as usize;
        let Some(e) = self.slots.lock().get(id).cloned().flatten() else { return };
        if e.epoch != (h.0 >> 8) as u32 || e.gone.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut x = e.xfer.lock();
            let mut ended = Vec::new();
            x.stop_all(UsbError::Gone, &mut ended);
            self.defer(ended);
        }
        e.wq.wake_all();
        self.deliver_pending();
        // Waits for an operation in flight (it sees `gone` and ends); proceeds anyway after the gate time.
        let op = e.op.lock_timeout(OP_GATE_NS);
        let r = if self.state() == State::Running { self.cmd(Trb::disable_slot(e.id)) } else { Err(UsbError::NotRunning) };
        drop(op);
        match r {
            Ok(_) => {
                self.set_dcbaa(e.id, 0);
                SLOTS_DISABLED.inc();
                klog::event!(Debug, "usb", "xhci.slot_disabled", ctrl = self.name.as_str(), slot = e.id, root_port = e.root_port,
                    route = klog::Hex(e.route as u64));
            }
            // Halted or parked (RS off, bus master off): nothing reaches the memory any more.
            Err(_) if self.state() != State::Running => {}
            Err(err) => {
                SLOTS_LEAKED.inc();
                klog::event!(Error, "usb", "xhci.slot_leaked", ctrl = self.name.as_str(), slot = e.id, error = err.as_str();
                    "Disable Slot not confirmed: its contexts and rings are kept, never reused");
                self.leaked.lock().push(e.clone());
            }
        }
        let mut t = self.slots.lock();
        if t.get(id).cloned().flatten().is_some_and(|x| Arc::ptr_eq(&x, &e)) {
            t[id] = None;
        }
    }

    /// Drops a slot that never got a device (Address Device failed, memory trouble).
    fn drop_slot(&self, e: &Arc<SlotEntry<E::Mem>>) {
        self.release_slot(Self::handle_of(e));
        // `release_slot` only finds entries in the table; one that never got there is disabled here.
        if !e.gone.swap(true, Ordering::AcqRel) {
            let _ = self.cmd(Trb::disable_slot(e.id));
            self.set_dcbaa(e.id, 0);
        }
    }

    /// The output context of a live slot, decoded.
    pub fn slot_info(&self, h: HcSlot) -> Option<SlotInfo> {
        let e = self.entry(h).ok()?;
        let op = e.op.lock_timeout(OP_GATE_NS)?;
        let oc = self.ctx_out(&op.out);
        let (s, ep0) = (oc.read8(0)?, oc.read8(1)?);
        Some(SlotInfo {
            id: e.id,
            state: (s[3] >> 27) as u8,
            address: s[3] as u8,
            route: s[0] & 0xF_FFFF,
            speed: ((s[0] >> 20) & 0xF) as u8,
            root_port: (s[1] >> 16) as u8,
            entries: (s[0] >> 27) as u8,
            hub: s[0] & SLOT_HUB != 0,
            mtt: s[0] & SLOT_MTT != 0,
            ports: (s[1] >> 24) as u8,
            tt_slot: s[2] as u8,
            tt_port: (s[2] >> 8) as u8,
            mps0: (ep0[1] >> 16) as u16,
            ep0_state: (ep0[0] & 7) as u8,
            dcbaa_ok: self.dcbaa(e.id) == Some(op.out.dev_addr()),
        })
    }

    /// The endpoint context state of `dci` (output context).
    pub(crate) fn ep_state(&self, op: &SlotOp<E::Mem>, dci: u8) -> u32 {
        self.ctx_out(&op.out).get(dci as usize, 0).map(|v| v & 7).unwrap_or(ctx::ep_state::DISABLED)
    }

    /// Live slots (ktests: none may survive its device).
    pub fn live_slots(&self) -> Vec<u8> {
        self.slots.lock().iter().flatten().map(|e| e.id).collect()
    }
}

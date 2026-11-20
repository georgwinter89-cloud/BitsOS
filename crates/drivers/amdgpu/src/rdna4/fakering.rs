//! Wave 44, item RG: the model's side of the trace's section gc-ring and of what a hung or faulted engine shows.
//! It takes, and refuses what Linux would not do there:
//!
//! - CP_MAX_CONTEXT and CP_DEVICE_ID only with Linux's values (7, 1), in that order, and only once the gfx queue
//!   is mapped (Linux writes them right after `amdgpu_gfx_enable_kgq`, gfx_v12_0.c:3140-3144; the model's order
//!   rule);
//! - the interrupt controls of the three pipes (CP_INT_CNTL_RING0, CP_ME1_PIPE0/1_INT_CNTL) only as Linux's
//!   read-modify-writes change them, and only once all five kernel queues are mapped: first the three fault
//!   enables in `amdgpu_irq_get`'s order (`gc12ring::FAULT_ENABLES`), then the end-of-pipe enables on or off
//!   (TIME_STAMP and GENERIC0 together) and the fault enables' clears of a suspend in `amdgpu_irq_put`'s order
//!   (`gc12ring::FAULT_DISABLES`, a clear whose bit is off passed over); a write that changes another bit, or
//!   skips a step, is refused. The idle
//!   interrupt's write of `gccp`, before the queues are mapped, stays `fakecp`'s.
//!
//! GRBM_STATUS and CP_STAT answer busy (GUI_ACTIVE and CP_BUSY, CP_STAT's CP_BUSY) while a mapped queue is halted
//! with packets it did not run, or its last run ended with work left (a poll not satisfied, the run's budget
//! spent, a packet not complete), else 0 (CP_STAT's other answers are `fakecp`'s): which bits a
//! real hung engine shows is in no trace (plan/spec/gpu-rdna4.md section 7, `ring-hang-look`). At every autoload
//! the block's registers go back to cold with the rest of the block.

use super::fakemodel::Model;
use alloc::format;
use alloc::string::String;
use rdna4_core::gc12ring::{self as rg, f};
use rdna4_core::hubseq::Op;
use rdna4_core::mes12::KERNEL_QUEUES;
use rdna4_core::regmap::Reg;

#[derive(Clone, Debug, Default)]
pub struct FakeRing {
    pub max_context: Option<u32>,
    pub device_id: Option<u32>,
    /// MEC 1's two pipes' interrupt controls (ME 0's is `fakecp`'s CP_INT_CNTL_RING0).
    pub me1: [u32; 2],
    /// How many of Linux's fault enables were taken, and how far its clears at a suspend came.
    pub fault_step: usize,
    pub off_step: usize,
}

impl FakeRing {
    pub fn cold(&mut self) {
        *self = FakeRing::default();
    }
}

fn named(r: Reg) -> &'static str {
    r.name
}

impl Model {
    fn ring_is(&self, byte: u32, r: Reg) -> bool {
        self.nb.n.byte(r) == Some(byte)
    }

    /// The interrupt control of a pipe as the card holds it.
    fn int_cntl(&self, r: Reg) -> u32 {
        if super::fakemodel::same(r, rg::CP_INT_CNTL_RING0) {
            return self.cp.val(&self.nb.n, rg::CP_INT_CNTL_RING0);
        }
        self.rings.me1[usize::from(super::fakemodel::same(r, rg::CP_ME1_PIPE1_INT_CNTL))]
    }

    /// The pipe's enables `bits` are all on.
    pub(super) fn cp_int_on(&self, me: u32, pipe: u32, bits: u32) -> bool {
        rg::int_cntl_of(me, pipe).is_some_and(|r| self.int_cntl(r) & bits == bits)
    }

    /// A mapped queue is halted with packets left, or its last run ended with work left (a poll, the budget, a
    /// packet not complete).
    fn ring_hung(&self) -> bool {
        self.cp.queues.iter().any(|q| q.halted.is_some() || q.waits.is_some())
    }

    /// A read of the section's registers, or `None` for any other byte.
    pub(super) fn ring_read(&mut self, byte: u32) -> Option<u32> {
        if !self.gc_awake() {
            return None;
        }
        if self.ring_is(byte, rg::GRBM_STATUS) {
            return Some(if self.ring_hung() { f::GUI_ACTIVE.mask | f::CP_BUSY.mask } else { 0 });
        }
        if self.ring_is(byte, rg::CP_STAT) && self.ring_hung() {
            return Some(f::CP_STAT_BUSY.mask);
        }
        if self.ring_is(byte, rg::CP_MAX_CONTEXT) {
            return Some(self.rings.max_context.unwrap_or(0));
        }
        if self.ring_is(byte, rg::CP_DEVICE_ID) {
            return Some(self.rings.device_id.unwrap_or(0));
        }
        [rg::CP_ME1_PIPE0_INT_CNTL, rg::CP_ME1_PIPE1_INT_CNTL].iter().position(|r| self.ring_is(byte, *r)).map(|i| self.rings.me1[i])
    }

    /// A write of the section's registers: `Some(reached)`, or `None` for any other byte (the idle interrupt's
    /// write of CP_INT_CNTL_RING0 included, which is `fakecp`'s).
    pub(super) fn ring_write(&mut self, byte: u32, v: u32) -> Option<bool> {
        let regs = [rg::CP_MAX_CONTEXT, rg::CP_DEVICE_ID, rg::CP_INT_CNTL_RING0, rg::CP_ME1_PIPE0_INT_CNTL, rg::CP_ME1_PIPE1_INT_CNTL, rg::GRBM_STATUS];
        let r = *regs.iter().find(|r| self.ring_is(byte, **r))?;
        let cur = self.int_cntl(r);
        let all_mapped = KERNEL_QUEUES.iter().all(|k| self.cp.queues.iter().any(|q| q.at.doorbell_dw == k.doorbell));
        // Before the queues are mapped CP_INT_CNTL_RING0 is `gccp`'s (the idle interrupt off), `fakecp`'s to judge.
        if super::fakemodel::same(r, rg::CP_INT_CNTL_RING0) && !all_mapped {
            return None;
        }
        if !self.gc_awake() {
            return Some(self.refuse(format!("{} written {v:#x} before the graphics block has power and its autoload is complete", named(r))));
        }
        if self.gc.grbm != 0 {
            return Some(self.refuse(format!("{} written while GRBM_GFX_CNTL still selects {:#x}: Linux selects pipe 0 again first", named(r), self.gc.grbm)));
        }
        let gfx_mapped = self.cp.queues.iter().any(|q| q.at.gfx);
        match r.name {
            "GRBM_STATUS" => Some(self.refuse(format!("GRBM_STATUS written {v:#x}: Linux only reads it"))),
            "CP_MAX_CONTEXT" if !gfx_mapped || v != rg::MAX_HW_CONTEXTS - 1 => Some(self.refuse(format!("CP_MAX_CONTEXT written {v:#x}: Linux writes max_hw_contexts - 1 = 7 once the gfx ring is mapped (gfx_v12_0.c:2702-2703, :3140-3144)"))),
            "CP_MAX_CONTEXT" => {
                self.rings.max_context = Some(v);
                Some(true)
            }
            "CP_DEVICE_ID" if self.rings.max_context.is_none() || v != rg::DEVICE_ID => Some(self.refuse(format!("CP_DEVICE_ID written {v:#x}: Linux writes 1 right after CP_MAX_CONTEXT (gfx_v12_0.c:2704)"))),
            "CP_DEVICE_ID" => {
                self.rings.device_id = Some(v);
                Some(true)
            }
            _ if !all_mapped => Some(self.refuse(format!("{} written {v:#x} before the five kernel queues are mapped: Linux enables the CP's interrupts after cp_resume (gfx_v12_0.c:3852-3862)", named(r)))),
            _ => Some(self.int_write(r, cur, v)),
        }
    }

    /// An interrupt control: the next of Linux's fault enables, or once they are all on the end-of-pipe enables.
    fn int_write(&mut self, r: Reg, cur: u32, v: u32) -> bool {
        let step = rg::FAULT_ENABLES.get(self.rings.fault_step).and_then(|op| match op {
            Op::Modify(reg, change) if super::fakemodel::same(*reg, r) => Some(change(cur)),
            _ => None,
        });
        let off = if step.is_none() && self.rings.fault_step == rg::FAULT_ENABLES.len() { self.fault_off_next(r, cur) } else { None };
        let fine = match step {
            Some(want) => want == v,
            None if self.rings.fault_step < rg::FAULT_ENABLES.len() => false,
            None => v == rg::eop_on(cur) || v == rg::eop_off(cur) || off.is_some_and(|(_, want)| want == v),
        };
        if !fine {
            let next = rg::FAULT_ENABLES.get(self.rings.fault_step).map_or(String::from("the end-of-pipe enables on or off, or the next of the clears at a suspend"), |op| format!("{}'s next enable", op.reg().name));
            return self.refuse(format!("{} written {v:#x} over {cur:#x}: Linux's next change here is {next} (gfx_v12_0.c:3852-3862, :3888-3890, :4752-4843)", named(r)));
        }
        if step.is_some() {
            self.rings.fault_step += 1;
        }
        if let Some((at, want)) = off
            && want == v
            && want != cur
        {
            self.rings.off_step = at + 1;
        }
        if super::fakemodel::same(r, rg::CP_INT_CNTL_RING0) {
            if let Some(b) = self.nb.n.byte(r) {
                self.cp.vals.insert(b, v);
            }
        } else {
            self.rings.me1[usize::from(super::fakemodel::same(r, rg::CP_ME1_PIPE1_INT_CNTL))] = v;
        }
        true
    }
}

impl Model {
    /// The next of Linux's fault clears (`gc12ring::FAULT_DISABLES`) that still changes something, when it is on
    /// register `r`: its index and the value it writes over `cur`. A clear whose bit is already off is passed
    /// over, as Linux puts only a source it got.
    fn fault_off_next(&self, r: Reg, cur: u32) -> Option<(usize, u32)> {
        let n = &self.nb.n;
        let (at, reg, change) = rg::FAULT_DISABLES.iter().enumerate().skip(self.rings.off_step).find_map(|(i, op)| match op {
            Op::Modify(reg, change) => {
                let v = n.byte(*reg).map(|_| self.int_cntl(*reg))?;
                (change(v) != v).then_some((i, *reg, *change))
            }
            _ => None,
        })?;
        super::fakemodel::same(reg, r).then(|| (at, change(cur)))
    }

    /// What MES's start stands on (the cross-item rule between `gccp` and `mes`): MEC, PFP and ME unhalted, the
    /// KIQ's place for MES set in RLC_CP_SCHEDULERS and NBIF's doorbell entry for MES routed (gfx_v12_0.c:3543-3550,
    /// mes_v12_0.c:1944-1953).
    pub(super) fn cp_ready_for_mes(&self) -> bool {
        use rdna4_core::{doorbell12 as db, gc12cp as cp};
        let kiq = self.cp.val(&self.nb.n, cp::RLC_CP_SCHEDULERS) & 0xFF;
        self.cp.mec_on && self.cp.gfx_on && kiq == cp::kiq_setting(0) & 0xFF && self.cp.entries[1] == Some(db::ENTRY_3_MES)
    }
}

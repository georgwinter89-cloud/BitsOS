//! Wave 44, item ME: the TLB flush once MES runs. Linux's `gmc_v12_0_flush_gpu_tlb` (gmc_v12_0.c:305-330) sees
//! `mes.ring[0].sched.ready`, set at the end of `mes_v12_0_hw_init` (mes_v12_0.c:2092-2096), and then hands each
//! hub's invalidation to MES: `amdgpu_gmc_fw_reg_write_reg_wait` (amdgpu_gmc.c:868-883) ->
//! `amdgpu_mes_reg_write_reg_wait` (amdgpu_mes.c:636-665), one MISC command (WAIT_REG_MEM, WR_WAIT_WR_REG) on the
//! KIQ pipe (`mes_v12_0_misc_op` :846-848 with `enable_uni_mes`) naming the hub's engine 17 request and
//! acknowledge. A bind's or an unbind's flush (`amdgpu_gart_invalidate_tlb`, amdgpu_gart.c:492-506) is then the HDP
//! flush of its own, and per hub (GFXHUB while it is on, then MMHUB) an HDP flush and that command: three HDP
//! flushes on the bus and nothing else, which is the trace-load's form after MES (lines 106997-107001 of
//! plan/hw/gaming-pc-gpu-2026-11-18/trace-load.txt.zst). No semaphore, no acknowledge poll by the CPU, no
//! MMHUB private invalidation: the firmware does the request and the wait.
//!
//! `gmc_v12_0_flush_gpu_tlb` returns nothing (`void`), so a command MES does not answer is logged and the flush
//! goes on, as Linux's does; a stage's own commands stop the stage instead (`messtage`).

use super::gart12::{FlushRegs, TlbFlush, flush_hdp};
use super::host::Card;
use super::mesring::{Place, Port, said};
use crate::gpucore::psp::PspError;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::wb::WbPool;
use rdna4_core::hub41;
use rdna4_core::mes12api::{self as api, Misc};

/// What a flush through MES needs of the running MES: the KIQ pipe's ring and the write-back slots.
pub struct MesTlb<'a> {
    pub f: FlushRegs,
    pub place: Place,
    pub kiq: &'a mut Port,
    pub pool: &'a mut WbPool,
    /// Commands sent and commands that got no good answer, for the stage's lines.
    pub sent: u32,
    pub unanswered: u32,
}

/// The one command of a hub's flush: write the invalidation request for VMID 0 into the hub's engine 17
/// request, then wait for bit 0 (`1 << vmid`) in its acknowledge. Dword offsets, as Linux passes them
/// (`hub->vm_inv_eng0_req + hub->eng_distance * eng`).
pub fn hub_command(req: u32, ack: u32) -> Misc {
    Misc::WriteWait { reg0: req / 4, reg1: ack / 4, reference: hub41::invalidate_req(0), mask: 1 }
}

impl MesTlb<'_> {
    /// One hub, as `gmc_v12_0_flush_gpu_tlb` does it with MES ready.
    fn hub(&mut self, card: &mut dyn Card, regs: Option<(u32, u32)>, out: &mut Vec<String>) -> Result<(), PspError> {
        let Some((req, ack)) = regs else { return Ok(()) };
        flush_hdp(card.hw(), &self.f)?;
        let mut frame = api::misc_frame(hub_command(req, ack));
        self.sent += 1;
        match self.kiq.submit(card, &self.place, self.pool, &mut frame) {
            Ok(s) if s.ok() => {}
            Ok(s) => {
                self.unanswered += 1;
                let line = said(self.kiq.pipe, &frame, s);
                klog::event!(Error, "gx", "rdna4.tlb_flush.mes", what = line.as_str(); "MES did not answer a TLB flush; going on as Linux does");
                out.push(format!("tlb flush: {line}; Linux logs it and goes on (gmc_v12_0.c:322-326), so does this"));
            }
            Err(e) => {
                self.unanswered += 1;
                out.push(format!("tlb flush: the command to MES could not be written ({e:?}); going on as Linux does"));
            }
        }
        Ok(())
    }
}

impl TlbFlush for MesTlb<'_> {
    /// `amdgpu_gart_invalidate_tlb`: an HDP flush of its own, then GFXHUB (bit 0 of `vmhubs_mask`) and MMHUB
    /// (bit 8).
    fn invalidate(&mut self, card: &mut dyn Card, out: &mut Vec<String>) -> Result<(), PspError> {
        flush_hdp(card.hw(), &self.f)?;
        let gc = self.f.gc;
        self.hub(card, gc, out)?;
        let mm = Some((self.f.req, self.f.ack));
        self.hub(card, mm, out)
    }
}

/// The flush in this boot's form: by registers until MES runs, through MES once its stage passed
/// (`messtage::State::up`).
pub enum Tlb<'a> {
    Regs(FlushRegs),
    Mes(MesTlb<'a>),
}

impl TlbFlush for Tlb<'_> {
    fn invalidate(&mut self, card: &mut dyn Card, out: &mut Vec<String>) -> Result<(), PspError> {
        match self {
            Tlb::Regs(f) => f.invalidate(card, out),
            Tlb::Mes(m) => m.invalidate(card, out),
        }
    }
}

/// The form for a flush now: `f` the boot's flush registers (`Session::flush_regs`).
pub fn tlb(f: FlushRegs, mes: &mut super::messtage::State) -> Tlb<'_> {
    match mes.up.as_mut() {
        Some(up) => Tlb::Mes(MesTlb { f, place: up.place, kiq: &mut up.ports[1], pool: &mut up.pool, sent: 0, unanswered: 0 }),
        None => Tlb::Regs(f),
    }
}

/// What a flush in this boot's form writes through the guard: the register form's registers, or, through MES,
/// the HDP flush register alone (the hubs' requests are the firmware's writes, not the CPU's).
pub fn written(f: &FlushRegs, mes_runs: bool) -> Vec<u32> {
    if mes_runs { alloc::vec![f.hdp] } else { f.written() }
}

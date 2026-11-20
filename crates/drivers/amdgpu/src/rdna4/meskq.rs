//! Wave 44, item ME: the stage `mesq`, the kernel's five queues mapped through MES as Linux's `gfx_v12_0_cp_resume`
//! does right after MES's start (gfx_v12_0.c:3566-3575): `gfx_v12_0_kcq_resume` (:3508-3522, four compute MQDs
//! under their selections, then `amdgpu_gfx_enable_kcq` -> `amdgpu_gfx_mes_enable_kcq`, amdgpu_gfx.c:698-737: an
//! HDP flush and an ADD_QUEUE per compute ring), then `gfx_v12_0_cp_async_gfx_ring_resume`'s first half
//! (:3130-3141: the gfx MQD under the gfx ring's selection, `amdgpu_gfx_enable_kgq` :801-823: an HDP flush and its
//! ADD_QUEUE). The second stage of the trace's section `mes` (init.txt 9554-9567): eight selections, an HDP flush,
//! two selections, an HDP flush; the five commands are memory and a doorbell, which the trace does not hold.
//!
//! The queues' memory lies in MES's piece, bound by `mes` (Linux bound it in `sw_init`): an MQD page each, the
//! compute queues' EOP buffers, 8 KiB rings (`mes12::KERNEL_QUEUES`). Their contents beyond the MQD (the rings'
//! packets, their tests: the trace's section gc-ring) are the rings' stage's, which takes `KernelQueue` from here.
//! Mapping and unmapping a kernel queue and the register operations through MES are `map_legacy`, `unmap_legacy`,
//! `rreg`, `wreg` and `reg_write_reg_wait` here.

use super::gart12::flush_hdp;
use super::mesring::{Place, Sent, Slots, lay, said};
use super::messtage::{Up, q_stop};
use super::nb::Io;
use super::regs14::Regs14;
use super::sdmaq::{QStop, sys_write};
use super::stage::{Session, Stop, psp_stop, stop};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::mes12::{self, Kq};
use rdna4_core::mes12::Pipe;
use rdna4_core::mes12api::{self as api, Legacy, Misc, Unmap};
use rdna4_core::mqd12::{self, Prop};
use rdna4_core::refuse;

/// One kernel queue as the rings' stage takes it: where its MQD, ring, EOP buffer and slots lie (GART and system
/// addresses), its doorbell and its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelQueue {
    pub kq: Kq,
    pub slots: Slots,
    pub mqd_mc: u64,
    pub mqd_sys: u64,
    pub ring_mc: u64,
    pub ring_sys: u64,
    pub eop_mc: Option<u64>,
}

impl KernelQueue {
    pub fn of(up: &Up, i: usize) -> Option<KernelQueue> {
        let (kq, slots, p) = (*mes12::KERNEL_QUEUES.get(i)?, *up.kq_slots.get(i)?, up.place);
        let eop = (!kq.gfx).then(|| p.mc + lay::KQ_EOP + i as u64 * u64::from(mes12::MEC_HPD_BYTES));
        Some(KernelQueue { kq, slots, mqd_mc: p.mc + lay::kq_mqd(i), mqd_sys: p.sys + lay::kq_mqd(i), ring_mc: p.mc + lay::kq_ring(i), ring_sys: p.sys + lay::kq_ring(i), eop_mc: eop })
    }

    pub fn prop(&self, p: &Place, i: usize) -> Prop {
        Prop {
            mqd: self.mqd_mc,
            ring: self.ring_mc,
            ring_bytes: mes12::KQ_RING_BYTES,
            rptr: p.slot_mc(self.slots.rptr),
            wptr: p.slot_mc(self.slots.wptr),
            eop: self.eop_mc.unwrap_or(0),
            doorbell: self.kq.doorbell,
            // amdgpu_gfx_is_high_priority_compute_queue: the first compute ring of several.
            high_priority: !self.kq.gfx && i == 0,
        }
    }

    /// What `amdgpu_mes_map_legacy_queue` hands MES for this queue.
    pub fn legacy(&self, p: &Place) -> Legacy {
        Legacy { queue_type: if self.kq.gfx { api::qtype::GFX } else { api::qtype::COMPUTE }, pipe: self.kq.pipe, queue: self.kq.queue, doorbell: self.kq.doorbell, mqd: self.mqd_mc, wptr: p.slot_mc(self.slots.wptr) }
    }
}

/// `mes_v12_0_map_legacy_queue` (mes_v12_0.c:694-728): ADD_QUEUE with `map_legacy_kq` on the scheduler pipe for
/// a gfx or compute queue, on the KIQ pipe for the scheduler's own queue (`enable_uni_mes`).
pub fn map_legacy(up: &mut Up, card: &mut dyn super::host::Card, q: &Legacy) -> Result<(Sent, String), QStop> {
    let port = if q.queue_type == api::qtype::SCHQ { 1 } else { 0 };
    let mut frame = api::add_legacy(q);
    let sent = up.ports[port].submit(card, &up.place, &mut up.pool, &mut frame)?;
    Ok((sent, said(up.ports[port].pipe, &frame, sent)))
}

/// `mes_v12_0_unmap_legacy_queue` (:730-783): REMOVE_QUEUE on the pipe the queue was mapped on. `trail`: the
/// trailing fence's address and value for `PreemptQueuesNoUnmap`.
pub fn unmap_legacy(up: &mut Up, card: &mut dyn super::host::Card, q: &Legacy, action: Unmap, trail: (u64, u64)) -> Result<(Sent, String), QStop> {
    let port = if q.queue_type == api::qtype::SCHQ { 1 } else { 0 };
    let mut frame = api::remove_legacy(q, action, mes12::fw_version(up.sched_version), trail);
    let sent = up.ports[port].submit(card, &up.place, &mut up.pool, &mut frame)?;
    Ok((sent, said(up.ports[port].pipe, &frame, sent)))
}

/// The stage's own order: MES runs (the chain's order says so), once in a boot.
pub fn order(s: &super::messtage::State) -> Result<(), Stop> {
    if s.kq {
        return Err(stop("die Warteschlangen sind schon beim Planer angemeldet", "mesq ran in this boot: a second run is refused"));
    }
    if s.kq_tried {
        return Err(stop("die Warteschlangen wurden in diesem Start schon angefasst: erst neu starten", "mesq stopped half-way in this boot: some kernel queues may be mapped"));
    }
    Ok(())
}

fn select(io: &mut Io, v: u32) -> Result<(), Stop> {
    io.write(mes12::GRBM_GFX_CNTL, v)
}

fn put(io: &mut Io, at: u64, m: &[u32]) -> Result<(), Stop> {
    let bytes: Vec<u8> = m.iter().flat_map(|w| w.to_le_bytes()).collect();
    sys_write(io.card, at, &bytes).map_err(q_stop)
}

/// One queue's MQD as `gfx_v12_0_kcq_init_queue` / `kgq_init_queue` makes it: cleared, then filled while the
/// queue is selected (`amdgpu_ring_init_mqd` under `soc24_grbm_select`).
fn init_queue(io: &mut Io, up: &Up, i: usize) -> Result<KernelQueue, Stop> {
    let q = KernelQueue::of(up, i).ok_or_else(|| stop("Reihenfolge stimmt nicht", "no such kernel queue"))?;
    put(io, q.mqd_sys, &alloc::vec![0u32; mqd12::MQD_DW])?;
    select(io, mes12::select(q.kq.me, q.kq.pipe, q.kq.queue))?;
    let prop = q.prop(&up.place, i);
    put(io, q.mqd_sys, &if q.kq.gfx { mqd12::gfx(&prop) } else { mqd12::compute(&prop) })?;
    select(io, 0)?;
    Ok(q)
}

/// `gfx_v12_0_kcq_resume`'s and `amdgpu_gfx_enable_kgq`'s map of one queue: `map_legacy`, a command whose failure
/// stops the stage.
fn map(up: &mut Up, io: &mut Io, q: &KernelQueue, out: &mut Vec<String>) -> Result<(), Stop> {
    let legacy = q.legacy(&up.place);
    let (sent, line) = map_legacy(up, io.card, &legacy).map_err(q_stop)?;
    out.push(line.clone());
    if sent.ok() {
        return Ok(());
    }
    klog::event!(Error, "gx", "rdna4.mes.command", what = line.as_str(); "MES did not answer a command of its start");
    let (what, kind) = if q.kq.gfx { ("der Planer nimmt die Grafik-Warteschlange nicht an", "gfx") } else { ("der Planer nimmt eine Rechen-Warteschlange nicht an", "compute") };
    Err(stop(what, format!("failed to map {kind} queue: {line}")))
}

pub fn run(s: &mut Session, io: &mut Io, out: &mut Vec<String>) -> Result<(), Stop> {
    order(&s.mes)?;
    let hdp = io.flush()?;
    let up = s.mes.up.as_mut().ok_or_else(|| stop("zuerst den Planer der Karte starten", "MES does not run in this boot"))?;
    s.mes.kq_tried = true;
    let mut compute = Vec::new();
    for i in 0..4 {
        compute.push(init_queue(io, up, i)?);
    }
    flush_hdp(io.card.hw(), &hdp).map_err(psp_stop)?;
    for q in &compute {
        map(up, io, q, out)?;
    }
    let gfx = init_queue(io, up, 4)?;
    flush_hdp(io.card.hw(), &hdp).map_err(psp_stop)?;
    map(up, io, &gfx, out)?;
    for q in compute.iter().chain([&gfx]) {
        out.push(format!(
            "mesq: {} queue me {} pipe {} queue {}: MQD at mc {:#x}, ring at mc {:#x} ({} bytes), doorbell dword {:#x}, write pointer slot at mc {:#x}",
            if q.kq.gfx { "gfx" } else { "compute" },
            q.kq.me,
            q.kq.pipe,
            q.kq.queue,
            q.mqd_mc,
            q.ring_mc,
            mes12::KQ_RING_BYTES,
            q.kq.doorbell,
            up.place.slot_mc(q.slots.wptr)
        ));
    }
    s.mes.kq = true;
    Ok(())
}

/// The registers' values a stage may read through MES once it runs (`amdgpu_mes_rreg`, amdgpu_mes.c:570-608):
/// the misc command READ_REG into a write-back slot, read back from there. A read changes nothing on this card
/// (the guard lets reads pass), so it is held against nothing.
pub fn rreg(up: &mut Up, card: &mut dyn super::host::Card, reg_dw: u32) -> Result<Result<u32, String>, QStop> {
    let slot = up.pool.get().map_err(QStop::Slots)?;
    let at = up.place.slot_sys(slot);
    sys_write(card, at, &0u32.to_le_bytes())?;
    let mut frame = api::misc_frame(Misc::ReadReg { reg: reg_dw, buffer: up.place.slot_mc(slot) });
    let sent = up.ports[1].submit(card, &up.place, &mut up.pool, &mut frame);
    let v = super::sdmaq::sys_read32(card, at);
    // A READ_REG without its fence may still write its value later: the slot stays taken, as the status slot does.
    if matches!(sent, Ok(Sent::Done { .. } | Sent::Failed { .. } | Sent::Full)) {
        up.pool.free(slot).map_err(QStop::Slots)?;
    }
    let sent = sent?;
    Ok(if sent.ok() { Ok(v?) } else { Err(said(Pipe::Kiq, &frame, sent)) })
}

/// What a stage lets MES write for it. The firmware writes the register itself, past the stage's guard, so the
/// API holds every write against the stage's own list (BAR5 bytes) and the classes refused by construction
/// (`rdna4_core::refuse::write_refused`: the flash, the bootloader's mailbox, the SR-IOV ring, the ring controls,
/// the SMU's debug mailbox and every SMU message but a reviewed question; and the index pair, which reaches any
/// address of the card) before the command goes out.
#[derive(Clone, Copy)]
pub struct MesWrites<'a> {
    pub regs: &'a Regs14,
    pub list: &'a [u32],
}

impl MesWrites<'_> {
    /// Why MES may not write `value` into the register at dword `reg_dw` for this stage, or `None`.
    pub fn refused(&self, reg_dw: u32, value: u32) -> Option<&'static str> {
        let Some(byte) = reg_dw.checked_mul(4) else { return Some("past the register space") };
        if let Some(why) = self.regs.reg_at(byte).and_then(|r| refuse::write_refused(r, value)) {
            return Some(why);
        }
        if byte == self.regs.rsmu_index || byte == self.regs.rsmu_data {
            return Some("the index pair reaches any address of the card: never through MES");
        }
        (!self.list.contains(&byte)).then_some("not on the list of what this stage lets MES write")
    }

    fn check(&self, reg_dw: u32, value: u32) -> Result<(), String> {
        let Some(why) = self.refused(reg_dw, value) else { return Ok(()) };
        let name = self.regs.name(reg_dw.wrapping_mul(4));
        klog::event!(Error, "gx", "rdna4.mes.write_refused", reg = name, dword = klog::Hex(u64::from(reg_dw)), value = klog::Hex(u64::from(value)), why = why; "a register write through MES refused before the command went out");
        Err(alloc::format!("refused before MES: {value:#x} into dword {reg_dw:#x} ({name}): {why}"))
    }
}

/// A register write through MES (`amdgpu_mes_wreg` :610-634), held against `may` first.
pub fn wreg(up: &mut Up, card: &mut dyn super::host::Card, may: MesWrites, reg_dw: u32, value: u32) -> Result<Result<(), String>, QStop> {
    if let Err(why) = may.check(reg_dw, value) {
        return Ok(Err(why));
    }
    let mut frame = api::misc_frame(Misc::WriteReg { reg: reg_dw, value });
    let sent = up.ports[1].submit(card, &up.place, &mut up.pool, &mut frame)?;
    Ok(if sent.ok() { Ok(()) } else { Err(said(Pipe::Kiq, &frame, sent)) })
}

/// A register write and wait through MES (`amdgpu_mes_reg_write_reg_wait` :636-665): `reg0` gets `reference`, then
/// MES waits for `reg1 & mask`; dword offsets. The write is held against `may` first; the wait only reads.
pub fn reg_write_reg_wait(up: &mut Up, card: &mut dyn super::host::Card, may: MesWrites, reg0: u32, reg1: u32, reference: u32, mask: u32) -> Result<Result<(), String>, QStop> {
    if let Err(why) = may.check(reg0, reference) {
        return Ok(Err(why));
    }
    let mut frame = api::misc_frame(Misc::WriteWait { reg0, reg1, reference, mask });
    let sent = up.ports[1].submit(card, &up.place, &mut up.pool, &mut frame)?;
    Ok(if sent.ok() { Ok(()) } else { Err(said(Pipe::Kiq, &frame, sent)) })
}

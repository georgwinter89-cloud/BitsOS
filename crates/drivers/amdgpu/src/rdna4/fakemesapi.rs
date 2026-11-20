//! Wave 44, item ME: what the model's MES does with one frame of its ring, and what it refuses
//! (include/mes_v12_api_def.h is the format; mes_v12_0.c and amdgpu_mes.c are what Linux sends on this card):
//!
//! - a frame whose header is not a scheduler frame of 64 dwords, an opcode Linux's code for this card never
//!   sends (SUSPEND, RESUME, RESET, SET_SCHEDULING_CONFIG, the log and debug commands, INV_TLBS, a misc op but
//!   WRITE_REG, READ_REG, WAIT_REG_MEM's WR_WAIT_WR_REG and CHANGE_CONFIG; MES_SCH_API_MAX and above);
//! - any command on a pipe before SET_HW_RESOURCES took on it, and SET_HW_RESOURCES twice; its fields other than
//!   Linux's for the pipe (the masks, the doorbells, the register bases of the measured table, the flags
//!   0x100447, the timer), slots GFXHUB does not find;
//! - ADD_QUEUE without `map_legacy_kq` or with a process's fields, the scheduler's own queue on any but the KIQ
//!   pipe or with another doorbell than 0x16, a gfx or compute queue on any but the scheduler pipe, at a place or
//!   doorbell that is none of the kernel's five, inside the queues SET_HW_RESOURCES handed MES, mapped twice,
//!   or on an MQD that is not bound or not Linux's;
//! - REMOVE_QUEUE of a queue that is not mapped; a register MES is to write or wait on that the model does not
//!   know; CHANGE_CONFIG with another option than Linux's here (no isolation).
//!
//! A refused command gets the header's failed status (lower dword 0, the error code above it); the trailing
//! QUERY_SCHEDULER_STATUS still writes its fence, as the firmware answers every frame. A mapped gfx or compute
//! queue is handed to the command processor's model through `Model::mes_queue_mapped`, from its MQD as the firmware
//! reads it (the ring's base and size, the read pointer's report address; the write pointer's poll address and
//! the doorbell from the command, which the MQD must agree with); that model refuses it on halted engines or with
//! a doorbell outside the CP's range, and the ADD_QUEUE fails then. An unmapped queue leaves the CP's model.

use super::fakemes::{HwSeen, Mapped};
use super::fakemeshqd::queue_of;
use super::fakemodel::Model;
use alloc::format;
use alloc::string::String;
use rdna4_core::mes12::{self, Pipe};
use rdna4_core::mes12api::{self as api, at, misc, op, qtype};
use rdna4_core::mqd12::{self, c, g};

/// How many reads of the acknowledge the model's firmware makes before it gives up on a WR_WAIT_WR_REG: the
/// model's own number (the firmware's bound is in no source; plan/spec/gpu-rdna4.md section 7, `mes-wr-wait`).
const WAIT_READS: u32 = 1000;

impl Model {
    /// One frame on pipe `p`: executed, its status written.
    pub(super) fn mes_frame(&mut self, p: Pipe, d: &[u32]) {
        let (ty, opcode, size) = api::header_fields(d[0]);
        let status = api::status_at(opcode).filter(|_| ty == api::TYPE_SCHEDULER && size as usize == api::FRAME_DW);
        // A frame the firmware cannot place its status in gets none: refused, and the driver's wait finds the
        // status slot 0.
        let Some(at) = status else {
            let why = if ty == api::TYPE_SCHEDULER && size as usize == api::FRAME_DW {
                format!("{} (opcode {opcode}): a command Linux's code for this card never sends", api::op_name(opcode))
            } else {
                format!("a frame with header {:#x} (type {ty}, opcode {opcode}, {size} dwords): Linux sends scheduler frames of 64 dwords only", d[0])
            };
            self.refused.push(format!("MES({}) {why}", p.index()));
            return;
        };
        let r = self.mes_exec(p, opcode, d);
        let (addr, value) = (api::get64(d, at), api::get64(d, at + 2));
        let nth = self.mes.frames.len() - 1;
        let answer = match &r {
            Ok(()) if self.mes.status_zero == Some(nth) => 0,
            Ok(()) if opcode == op::QUERY_SCHEDULER_STATUS && self.mes.wrong_fence => value.wrapping_sub(1),
            Ok(()) => value,
            Err(why) => {
                self.refused.push(format!("MES({}) {}: {why}", p.index(), api::op_name(opcode)));
                api::error_status(1, opcode, if opcode == op::MISC { d[at::MISC_OPCODE] } else { 0 }, api::ERROR_API)
            }
        };
        self.mes_put64(addr, answer);
    }

    /// The firmware writes a 64-bit value into system memory through GFXHUB.
    pub(super) fn mes_put64(&mut self, mc: u64, v: u64) {
        match self.gc_walk(mc) {
            Some(sys) => self.nb.sys_poke(sys, &v.to_le_bytes()),
            None => {
                self.refused.push(format!("MES wrote its answer to {mc:#x}, which GFXHUB's walker does not find"));
                self.mes_fault(mc);
            }
        }
    }

    fn mes_exec(&mut self, p: Pipe, opcode: u32, d: &[u32]) -> Result<(), String> {
        let i = p.index();
        if opcode == op::SET_HW_RSRC {
            return self.mes_hw(p, d);
        }
        if self.mes.hw[i].is_none() {
            return Err(format!("{} before SET_HW_RESOURCES took on the {}", api::op_name(opcode), p.name()));
        }
        match opcode {
            op::SET_HW_RSRC_1 => {
                let fence = api::get64(d, at::HW1_CLEANER_FENCE);
                if d[at::HW1_KIQ_UNMAP_TIMEOUT] != api::KIQ_UNMAP_TIMEOUT || !fence.is_multiple_of(rdna4_core::pte12::PAGE) || !self.in_vram(fence, rdna4_core::pte12::PAGE) {
                    return Err(format!("SET_HW_RESOURCES_1 with the unmap timeout {:#x} and the cleaner fence at {fence:#x}: Linux gives 0xa and a VRAM page (mes_v12_0.c:918-934)", d[at::HW1_KIQ_UNMAP_TIMEOUT]));
                }
                self.mes.hw1[i] += 1;
                Ok(())
            }
            op::QUERY_SCHEDULER_STATUS => if d[at::QUERY_SUBOPCODE] == 0 { Ok(()) } else { Err(format!("QUERY_SCHEDULER_STATUS with subopcode {}: Linux sends 0", d[at::QUERY_SUBOPCODE])) },
            op::ADD_QUEUE => self.mes_add(p, d),
            op::REMOVE_QUEUE => self.mes_remove(p, d),
            // `mes_frame` takes no other opcode here.
            _ => self.mes_misc(p, d),
        }
    }

    fn mes_hw(&mut self, p: Pipe, d: &[u32]) -> Result<(), String> {
        let i = p.index();
        let want = api::set_hw_resources(&api::HwRes {
            sched: p == Pipe::Sched,
            sch_ctx: api::get64(d, at::HW_SCH_CTX),
            query_fence: api::get64(d, at::HW_QUERY_FENCE),
            gc_base: self.mes.bases[0],
            mmhub_base: self.mes.bases[1],
            osssys_base: self.mes.bases[2],
            oversubscription_timer: mes12::oversubscription_timer(mes12::fw_version(self.mes.gp3)),
        });
        if let Some(k) = (1..at::HW_STATUS).chain(at::HW_FLAGS..api::FRAME_DW).find(|k| d[*k] != want[*k]) {
            return Err(format!("SET_HW_RESOURCES on the {} with dword {k} {:#x} where Linux's struct holds {:#x} (mes_v12_0.c:936-1019)", p.name(), d[k], want[k]));
        }
        if self.mes.hw[i].is_some() {
            return Err(format!("SET_HW_RESOURCES a second time on the {}", p.name()));
        }
        for slot in [api::get64(d, at::HW_SCH_CTX), api::get64(d, at::HW_QUERY_FENCE)] {
            if self.gc_walk(slot).is_none() {
                return Err(format!("SET_HW_RESOURCES names a slot at {slot:#x} that GFXHUB's walker does not find"));
            }
        }
        let mut compute = [0; 8];
        compute.copy_from_slice(&d[at::HW_COMPUTE_HQD..at::HW_COMPUTE_HQD + 8]);
        self.mes.hw[i] = Some(HwSeen { compute_hqd: compute, gfx_hqd: [d[at::HW_GFX_HQD], d[at::HW_GFX_HQD + 1]] });
        Ok(())
    }

    fn mes_add(&mut self, p: Pipe, d: &[u32]) -> Result<(), String> {
        let named = [0, at::ADD_DOORBELL, at::ADD_MQD, at::ADD_MQD + 1, at::ADD_WPTR, at::ADD_WPTR + 1, at::ADD_QUEUE_TYPE, at::ADD_FLAGS, at::ADD_PIPE, at::ADD_QUEUE];
        let status = at::ADD_STATUS..at::ADD_STATUS + 4;
        if d[at::ADD_FLAGS] != api::ADD_MAP_LEGACY_KQ {
            return Err(format!("ADD_QUEUE with the flags {:#x}: Linux maps only kernel queues here (map_legacy_kq alone, mes_v12_0.c:694-728)", d[at::ADD_FLAGS]));
        }
        if let Some(k) = (1..api::FRAME_DW).find(|k| !named.contains(k) && !status.contains(k) && d[*k] != 0) {
            return Err(format!("ADD_QUEUE of a kernel queue with dword {k} {:#x}: Linux leaves every process and gang field 0", d[k]));
        }
        let q = Mapped { queue_type: d[at::ADD_QUEUE_TYPE], pipe: d[at::ADD_PIPE], queue: d[at::ADD_QUEUE], doorbell: d[at::ADD_DOORBELL], mqd: api::get64(d, at::ADD_MQD), wptr: api::get64(d, at::ADD_WPTR) };
        if self.mes.mapped.iter().any(|m| m.doorbell == q.doorbell || (m.queue_type, m.pipe, m.queue) == (q.queue_type, q.pipe, q.queue)) {
            return Err(format!("ADD_QUEUE of a queue mapped already (type {}, pipe {}, queue {}, doorbell {:#x})", q.queue_type, q.pipe, q.queue, q.doorbell));
        }
        let mqd = self.mes_mqd(q.mqd).ok_or_else(|| format!("ADD_QUEUE names an MQD at {:#x} that GFXHUB's walker does not find", q.mqd))?;
        match q.queue_type {
            qtype::SCHQ => {
                if p != Pipe::Kiq || (q.pipe, q.queue, q.doorbell) != (0, 0, Pipe::Sched.doorbell()) {
                    return Err(format!("the scheduler's queue mapped on the {} as pipe {} queue {} doorbell {:#x}: Linux maps it on the KIQ pipe as pipe 0 queue 0 with doorbell 0x16 (mes_v12_0.c:711-722)", p.name(), q.pipe, q.queue, q.doorbell));
                }
                mes_mqd_is_linux(&mqd, q.mqd, q.doorbell, q.wptr)?;
                self.mes.queue[Pipe::Sched.index()] = Some(queue_of(&mqd, q.mqd));
                self.mes.mapped.push(q);
                Ok(())
            }
            qtype::GFX | qtype::COMPUTE => {
                let gfx = q.queue_type == qtype::GFX;
                if p != Pipe::Sched {
                    return Err(String::from("a gfx or compute queue mapped on the KIQ pipe: Linux maps the kernel's queues on the scheduler pipe (mes_v12_0.c:716-720)"));
                }
                let hw = self.mes.hw[Pipe::Sched.index()].clone().unwrap_or(HwSeen { compute_hqd: [0; 8], gfx_hqd: [0; 2] });
                let mask = if gfx { hw.gfx_hqd.get(q.pipe as usize) } else { hw.compute_hqd.get(q.pipe as usize) }.copied().unwrap_or(0);
                if mask & 1u32.checked_shl(q.queue).unwrap_or(0) != 0 {
                    return Err(format!("ADD_QUEUE of queue {} on pipe {}, which SET_HW_RESOURCES handed MES itself (mask {mask:#x})", q.queue, q.pipe));
                }
                if !mes12::KERNEL_QUEUES.iter().any(|k| k.gfx == gfx && (k.pipe, k.queue, k.doorbell) == (q.pipe, q.queue, q.doorbell)) {
                    return Err(format!("ADD_QUEUE of a {} queue at pipe {} queue {} doorbell {:#x}, which is none of the kernel's queues on this card", if gfx { "gfx" } else { "compute" }, q.pipe, q.queue, q.doorbell));
                }
                kernel_mqd_is_linux(&mqd, gfx, q.mqd, q.doorbell, q.wptr)?;
                self.mes_queue_mapped(q, &mqd).map_err(|why| format!("the command processor's model refused the queue: {why}"))?;
                self.mes.mapped.push(q);
                Ok(())
            }
            other => Err(format!("ADD_QUEUE of queue type {other}: Linux maps gfx, compute and the scheduler's queue here")),
        }
    }

    fn mes_remove(&mut self, p: Pipe, d: &[u32]) -> Result<(), String> {
        let flags = d[at::REMOVE_FLAGS];
        let (doorbell, pipe, queue) = (d[at::REMOVE_DOORBELL], d[at::REMOVE_PIPE], d[at::REMOVE_QUEUE]);
        let found = self.mes.mapped.iter().position(|m| (m.doorbell, m.pipe, m.queue) == (doorbell, pipe, queue));
        let Some(k) = found else { return Err(format!("REMOVE_QUEUE of pipe {pipe} queue {queue} doorbell {doorbell:#x}, which MES did not map")) };
        let m = self.mes.mapped[k];
        let on = if m.queue_type == qtype::SCHQ { Pipe::Kiq } else { Pipe::Sched };
        if p != on {
            return Err(format!("REMOVE_QUEUE on the {}: Linux unmaps this queue on the {}", p.name(), on.name()));
        }
        if flags == api::REMOVE_PREEMPT_LEGACY_GFX && m.queue_type == qtype::GFX {
            let tf = api::get64(d, at::REMOVE_TF_ADDR);
            let sys = self.gc_walk(tf).ok_or_else(|| format!("REMOVE_QUEUE's trailing fence at {tf:#x} lies where GFXHUB's walker finds nothing"))?;
            self.nb.sys_poke(sys, &d[at::REMOVE_TF_DATA].to_le_bytes());
            return Ok(());
        }
        let after = flags & api::REMOVE_AFTER_RESET != 0;
        if flags & !api::REMOVE_AFTER_RESET != api::REMOVE_UNMAP_LEGACY || d[at::REMOVE_QUEUE_TYPE] != m.queue_type || (after && mes12::fw_version(self.mes.gp3) < mes12::REMOVE_AFTER_RESET_FROM) {
            return Err(format!("REMOVE_QUEUE with flags {flags:#x} and type {}: not Linux's unmap of a kernel queue (mes_v12_0.c:730-783)", d[at::REMOVE_QUEUE_TYPE]));
        }
        self.mes.mapped.remove(k);
        if m.queue_type == qtype::SCHQ {
            self.mes.queue[Pipe::Sched.index()] = None;
        } else {
            self.mes_queue_unmapped(m);
        }
        Ok(())
    }

    fn mes_misc(&mut self, p: Pipe, d: &[u32]) -> Result<(), String> {
        if p != Pipe::Kiq {
            return Err(String::from("MISC on the scheduler pipe: with one uni_mes file Linux sends it on the KIQ pipe (mes_v12_0.c:846-848)"));
        }
        match (d[at::MISC_OPCODE], d[at::MISC_DATA]) {
            (misc::WAIT_REG_MEM, misc::WRM_WR_WAIT_WR_REG) => {
                let (reg0, reg1, reference, mask) = (d[at::MISC_WRM_REG1], d[at::MISC_WRM_REG2], d[at::MISC_WRM_REFERENCE], d[at::MISC_WRM_MASK]);
                if !self.mes_hub_pair(reg0, reg1) {
                    return Err(format!("WR_WAIT_WR_REG on dwords {reg0:#x} and {reg1:#x}: Linux names a hub's engine 17 request and acknowledge only (gmc_v12_0.c:315-327)"));
                }
                if !self.mes_firmware_write(reg0, reference) {
                    return Err(format!("WR_WAIT_WR_REG: the model refused {reference:#x} into dword {reg0:#x}"));
                }
                for _ in 0..WAIT_READS {
                    if self.mes_firmware_read(reg1).is_some_and(|v| v & mask == mask) {
                        return Ok(());
                    }
                }
                Err(format!("WR_WAIT_WR_REG: dword {reg1:#x} never showed {mask:#x}"))
            }
            (misc::READ_REG, reg) => {
                let v = self.mes_firmware_read(reg).ok_or_else(|| format!("READ_REG of dword {reg:#x}, a register the model does not know"))?;
                let buf = api::get64(d, at::MISC_READ_BUFFER);
                let sys = self.gc_walk(buf).ok_or_else(|| format!("READ_REG into {buf:#x}, where GFXHUB's walker finds nothing"))?;
                self.nb.sys_poke(sys, &v.to_le_bytes());
                Ok(())
            }
            (misc::WRITE_REG, reg) => if self.mes_firmware_write(reg, d[at::MISC_WRITE_VALUE]) { Ok(()) } else { Err(format!("WRITE_REG of dword {reg:#x}: the model refused it")) },
            (misc::CHANGE_CONFIG, misc::CHANGE_CONFIG_LIMIT_SINGLE_PROCESS) if d[at::MISC_CHANGE_CONFIG_OPTION] == 0 => {
                self.mes.change_config += 1;
                Ok(())
            }
            (op, data) => Err(format!("MISC {} ({op}, {data:#x}): a misc command Linux's code for this card never sends", api::misc_name(op))),
        }
    }

    /// A hub's engine 17 request and acknowledge, as dwords.
    fn mes_hub_pair(&self, reg0: u32, reg1: u32) -> bool {
        use rdna4_core::hub41;
        let n = &self.nb.n;
        let eng = |r| n.byte(rdna4_core::nbreg::member(r, hub41::GART_ENGINE, 1)).map(|b| b / 4);
        [hub41::GC, hub41::MM].iter().any(|h| eng(h.eng0_req) == Some(reg0) && eng(h.eng0_ack) == Some(reg1))
    }

    /// The firmware writes a register: through the model as a CPU write would arrive, but without MMHUB's
    /// invalidation semaphore, which Linux's MES path takes none of (the firmware's own handling of it is in no
    /// source).
    fn mes_firmware_write(&mut self, dword: u32, v: u32) -> bool {
        use crate::gpucore::psp::Hw;
        let Some(byte) = self.mes_known(dword) else { return false };
        let held = self.nb.sem_held;
        self.nb.sem_held = true;
        let ok = self.reg_write(byte, v);
        self.nb.sem_held = held;
        if ok {
            self.mes.misc_writes.push((dword, v));
        }
        ok
    }

    fn mes_firmware_read(&mut self, dword: u32) -> Option<u32> {
        use crate::gpucore::psp::Hw;
        let byte = self.mes_known(dword)?;
        self.reg_read(byte)
    }

    /// A register's byte when the model knows the register (by the card's table or by Linux's trace).
    fn mes_known(&self, dword: u32) -> Option<u32> {
        let byte = dword.checked_mul(4)?;
        (self.reg_of(byte).is_some() || super::linuxseq::name_of(dword).is_some()).then_some(byte)
    }

    /// The engines' side of a queue MES mapped: the command processor's model takes it from its MQD.
    pub fn mes_queue_mapped(&mut self, q: Mapped, mqd: &[u32]) -> Result<(), String> {
        let at = queue_at(q, mqd).ok_or_else(|| String::from("the MQD is shorter than its layout"))?;
        self.cp_place_queue(at)
    }

    /// The engines' side of a queue MES unmapped: it leaves the command processor's model.
    pub fn mes_queue_unmapped(&mut self, q: Mapped) {
        if !self.cp_unplace_queue(q.doorbell) {
            self.refused.push(format!("MES unmapped the queue at doorbell {:#x}, which the command processor's model did not hold", q.doorbell));
        }
    }
}

/// Where a kernel queue's MQD puts it (`v12_compute_mqd`, `v12_gfx_mqd`): the ring's base in units of 256 bytes,
/// its size as log2(dwords) - 1, the read pointer's report address.
fn queue_at(q: Mapped, m: &[u32]) -> Option<super::fakecpeng::QueueAt> {
    let at = |i: usize| m.get(i).copied().map(u64::from);
    let gfx = q.queue_type == qtype::GFX;
    let (base, base_hi, rptr, rptr_hi) = if gfx { (g::HQD_BASE, g::HQD_BASE_HI, g::HQD_RPTR_ADDR, g::HQD_RPTR_ADDR_HI) } else { (c::HQD_PQ_BASE_LO, c::HQD_PQ_BASE_HI, c::HQD_PQ_RPTR_REPORT_ADDR_LO, c::HQD_PQ_RPTR_REPORT_ADDR_HI) };
    let size = if gfx { mqd12::RB_BUFSZ.get(m.get(g::HQD_CNTL).copied()?) } else { mes12::f::QUEUE_SIZE.get(m.get(c::HQD_PQ_CONTROL).copied()?) };
    Some(super::fakecpeng::QueueAt {
        gfx,
        me: if gfx { 0 } else { 1 },
        pipe: q.pipe,
        queue: q.queue,
        ring_mc: (at(base_hi)? << 32 | at(base)?) << 8,
        ring_dw: 1 << (size + 1),
        rptr_mc: at(rptr_hi)? << 32 | at(rptr)?,
        wptr_mc: q.wptr,
        doorbell_dw: q.doorbell,
        mqd_rptr: m.get(if gfx { g::HQD_RPTR } else { c::HQD_PQ_RPTR }).copied()?,
        mqd_wptr: at(if gfx { g::HQD_WPTR_HI } else { c::HQD_PQ_WPTR_HI })? << 32 | at(if gfx { g::HQD_WPTR } else { c::HQD_PQ_WPTR_LO })?,
    })
}

/// The scheduler's own MQD as `mes_v12_0_mqd_init` fills it, for the queue the ADD_QUEUE names.
fn mes_mqd_is_linux(m: &[u32], mqd: u64, doorbell: u32, wptr: u64) -> Result<(), String> {
    let poll = u64::from(m[c::HQD_PQ_WPTR_POLL_ADDR_HI]) << 32 | u64::from(m[c::HQD_PQ_WPTR_POLL_ADDR_LO]);
    let checks = [(c::HEADER, mqd12::HEADER), (c::MQD_BASE_ADDR_LO, mqd as u32 & 0xFFFF_FFFC), (c::HQD_PQ_DOORBELL_CONTROL, mes12::doorbell_enabled(doorbell)), (c::HQD_PQ_CONTROL, mes12::mes_pq_control(mes12::RING_BYTES)), (c::HQD_ACTIVE, 1), (c::RESERVED_184, 1 << 15)];
    if let Some((at, want)) = checks.iter().find(|(at, want)| m[*at] != *want) {
        return Err(format!("the scheduler's MQD holds {:#x} at dword {at}, not {want:#x} (mes_v12_0_mqd_init)", m[*at]));
    }
    if poll != wptr {
        return Err(format!("ADD_QUEUE names the write pointer at {wptr:#x}, the MQD polls {poll:#x}"));
    }
    Ok(())
}

/// A kernel queue's MQD as `gfx_v12_0_compute_mqd_init` or `gfx_v12_0_gfx_mqd_init` fills it.
fn kernel_mqd_is_linux(m: &[u32], gfx: bool, mqd: u64, doorbell: u32, wptr: u64) -> Result<(), String> {
    let (base, bell, plo, phi) = if gfx { (g::MQD_BASE_ADDR, g::RB_DOORBELL_CONTROL, g::RB_WPTR_POLL_ADDR_LO, g::RB_WPTR_POLL_ADDR_HI) } else { (c::MQD_BASE_ADDR_LO, c::HQD_PQ_DOORBELL_CONTROL, c::HQD_PQ_WPTR_POLL_ADDR_LO, c::HQD_PQ_WPTR_POLL_ADDR_HI) };
    if !gfx && m[c::HEADER] != mqd12::HEADER {
        return Err(format!("a compute MQD with header {:#x}", m[c::HEADER]));
    }
    if m[base] != mqd as u32 & 0xFFFF_FFFC || mes12::f::DOORBELL_OFFSET.get(m[bell]) != doorbell || mes12::f::DOORBELL_EN.get(m[bell]) != 1 {
        return Err(format!("a kernel queue's MQD naming itself at {:#x} with doorbell control {:#x}: not the queue ADD_QUEUE names ({mqd:#x}, doorbell {doorbell:#x})", m[base], m[bell]));
    }
    let poll = u64::from(m[phi]) << 32 | u64::from(m[plo]);
    if poll != wptr {
        return Err(format!("ADD_QUEUE names the write pointer at {wptr:#x}, the MQD polls {poll:#x}"));
    }
    Ok(())
}

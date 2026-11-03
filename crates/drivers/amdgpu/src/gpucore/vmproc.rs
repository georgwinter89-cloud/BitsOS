//! GX-7m, `until=vm` step 6: the test VM through a MES process queue, as amdgpu_mes_self_test adds its queues
//! (6.6 amdgpu_mes.c:1228-1290, run at every boot of a GC 11.0.1 such as the 780M: mes_v11_0.c:1314-1317).
//!
//! The queue is a compute queue of one process (the test VM, its PASID) and one gang: ADD_QUEUE without
//! map_legacy_kq names the VM's root table (page_table_base_addr: the root's physical address, no PDE bits,
//! amdgpu_mes.c:625-627), the VA range 0 .. (max_pfn - 1) << 12 (:629-631), the process and gang context pages
//! (one page each, zeroed: AMDGPU_MES_PROC_CTX_SIZE/GANG_CTX_SIZE), both quanta 0 (6.6 never sets the defaults),
//! NORMAL priorities (:1233-1237), the doorbell, the MQD (VMID 0), the write pointer's MC address (scheduler API 2
//! and later; the 780M's reads 0x26) and GFXHUB's VM_CONTEXT_CNTL (mes_v11_0.c:166-167, hub->vm_cntx_cntl: the value
//! gmc::enable left in VM_CONTEXT1_CNTL). MES gives the process one of its VMIDs (SET_HW_RESOURCES' mask 0xff00:
//! 8-15), programs and flushes that context and the IH LUT itself ("MES fw manages the LUT", gmc_v11_0.c:437), and
//! maps the queue on an HQD of its choice.
//!
//! The queue's memory lies in the VM's mapped pages, as amdgpu_mes_add_ring puts it in the mapped context BO
//! (amdgpu_mes_ring_to_queue_props :896-912): the ring (1024 dwords), the rptr report, the write pointer, the EOP
//! buffer and the fence slot are VAs to the queue (the MQD names VAs) and MC addresses to the CPU (the same pages
//! through the carve-out), so the MQD's WPTR_POLL is the VA and ADD_QUEUE's wptr the MC address of one slot
//! (wptr_gpu_addr / wptr_mc_addr). The doorbell is AMDGPU_NAVI10_DOORBELL_USERQUEUE_START (amdgpu_doorbell.h:201),
//! inside the MEC's doorbell range (`gfx11::doorbell_ranges`).
//!
//! Rung 1 is ADD_QUEUE answered plus a ring test (a fence to a VA); the CPU then reads MES's contexts 8-15 to find
//! the one holding the VM's root (reads only) and that VMID's LUT entry. Rungs 2-4 are `vmring::vm_rungs` with the
//! fault check asking for a MES VMID (the found one, when found). The queue runs on CPC like the compute ring, so it
//! too stalls at its intended fault (GX-7m3, `vmrecover`): `again` removes and adds it, and rung 4 runs on that. The
//! teardown removes the queue (REMOVE_QUEUE with the doorbell and the gang context, mes_v11_0.c:219-237) before the
//! kernel VMID is torn down; an unanswered ADD_QUEUE or REMOVE_QUEUE leaves the queue stranded (reported; `until=stop`
//! halts MES).

use super::bringup::Bringup;
use super::mes::{self, MesRing, PmQueue, Rung, hx, hx32};
use super::psp::Hw;
use super::vmring::{self, Ctx, Q, Queue, VmError, VmInputs, VmReport, lay};
use super::watch::Watch;
use crate::ipmod::gfx11;
use crate::ipmod::gmc11::{self, R};
use crate::ipmod::mes11;
use crate::ipmod::mesapi::{self, Frame, ProcessQueue};
use crate::ipmod::mesmqd::{self, Prop, c};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::fence::Timeline;
use amdgpu_fmt::ring::RingError;

/// MES's VMIDs on GFXHUB (`HwMasks::LINUX_780M` vmid_gfxhub 0xff00).
pub const MES_VMIDS: core::ops::RangeInclusive<u8> = 8..=15;

pub fn is_mes_vmid(vmid: u32) -> bool {
    u8::try_from(vmid).is_ok_and(|v| MES_VMIDS.contains(&v))
}

/// AMDGPU_NAVI10_DOORBELL_USERQUEUE_START (64-bit units) and as a dword index.
pub const DOORBELL64: u32 = 0x0d;
pub const DOORBELL_DW: u32 = DOORBELL64 << 1;
const _: () = assert!(DOORBELL64 <= gfx11::DOORBELL_USERQUEUE_END64 && DOORBELL_DW != mes11::COMPUTE_DOORBELL_DW && DOORBELL_DW != mes11::SCHED_DOORBELL_DW && DOORBELL_DW != mes11::KIQ_DOORBELL_DW);
/// amdgpu_ring_init(adev, ring, 1024, ...) (amdgpu_mes.c:1006).
pub const RING_BYTES: u64 = 1024 * 4;
/// The queue page: rptr report, write pointer (8 bytes), fence slot, EOP buffer (GFX11_MEC_HPD_SIZE).
const RPTR: u64 = 0;
const WPTR: u64 = 8;
const FENCE: u64 = 0x40;
const EOP: u64 = 0x800;
/// AMDGPU_GFX_PIPE_PRIO_NORMAL (amdgpu_mes_ring_to_queue_props :908; the queue priority stays MINIMUM 0).
const PIPE_PRIO_NORMAL: u32 = 1;
const _: () = assert!(lay::PROC_PAGE == lay::PROC_RING + 0x1000 && RING_BYTES <= 0x1000 && EOP + mes11::EOP_BYTES <= 0x1000);

/// A process queue MES mapped (what the teardown needs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcUp {
    pub doorbell: u32,
    pub gang_ctx: u64,
    /// The MES stage's layout (the scheduler ring, its write pointer, fence and status slots).
    pub mes: mes::Layout,
    /// MES answered ADD_QUEUE (otherwise REMOVE_QUEUE is not tried: MES is deaf).
    pub answered: bool,
}

/// The queue's addresses: GPU side (VA) and CPU side (MC) of the same pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcAddrs {
    pub ring_va: u64,
    pub ring_mc: u64,
    pub page_va: u64,
    pub page_mc: u64,
    pub mqd: u64,
    pub process_ctx: u64,
    pub gang_ctx: u64,
}

impl ProcAddrs {
    /// `base`: the MES buffer's MC address.
    pub fn new(base: u64) -> ProcAddrs {
        let va = |off: u64| vmring::VA_BASE + (off - lay::DATA);
        ProcAddrs {
            ring_va: va(lay::PROC_RING),
            ring_mc: base + lay::PROC_RING,
            page_va: va(lay::PROC_PAGE),
            page_mc: base + lay::PROC_PAGE,
            mqd: base + lay::PROC_MQD,
            process_ctx: base + lay::PROC_CTX,
            gang_ctx: base + lay::GANG_CTX,
        }
    }

    /// The MQD: gfx_v11_0_compute_mqd_init of the queue's VAs (HQD_VMID 0, inactive: MES sets both).
    pub fn mqd(&self) -> Vec<u32> {
        let p = Prop {
            mqd: self.mqd,
            ring: self.ring_va,
            ring_bytes: RING_BYTES,
            rptr: self.page_va + RPTR,
            wptr: self.page_va + WPTR,
            eop: self.page_va + EOP,
            doorbell_dw: Some(DOORBELL_DW),
            active: false,
        };
        let mut m = mesmqd::compute_mqd(&p);
        m[c::PIPE_PRIORITY] = PIPE_PRIO_NORMAL;
        m
    }

    /// ADD_QUEUE's fields.
    pub fn frame(&self, root: u64, pasid: u32, vm_context_cntl: u32) -> ProcessQueue {
        ProcessQueue {
            pasid,
            page_table_base: root,
            va_start: 0,
            va_end: gmc11::MAX_PFN_LAST << 12,
            process_quantum: 0,
            process_ctx: self.process_ctx,
            gang_quantum: 0,
            gang_ctx: self.gang_ctx,
            inprocess_gang_priority: mesapi::PRIORITY_NORMAL,
            gang_global_priority: mesapi::PRIORITY_NORMAL,
            doorbell_dw: DOORBELL_DW,
            mqd: self.mqd,
            wptr: self.page_mc + WPTR,
            queue_type: mesapi::qtype::COMPUTE,
            vm_context_cntl,
        }
    }
}

/// The scheduler ring at the position its buffer holds (the wptr shadow, the fence slot's sequence number).
pub(super) fn sched_ring(hw: &mut dyn Hw, l: &mes::Layout) -> MesRing {
    let wptr = mes::rd64(hw, l.sched_wptr).unwrap_or(0);
    let seq = mes::rd64(hw, l.mes_fence).unwrap_or(0) & 0xffff_ffff;
    MesRing { base: l.sched_ring, size_dw: mes::SCHED_RING_BYTES / 4, wptr, seq }
}

/// Which of MES's contexts holds the root `root` (CPU reads of 8-15).
fn find_vmid(hw: &mut dyn Hw, a: &vmring::VmAddrs, root: u64) -> Option<u8> {
    MES_VMIDS.into_iter().find(|&n| vmring::ctx_base(hw, a, n).is_some_and(|b| b != u64::MAX && b & 0x0000_ffff_ffff_f000 == root))
}

/// What ADD_QUEUE needs of the VM (the first add and a recovery's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcSpec {
    pub root: u64,
    pub pasid: u32,
    pub cntl: u32,
}

/// The queue's memory fresh (MQD, ring, queue page; nothing runs on it) and ADD_QUEUE. `Err`: not answered.
#[inline(never)]
fn add<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &VmInputs, s: &ProcSpec, r: &mut VmReport) -> Result<(PmQueue, String), String> {
    let base = inp.carve.mc_base() + inp.mes_bo.offset;
    let p = ProcAddrs::new(base);
    let memory = |_| String::from("the queue's memory");
    mes::zero(owner, p.ring_mc, RING_BYTES).map_err(memory)?;
    mes::zero(owner, p.page_mc, 0x1000).map_err(memory)?;
    mes::mem(owner, p.mqd, &mes::dwords(&p.mqd())).map_err(memory)?;
    owner.store_barrier();
    let q = PmQueue::new(p.ring_mc, RING_BYTES, Some(p.page_mc + RPTR), p.page_mc + WPTR, DOORBELL_DW).map_err(memory)?;
    q.reset(owner).map_err(memory)?;
    let mut ring = sched_ring(owner, &inp.mes);
    let mut dbs = Vec::new();
    let fq = p.frame(s.root, s.pasid, s.cntl);
    let res = ring.submit(owner, &inp.mes, Frame::add_process_queue(&fq), inp.api_us, &mut dbs);
    r.doorbells.append(&mut dbs);
    r.proc = Some(ProcUp { doorbell: DOORBELL_DW, gang_ctx: p.gang_ctx, mes: inp.mes, answered: res.is_ok() });
    match res {
        Ok((us, _)) => {
            klog::event!(Info, "gx", "vm.proc_added", doorbell = DOORBELL_DW, pasid = s.pasid);
            Ok((q, format!("ADD_QUEUE (PASID {:#x}, page table {:#x}, VA 0..{:#x}, VM_CONTEXT_CNTL {:#x}, doorbell {DOORBELL_DW:#x}, MQD {:#x}, wptr MC {:#x}) answered in {us} us", s.pasid, s.root, fq.va_end, s.cntl, p.mqd, fq.wptr)))
        }
        Err(e) => {
            klog::event!(Error, "gx", "vm.proc_refused", error = format!("{e:?}").as_str(); "MES did not answer ADD_QUEUE of the process queue");
            Err(format!("ADD_QUEUE not answered: {e:?}"))
        }
    }
}

/// GX-7m3: a queue stalled at its fault: REMOVE_QUEUE, then the queue fresh and ADD_QUEUE (`vmrecover`). MES not
/// answering either leaves it stranded. Once removed it is off the report: the teardown sends no second REMOVE_QUEUE
/// (`add` puts the new queue back on it when it got as far as asking MES).
#[inline(never)]
pub(super) fn again<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &VmInputs, s: &ProcSpec, r: &mut VmReport, d: &mut String) -> Result<Queue, &'static str> {
    let up = r.proc.ok_or("the process queue is not mapped")?;
    let mut lines = Vec::new();
    let removed = remove(owner, &up, inp.api_us, &mut lines);
    for l in &lines {
        d.push_str(l.trim_start_matches("vm teardown: "));
        d.push_str("; ");
    }
    if !removed {
        r.stranded = true;
        r.proc = Some(ProcUp { answered: false, ..up });
        return Err("MES did not remove the queue");
    }
    r.proc = None;
    if let Some(a) = inp.addrs.hqd {
        super::vmrecover::cpc_release(owner, &a, d);
    }
    match add(owner, inp, s, r) {
        Ok((q, line)) => {
            d.push_str(&line);
            d.push_str("; ");
            Ok(Queue::Pm4 { q, gfx: false })
        }
        Err(e) => {
            d.push_str(&e);
            d.push_str("; ");
            if r.proc.is_some() {
                r.stranded = true;
                d.push_str("STRANDED; ");
            }
            klog::event!(Error, "gx", "vm.proc_again_failed", stranded = r.proc.is_some(); "the process queue was removed and not added again");
            Err("the queue was not added again")
        }
    }
}

/// The VMID MES gave the queue after a recovery: its ADD_QUEUE may have picked another one.
pub(super) fn refind(hw: &mut dyn Hw, c: &Ctx, r: &mut VmReport) {
    let Some(s) = c.proc else { return };
    let vmid = find_vmid(hw, c.a, s.root);
    r.proc_vmid = vmid;
    r.proc_lut = vmid.and_then(|n| vmring::rd_reg(hw, c.a.lut(n)));
}

/// The process queue's rungs (see the module text).
pub(super) fn run<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &VmInputs, root: u64, pasid: u32, pd: u64, r: &mut VmReport) -> Result<(), VmError> {
    let a = inp.addrs;
    let base = inp.carve.mc_base() + inp.mes_bo.offset;
    let p = ProcAddrs::new(base);
    let names = Q::Proc.rungs();
    let cntl_b = a.hub.byte(R::Ctx(1)).ok_or(VmError::NoAddress)?;
    let cntl = owner.reg_read(cntl_b).filter(|&v| v != u32::MAX).ok_or(VmError::Read { byte: cntl_b })?;
    let spec = ProcSpec { root, pasid, cntl };
    // 1. ADD_QUEUE, then a ring test (a fence to the queue page's VA).
    let (q, add) = match add(owner, inp, &spec, r) {
        Ok(x) => x,
        Err(why) => {
            r.rungs.push(Rung { name: names[0], pass: false, done: false, wptr: 0, rptr: None, fence: None, polls: 0, us: inp.api_us, detail: why });
            return Err(VmError::ProcRefused);
        }
    };
    let fence_mc = p.page_mc + FENCE;
    let fence_va = p.page_va + FENCE;
    let mut t = Timeline::new(0, 16);
    let s = t.emit().map_err(|_| VmError::Ring(RingError::State))?;
    mes::mem(owner, fence_mc, &[0; 8]).map_err(vmring::mem_err)?;
    owner.store_barrier();
    let mut queue = Queue::Pm4 { q, gfx: false };
    let w = queue.submit(owner, 16, |g| g.emit_fence(fence_va, s, false, false), &mut r.doorbells)?;
    let mut x = queue.wait(owner, inp.timeout_us, names[0], w, &mut |hw| mes::rd32(hw, fence_mc) == Some(s as u32));
    x.fence = mes::rd32(owner, fence_mc).map(u64::from);
    let vmid = find_vmid(owner, &a, root);
    let lut = vmid.and_then(|n| vmring::rd_reg(owner, a.lut(n)));
    r.proc_vmid = vmid;
    r.proc_lut = lut;
    x.detail = format!(
        "{add}; ring test: fence {s} to VA {fence_va:#x} read {}; MES's context holding the root: {} (IH LUT there {})",
        hx(x.fence),
        vmid.map_or(String::from("none of 8-15 reads it"), |n| format!("VMID {n}")),
        hx32(lut)
    );
    vmring::push(r, x)?;
    // 2-4. In the VM: an IB, the intended fault (a MES VMID), an IB after it.
    let i = 3u64;
    let ctx = Ctx {
        a: &a,
        vmid: vmid.unwrap_or(0),
        pd,
        pasid,
        ib_mc: base + lay::DATA + i * 0x2000,
        target_mc: base + lay::DATA + i * 0x2000 + 0x1000,
        fence: fence_mc,
        fence_gpu: fence_va,
        us: inp.timeout_us,
        ack: inp.ack,
        mes: true,
        inp,
        proc: Some(spec),
    };
    const _: () = assert!(lay::DATA + 3 * 0x2000 + 0x2000 <= lay::PROC_RING);
    vmring::vm_rungs(owner, &mut queue, Q::Proc, &ctx, r, &mut t)
}

/// REMOVE_QUEUE of the process queue (bounded by `api_us`). `false`: not removed (MES did not answer it, or never
/// answered the ADD_QUEUE): the queue's HQD may stay active.
pub(super) fn remove<H: Hw>(owner: &mut Bringup<Watch<H>>, p: &ProcUp, api_us: u64, lines: &mut Vec<String>) -> bool {
    if !p.answered {
        lines.push(String::from("vm teardown: REMOVE_QUEUE of the MES process queue not attempted (MES did not answer its ADD_QUEUE): STRANDED"));
        klog::event!(Error, "gx", "vm.proc_stranded", answered = false);
        return false;
    }
    let mut ring = sched_ring(owner, &p.mes);
    let mut dbs = Vec::new();
    let res = ring.submit(owner, &p.mes, Frame::remove_process_queue(p.doorbell, p.gang_ctx), api_us, &mut dbs);
    let ok = res.is_ok();
    klog::event!(Info, "gx", "vm.proc_removed", ok = ok, doorbell = p.doorbell);
    lines.push(format!("vm teardown: MES REMOVE_QUEUE of the process queue (doorbell {:#x}, gang context {:#x}): {}", p.doorbell, p.gang_ctx, match &res {
        Ok((us, _)) => format!("answered in {us} us"),
        Err(e) => format!("FAILED {e:?}: STRANDED"),
    }));
    ok
}

/// The process queue's lines (before the rung lines).
pub fn render(r: &VmReport) -> Vec<String> {
    let mut v = Vec::new();
    if r.proc.is_some() {
        v.push(format!(
            "vm process queue: compute, doorbell {DOORBELL_DW:#x}, ring at VA {:#x}; MES gave it {} (IH LUT {})",
            ProcAddrs::new(0).ring_va,
            r.proc_vmid.map_or(String::from("a VMID the CPU did not find in 8-15"), |n| format!("VMID {n}")),
            hx32(r.proc_lut)
        ));
    }
    v
}

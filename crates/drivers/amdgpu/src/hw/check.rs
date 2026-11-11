//! GX-8a: the proof a bring-up runs before the render node appears, the same on the chip and on the fake chain: an
//! object per kind of memory, a VM, every page mapped and walked with GR-1's software walker through the CPU windows,
//! the CPU's view of the objects (wiped, written, read back), a CLEAR with its holes, everything given back. The GPU does
//! not see any of it. Leaves nothing behind on success or failure.
//!
//! GX-8b: `jobs`, once the job half runs: one job per ring through `GpuDevice::submit`, in a VM with its own VMID,
//! from an IB in VRAM into a GTT object and into VRAM: the first time the GPU walks tables this driver wrote.

use super::HardwareGpu;
use super::frame::RingType;
use super::vm::HwVm;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu_fmt::pte::{Mtype, Target};
use amdgpu_fmt::walk::Walk;
use amdgpu_fmt::pm4;
use core::sync::atomic::{AtomicBool, Ordering};
use drm::dma_buf::Exporter;
use drm::fence::{self, Fence};
use drm::gpu::{BoPlace, GpuDevice, GpuVm, PtOp};
use drm::job::{Entity, Job, JobIb, ResetSnaps, SchedPriority, Submission};
use drm::uapi::*;
use mm::dma_pages::PinOwner;

const PAGE: u64 = 4096;
/// Where the check maps (nothing else uses this VM).
const AT: u64 = 1 << 34;
const RW: u32 = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE;
/// A pin share large enough for what the check takes.
const PIN_SHARE: u64 = 50;
/// What the proof's jobs write ("JOB0" and up).
const JOB_VALUE: u32 = 0x4a4f_4230;

/// One mapping of the check.
struct Case {
    what: &'static str,
    mem: Arc<dyn Exporter>,
    place: BoPlace,
    uswc: bool,
    flags: u32,
    /// The walk's target and MTYPE.
    target: Target,
    mtype: Mtype,
}

/// `Ok(lines)`: all as it must be.
pub fn run(g: &HardwareGpu) -> Result<Vec<String>, String> {
    let before = g.heaps().bytes();
    let gtt_before = g.memory().gtt_usage;
    let r = checked(g);
    let after = g.heaps().bytes();
    let gtt_after = g.memory().gtt_usage;
    let mut lines = r?;
    if after.user_used != before.user_used || after.table_used != before.table_used || gtt_after != gtt_before {
        return Err(format!("not everything came back: {before:?} GTT {gtt_before} before, {after:?} GTT {gtt_after} after"));
    }
    lines.push(format!("backend check: everything given back ({} MiB of user slabs, {} MiB of table slabs stay with the heaps)", after.user_slabs >> 20, after.table_slabs >> 20));
    Ok(lines)
}

fn checked(g: &HardwareGpu) -> Result<Vec<String>, String> {
    let e = |what: &'static str| move |x: i64| format!("{what}: errno {x}");
    let mut lines = Vec::new();
    let owner = PinOwner::new(PIN_SHARE);
    let vram = g.bo_memory(BoPlace::Vram, 2048 + 16, 1, false, &owner).map_err(e("VRAM object"))?;
    let gtt = g.bo_memory(BoPlace::Gtt, 64, 1, false, &owner).map_err(e("GTT object"))?;
    let wc = g.bo_memory(BoPlace::Gtt, 8, 1, true, &owner).map_err(e("write-combined GTT object"))?;
    // The CPU's view of the carve-out: wiped, written, read back, at both ends (two windows' worth of pages).
    for off in [0, vram.size() - 8] {
        let mut b = [0xffu8; 8];
        if !vram.read(off, &mut b) || b != [0; 8] {
            return Err(format!("VRAM object at {off:#x} reads {b:x?}, not wiped"));
        }
        let pat = (off | 0x5a5a_0000_0000_0001).to_le_bytes();
        if !vram.write(off, &pat) || !vram.read(off, &mut b) || b != pat {
            return Err(format!("VRAM object at {off:#x}: wrote {pat:x?}, read {b:x?}"));
        }
    }
    lines.push(format!("backend check: VRAM object of {} KiB wiped, written and read back through its window", vram.size() >> 10));
    // More than the cushion: its slab goes back, so the window's unmap on every CPU runs here first, not under a program.
    let big_bytes = g.heaps().reserve() + super::heap::SLAB;
    let big = g.bo_memory(BoPlace::Vram, big_bytes / PAGE, 1, false, &owner).map_err(e("large VRAM object"))?;
    let mut b = [0xffu8; 8];
    if !big.read(big_bytes - 8, &mut b) || b != [0; 8] {
        return Err(format!("large VRAM object reads {b:x?} at its end, not wiped"));
    }
    let grown = g.heaps().bytes().user_slabs;
    drop(big);
    let back = g.heaps().bytes().user_slabs;
    if back >= grown {
        return Err(format!("the slab of a large VRAM object stayed: {back:#x} bytes of user slabs, {grown:#x} with it"));
    }
    lines.push(format!("backend check: VRAM object of {} MiB wiped; its slab went back ({} MiB of user slabs stay)", big_bytes >> 20, back >> 20));
    let vm = g.new_vm(0).map_err(e("VM"))?;
    let cases = [
        Case { what: "VRAM", mem: vram.clone(), place: BoPlace::Vram, uswc: false, flags: RW, target: Target::Local, mtype: Mtype::Nc },
        Case { what: "GTT", mem: gtt.clone(), place: BoPlace::Gtt, uswc: false, flags: RW | AMDGPU_VM_PAGE_EXECUTABLE, target: Target::System { snooped: true }, mtype: Mtype::Nc },
        Case { what: "GTT USWC", mem: wc.clone(), place: BoPlace::Gtt, uswc: true, flags: AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_MTYPE_UC, target: Target::System { snooped: false }, mtype: Mtype::Uc },
    ];
    let mut at = AT;
    for c in &cases {
        let pages = c.mem.size() / PAGE;
        let op = PtOp::Map { va: at, pages, mem: c.mem.clone(), first: 0, place: Some(c.place), uswc: c.uswc, flags: c.flags };
        let r = vm.update(core::slice::from_ref(&op));
        if let Some(err) = r.error {
            return Err(format!("{} map: errno {err}", c.what));
        }
        walk_all(&vm, c, at)?;
        lines.push(format!("backend check: {}: {pages} pages walk to their frames", c.what));
        at = (at + pages * PAGE).next_multiple_of(2 << 20);
    }
    if let Some(err) = vm.update(&[PtOp::Prt { va: at, pages: 512 + 8, flags: AMDGPU_VM_PAGE_PRT }]).error {
        return Err(format!("PRT map: errno {err}"));
    }
    if !matches!(vm.walk(at + (512 + 7) * PAGE), Ok(Walk::Prt { .. })) {
        return Err(String::from("PRT range does not walk as PRT"));
    }
    // A CLEAR in the middle of the first mapping: a hole, the rest stays; the memory stays until it ran.
    if let Some(err) = vm.update(&[PtOp::Clear { va: AT + PAGE, pages: 2 }]).error {
        return Err(format!("clear: errno {err}"));
    }
    if !matches!(vm.walk(AT + PAGE), Ok(Walk::Hole { .. })) || !matches!(vm.walk(AT + 3 * PAGE), Ok(Walk::Page(_))) {
        return Err(String::from("CLEAR left the wrong walk"));
    }
    lines.push(format!("backend check: PRT and CLEAR walk as they must; {:?}; PASID {}", vm.counts(), vm.pasid()));
    drop(vm);
    Ok(lines)
}

/// How long the proof waits for a job: the longest ring's timeout and the time to tell.
const JOB_WAIT_NS: u64 = 70_000_000_000;

/// A job's IB: the proof's value into `va`, as the ring's kind writes (SDMA's IB in 32-byte units).
fn write_ib(ty: RingType, va: u64, value: u32) -> Result<Vec<u32>, String> {
    let mut v = Vec::new();
    let r = match ty {
        RingType::Sdma => amdgpu_fmt::sdma::write_linear(&mut v, va, &[value]),
        _ => amdgpu_fmt::pm4::write_data(&mut v, amdgpu_fmt::pm4::dst_sel::MEM, va, amdgpu_fmt::pm4::WR_CONFIRM, &[value]),
    };
    r.map_err(|e| format!("IB: {e:?}"))?;
    if ty == RingType::Sdma {
        v.resize(v.len().next_multiple_of(8), 0);
    }
    Ok(v)
}

/// `Ok(lines)`: a job ran on each ring and its writes are in memory.
pub fn jobs(g: &HardwareGpu) -> Result<Vec<String>, String> {
    let before = g.heaps().bytes();
    let gtt_before = g.memory().gtt_usage;
    let lines = jobs_checked(g)?;
    let (after, gtt_after) = (g.heaps().bytes(), g.memory().gtt_usage);
    if after.user_used != before.user_used || after.table_used != before.table_used || gtt_after != gtt_before {
        return Err(format!("the jobs' memory did not come back: {before:?} GTT {gtt_before} before, {after:?} GTT {gtt_after} after"));
    }
    Ok(lines)
}

fn jobs_checked(g: &HardwareGpu) -> Result<Vec<String>, String> {
    let e = |what: &'static str| move |x: i64| format!("{what}: errno {x}");
    let owner = PinOwner::new(PIN_SHARE);
    let code = g.bo_memory(BoPlace::Vram, 1, 1, false, &owner).map_err(e("IB object"))?;
    let gtt = g.bo_memory(BoPlace::Gtt, 1, 1, false, &owner).map_err(e("GTT object"))?;
    let vram = g.bo_memory(BoPlace::Vram, 1, 1, false, &owner).map_err(e("VRAM object"))?;
    let vm = g.new_vm(0).map_err(e("VM"))?;
    let at = |n: u64| AT + n * (2 << 20);
    let map = |n: u64, m: &Arc<dyn Exporter>, place, flags| PtOp::Map { va: at(n), pages: 1, mem: m.clone(), first: 0, place: Some(place), uswc: false, flags };
    let ops = [map(0, &code, BoPlace::Vram, RW | AMDGPU_VM_PAGE_EXECUTABLE), map(1, &gtt, BoPlace::Gtt, RW), map(2, &vram, BoPlace::Vram, RW)];
    if let Some(err) = vm.update(&ops).error {
        return Err(format!("map: errno {err}"));
    }
    let mut lines = Vec::new();
    let mut run = |ty: RingType, slot: u64, to_vram: bool| -> Result<(), String> {
        let value = JOB_VALUE + slot as u32;
        let (target, mem, name) = if to_vram { (at(2), &vram, "VRAM") } else { (at(1), &gtt, "GTT") };
        let ib_at = slot * 64;
        let words = write_ib(ty, target + slot * 8, value)?;
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        if !code.write(ib_at, &bytes) {
            return Err(String::from("the IB did not reach VRAM"));
        }
        let entity = Entity::new(ty.ip(), 0, SchedPriority::Normal, Arc::new(AtomicBool::new(false)), Arc::new(ResetSnaps::new(g.reset_counter())));
        let ctx = fence::context_alloc(2);
        let scheduled = Fence::new(ctx, 1, "amdgpu", "check");
        let finished = Fence::new(ctx + 1, 1, "amdgpu", "check");
        let job = Job { entity, ibs: alloc::vec![JobIb { va: at(0) + ib_at, len_dw: words.len() as u32, flags: 0 }], shadow: None, scheduled, finished: finished.clone() };
        let sub = Submission { jobs: alloc::vec![job], vm: vm.clone(), deps: Vec::new(), user_fence: None, bos: Vec::new(), generation: g.vram_lost_counter() };
        g.submit(sub).map_err(|x| format!("{}: submit: errno {x}", ty.name()))?;
        g.kick();
        if !wait(g, &finished, JOB_WAIT_NS) {
            return Err(format!("{}: no end in {} s", ty.name(), JOB_WAIT_NS / 1_000_000_000));
        }
        let st = fence::status(&finished);
        if st < 0 {
            return Err(format!("{}: the job ended with errno {}", ty.name(), -st));
        }
        let mut b = [0u8; 4];
        if !mem.read(slot * 8, &mut b) || u32::from_le_bytes(b) != value {
            return Err(format!("{}: {name} at {:#x} reads {:#x}, the IB wrote {value:#x}", ty.name(), target + slot * 8, u32::from_le_bytes(b)));
        }
        lines.push(format!("backend check: {} ran a job in the VM: an IB in VRAM wrote {name} ({value:#x} read back)", ty.name()));
        Ok(())
    };
    let cases = [(RingType::Gfx, false), (RingType::Compute, false), (RingType::Sdma, false), (RingType::Gfx, true), (RingType::Compute, true), (RingType::Sdma, true)];
    for (n, (ty, to_vram)) in cases.iter().enumerate() {
        run(*ty, n as u64, *to_vram)?;
    }
    // The last job's hold goes just after its fence.
    let t0 = super::clock_ns();
    while Arc::strong_count(&vm) > 1 && super::clock_ns().saturating_sub(t0) < 1_000_000_000 {
        nap(g);
    }
    if let Some(f) = vm.last_fault() {
        return Err(format!("the jobs faulted: {f:?}"));
    }
    drop(vm);
    Ok(lines)
}

/// A moment for the job thread (host tests have none: a round here).
fn nap(g: &HardwareGpu) {
    #[cfg(not(test))]
    {
        let _ = g;
        sched::sleep_ns(100_000);
    }
    #[cfg(test)]
    g.kick();
}

/// GX-7n: what a job that waits costs: a job on the GFX ring waits for a word in GTT `hold_ns` long, once with the thread on
/// interrupts and once polling, and the thread's rounds and CPU time are read around that. Nothing else should run.
/// `nudge`: after the CPU's write (the chip's WAIT_REG_MEM sees it by itself; a fake engine must be asked).
pub fn hold_load(g: &HardwareGpu, hold_ns: u64, nudge: &dyn Fn()) -> Result<Vec<String>, String> {
    let jobs = g.jobs().ok_or("the backend has no job half")?;
    let feed = jobs.feed().cloned();
    let was = feed.as_ref().is_some_and(|f| f.force_poll.load(Ordering::Acquire));
    let r = hold_each(g, &jobs, feed.as_deref(), hold_ns, nudge);
    if let Some(f) = &feed {
        f.force_poll.store(was, Ordering::Release);
    }
    r
}

fn hold_each(g: &HardwareGpu, jobs: &super::jobs::Jobs, feed: Option<&super::ih::IrqFeed>, hold_ns: u64, nudge: &dyn Fn()) -> Result<Vec<String>, String> {
    let e = |what: &'static str| move |x: i64| format!("{what}: errno {x}");
    let owner = PinOwner::new(PIN_SHARE);
    let code = g.bo_memory(BoPlace::Vram, 1, 1, false, &owner).map_err(e("IB object"))?;
    let word = g.bo_memory(BoPlace::Gtt, 1, 1, false, &owner).map_err(e("GTT object"))?;
    let vm = g.new_vm(0).map_err(e("VM"))?;
    let at = |n: u64| AT + n * (2 << 20);
    let map = |n: u64, m: &Arc<dyn Exporter>, place, flags| PtOp::Map { va: at(n), pages: 1, mem: m.clone(), first: 0, place: Some(place), uswc: false, flags };
    if let Some(err) = vm.update(&[map(0, &code, BoPlace::Vram, RW | AMDGPU_VM_PAGE_EXECUTABLE), map(1, &word, BoPlace::Gtt, RW)]).error {
        return Err(format!("map: errno {err}"));
    }
    let mut lines = Vec::new();
    for (mode, poll) in [("on interrupts", false), ("polling", true)] {
        match feed {
            Some(f) => f.force_poll.store(poll, Ordering::Release),
            None if poll => {}
            None => {
                lines.push(String::from("irq_load: no IH ring: only the polling half"));
                continue;
            }
        }
        if !word.write(0, &0u32.to_le_bytes()) {
            return Err(String::from("the word did not reach GTT"));
        }
        let mut words = Vec::new();
        let w = pm4::WaitRegMem { function: 3, mem: true, operation: 0, engine: 0, addr0: at(1) as u32, addr1: (at(1) >> 32) as u32, reference: JOB_VALUE, mask: u32::MAX, interval: 4 };
        pm4::wait_reg_mem(&mut words, &w).map_err(|x| format!("{x:?}"))?;
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        if !code.write(0, &bytes) {
            return Err(String::from("the IB did not reach VRAM"));
        }
        let entity = Entity::new(RingType::Gfx.ip(), 0, SchedPriority::Normal, Arc::new(AtomicBool::new(false)), Arc::new(ResetSnaps::new(g.reset_counter())));
        let ctx = fence::context_alloc(2);
        let scheduled = Fence::new(ctx, 1, "amdgpu", "check");
        let finished = Fence::new(ctx + 1, 1, "amdgpu", "check");
        let job = Job { entity, ibs: alloc::vec![JobIb { va: at(0), len_dw: words.len() as u32, flags: 0 }], shadow: None, scheduled: scheduled.clone(), finished: finished.clone() };
        let sub = Submission { jobs: alloc::vec![job], vm: vm.clone(), deps: Vec::new(), user_fence: None, bos: Vec::new(), generation: g.vram_lost_counter() };
        g.submit(sub).map_err(|x| format!("submit: errno {x}"))?;
        g.kick();
        if !wait(g, &scheduled, JOB_WAIT_NS) {
            return Err(String::from("the job never reached the ring"));
        }
        let (r0, c0) = (jobs.shared.rounds.load(Ordering::Acquire), jobs.thread_cpu_ns());
        hold(g, hold_ns);
        let (r1, c1) = (jobs.shared.rounds.load(Ordering::Acquire), jobs.thread_cpu_ns());
        if !word.write(0, &JOB_VALUE.to_le_bytes()) {
            return Err(String::from("the word did not reach GTT"));
        }
        nudge();
        if !wait(g, &finished, JOB_WAIT_NS) {
            return Err(format!("{mode}: the job did not end after its word came"));
        }
        let cpu = match (c0, c1) {
            (Some(a), Some(b)) => format!("{} us", b.saturating_sub(a) / 1000),
            _ => String::from("not readable"),
        };
        lines.push(format!("irq_load: a job waiting {} ms, the job thread {mode}: {} rounds, {cpu} of its CPU time", hold_ns / 1_000_000, r1.saturating_sub(r0)));
    }
    Ok(lines)
}

/// `ns` for the job thread; host tests have none, so they do its rounds here.
fn hold(g: &HardwareGpu, ns: u64) {
    #[cfg(test)]
    {
        let _ = ns;
        g.kick();
    }
    #[cfg(not(test))]
    {
        let _ = g;
        sched::sleep_ns(ns);
    }
}

fn wait(g: &HardwareGpu, f: &Arc<Fence>, ns: u64) -> bool {
    let t0 = super::clock_ns();
    while !fence::is_signaled(f) {
        if super::clock_ns().saturating_sub(t0) > ns {
            return false;
        }
        nap(g);
    }
    true
}

fn walk_all(vm: &Arc<HwVm>, c: &Case, at: u64) -> Result<(), String> {
    let write = c.flags & AMDGPU_VM_PAGE_WRITEABLE != 0;
    let exec = c.flags & AMDGPU_VM_PAGE_EXECUTABLE != 0;
    for k in 0..c.mem.size() / PAGE {
        let want = c.mem.page_phys(k).ok_or("no frame")?;
        match vm.walk(at + k * PAGE + 0x18) {
            Ok(Walk::Page(l)) if l.phys == want + 0x18 && l.target == c.target && l.flags.read && l.flags.write == write && l.flags.exec == exec && l.flags.mtype == c.mtype => {}
            w => return Err(format!("{} page {k} at {:#x}: {w:?}, want frame {want:#x}", c.what, at + k * PAGE)),
        }
    }
    Ok(())
}

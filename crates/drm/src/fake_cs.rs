//! DR-8: the fake GPU's job queue. Submissions wait until their dependencies signaled and the earlier jobs of
//! their entities ran, then the highest scheduler priority goes first (the oldest among equals). A gang's IBs run
//! in amdgpu-fmt's interpreters (PM4 for GFX and compute, SDMA 6.0 for DMA), its members taking turns, on a bus
//! over the client VM's fake translations, so a job reads and writes the objects' memory as the GPU would.
//!
//! The queue runs on whichever thread made it ready (the CS in `kick` once it dropped its locks, or the signal of
//! a dependency): one thread pumps at a time and the others leave it their work, so signals inside a job never
//! nest, and `submit` only queues (it is called under the CS locks). Every job is bounded: a run
//! fetches at most `LIMITS.max_dw` dwords, a job at most `JOB_DW`, and members that only wait give up after
//! `STALL_ROUNDS` turns. A hang or a timeout ends the job with ETIME, makes its context guilty and counts a reset,
//! as Linux's job timeout and recovery do (amdgpu_job.c:34).

use crate::fake_vm::{FakeVm, Held};
use crate::fence::{self, FenceCb, Registration};
use crate::job::{Job, SchedPriority, Submission};
use crate::uapi::*;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use amdgpu_fmt::bus::{GpuBus, MemFault};
use amdgpu_fmt::sim::{Engine, Hang, Isa, Limits, Stop};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use ksync::SpinLock;
use linux::errno::*;
use linux::ab::errno_more::ECANCELED;
use vfs::Errno;

/// One interpreter run: dwords fetched, bytes moved, IB nesting (ring -> IB -> IB2 on the CP).
const LIMITS: Limits = Limits { max_dw: 1 << 16, max_bytes: 1 << 24, max_ib_depth: 2 };
/// Dwords one job may fetch in all (Linux's 10 s job timeout stands behind it on hardware).
const JOB_DW: u64 = 1 << 26;
/// Turns in a row in which every member only waited before the job counts as hung.
const STALL_ROUNDS: u32 = 4096;
/// The VMID the fake gives client jobs (one address space per job: the bus is the job's VM).
const VMID: u8 = 1;

struct Pending {
    sub: Submission,
    prio: SchedPriority,
    _regs: Vec<Registration>,
}

/// The queue and the counters the fake's recovery moves.
pub struct FakeSched {
    me: Weak<FakeSched>,
    pending: SpinLock<Vec<Pending>>,
    pumping: AtomicBool,
    again: AtomicBool,
    pub(crate) reset_counter: AtomicU32,
    pub(crate) vram_lost: AtomicU32,
    jobs_run: AtomicU64,
    hangs: AtomicU64,
    /// WY-21: CONFIG `drm_fake_hang=lose`: a hang ends the device as GX-9a's ladder does when no rung works.
    pub(crate) lose_on_hang: AtomicBool,
    /// The device is lost: new files and submissions answer ENODEV, as on the chip (`hw/mod.rs`, `hw/jobs.rs`).
    pub(crate) lost: AtomicBool,
}

struct Wake(Weak<FakeSched>);

impl FenceCb for Wake {
    fn call(&self) {
        if let Some(s) = self.0.upgrade() {
            s.pump();
        }
    }
}

impl FakeSched {
    pub fn new() -> Arc<FakeSched> {
        Arc::new_cyclic(|me| FakeSched {
            me: me.clone(),
            pending: SpinLock::named("drm.fake.sched", Vec::new()),
            pumping: AtomicBool::new(false),
            again: AtomicBool::new(false),
            reset_counter: AtomicU32::new(0),
            vram_lost: AtomicU32::new(0),
            jobs_run: AtomicU64::new(0),
            hangs: AtomicU64::new(0),
            lose_on_hang: AtomicBool::new(false),
            lost: AtomicBool::new(false),
        })
    }

    /// Jobs (gang members) that ran or were ended, hangs so far, submissions waiting.
    pub fn stats(&self) -> (u64, u64, usize) {
        (self.jobs_run.load(Ordering::Relaxed), self.hangs.load(Ordering::Relaxed), self.pending.lock().len())
    }

    pub(crate) fn submit(&self, sub: Submission) -> Result<(), Errno> {
        if self.lost.load(Ordering::Acquire) {
            sub.cancel(ENODEV);
            return Err(ENODEV);
        }
        let prio = sub.jobs.iter().map(|j| j.entity.priority).max().unwrap_or(SchedPriority::Normal);
        let mut regs = Vec::new();
        if regs.try_reserve_exact(sub.deps.len()).is_err() {
            sub.cancel(ENOMEM);
            return Err(ENOMEM);
        }
        let wake: Arc<dyn FenceCb> = Arc::new(Wake(self.me.clone()));
        for d in &sub.deps {
            if let Some(r) = fence::add_callback(d, wake.clone()) {
                regs.push(r);
            }
        }
        {
            let mut q = self.pending.lock();
            if q.try_reserve(1).is_err() {
                drop(q);
                sub.cancel(ENOMEM);
                return Err(ENOMEM);
            }
            q.push(Pending { sub, prio, _regs: regs });
        }
        Ok(())
    }

    /// Runs what is ready; a second thread leaves its turn to the one already pumping.
    pub(crate) fn pump(&self) {
        loop {
            if self.pumping.swap(true, Ordering::AcqRel) {
                self.again.store(true, Ordering::Release);
                return;
            }
            while let Some(p) = self.pick() {
                self.run(p.sub);
            }
            self.pumping.store(false, Ordering::Release);
            if !self.again.swap(false, Ordering::AcqRel) {
                return;
            }
        }
    }

    /// The ready submission with the highest priority: its dependencies signaled and no earlier submission
    /// still holds a job of one of its entities.
    fn pick(&self) -> Option<Pending> {
        let mut q = self.pending.lock();
        let mut best: Option<usize> = None;
        for (i, p) in q.iter().enumerate() {
            if !p.sub.deps.iter().all(fence::is_signaled) {
                continue;
            }
            let blocked = q[..i].iter().any(|e| e.sub.jobs.iter().any(|a| p.sub.jobs.iter().any(|b| a.entity.id() == b.entity.id())));
            if blocked {
                continue;
            }
            if best.is_none_or(|b| q[b].prio < p.prio) {
                best = Some(i);
            }
        }
        best.map(|i| q.remove(i))
    }

    fn run(&self, sub: Submission) {
        let Some(vm) = sub.vm.as_fake() else {
            // A configuration mismatch, not a fault of the program: no guilty context, no reset.
            klog::event!(Error, "drm", "fake.cs_no_vm", jobs = sub.jobs.len(); "the submission's VM is not the fake's: its jobs end with ENODEV");
            sub.cancel(ENODEV);
            return;
        };
        let generation = self.vram_lost.load(Ordering::Acquire);
        let refused: Vec<Option<Errno>> =
            sub.jobs.iter().map(|j| j.entity.refused().or((sub.generation != generation).then_some(ECANCELED))).collect();
        for j in &sub.jobs {
            j.scheduled.signal();
        }
        self.jobs_run.fetch_add(sub.jobs.len() as u64, Ordering::Relaxed);
        let leader_ran = refused.last().is_some_and(|r| r.is_none());
        let r = exec(vm, &sub.jobs, &refused);
        match r {
            Ok(()) => {
                if leader_ran && let Some(uf) = &sub.user_fence && !uf.mem.write(uf.offset, &uf.value.to_le_bytes()) {
                    klog::event!(Warn, "drm", "fake.user_fence_failed", offset = uf.offset);
                }
                for (j, e) in sub.jobs.iter().zip(&refused) {
                    match e {
                        Some(e) => {
                            klog::event!(Info, "drm", "fake.cs_skipped", ip = j.entity.ip_type, errno = *e; "a job of a guilty, ended or VRAM-lost context ends unrun");
                            j.finished.signal_error(*e as i32);
                        }
                        None => {
                            j.finished.signal();
                        }
                    }
                }
            }
            Err(f) => {
                self.hangs.fetch_add(1, Ordering::Relaxed);
                self.reset_counter.fetch_add(1, Ordering::AcqRel);
                let what = alloc::format!("{f:?}");
                let engine_errno = match &f {
                    Fail::Hang(h) => h.errno(),
                    _ => 0,
                };
                klog::event!(Warn, "drm", "fake.cs_hang", why = what.as_str(), engine_errno = engine_errno; "the job ends with ETIME, its context is guilty, the fake counts a reset");
                if self.lose_on_hang.load(Ordering::Acquire) && !self.lost.swap(true, Ordering::AcqRel) {
                    klog::event!(Error, "drm", "fake.device_lost"; "drm_fake_hang=lose: new files and submissions answer ENODEV until the next boot");
                }
                for (j, e) in sub.jobs.iter().zip(&refused) {
                    if e.is_none() {
                        j.entity.guilty.store(true, Ordering::Release);
                    }
                    j.finished.signal_error(e.unwrap_or(ETIME) as i32);
                }
            }
        }
        drop(sub);
    }
}

impl Drop for FakeSched {
    /// A dropped device ends what still waits (Mesa: device lost), so no waiter hangs on it.
    fn drop(&mut self) {
        let q = core::mem::take(self.pending.get_mut());
        if !q.is_empty() {
            klog::event!(Info, "drm", "fake.cs_cancelled", submissions = q.len(); "a dropped device ends its queued jobs");
        }
        for p in q {
            p.sub.cancel(ECANCELED);
        }
    }
}

#[derive(Debug)]
enum Fail {
    Hang(Hang),
    Stalled,
    Budget,
}

/// One gang member while it runs.
struct Part<'a> {
    job: &'a Job,
    engine: Option<Engine>,
    next: usize,
    open: bool,
    done: bool,
}

fn isa(ip: u32) -> Option<Isa> {
    match ip {
        AMDGPU_HW_IP_GFX | AMDGPU_HW_IP_COMPUTE => Some(Isa::Pm4),
        AMDGPU_HW_IP_DMA => Some(Isa::Sdma),
        _ => None,
    }
}

/// The members' IBs, taking turns (a gang's members may wait for each other through memory).
fn exec(vm: &FakeVm, jobs: &[Job], refused: &[Option<Errno>]) -> Result<(), Fail> {
    let mut bus = VmBus { vm, cid: 0 };
    let mut parts: Vec<Part> = Vec::new();
    for (j, r) in jobs.iter().zip(refused) {
        let limits = Limits { max_ib_depth: if j.entity.ip_type == AMDGPU_HW_IP_DMA { 1 } else { LIMITS.max_ib_depth }, ..LIMITS };
        let mut engine = isa(j.entity.ip_type).and_then(|i| Engine::new(i, 0, 256, 0, limits).ok());
        // TS-2: an INDIRECT_BUFFER a program writes into its own IB reaches the MEC as written (the job's own IBs are
        // the driver's to sign-extend, hw/frame.rs, so `exec_ib` below goes around this check).
        if j.entity.ip_type == AMDGPU_HW_IP_COMPUTE
            && let Some(e) = engine.as_mut()
        {
            e.refuse_non_canonical_ibs();
        }
        if engine.is_none() && r.is_none() && !j.ibs.is_empty() {
            klog::event!(Debug, "drm", "fake.cs_no_engine", ip = j.entity.ip_type; "the fake has no interpreter for this IP: its IBs count as done");
        }
        let done = r.is_some() || engine.is_none();
        parts.push(Part { job: j, engine, next: 0, open: false, done });
    }
    let (mut fetched, mut stalls) = (0u64, 0u32);
    loop {
        let mut progress = false;
        for p in parts.iter_mut().filter(|p| !p.done) {
            let Some(engine) = p.engine.as_mut() else { continue };
            bus.cid = client_id(p.job.entity.ip_type);
            let report = if p.open {
                let w = engine.rptr();
                engine.run(&mut bus, w)
            } else {
                let Some(ib) = p.job.ibs.get(p.next) else {
                    p.done = true;
                    continue;
                };
                p.next += 1;
                p.open = true;
                engine.exec_ib(&mut bus, VMID, ib.va, ib.len_dw as u64)
            };
            fetched += report.fetched_dw;
            match report.stop {
                Stop::Idle | Stop::Incomplete { .. } => {
                    p.open = false;
                    progress = true;
                }
                Stop::Budget => progress = true,
                Stop::Wait(_) => {}
                Stop::Hang(h) => return Err(Fail::Hang(h)),
            }
        }
        if parts.iter().all(|p| p.done) {
            return Ok(());
        }
        if fetched > JOB_DW {
            return Err(Fail::Budget);
        }
        stalls = if progress { 0 } else { stalls + 1 };
        if stalls > STALL_ROUNDS {
            return Err(Fail::Stalled);
        }
    }
}

/// The client VM as the engines see it: the fake's translations, the uAPI's page flags (read needs READABLE,
/// write WRITEABLE, a command fetch READABLE and EXECUTABLE as on the 780M, CR-GX7m2-2), PRT ranges read zero
/// and drop writes.
struct VmBus<'a> {
    vm: &'a FakeVm,
    /// The GFX hub client of the engine running now (fault status CID).
    cid: u32,
}

/// The GFX hub's client id of an engine (gfxhub_v3_0.c's `gfxhub_client_ids`: CPG 6, CPC 5, SDMA0 13): what
/// the fake puts in a fault's status.
fn client_id(ip: u32) -> u32 {
    match ip {
        AMDGPU_HW_IP_COMPUTE => 5,
        AMDGPU_HW_IP_DMA => 13,
        _ => 6,
    }
}

/// DR-9: the fault status the fake reports (GCVM_L2_PROTECTION_FAULT_STATUS of GFX 11): a hole is an invalid
/// PTE (PERMISSION_FAULTS bit 0) with MAPPING_ERROR; a page without the right gives PERMISSION_FAULTS bit 1
/// (read) or 2 (write); a command fetch of a page without EXECUTABLE bit 3 (with bit 1 when READABLE is missing
/// too), as the 780M reported it on 29.10. (0x00100880, a read by CPF); the client id, RW and VMID as the hub
/// sets them.
fn fault_status(hole: bool, write: bool, perm_missing: u32, cid: u32, vmid: u8) -> u32 {
    let perm: u32 = if hole { 0x1 } else if write { 0x4 } else { perm_missing };
    (perm << 4) | ((hole as u32) << 8) | ((cid & 0x1ff) << 9) | ((write as u32) << 18) | (((vmid & 0xf) as u32) << 20)
}

impl VmBus<'_> {
    /// `need`: the page flags the access needs (a write WRITEABLE; a read READABLE; a fetch also EXECUTABLE).
    fn access(&mut self, vmid: u8, addr: u64, len: usize, need: u32, mut f: impl FnMut(Option<(&Arc<dyn crate::dma_buf::Exporter>, u64)>, usize, usize) -> bool) -> Result<(), MemFault> {
        let write = need & AMDGPU_VM_PAGE_WRITEABLE != 0;
        let mut done = 0usize;
        while done < len {
            let cur = addr.checked_add(done as u64).ok_or(MemFault { vmid, addr, write })?;
            let n = ((4096 - (cur & 4095)) as usize).min(len - done);
            let fault = MemFault { vmid, addr: cur, write };
            let Some(t) = self.vm.translate(cur) else {
                self.note(cur, true, write, 0, vmid);
                return Err(fault);
            };
            // A PRT range reads zero for a fetch too (assumed, not seen on the 780M).
            let missing = match &t.held {
                Held::Prt => 0,
                Held::Mem { .. } => need & !t.flags,
            };
            let ok = missing == 0
                && match &t.held {
                    Held::Prt => f(None, done, n),
                    Held::Mem { mem, .. } => f(Some((mem, t.page * 4096 + (cur & 4095))), done, n),
                };
            if !ok {
                // PERMISSION_FAULTS: READABLE missing is bit 1, EXECUTABLE bit 3 (the uAPI's bits 1 and 3 too);
                // a refused copy into or out of the backing memory counts as a missing read.
                let perm = if missing == 0 { 0x2 } else { missing & (AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_EXECUTABLE) };
                self.note(cur, false, write, perm, vmid);
                return Err(fault);
            }
            done += n;
        }
        Ok(())
    }

    fn note(&self, va: u64, hole: bool, write: bool, perm_missing: u32, vmid: u8) {
        let addr = va & 0x0000_ffff_ffff_f000;
        let status = fault_status(hole, write, perm_missing, self.cid, vmid);
        self.vm.note_fault(crate::gpu::VmFault { addr, status, vmhub: AMDGPU_VMHUB_TYPE_GFX });
        klog::event!(Info, "drm", "fake.vm_fault", va = klog::Hex(addr), status = klog::Hex(status as u64), write = write);
    }
}

impl GpuBus for VmBus<'_> {
    fn read(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.load(vmid, addr, out, AMDGPU_VM_PAGE_READABLE)
    }
    fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.access(vmid, addr, data.len(), AMDGPU_VM_PAGE_WRITEABLE, |m, off, n| match m {
            None => true,
            Some((mem, at)) => mem.write(at, &data[off..off + n]),
        })
    }
    /// CR-GX7m2-2: ring and IB dwords need EXECUTABLE besides READABLE; a refused fetch is a read fault.
    fn fetch(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.load(vmid, addr, out, AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_EXECUTABLE)
    }
}

impl VmBus<'_> {
    fn load(&mut self, vmid: u8, addr: u64, out: &mut [u8], need: u32) -> Result<(), MemFault> {
        let len = out.len();
        self.access(vmid, addr, len, need, |m, off, n| match m {
            None => {
                out[off..off + n].fill(0);
                true
            }
            Some((mem, at)) => mem.read(at, &mut out[off..off + n]),
        })
    }
}

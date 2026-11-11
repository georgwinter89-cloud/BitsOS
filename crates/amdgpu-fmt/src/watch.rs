//! Hang detection for one ring: the job timeout over the fence timeline, and the dump of a hung ring.
//!
//! Linux's timer runs per scheduler and fires when the oldest job has not finished in time; it starts with the
//! first job and restarts when a job finishes, a later submission does not move it (sched_main.c
//! drm_sched_start_timeout, amdgpu_job.c:34). BitsOS counts read pointer movement as progress too, so a long ring
//! of many small packets is not called hung. Only forward movement inside the submitted window counts: a chip that
//! answers garbage (all ones after a dead link) cannot postpone the verdict, and both kinds of progress are bounded
//! by what was submitted. The pointer is known only modulo the ring, so with more than half the ring in flight a read
//! from behind can look like a big step ahead: such a step counts only when the next sample agrees. A poll (`Stop::Wait`) gets no special case: on the chip a CP waiting forever and a CP
//! stalled at a fault look the same, the timeout decides for both. `Stop::Hang` is a verdict at once: the fakes
//! report it for a bad packet, the hardware backend for the chip's CPC stop at a VM fault (`poll_report`).

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::fence::{FenceError, Observed, Timeline};
use crate::hang::{Dump, Line, decode_ring, decode_stream};
use crate::ring::Kind;
use crate::sim::{self, Isa, Report, Stop};

/// A ring of a [`crate::recover::ResetDomain`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RingId(pub u32);

/// Who a job belongs to. Linux never blames kernel jobs (sched_main.c:1182, `job->vm` at amdgpu_device.c:4744).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Kernel,
    /// `ctx`: the context whose guilty flag is shared by its entities. `entity`: its queue on one ring.
    Client { ctx: u64, entity: u64 },
}

/// What the watch keeps of a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobMeta {
    pub owner: Owner,
    pub vmid: u8,
    /// For the report's process line (amdgpu_job.c:62-66).
    pub pasid: u32,
}

/// A job on the ring, not yet signalled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    pub seq: u64,
    pub meta: JobMeta,
    /// Ring positions (dword counters) of the job's frame.
    pub start: u64,
    pub end: u64,
}

/// Which job timeout applies (amdgpu_device.c:3395-3401).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutClass {
    Gfx,
    Compute,
    Sdma,
    Video,
}

impl TimeoutClass {
    /// Linux's default on bare metal.
    pub fn linux_us(self) -> u64 {
        match self {
            TimeoutClass::Compute => 60_000_000,
            TimeoutClass::Gfx | TimeoutClass::Sdma | TimeoutClass::Video => 10_000_000,
        }
    }
}

/// How one queue can be reset without touching the rest of GC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueReset {
    /// Only the whole of GC can restart it.
    None,
    /// A process queue: MES REMOVE_QUEUE (mes_v11_0.c:219), rebuild, ADD_QUEUE.
    MesProcess { doorbell_dw: u32, gang_ctx: u64 },
    /// A kernel queue MES mapped: REMOVE_QUEUE with unmap_legacy_queue, rebuild, map again.
    MesLegacy { queue_type: u32, pipe: u32, queue: u32, doorbell_dw: u32 },
    /// An SDMA ring stopped and started again (`gx.sdma.restart`). Its pointers may survive: see `Reply::Ring`.
    SdmaRestart,
}

/// The kinds of ring on the 780M, with what each allows (decision table in plan/notes/gr-GR-3.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingClass {
    /// CP_RB0, started by register writes: GC's re-init is its only reset.
    GfxDirect,
    GfxMesLegacy { queue_type: u32, pipe: u32, queue: u32, doorbell_dw: u32 },
    ComputeMesLegacy { queue_type: u32, pipe: u32, queue: u32, doorbell_dw: u32 },
    Sdma { burst_nop: bool },
    GfxProcess { doorbell_dw: u32, gang_ctx: u64 },
    ComputeProcess { doorbell_dw: u32, gang_ctx: u64 },
}

/// One ring's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingCfg {
    pub name: String,
    pub kind: Kind,
    /// Power of two.
    pub size_dw: u64,
    /// The value the ring's pointers start at.
    pub start: u64,
    /// The value the fence slot starts at.
    pub fence_start: u64,
    pub max_in_flight: u64,
    /// Not 0.
    pub timeout_us: u64,
    /// `soft_recovery` exists on this ring: in gfx_v11_0.c only the GFX ring funcs have it (:6128).
    pub kill_waves: bool,
    pub queue: QueueReset,
}

impl RingCfg {
    /// A ring of the 780M with Linux's timeout for its class; the fields stay public for what GX-9a measures.
    pub fn for_780m(name: &str, class: RingClass, size_dw: u64, start: u64, max_in_flight: u64) -> RingCfg {
        let (kind, timeout, kill_waves, queue) = match class {
            RingClass::GfxDirect => (Kind::Gfx, TimeoutClass::Gfx, true, QueueReset::None),
            RingClass::GfxMesLegacy { queue_type, pipe, queue, doorbell_dw } => {
                (Kind::Gfx, TimeoutClass::Gfx, true, QueueReset::MesLegacy { queue_type, pipe, queue, doorbell_dw })
            }
            RingClass::ComputeMesLegacy { queue_type, pipe, queue, doorbell_dw } => {
                (Kind::Gfx, TimeoutClass::Compute, false, QueueReset::MesLegacy { queue_type, pipe, queue, doorbell_dw })
            }
            RingClass::Sdma { burst_nop } => (Kind::Sdma { burst_nop }, TimeoutClass::Sdma, false, QueueReset::SdmaRestart),
            RingClass::GfxProcess { doorbell_dw, gang_ctx } => {
                (Kind::Gfx, TimeoutClass::Gfx, true, QueueReset::MesProcess { doorbell_dw, gang_ctx })
            }
            RingClass::ComputeProcess { doorbell_dw, gang_ctx } => {
                (Kind::Gfx, TimeoutClass::Compute, false, QueueReset::MesProcess { doorbell_dw, gang_ctx })
            }
        };
        RingCfg {
            name: String::from(name),
            kind,
            size_dw,
            start,
            fence_start: 0,
            max_in_flight,
            timeout_us: timeout.linux_us(),
            kill_waves,
            queue,
        }
    }
}

/// What the driver read from the chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// The rptr register or writeback in the hardware's form (bytes on SDMA).
    pub rptr_hw: u64,
    pub fence: u64,
}

/// Why a ring counts as hung.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// Neither the read pointer nor the fence moved for `stalled_us`.
    Timeout { stalled_us: u64, timeout_us: u64 },
    /// The interpreter halted (fakes only).
    Engine(sim::Hang),
}

/// A ring's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingState {
    Idle,
    Waiting { remaining_us: u64 },
    Hung(Cause),
    /// A recovery works on this ring; samples are not judged.
    Recovering,
    /// The whole of GC is being reset.
    InReset,
    /// The device needs a reboot.
    Dead,
}

/// The result of a poll. The caller logs the garbage (B6) and ends the jobs up to `advanced`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Polled {
    pub state: RingState,
    /// The fence moved over (from, to].
    pub advanced: Option<(u64, u64)>,
    /// The slot held a value beyond the last emitted one.
    pub fence_garbage: Option<u64>,
    /// The read pointer lay outside the unconsumed part of the ring.
    pub rptr_garbage: Option<u64>,
}

impl Polled {
    pub(crate) fn only(state: RingState) -> Polled {
        Polled { state, advanced: None, fence_garbage: None, rptr_garbage: None }
    }
}

/// The IB words of a hung job, read by the driver.
#[derive(Clone, Copy, Debug)]
pub struct IbWords<'a> {
    pub addr: u64,
    pub words: &'a [u32],
}

/// An IB as readable packets; positions are dword offsets into it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IbDump {
    pub addr: u64,
    pub lines: Vec<Line>,
}

/// The report of a hung ring: one klog event's worth, taken before any recovery changes the ring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HangReport {
    pub ring: RingId,
    pub name: String,
    pub cause: Cause,
    pub signalled: u64,
    pub emitted: u64,
    pub rptr: u64,
    pub wptr: u64,
    /// The oldest unsignalled job: the one to blame.
    pub job: Pending,
    pub ring_dump: Dump,
    pub ibs: Vec<IbDump>,
}

impl fmt::Display for HangReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.cause {
            Cause::Timeout { stalled_us, timeout_us } => {
                write!(f, "ring {} timeout: no progress for {stalled_us} us (limit {timeout_us} us)", self.name)?
            }
            Cause::Engine(h) => write!(f, "ring {} halted: {h:?}", self.name)?,
        }
        writeln!(f, ", signaled seq={:#x}, emitted seq={:#x}", self.signalled, self.emitted)?;
        writeln!(f, "job seq={:#x} {:?} vmid={} pasid={}", self.job.seq, self.job.meta.owner, self.job.meta.vmid, self.job.meta.pasid)?;
        write!(f, "{}", self.ring_dump)?;
        for ib in &self.ibs {
            writeln!(f, "ib addr={:#x}", ib.addr)?;
            for l in &ib.lines {
                writeln!(f, "{l}")?;
            }
        }
        Ok(())
    }
}

/// One ring's timeline, jobs and timer.
#[derive(Clone, Debug)]
pub struct RingWatch {
    id: RingId,
    cfg: RingCfg,
    timeline: Timeline,
    jobs: VecDeque<Pending>,
    rptr: u64,
    wptr: u64,
    armed_us: Option<u64>,
    halted: Option<sim::Hang>,
    /// A step of more than half the ring, in dwords ahead of `rptr`, waiting for the next sample to agree.
    suspect: Option<u64>,
}

impl RingWatch {
    /// None if the size is no power of two or the timeout is 0.
    pub fn new(id: RingId, cfg: RingCfg) -> Option<RingWatch> {
        if !cfg.size_dw.is_power_of_two() || cfg.timeout_us == 0 {
            return None;
        }
        let timeline = Timeline::new(cfg.fence_start, cfg.max_in_flight);
        let start = cfg.start;
        Some(RingWatch { id, cfg, timeline, jobs: VecDeque::new(), rptr: start, wptr: start, armed_us: None, halted: None, suspect: None })
    }
    pub fn id(&self) -> RingId {
        self.id
    }
    pub fn cfg(&self) -> &RingCfg {
        &self.cfg
    }
    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }
    pub fn jobs(&self) -> impl Iterator<Item = &Pending> {
        self.jobs.iter()
    }
    /// The read pointer last accepted, as a dword counter.
    pub fn rptr(&self) -> u64 {
        self.rptr
    }
    pub fn wptr(&self) -> u64 {
        self.wptr
    }

    /// A job whose frame was written to `start..end`. A later submission does not move a running timer.
    pub fn submit(&mut self, meta: JobMeta, start: u64, end: u64, now_us: u64) -> Result<u64, FenceError> {
        let seq = self.timeline.emit()?;
        self.jobs.push_back(Pending { seq, meta, start, end });
        self.wptr = end;
        if self.armed_us.is_none() {
            self.armed_us = Some(now_us);
        }
        Ok(seq)
    }

    /// Takes back the last job (its frame was undone before the engine saw it).
    pub fn undo_submit(&mut self) -> Option<Pending> {
        let job = self.jobs.pop_back()?;
        self.timeline.unemit();
        self.wptr = job.start;
        if self.jobs.is_empty() {
            self.armed_us = None;
        }
        Some(job)
    }

    /// Judges a sample. Progress is looked at first: a poller that slept past the timeout does not hang a ring
    /// that moved meanwhile.
    pub fn poll(&mut self, now_us: u64, s: Sample) -> Polled {
        let mut p = Polled::only(RingState::Idle);
        let mut progress = false;
        match self.timeline.observe(s.fence) {
            Observed::Advanced { from, to } => {
                p.advanced = Some((from, to));
                progress = true;
                while self.jobs.front().is_some_and(|j| self.timeline.is_signalled(j.seq)) {
                    self.jobs.pop_front();
                }
            }
            Observed::Ahead(v) => p.fence_garbage = Some(v),
            Observed::Unchanged | Observed::Stale(_) => {}
        }
        // Compared in the ring's own positions: the hardware's pointer may be masked to the ring or not.
        let pos = self.cfg.kind.ptr_from_hw(s.rptr_hw);
        let moved = pos.wrapping_sub(self.rptr) & (self.cfg.size_dw - 1);
        let last = self.suspect.take();
        // The first look at a suspect step is no verdict either: a poller that slept may be seeing real progress.
        let mut hold = false;
        if moved > self.wptr.wrapping_sub(self.rptr) {
            p.rptr_garbage = Some(pos);
        } else if moved > self.cfg.size_dw / 2 && !last.is_some_and(|m| moved >= m) {
            hold = last.is_none();
            self.suspect = Some(moved);
            p.rptr_garbage = last.map(|m| self.rptr.wrapping_add(m));
        } else {
            if last.is_some_and(|m| moved < m) {
                p.rptr_garbage = last.map(|m| self.rptr.wrapping_add(m));
            }
            if moved != 0 {
                self.rptr = self.rptr.wrapping_add(moved);
                progress = true;
            }
        }
        if self.jobs.is_empty() {
            self.armed_us = None;
            return p;
        }
        if progress || self.armed_us.is_none() {
            self.armed_us = Some(now_us);
        }
        p.state = match self.cause(now_us) {
            Some(Cause::Timeout { .. }) if hold => RingState::Waiting { remaining_us: 0 },
            Some(c) => RingState::Hung(c),
            None => RingState::Waiting { remaining_us: self.cfg.timeout_us.saturating_sub(self.stalled_us(now_us)) },
        };
        p
    }

    /// The same for a fake's run report; `fence` is what its slot holds now.
    pub fn poll_report(&mut self, now_us: u64, r: &Report, fence: u64) -> Polled {
        self.halted = match r.stop {
            Stop::Hang(h) => Some(h),
            _ => None,
        };
        self.poll(now_us, Sample { rptr_hw: self.cfg.kind.hw_ptr(r.at.rptr), fence })
    }

    fn stalled_us(&self, now_us: u64) -> u64 {
        self.armed_us.map_or(0, |a| now_us.saturating_sub(a))
    }

    /// Why the ring is hung now, if it is. Reads no sample: call it after [`RingWatch::poll`].
    pub fn cause(&self, now_us: u64) -> Option<Cause> {
        if self.jobs.is_empty() {
            return None;
        }
        if let Some(h) = self.halted {
            return Some(Cause::Engine(h));
        }
        let stalled_us = self.stalled_us(now_us);
        (stalled_us >= self.cfg.timeout_us).then_some(Cause::Timeout { stalled_us, timeout_us: self.cfg.timeout_us })
    }

    /// The report of a hung ring (None if it is not hung). `ring` is a snapshot of the whole ring; an empty one
    /// (unreadable) gives a report without lines. Garbage in it only shows as raw lines.
    pub fn report(&self, now_us: u64, ring: &[u32], ibs: &[IbWords<'_>]) -> Option<HangReport> {
        let cause = self.cause(now_us)?;
        let job = *self.jobs.front()?;
        let isa = match self.cfg.kind {
            Kind::Gfx => Isa::Pm4,
            Kind::Sdma { .. } => Isa::Sdma,
        };
        Some(HangReport {
            ring: self.id,
            name: self.cfg.name.clone(),
            cause,
            signalled: self.timeline.signalled(),
            emitted: self.timeline.emitted(),
            rptr: self.rptr,
            wptr: self.wptr,
            job,
            ring_dump: decode_ring(isa, ring, job.start, self.rptr, self.wptr),
            ibs: ibs.iter().map(|ib| IbDump { addr: ib.addr, lines: decode_stream(isa, ib.words, 0) }).collect(),
        })
    }

    /// Every job leaves the ring: the fences count as done (amdgpu_fence_driver_force_completion) and the timer
    /// stops. Returns the jobs, oldest first.
    pub(crate) fn take_jobs(&mut self) -> Vec<Pending> {
        self.timeline.force_complete();
        self.armed_us = None;
        self.halted = None;
        self.suspect = None;
        self.jobs.drain(..).collect()
    }

    /// The ring runs again with both pointers at `ptr`.
    pub(crate) fn resync(&mut self, ptr: u64) {
        self.rptr = ptr;
        self.wptr = ptr;
        self.armed_us = None;
        self.halted = None;
        self.suspect = None;
    }

    /// The timer of the jobs a soft recovery left on the ring starts over (Linux: the job finished).
    pub(crate) fn rearm(&mut self, now_us: u64) {
        self.armed_us = (!self.jobs.is_empty()).then_some(now_us);
        self.halted = None;
    }
}

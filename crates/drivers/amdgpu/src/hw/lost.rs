//! GX-8b: the poll of a round, and when a job does not end: a VM fault, a hang.
//!
//! A fault is read from GFXHUB (with GX-7n its interrupt wakes this look, the look stays what finds it), told to the VM that has a job in that VMID, cleared. CPG and
//! SDMA drop the access and their job goes on to its fence, as in Linux; CPC stops at it and has nothing to program
//! against that (CONTRACTS 6b, `vmrecover`), so a compute queue with no progress after a fault of its head job takes
//! GR-3's queue rung at once rather than after 60 s: REMOVE_QUEUE, the queue rewritten, mapped, tested. That job ends ETIME
//! and its context guilty; whatever stood behind it on that ring ends ECANCELED. Nothing else notices.
//!
//! A hang (no progress of fence and read pointer for the ring's timeout) writes the decoded ring in one klog event and
//! walks GR-3's ladder (`rungs`: GX-9a): the kill, the queue, GC's re-init, mode 2 behind its CONFIG key. A rung that works
//! leaves the device up, with the fates `Finish` names. When none does, `NeedsReboot`: the hung job ETIME, its context guilty,
//! the reset counter moved, every other job ECANCELED, and the device lost until `until=stop` and the next bring-up (`submit`
//! answers ENODEV). Jobs told to have ended while the hardware may still run them keep what they reach (`grave`) until the
//! engines are halted: nothing a program can reach goes back to the allocators under a running engine. A ladder that refuses
//! a ring ends the same way (`give_up`).

use super::frame::RingType;
use super::rings::{OnRing, Stall};
use super::jobs::{CANCELED, Rung};
use super::run::{Ended, Inner, finish, let_go};
use super::vm::HwVm;
use crate::gpucore::gmc::{self, FaultRead};
use crate::ipmod::gmc11::Fault;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::bus::MemFault;
use amdgpu_fmt::recover::{Finish, Outcome, Scope};
use amdgpu_fmt::sim::{Hang, Report, Stop, Where};
use amdgpu_fmt::watch::{IbWords, Polled, RingState, Sample};
use core::sync::atomic::Ordering;
use drm::gpu::VmFault;
use drm::uapi::AMDGPU_VMHUB_TYPE_GFX;
use linux::errno::ETIME;

/// gfxhub_client_ids (gfxhub_v3_0.c): the compute CP's client, the one that stops at a fault.
const CID_CPC: u32 = 5;
/// How many lines of a full reset the tool keeps.
const RESET_LINES: usize = 80;
/// How much of a hung job's IBs goes into the report.
const DUMP_IBS: usize = 4;
const DUMP_IB_DW: u32 = 256;

impl Inner {
    /// Reads every ring with jobs and the fault registers, ends what ended. True when something moved.
    pub(super) fn poll(&mut self, now: u64) -> bool {
        if self.rings.iter().all(|r| r.jobs.is_empty()) {
            return false;
        }
        let mut samples: [Option<Sample>; 3] = [None; 3];
        let rptrs: [u64; 3] = core::array::from_fn(|i| self.domain.ring(self.rings[i].id).map_or(0, |w| self.rings[i].queue.kind().hw_ptr(w.rptr())));
        let rings = &self.rings;
        self.chain.with(&mut |o| {
            for (i, r) in rings.iter().enumerate() {
                if !r.jobs.is_empty() {
                    samples[i] = r.sample(o, rptrs[i]);
                }
            }
        });
        // After the fences: an engine's fault comes before its fence, so a job seen ended here has its fault latched by
        // now, and the job that made it is still on the list to be told.
        self.look_for_fault(now, samples[RingType::Compute.index()]);
        let mut moved = false;
        let mut ended = Vec::new();
        let mut gone = Vec::new();
        for i in 0..3 {
            let Some(s) = samples[i] else { continue };
            // A hang on an earlier ring took this ring's jobs with it.
            if self.rings[i].jobs.is_empty() {
                continue;
            }
            let Some(p) = self.domain.poll(self.rings[i].id, now, s) else { continue };
            moved |= self.note_polled(i, &p, &mut ended);
            let hung = matches!(p.state, RingState::Hung(_));
            let stopped = !hung && self.stalled_now(i, now, s);
            if stopped && let Some(p) = self.report_stall(i, now, s) {
                moved |= self.note_polled(i, &p, &mut ended);
            }
            if hung || stopped {
                for e in ended.drain(..) {
                    gone.push(finish(e, &self.shared));
                }
                self.hung(i, now);
                moved = true;
                // A full reset has the chain now: no look until it answers.
                if self.long_running() {
                    break;
                }
            }
        }
        for e in ended {
            gone.push(finish(e, &self.shared));
        }
        let_go(gone);
        moved
    }

    /// The fence moved: those jobs ended. Garbage from the chip is said, it decides nothing.
    pub(super) fn note_polled(&mut self, i: usize, p: &Polled, ended: &mut Vec<Ended>) -> bool {
        let r = &mut self.rings[i];
        if let Some(v) = p.fence_garbage {
            klog::event!(Warn, "gx", "hw.fence_garbage", ring = r.ty.name(), value = klog::Hex(v));
        }
        if let Some(v) = p.rptr_garbage {
            klog::event!(Warn, "gx", "hw.rptr_garbage", ring = r.ty.name(), value = klog::Hex(v));
        }
        let Some(w) = self.domain.ring(r.id) else { return false };
        let mut any = p.advanced.is_some();
        if any {
            self.shared.fences_moved.fetch_add(1, Ordering::Relaxed);
        }
        while r.jobs.front().is_some_and(|j| w.timeline().is_signalled(j.seq)) {
            if let Some(on) = r.jobs.pop_front() {
                ended.push(Ended { on, errno: None });
                any = true;
            }
        }
        any
    }

    fn look_for_fault(&mut self, now: u64, compute_at: Option<Sample>) {
        let hub = self.vmi.addrs.hub;
        let mut f: Option<FaultRead> = None;
        self.chain.with(&mut |o| f = Some(gmc::read_fault(o.io(), &hub)));
        let Some(f) = f else { return };
        let Some(status) = f.status.filter(|&s| s != 0 && s != u32::MAX) else { return };
        let d = Fault::decode(status);
        let addr = f.addr.unwrap_or(0);
        let vmid = d.vmid as u8;
        self.shared.faults.fetch_add(1, Ordering::Relaxed);
        let owner = self.rings.iter().flat_map(|r| r.jobs.iter()).find(|j| j.vm.as_ref().is_some_and(|v| v.vmid as u8 == vmid));
        match owner {
            Some(on) => {
                if let Some(vm) = on.hold.sub.vm.as_any().and_then(|a| a.downcast_ref::<HwVm>()) {
                    vm.note_fault(VmFault { addr, status, vmhub: AMDGPU_VMHUB_TYPE_GFX });
                }
            }
            None => {
                klog::event!(Warn, "gx", "hw.fault_no_job", vmid = vmid, status = klog::Hex(u64::from(status)), page = klog::Hex(addr); "a fault in a VMID with no job on a ring");
            }
        }
        // A program that faults at every draw must not fill the log: one line per place.
        if self.last_fault != Some((addr, status)) {
            self.last_fault = Some((addr, status));
            klog::event!(Warn, "gx", "hw.vm_fault", vmid = vmid, status = klog::Hex(u64::from(status)), page = klog::Hex(addr), cid = d.cid, write = d.write);
        }
        let compute = RingType::Compute.index();
        if d.cid == CID_CPC && self.rings[compute].jobs.front().is_some_and(|j| j.vm.as_ref().is_some_and(|v| v.vmid as u8 == vmid)) {
            let seq = self.rings[compute].jobs.front().map_or(0, |j| j.seq);
            self.rings[compute].stall.get_or_insert(Stall { at_us: now, addr, write: d.write, vmid, seq, seen: compute_at });
        }
        let (a, us) = (self.vmi.addrs, self.vmi.flush_us);
        let mut left = None;
        self.chain.with(&mut |o| left = Some(o.clear_fault(&a, us)));
        if left.flatten() != Some(0) {
            klog::event!(Warn, "gx", "hw.fault_not_cleared", status = klog::Hex(u64::from(left.flatten().unwrap_or(u32::MAX))));
        }
    }

    /// The queue seen at a fault has not moved since, for long enough.
    fn stalled_now(&mut self, i: usize, now: u64, s: Sample) -> bool {
        let grace = self.cfg.stall_grace_us;
        let r = &mut self.rings[i];
        let head = r.jobs.front().map(|j| j.seq);
        let Some(st) = r.stall.as_mut() else { return false };
        if head != Some(st.seq) {
            // It went on after all: CPC did not stop at this one.
            r.stall = None;
            return false;
        }
        st.stopped(now, s, grace)
    }

    /// GR-3's watch hears of the stop as of an engine that halted.
    fn report_stall(&mut self, i: usize, now: u64, s: Sample) -> Option<Polled> {
        let r = &self.rings[i];
        let st = r.stall?;
        let at = Where { rptr: self.domain.ring(r.id)?.rptr(), ib: None };
        let rep = Report { stop: Stop::Hang(Hang::Fault(MemFault { vmid: st.vmid, addr: st.addr, write: st.write })), at, events: Vec::new(), fetched_dw: 0 };
        self.domain.poll_report(r.id, now, &rep, s.fence)
    }

    /// The head job's IBs, as far as the VM's bookkeeping reaches them.
    fn ib_dumps(&self, on: &OnRing) -> Vec<(u64, Vec<u32>)> {
        let Some(vm) = on.hold.sub.vm.as_any().and_then(|a| a.downcast_ref::<HwVm>()) else { return Vec::new() };
        let ibs = &on.hold.sub.jobs[on.member].ibs;
        ibs.iter().take(DUMP_IBS).map(|b| (b.va, read_ib(vm, b.va, b.len_dw.min(DUMP_IB_DW)))).collect()
    }

    /// The report in one event, then GR-3's ladder to its end. It always ends here: a ring left as it is would report again
    /// at every round, with its jobs never ending.
    pub(crate) fn hung(&mut self, i: usize, now: u64) {
        self.shared.hangs.fetch_add(1, Ordering::Relaxed);
        let mut words = None;
        let q = &self.rings[i].queue;
        self.chain.with(&mut |o| words = q.words(o.io()));
        let words = words.unwrap_or_default();
        let ibs: Vec<(u64, Vec<u32>)> = self.rings[i].jobs.front().map(|on| self.ib_dumps(on)).unwrap_or_default();
        let views: Vec<IbWords<'_>> = ibs.iter().map(|(addr, w)| IbWords { addr: *addr, words: w }).collect();
        let id = self.rings[i].id;
        let Some(report) = self.domain.report(id, now, &words, &views) else {
            self.give_up(i, "the watch has no report for this ring");
            return;
        };
        // The frame's padding says nothing, and a klog record has a limit.
        let text: String = format!("{report}").lines().filter(|l| !l.ends_with("NOP (pad)")).flat_map(|l| [l, "\n"]).collect();
        klog::event!(Error, "gx", "hw.job_hang", report = text.as_str());
        self.last_hang = Some(text);
        self.mode2_rung = false;
        match self.domain.begin(&report, now) {
            Ok(next) => self.walk(i, next),
            Err(e) => self.give_up(i, format!("{e:?}").as_str()),
        }
    }

    /// GR-3's ladder would not take the ring: `NeedsReboot` without its `Finish`. The head job ETIME and its context guilty,
    /// every other job ECANCELED, the counter moved, the device lost; with no job left on a ring the poll stops here.
    pub(super) fn give_up(&mut self, i: usize, why: &str) {
        klog::event!(Error, "gx", "hw.recovery_refused", ring = self.rings[i].ty.name(), why = why; "the ladder would not take it: the device counts as lost");
        let reset = self.domain.counters().reset;
        if let Some(on) = self.rings[i].jobs.front() {
            on.hold.sub.jobs[on.member].entity.blame(reset);
        }
        // Before any fence, as in `apply`.
        self.shared.reset_counter.store(reset.wrapping_add(1), Ordering::Release);
        self.shared.lost.store(true, Ordering::Release);
        let mut jobs = 0;
        for k in 0..self.rings.len() {
            let mut errno = if k == i { ETIME } else { CANCELED };
            while let Some(on) = self.rings[k].jobs.pop_front() {
                on.hold.sub.jobs[on.member].finished.signal_error(errno as i32);
                errno = CANCELED;
                jobs += 1;
                self.grave.push(on);
            }
            self.rings[k].stall = None;
            self.rings[k].current_ctx = None;
        }
        self.fresh = Default::default();
        self.magic = None;
        self.shared.jobs_done.fetch_add(jobs, Ordering::Relaxed);
        klog::event!(Error, "gx", "hw.device_lost", ring = self.rings[i].ty.name(), jobs = jobs; "until=stop and a bring-up bring it back; nothing touched the picture");
        self.chain.device_lost();
        self.shared.recovered[Rung::None as usize].fetch_add(1, Ordering::Release);
    }

    /// `Finish`'s fates to the fences, its counters to the device's, and who keeps what.
    pub(super) fn apply(&mut self, i: usize, f: Finish) {
        // VRAM that did not hold after a reset leaves page tables nobody can trust, and a carve-out that is system memory has
        // no reason to lose them: the counter tells the programs, and the device stays down.
        let recovered = matches!(f.outcome, Outcome::Recovered(_)) && !f.vram_lost;
        // Before any fence: the waiter a fate wakes asks at once for its context's state and may submit. Stored after them,
        // abi-drm-jobs read `reset 0` and a CS that passed on a device about to be lost (a gate under load, 3.11.).
        self.shared.reset_counter.store(f.counters.reset, Ordering::Release);
        self.shared.vram_lost.store(f.counters.vram_lost, Ordering::Release);
        self.shared.in_reset.store(f.counters.in_reset, Ordering::Release);
        if !recovered {
            self.shared.lost.store(true, Ordering::Release);
        }
        self.tell_finish(&f);
        let mut gone = Vec::new();
        for fate in &f.fates {
            let Some(r) = self.rings.iter_mut().find(|r| r.id == fate.ring) else { continue };
            let Some(pos) = r.jobs.iter().position(|j| j.seq == fate.job.seq) else { continue };
            let Some(on) = r.jobs.remove(pos) else { continue };
            on.hold.sub.jobs[on.member].finished.signal_error(fate.fate.errno());
            self.shared.jobs_done.fetch_add(1, Ordering::Relaxed);
            if recovered {
                gone.push(on);
            } else {
                self.grave.push(on);
            }
        }
        // The queue is gone with whatever stood on it. Their flushes may not have run: the VM flushes again. After every
        // fence: this takes a VM's lock.
        for mut on in gone {
            if let Some(v) = on.vm.take() {
                v.cancel();
            }
        }
        if self.rings[i].stall.take().is_some() && recovered {
            self.shared.stalls.fetch_add(1, Ordering::Relaxed);
        }
        self.rings[i].current_ctx = None;
        self.fresh = Default::default();
        self.magic = None;
        match f.outcome {
            Outcome::Recovered(scope) if recovered => {
                if matches!(scope, Scope::Gc | Scope::Mode2) {
                    self.full_reset_passed();
                }
                // Last: a test, and the tool, read this as "the ladder is through".
                self.shared.recovered[Rung::of(scope) as usize].fetch_add(1, Ordering::Release);
                klog::event!(Warn, "gx", "hw.queue_back", ring = self.rings[i].ty.name(), scope = format!("{scope:?}").as_str(), jobs = f.fates.len());
            }
            _ => {
                klog::event!(Error, "gx", "hw.device_lost", ring = self.rings[i].ty.name(), outcome = format!("{:?}", f.outcome).as_str(), jobs = f.fates.len(); "until=stop and a bring-up bring it back; nothing touched the picture");
                self.chain.device_lost();
                self.shared.recovered[Rung::None as usize].fetch_add(1, Ordering::Release);
            }
        }
    }

    /// What the ladder's end has to say beyond the fates (B6).
    fn tell_finish(&mut self, f: &Finish) {
        if let Some(why) = f.reinit_refused {
            klog::event!(Warn, "gx", "hw.reinit_refused", why = why);
            self.reset_lines.push(format!("reset: {why}"));
        }
        if let Outcome::NeedsReboot { mode2: Some(why) } = f.outcome {
            klog::event!(Warn, "gx", "hw.mode2_refused", why = why);
        }
        if f.vram_lost {
            klog::event!(Error, "gx", "hw.vram_lost", counter = f.counters.vram_lost; "the reset magic did not hold: the device stays down");
        }
        if f.display_changed {
            klog::event!(Error, "gx", "hw.reset_moved_picture"; "GX-D9 was broken: re-init and mode 2 are closed for this boot");
        }
        if f.expired != 0 {
            klog::event!(Error, "gx", "hw.recovery_expired", steps = f.expired; "steps nobody answered: a bug in the glue, or a step that wedged");
        }
        if f.overruns != 0 {
            klog::event!(Warn, "gx", "hw.recovery_overran", steps = f.overruns);
        }
        if !self.reset_lines.is_empty() {
            // The tail says how it ended; `until=stop` and `until=backend` print a whole run when the desk wants one.
            let lines = core::mem::take(&mut self.reset_lines);
            let from = lines.len().saturating_sub(RESET_LINES);
            self.last_reset = Some(lines[from..].join("\n"));
        }
    }

    /// Every engine was stopped, started from the PSP on a rewritten ring and answered its ring test: what the hung jobs and
    /// an earlier stop left can go, and no ring has a context in hand.
    fn full_reset_passed(&mut self) {
        for r in &mut self.rings {
            r.stall = None;
            r.current_ctx = None;
        }
        let gone = self.take_grave();
        let n = gone.len();
        let_go(gone);
        self.chain.proven_halted();
        klog::event!(Info, "gx", "hw.engines_proven_halted", jobs = n);
    }

    /// Tests: the VMID of the job at the head of a ring.
    pub(crate) fn head_vmid(&self, ty: RingType) -> Option<u8> {
        self.rings[ty.index()].jobs.front()?.vm.as_ref().map(|v| v.vmid as u8)
    }

    /// The tool's lines.
    pub(super) fn describe(&self) -> Vec<String> {
        let s = &self.shared;
        let mut v = Vec::new();
        for r in &self.rings {
            let Some(w) = self.domain.ring(r.id) else { continue };
            let t = w.timeline();
            v.push(format!("backend jobs: {}: emitted {}, signaled {}, {} on the ring, rptr {:#x}, wptr {:#x}", r.ty.name(), t.emitted(), t.signalled(), r.jobs.len(), w.rptr(), w.wptr()));
        }
        v.push(format!(
            "backend jobs: {} ended, {} queued, {} VM faults, {} queues rebuilt after a fault, {} hangs, reset counter {}, device {}",
            s.jobs_done.load(Ordering::Relaxed),
            s.pending.lock().len() + self.queue.len(),
            s.faults.load(Ordering::Relaxed),
            s.stalls.load(Ordering::Relaxed),
            s.hangs.load(Ordering::Relaxed),
            s.reset_counter.load(Ordering::Acquire),
            if s.lost.load(Ordering::Acquire) { "LOST (until=stop, then until=backend)" } else { "up" }
        ));
        let rec = |r: Rung| s.recovered[r as usize].load(Ordering::Relaxed);
        v.push(format!(
            "backend recovery: {} by a kill, {} by a queue reset, {} by GC's re-init, {} by mode 2, {} ended the device; the ladder starts at {}; mode 2 {}",
            rec(Rung::Soft),
            rec(Rung::Queue),
            rec(Rung::Gc),
            rec(Rung::Mode2),
            rec(Rung::None),
            Rung::from_u8(s.ladder_from.load(Ordering::Acquire)).name(),
            match self.domain.policy().mode2 {
                amdgpu_fmt::recover::Mode2Gate::Refused(why) => format!("refused: {why}"),
                amdgpu_fmt::recover::Mode2Gate::Measured { proof } => format!("open: {proof}"),
            }
        ));
        if let Some(l) = &self.last_reset {
            v.push(format!("backend recovery: the last full reset:\n{l}"));
        }
        v.push(format!("backend jobs: {} rounds of the job thread, {} fence moves", s.rounds.load(Ordering::Relaxed), s.fences_moved.load(Ordering::Relaxed)));
        if let Some(h) = &self.last_hang {
            v.push(format!("backend jobs: the last hang:\n{h}"));
        }
        let mut writes = None;
        self.chain.with(&mut |o| writes = Some(o.display_writes()));
        v.push(match writes {
            Some(n) => format!("display writes while the backend ran: {n} (must be 0)"),
            None => String::from("display writes while the backend ran: the owner is busy"),
        });
        v
    }
}

/// `len` dwords of the VM at `va`, as far as they are mapped.
fn read_ib(vm: &HwVm, va: u64, len: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for k in 0..u64::from(len) {
        let at = va + k * 4;
        let Some((Some(mem), page)) = vm.held_at(at) else { break };
        let mut b = [0u8; 4];
        if !mem.read(page * 4096 + (at & 0xfff), &mut b) {
            break;
        }
        out.push(u32::from_le_bytes(b));
    }
    out
}

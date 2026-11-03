//! Detection and recovery end to end on the interpreters, and the compute stall the chip showed on 30.10.

use std::string::ToString;
use std::vec;
use std::vec::Vec;

use super::watch::client;
use crate::bus::SimMem;
use crate::errno;
use crate::pm4;
use crate::recover::{Action, IpVersion, MODE2_UNMEASURED, Next, Outcome, Policy, Reply, ResetDomain, Scope};
use crate::ring::{Kind, Ring};
use crate::sdma;
use crate::sim::{Engine, Isa, Stop};
use crate::told::{Fate, query2};
use crate::watch::{Cause, IbWords, RingCfg, RingClass, RingId, RingState, Sample};

const RING: u64 = 0x10_0000;
const SLOT: u64 = 0x30_0000;
const IB: u64 = 0x40_0000;
const NEVER: u64 = 0x50_0000;
const SIZE: u64 = 1024;
const VMID: u8 = 3;

/// One ring on the interpreter, with its fake clock.
struct Chip {
    mem: SimMem,
    ring: Ring,
    eng: Engine,
    shadow: Vec<u32>,
    wptr: u64,
    now: u64,
    d: ResetDomain,
    id: RingId,
    /// What the fake's steps did, in order.
    fail_reinit: bool,
    reinits: u32,
    fence_writes: Vec<u64>,
}

impl Chip {
    fn new(kind: Kind, class: RingClass, timeout_us: u64) -> Chip {
        let mut mem = SimMem::new();
        mem.map(0, RING, SIZE as usize * 4).unwrap();
        mem.map(0, SLOT, 0x1000).unwrap();
        mem.map(VMID, IB, 0x1000).unwrap();
        mem.map(VMID, NEVER, 0x1000).unwrap();
        let isa = if kind == Kind::Gfx { Isa::Pm4 } else { Isa::Sdma };
        let mut d = ResetDomain::new(Policy::for_mp1(IpVersion(13, 0, 4)));
        let mut cfg = RingCfg::for_780m("fake", class, SIZE, 0, 16);
        cfg.kind = kind;
        cfg.timeout_us = timeout_us;
        let id = d.add_ring(cfg).unwrap();
        Chip {
            mem,
            ring: Ring::new(kind, SIZE, 0).unwrap(),
            eng: Engine::new(isa, RING, SIZE, 0, super::limits()).unwrap(),
            shadow: vec![0; SIZE as usize],
            wptr: 0,
            now: 0,
            d,
            id,
            fail_reinit: false,
            reinits: 0,
            fence_writes: Vec::new(),
        }
    }

    /// One job: `ib` as its IB, then the fence.
    fn submit(&mut self, ctx: u64, ib: &[u32]) -> u64 {
        self.mem.poke_dwords(VMID, IB, ib).unwrap();
        let start = self.ring.wptr();
        self.ring.begin(32, self.eng.rptr()).unwrap();
        self.ring.emit_ib(VMID, IB, ib.len() as u32, 0, false).unwrap();
        let seq = self.d.ring(self.id).unwrap().timeline().emitted() + 1;
        self.ring.emit_fence(SLOT, seq, true, true).unwrap();
        self.wptr = self.ring.commit(&mut self.shadow).unwrap();
        self.mem.poke_dwords(0, RING, &self.shadow).unwrap();
        assert_eq!(self.d.submit(self.id, client(ctx), start, self.wptr, self.now).unwrap(), seq);
        seq
    }

    /// Runs the engine for `us` of fake time and judges what it did.
    fn tick(&mut self, us: u64) -> crate::watch::Polled {
        self.now += us;
        let r = self.eng.run(&mut self.mem, self.wptr);
        let fence = self.mem.peek_u64(0, SLOT).unwrap();
        self.d.poll_report(self.id, self.now, &r, fence).unwrap()
    }

    fn perform(&mut self, a: &Action) -> Reply {
        match *a {
            Action::Blame { .. } | Action::Unmap { .. } | Action::Map { .. } => Reply::Ok,
            Action::ResetQueue { .. } => Reply::Ok,
            Action::KillWaves { .. } => {
                // A CP stuck on a poll does not care about waves.
                self.now += 10_000;
                let _ = self.tick(0);
                Reply::Ok
            }
            Action::RestartSdma { .. } | Action::ReinitGc => {
                self.reinits += u32::from(*a == Action::ReinitGc);
                if self.fail_reinit {
                    return Reply::Failed;
                }
                // The ring is not moved: the driver's next frame goes on behind the old wptr.
                let w = self.ring.wptr();
                self.eng.reset(w);
                if *a == Action::ReinitGc { Reply::Reset { vram_intact: true, display_equal: true } } else { Reply::Ok }
            }
            Action::TestRing { fence_value, .. } => {
                self.mem.poke(0, SLOT, &fence_value.to_le_bytes()).unwrap();
                self.fence_writes.push(fence_value);
                Reply::Ring { ptr: self.ring.wptr() }
            }
            Action::SuspendGc | Action::Mode2Reset => panic!("mode 2 on the default policy"),
        }
    }

    fn recover(&mut self) -> (Vec<Action>, crate::recover::Finish) {
        let rep = self.d.report(self.id, self.now, &self.shadow, &[]).unwrap();
        let mut next = self.d.begin(&rep, self.now).unwrap();
        let mut acts = Vec::new();
        loop {
            match next {
                Next::Done(f) => return (acts, f),
                Next::Do(s) => {
                    acts.push(s.action);
                    let r = self.perform(&s.action);
                    next = self.d.reply(r, self.now).unwrap();
                }
            }
        }
    }
}

fn wait_never() -> Vec<u32> {
    let mut v = Vec::new();
    pm4::wait_reg_mem(
        &mut v,
        &pm4::WaitRegMem {
            function: 3,
            mem: true,
            operation: 0,
            engine: 0,
            addr0: NEVER as u32,
            addr1: 0,
            reference: 1,
            mask: 0xffff_ffff,
            interval: 4,
        },
    )
    .unwrap();
    v
}

fn write_ok() -> Vec<u32> {
    let mut v = Vec::new();
    pm4::write_data(&mut v, pm4::dst_sel::MEM, IB + 0x100, 0, &[0xCAFE]).unwrap();
    v
}

/// plan/hw/gx-780m-minipc-2026-10-30/c1/g2-vm.txt: the compute ring's third job faults on purpose, CPC does not go
/// on: rptr stays 0x500 of 0x600, the fence slot never reaches 3. Nothing halts: only the timeout can see it.
#[test]
fn the_compute_stall_of_the_chip_is_a_timeout_then_a_queue_reset() {
    let mut ring = Ring::new(Kind::Gfx, 0x800, 0).unwrap();
    let mut shadow = vec![0u32; 0x800];
    let mut d = ResetDomain::new(Policy::for_mp1(IpVersion(13, 0, 4)));
    let class = RingClass::ComputeMesLegacy { queue_type: 1, pipe: 0, queue: 0, doorbell_dw: 0x10 };
    let mut cfg = RingCfg::for_780m("compute", class, 0x800, 0, 8);
    assert_eq!(cfg.timeout_us, 60_000_000);
    cfg.timeout_us = 100_000; // what the chip's rung waited.
    let id = d.add_ring(cfg).unwrap();
    // Six frames of 0x100, so that the stall stands where the chip's stood.
    let mut seqs = Vec::new();
    for n in 0..6u64 {
        let start = ring.wptr();
        ring.begin(32, ring.wptr() - start.min(0x100)).unwrap();
        ring.emit_ib(1, 0x1_0010_1000, 16, 0, false).unwrap();
        ring.emit_fence(0x3000, n + 1, true, true).unwrap();
        let end = ring.commit(&mut shadow).unwrap();
        seqs.push(d.submit(id, client(1), start, end, 0).unwrap());
        assert_eq!(end, (n + 1) * 0x100);
    }
    assert_eq!(seqs, [1, 2, 3, 4, 5, 6]);
    // Five frames went through, the sixth stands at its first packet; five fences arrived.
    let stall = Sample { rptr_hw: 0x500, fence: 5 };
    assert_eq!(d.poll(id, 10, stall).unwrap().advanced, Some((0, 5)));
    // A step of more than half the ring counts at the second look (`watch`).
    assert_eq!(d.ring(id).unwrap().rptr(), 0);
    d.poll(id, 10, stall).unwrap();
    assert_eq!(d.ring(id).unwrap().rptr(), 0x500);
    assert_eq!(d.poll(id, 100_009, stall).unwrap().state, RingState::Waiting { remaining_us: 1 });
    let st = d.poll(id, 100_010, stall).unwrap().state;
    assert_eq!(st, RingState::Hung(Cause::Timeout { stalled_us: 100_000, timeout_us: 100_000 }));

    let ib = [0xc000_1000u32, 0, 0, 0];
    let rep = d.report(id, 100_010, &shadow, &[IbWords { addr: 0x1_0010_1000, words: &ib }]).unwrap();
    assert_eq!((rep.rptr, rep.wptr, rep.job.seq), (0x500, 0x600, 6));
    assert_eq!(rep.ring_dump.from, 0x500);
    assert!(rep.ring_dump.lines[0].at_rptr);
    assert!(rep.ring_dump.lines[0].text.starts_with("INDIRECT_BUFFER addr=0x100101000"), "{}", rep.ring_dump.lines[0].text);
    assert!(rep.to_string().contains("=> 0x000500"), "{rep}");

    // No kill on compute, the queue first; MES may not get the queue out of the stall, which GX-9a measures.
    let Next::Do(blame) = d.begin(&rep, 100_010).unwrap() else { panic!() };
    assert!(matches!(blame.action, Action::Blame { ctx: 1, .. }));
    let Next::Do(unmap) = d.reply(Reply::Ok, 100_010).unwrap() else { panic!() };
    assert!(matches!(unmap.action, Action::Unmap { .. }));
    let Next::Do(reinit) = d.reply(Reply::Failed, 100_010 + 2_100_000).unwrap() else { panic!() };
    assert_eq!(reinit.action, Action::ReinitGc);
    assert!(d.counters().in_reset);
    let Next::Do(test) = d.reply(Reply::Reset { vram_intact: true, display_equal: true }, 3_000_000).unwrap() else { panic!() };
    assert_eq!(test.action, Action::TestRing { ring: id, fence_value: 6 });
    let Next::Done(f) = d.reply(Reply::Ring { ptr: 0x600 }, 3_000_000).unwrap() else { panic!() };
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Gc));
    assert_eq!(f.fates.len(), 1);
    assert_eq!(f.fates[0].job.seq, 6);
    assert_eq!(f.fates[0].fate.errno(), errno::ETIME);
}

#[test]
fn good_jobs_never_trip_the_watch() {
    let mut c = Chip::new(Kind::Gfx, RingClass::GfxDirect, 1000);
    let ib = write_ok();
    for i in 1..20u64 {
        // Slower than the timeout per job, but each poll sees it done.
        assert_eq!(c.submit(1, &ib), i);
        assert_eq!(c.tick(999).state, RingState::Idle);
        c.now += 10 * 1000;
        assert_eq!(c.tick(0).state, RingState::Idle);
    }
    assert_eq!(c.d.counters().reset, 0);
}

#[test]
fn a_cp_waiting_forever_recovers_through_gc_four_times_in_a_row() {
    // The chip's `gfx_ring again:true` passed 4/4; here the whole cycle runs, spread out enough for the limit.
    let mut c = Chip::new(Kind::Gfx, RingClass::GfxDirect, 1000);
    let mut ctx = 1;
    for round in 0..4u64 {
        let good = c.submit(ctx, &write_ok());
        assert_eq!(c.tick(1).state, RingState::Idle);
        let bad = c.submit(ctx, &wait_never());
        assert_eq!(bad, good + 1);
        let waiting = c.tick(1);
        assert!(matches!(waiting.state, RingState::Waiting { .. }));
        let ctx_state = c.d.counters().new_context();
        let hung = c.tick(1000);
        assert!(matches!(hung.state, RingState::Hung(Cause::Timeout { .. })), "{hung:?}");

        let rep = c.d.report(c.id, c.now, &c.shadow, &[IbWords { addr: IB, words: &wait_never() }]).unwrap();
        assert!(rep.ibs[0].lines[0].text.starts_with("WAIT_REG_MEM"));
        let (acts, f) = c.recover();
        assert!(matches!(acts[0], Action::KillWaves { vmid: 3, seq, .. } if seq == bad), "{acts:?}");
        assert_eq!(f.outcome, Outcome::Recovered(Scope::Gc), "round {round}");
        assert_eq!(f.guilty, vec![ctx]);
        assert_eq!(f.fates.len(), 1);
        assert_eq!(f.fates[0].fate, Fate::Hung);
        assert_eq!(c.fence_writes.last(), Some(&bad));
        assert_eq!(c.d.counters().query2(&ctx_state), query2::RESET);

        // The next context works at once, on the same ring.
        ctx += 1;
        let again = c.submit(ctx, &write_ok());
        assert_eq!(again, bad + 1);
        assert_eq!(c.tick(1).state, RingState::Idle);
        assert_eq!(c.d.ring(c.id).unwrap().timeline().signalled(), again);
        c.now += 60_000_000;
    }
    assert_eq!(c.reinits, 4);
}

#[test]
fn a_fault_in_an_ib_halts_the_fake_and_the_reset_clears_it() {
    let mut c = Chip::new(Kind::Gfx, RingClass::GfxDirect, 1000);
    c.submit(1, &[]);
    // An IB of one dword in unmapped memory.
    c.mem.unmap(VMID, IB);
    let start = c.ring.wptr();
    c.ring.begin(32, c.eng.rptr()).unwrap();
    c.ring.emit_ib(VMID, IB, 4, 0, false).unwrap();
    c.ring.emit_fence(SLOT, 2, true, true).unwrap();
    c.wptr = c.ring.commit(&mut c.shadow).unwrap();
    c.mem.poke_dwords(0, RING, &c.shadow).unwrap();
    c.d.submit(c.id, client(2), start, c.wptr, c.now).unwrap();
    let p = c.tick(1);
    let RingState::Hung(Cause::Engine(h)) = p.state else { panic!("{p:?}") };
    assert_eq!(h.errno(), errno::EFAULT);
    assert!(c.eng.halted().is_some());
    let (_, f) = c.recover();
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Gc));
    assert_eq!(f.guilty, vec![2], "the job's own context, not the one before");
    assert!(c.eng.halted().is_none());
    assert!(matches!(c.eng.run(&mut c.mem, c.wptr).stop, Stop::Idle));
}

#[test]
fn a_soft_kill_ends_the_halt_the_fake_reported() {
    let mut c = Chip::new(Kind::Gfx, RingClass::GfxDirect, 1000);
    let ib = write_ok();
    c.submit(1, &ib);
    c.submit(1, &ib);
    c.mem.unmap(VMID, IB);
    assert!(matches!(c.tick(1).state, RingState::Hung(Cause::Engine(_))));
    let rep = c.d.report(c.id, c.now, &c.shadow, &[]).unwrap();
    let Next::Do(kill) = c.d.begin(&rep, c.now).unwrap() else { panic!() };
    assert!(matches!(kill.action, Action::KillWaves { .. }));
    let sample = Sample { rptr_hw: c.d.ring(c.id).unwrap().rptr(), fence: 1 };
    c.d.poll(c.id, c.now, sample).unwrap();
    let Next::Done(f) = c.d.reply(Reply::Ok, c.now).unwrap() else { panic!() };
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Soft));
    // The second job gets a new timer, not the old halt.
    assert_eq!(c.d.poll(c.id, c.now, sample).unwrap().state, RingState::Waiting { remaining_us: 1000 });
}

#[test]
fn an_sdma_poll_is_cleared_by_restarting_only_that_ring() {
    let mut c = Chip::new(Kind::Sdma { burst_nop: true }, RingClass::Sdma { burst_nop: true }, 1000);
    // A frame on the ring itself, waiting on memory.
    c.mem.map(0, NEVER, 0x1000).unwrap();
    let start = c.ring.wptr();
    c.ring.begin(32, 0).unwrap();
    let wait = sdma::PollRegMem {
        hdp_flush: false,
        mem: true,
        function: 3,
        addr_lo: NEVER as u32,
        addr_hi: 0,
        reference: 1,
        mask: 0xffff_ffff,
        interval: 10,
        retry: 0xFFF,
    };
    c.ring.emit(|v| sdma::poll_regmem(v, &wait)).unwrap();
    c.ring.emit_fence(SLOT, 1, true, true).unwrap();
    c.wptr = c.ring.commit(&mut c.shadow).unwrap();
    c.mem.poke_dwords(0, RING, &c.shadow).unwrap();
    c.d.submit(c.id, client(1), start, c.wptr, 0).unwrap();
    assert!(matches!(c.tick(1).state, RingState::Waiting { .. }));
    assert!(matches!(c.tick(1000).state, RingState::Hung(Cause::Timeout { .. })));
    let (acts, f) = c.recover();
    assert_eq!(acts[1..], [Action::RestartSdma { ring: c.id }, Action::TestRing { ring: c.id, fence_value: 1 }]);
    assert_eq!(f.outcome, Outcome::Recovered(Scope::Queue));
    assert_eq!(c.d.counters().reset, 0, "only its own context lost anything");
    assert_eq!(c.reinits, 0);
}

#[test]
fn when_gc_will_not_come_back_the_default_policy_asks_for_a_reboot() {
    let mut c = Chip::new(Kind::Gfx, RingClass::GfxDirect, 1000);
    c.submit(1, &wait_never());
    c.tick(1);
    c.tick(1000);
    c.fail_reinit = true;
    let (acts, f) = c.recover();
    assert!(acts.contains(&Action::ReinitGc));
    assert_eq!(f.outcome, Outcome::NeedsReboot { mode2: Some(MODE2_UNMEASURED) });
    assert_eq!(f.fates[0].fate.errno(), errno::ETIME);
    assert!(c.d.is_dead());
}

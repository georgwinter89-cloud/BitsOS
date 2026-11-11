//! GX-8c: the boot's bring-up (`hw::boot::run`) on the fake chain: the stages as `Rig::chain` runs them, the proof, a node
//! slot of its own, and the stop as psplive's `stop_kept` does it. Time is the test's: nothing waits out the bound here;
//! the bound on the wall clock and the desktop's wait are host/tests/gx/10-boot.sh. Nothing touches a device or a screen.

use crate::bo::BoManager;
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::{self, Crash, StartAs};
use crate::gpucore::gcfake::GcHostile;
use crate::gpucore::gfxring::{self, Variant};
use crate::gpucore::handoff::{self, GcFate, RingFate, StopNote, UnloadAddrs};
use crate::gpucore::hubfake::HubHostile;
use crate::gpucore::mes;
use crate::gpucore::mesfake::MesHostile;
use crate::gpucore::psp::{Carve, Hw, Timeouts, Until};
use crate::gpucore::pspbringup::{self, GfxInputs, Inputs, Outcome as Chain};
use crate::gpucore::pspfake::{self, FakeGpu, Hostile, MapSource};
use crate::gpucore::sdma;
use crate::gpucore::sdmafake::SdmaHostile;
use crate::gpucore::smu::SmuTimeouts;
use crate::gpucore::smufake::SmuHostile;
use crate::gpucore::ucode::ListOpts;
use crate::gpucore::{classify, fwload};
use crate::hw::boot::{self, BOUND_NS, Chip, Fail, Outcome, Report, StopEnd};
use crate::hw::check;
use crate::hw::jobs::JobCfg;
use crate::hw::live::{NodeSlot, ToolGate};
use crate::ipmod::gmc11::Hub;
use crate::ktests_bringup::{GOP, Owner, STARTS, one_variant};
use crate::ktests_hw::Rig;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use ktest::kassert;

const SIZE: u64 = 512 << 20;
/// A deadline that nothing here reaches by itself.
const DEADLINE: u64 = BOUND_NS;

/// What goes wrong after the stages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    /// The chip's facts cannot be read.
    Device,
    /// `hw::check::run` meets GPU memory that fails.
    ProofMemory,
    /// The job half does not start.
    Jobs,
    /// The slot holds a node already.
    NodeTaken,
}

/// A run the stop finds: before or after the backend went onto the chain.
enum Run {
    Bare(Box<Chain>, Box<Owner>),
    Rigged(Box<Chain>, Rig),
}

/// `run`'s chip.
struct FakeBoot {
    d: Discovery,
    start: StartAs,
    hostile: Hostile,
    setup: fn(&mut FakeGpu),
    fault: Fault,
    /// Test time the stages and the proof take.
    chain_ns: u64,
    proof_ns: u64,
    /// Looks at the stick that find nothing.
    no_stick: Cell<u32>,
    clock: Cell<u64>,
    gate: ToolGate,
    slot: NodeSlot,
    run: RefCell<Option<Run>>,
    backends: Cell<u32>,
    stops: Cell<u32>,
    /// The register writes of a run the chain refused (`usize::MAX`: none was refused).
    refused_writes: Cell<usize>,
}

fn no_setup(_: &mut FakeGpu) {}

impl FakeBoot {
    fn new(d: &Discovery) -> FakeBoot {
        FakeBoot {
            d: d.clone(),
            start: StartAs::Cold,
            hostile: Hostile::None,
            setup: no_setup,
            fault: Fault::None,
            chain_ns: 1_000_000_000,
            proof_ns: 1_000_000_000,
            no_stick: Cell::new(0),
            clock: Cell::new(0),
            gate: ToolGate::new(),
            slot: NodeSlot::new(),
            run: RefCell::new(None),
            backends: Cell::new(0),
            stops: Cell::new(0),
            refused_writes: Cell::new(usize::MAX),
        }
    }

    fn pass(&self) -> u64 {
        self.clock.get()
    }

    fn tick(&self, ns: u64) {
        self.clock.set(self.pass() + ns);
    }

    /// The fake as the run left it: `f` over it.
    fn look<T>(&self, f: impl FnOnce(&mut FakeGpu) -> T) -> Option<T> {
        match self.run.borrow_mut().as_mut()? {
            Run::Bare(_, o) => Some(f(o.hw().hw())),
            Run::Rigged(_, r) => Some(r.fake(f)),
        }
    }

    /// After a stop: no engine runs and nothing touched the picture.
    fn check_stopped(&self, what: &str) -> ktest::TestResult {
        self.look(|f| {
            kassert!(!f.mes.running() && !f.gfx.running() && !f.sdma.running(), "{what}: an engine runs: mes {} gfx {} sdma {}", f.mes.running(), f.gfx.running(), f.sdma.running());
            kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty(), "{what}: the picture was touched: {:x?} {:x?}", f.display_writes, f.scanout_writes);
            Ok(())
        })
        .unwrap_or(Ok(()))
    }

    /// Everything after the stages, on a run that passed them.
    fn up(&self, out: Box<Chain>, owner: Box<Owner>, bo: BoManager, node_by: u64, lines: &mut Vec<String>) -> Result<u32, Fail> {
        let rig = match Rig::on_chain(&self.d, SIZE, &out, owner, bo) {
            Ok(r) => r,
            Err(why) => {
                lines.push(format!("backend: {why}: FAIL"));
                return Err(Fail::Device);
            }
        };
        if self.fault == Fault::ProofMemory {
            rig.fake(|f| f.hostile = Hostile::MemFail);
        }
        let gpu = rig.gpu.clone();
        *self.run.borrow_mut() = Some(Run::Rigged(out, rig));
        if self.fault == Fault::Device {
            lines.push(String::from("backend: GB_ADDR_CONFIG did not read: FAIL, no node"));
            return Err(Fail::Device);
        }
        let held = || drop(gpu.stop_jobs());
        let mut proof = check::run(&gpu);
        if let Ok(l) = &mut proof {
            let jobs = if self.fault == Fault::Jobs {
                Err(String::from("no job thread"))
            } else {
                self.with_rig(|r| r.start_jobs(JobCfg::default())).and_then(|()| check::jobs(&gpu))
            };
            match jobs {
                Ok(j) => l.extend(j),
                Err(why) => proof = Err(format!("jobs: {why}")),
            }
        }
        self.tick(self.proof_ns);
        if self.fault == Fault::ProofMemory {
            self.look(|f| f.hostile = Hostile::None);
        }
        match proof {
            Ok(l) => lines.extend(l),
            Err(why) => {
                lines.push(format!("backend check: FAIL ({why})"));
                held();
                return Err(Fail::Proof);
            }
        }
        if !boot::in_time(self.pass(), node_by) {
            lines.push(String::from("backend: past the boot's bound before the node: FAIL, no node"));
            held();
            return Err(Fail::Late);
        }
        if self.fault == Fault::NodeTaken {
            self.slot.register(gpu.clone()).map_err(|_| Fail::Node)?;
        }
        match self.slot.register(gpu.clone()) {
            Ok(m) => {
                lines.push(format!("backend: renderD{m} up"));
                Ok(m)
            }
            Err(why) => {
                lines.push(format!("backend: no node: FAIL {why}"));
                held();
                Err(Fail::Node)
            }
        }
    }

    fn with_rig<T>(&self, f: impl FnOnce(&Rig) -> T) -> T {
        let g = self.run.borrow();
        match &*g {
            Some(Run::Rigged(_, r)) => f(r),
            // `up` set it just before.
            _ => unreachable!("no rig"),
        }
    }
}

impl Chip for FakeBoot {
    fn gate(&self) -> &ToolGate {
        &self.gate
    }
    fn now_ns(&self) -> u64 {
        self.pass()
    }
    fn sleep_ns(&self, ns: u64) {
        self.tick(ns);
    }
    fn stick_ready(&self) -> bool {
        let n = self.no_stick.get();
        self.no_stick.set(n.saturating_sub(1));
        n == 0
    }
    fn backend(&self, node_by: u64, lines: &mut Vec<String>) -> Result<u32, Fail> {
        self.backends.set(self.backends.get() + 1);
        if let Some(m) = self.slot.minor() {
            lines.push(format!("backend: already up on renderD{m}"));
            return Ok(m);
        }
        let (out, owner, bo, start) = one_variant(&self.d, SIZE, self.start, Until::Backend, true, self.hostile, &ListOpts::PSP13_0_4, &[], GOP, Variant::DEFAULT, &self.setup).map_err(|e| {
            lines.push(format!("backend: FAIL {e}"));
            Fail::Refused
        })?;
        // As `psplive::fresh`: a start state not admitted is refused before the first write.
        if let Err(why) = pspbringup::admit(start) {
            lines.push(format!("refused: {why}"));
            self.refused_writes.set(out.entries.len());
            return Err(Fail::Refused);
        }
        self.tick(self.chain_ns);
        if !out.passed() {
            lines.push(format!("backend: the stages FAIL: {:?}", out.checks.iter().filter(|c| !c.pass).map(|c| c.name).collect::<Vec<_>>()));
            *self.run.borrow_mut() = Some(Run::Bare(out, owner));
            return Err(Fail::Stage(Until::Backend));
        }
        if !boot::in_time(self.pass(), node_by) {
            lines.push(String::from("backend: past the boot's bound after the stages: FAIL, no node"));
            *self.run.borrow_mut() = Some(Run::Bare(out, owner));
            return Err(Fail::Late);
        }
        self.up(out, owner, bo, node_by, lines)
    }
    fn stop(&self, lines: &mut Vec<String>) -> Result<bool, ()> {
        self.stops.set(self.stops.get() + 1);
        match self.slot.stop_gate() {
            Ok(n) => lines.extend(n),
            Err(why) => {
                lines.push(why);
                return Err(());
            }
        }
        let mut g = self.run.borrow_mut();
        let Some(run) = g.as_mut() else {
            lines.push(String::from("no ring kept: nothing to stop"));
            return Ok(true);
        };
        match run {
            Run::Bare(out, owner) => Ok(stop_chain(&self.d, owner, out, lines)),
            Run::Rigged(out, rig) => {
                drop(rig.gpu.stop_jobs());
                Ok(rig.with_owner(|o| stop_chain(&self.d, o, out, lines)))
            }
        }
    }
}

/// psplive's `stop_kept` on the fake's owner: MES, the CP, SDMA, GC unloaded toward PMFW, the PSP. True: the PSP holds nothing.
#[inline(never)]
fn stop_chain(d: &Discovery, owner: &mut Owner, out: &mut Chain, lines: &mut Vec<String>) -> bool {
    if let (Some(a), Some(m)) = (mes::MesAddrs::from_discovery(d), &out.mes) {
        let st = mes::stop(owner, &a, m.layout.map(|l| l.bo_mc), !m.mapped.is_empty());
        lines.extend(st.lines);
    }
    if let (Some(a), true) = (gfxring::GfxAddrs::from_discovery(d), out.gfx_ring.is_some() || out.mes.is_some()) {
        gfxring::halt(owner, &a);
    }
    if let (Some(a), true) = (sdma::SdmaAddrs::from_discovery(d), out.sdma.is_some()) {
        sdma::halt(owner, &a);
    }
    let gc = match &out.gfx {
        Some(g) if g.error.is_none() && g.gc_touched => GcFate::Booted,
        Some(g) if g.gc_touched => GcFate::Unsettled,
        _ => GcFate::Untouched,
    };
    let un = GfxInputs::from_discovery(d).map(|g| UnloadAddrs { smu: g.smu, gc: g.gc });
    let (_, told) = handoff::unload_gc(owner, gc, un.as_ref(), SmuTimeouts::LINUX, false);
    lines.extend(told.map(|u| format!("stop: GC unloaded: {}", u.text())));
    if out.quarantined.is_some() {
        lines.push(String::from("stop: the PSP was parked: it may still hold its buffers"));
        return false;
    }
    let Some(p) = out.kept.as_mut() else {
        lines.push(String::from("stop: no ring kept"));
        return true;
    };
    let r = p.stop(owner);
    lines.push(format!("stop: {r:?}"));
    r.is_ok() && p.quiet()
}

fn failed_at(r: &Report) -> Result<(Fail, StopEnd), String> {
    match &r.outcome {
        Outcome::Failed { fail, stop, why } => {
            kassert!(!why.is_empty(), "no reason: {:?}", r.lines);
            Ok((*fail, *stop))
        }
        Outcome::Up(m) => Err(format!("up on {m}")),
    }
}

/// Up from every state a boot can find the chip in that a chain passes from (a Reload's among them), the node there, the
/// gate free; `until=backend` answers "already up", `until=stop` takes it down, with no engine left.
fn up() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    for &s in STARTS {
        // Chips the chain refuses: `fail_stages`.
        if matches!(s, StartAs::PspDown | StartAs::AfterCrash(Crash::SmuUnanswered)) {
            continue;
        }
        let mut c = FakeBoot::new(&d);
        c.start = s;
        let count = drm::device::devices().len();
        let r = boot::run(&c, DEADLINE);
        let Outcome::Up(m) = r.outcome else { return Err(format!("{s:?}: {:?} {:?}", r.outcome, r.lines)) };
        kassert!(c.slot.minor() == Some(m) && drm::device::devices().len() == count + 1, "{s:?}: the node");
        kassert!(c.gate.enter().is_some(), "{s:?}: the gate stayed shut");
        kassert!(c.stops.get() == 0 && r.took_ns == 2_000_000_000, "{s:?}: stops {} took {}", c.stops.get(), r.took_ns);
        kassert!(boot::render(&r).starts_with("gx_bringup: PASS\n"), "{s:?}: {}", boot::render(&r));
        // The tool on the chip the boot brought up.
        let mut lines = Vec::new();
        kassert!(c.backend(DEADLINE, &mut lines) == Ok(m) && lines.iter().any(|l| l.contains("already up")) && c.backends.get() == 2, "{s:?}: until=backend: {lines:?}");
        kassert!(c.stop(&mut lines) == Ok(true), "{s:?}: until=stop: {lines:?}");
        c.check_stopped("until=stop")?;
        kassert!(c.slot.minor().is_none() && drm::device::devices().len() == count, "{s:?}: the node stayed");
    }
    Ok(())
}
ktest::ktest!("gx.hw.boot_up", Safe, up);

/// A failure at each stage: its word, the stop run as `until=stop` does, no node, the gate free, the picture untouched.
fn fail_stages() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    let stage = Fail::Stage(Until::Backend);
    let case: &[(&str, Hostile, fn(&mut FakeGpu), Fail, StopEnd)] = &[
        ("psp: never ready", Hostile::NotReady, no_setup, stage, StopEnd::Stopped),
        ("psp: no fence", Hostile::NoFence, no_setup, stage, StopEnd::Left),
        ("smu: PMFW dead", Hostile::None, |f| f.smu.hostile.push(SmuHostile::Dead), stage, StopEnd::Stopped),
        ("gc: the bootload never ends", Hostile::None, |f| f.gc.hostile = GcHostile::AutoloadNever, stage, StopEnd::Stopped),
        ("gmc: no flush ack", Hostile::None, |f| f.hubs.hostile = HubHostile::NoAck(Hub::Gfx), stage, StopEnd::Stopped),
        ("sdma: the engine never runs", Hostile::None, |f| f.sdma.hostile = SdmaHostile::Dead, stage, StopEnd::Stopped),
        ("mes: no firmware", Hostile::None, |f| f.mes.hostile = MesHostile::NoFirmware, stage, StopEnd::Stopped),
        ("mes: KIQ dead", Hostile::None, |f| f.mes.hostile = MesHostile::KiqDead, stage, StopEnd::Stopped),
        ("mes: the queues refused", Hostile::None, |f| f.mes.hostile = MesHostile::MapRefused, stage, StopEnd::Stopped),
        ("ring tests: the queues dead", Hostile::None, |f| f.mes.hostile = MesHostile::QueuesDead, stage, StopEnd::Stopped),
    ];
    for &(what, hostile, setup, want, stop) in case {
        let mut c = FakeBoot::new(&d);
        c.hostile = hostile;
        c.setup = setup;
        let r = boot::run(&c, DEADLINE);
        let (fail, ended) = failed_at(&r).map_err(|e| format!("{what}: {e}"))?;
        kassert!(fail == want && ended == stop, "{what}: {fail:?} {ended:?}: {:?}", r.lines);
        kassert!(c.stops.get() == 1 && c.slot.minor().is_none(), "{what}: stops {}, node {:?}", c.stops.get(), c.slot.minor());
        kassert!(c.gate.enter().is_some(), "{what}: the gate stayed shut");
        kassert!(boot::render(&r).starts_with("gx_bringup: FAIL\n"), "{what}");
        if ended == StopEnd::Stopped {
            c.check_stopped(what)?;
        }
    }
    // A chip the chain refuses as it finds it: nothing written, so nothing to stop.
    let mut c = FakeBoot::new(&d);
    c.start = StartAs::PspDown;
    let r = boot::run(&c, DEADLINE);
    let (fail, ended) = failed_at(&r)?;
    kassert!(fail == Fail::Refused && ended == StopEnd::NotNeeded && c.stops.get() == 0 && c.slot.minor().is_none(), "psp down: {fail:?} {ended:?}: {:?}", r.lines);
    kassert!(c.refused_writes.get() == 0, "a refusal wrote {} registers", c.refused_writes.get());
    kassert!(c.gate.enter().is_some(), "psp down: the gate stayed shut");
    // PMFW stuck: admitted, the stage ends before anything loads, and the stop comes as after any stage.
    let mut c = FakeBoot::new(&d);
    c.start = StartAs::AfterCrash(Crash::SmuUnanswered);
    let r = boot::run(&c, DEADLINE);
    let (fail, ended) = failed_at(&r)?;
    kassert!(fail == stage && ended == StopEnd::Stopped && c.stops.get() == 1 && c.slot.minor().is_none(), "pmfw stuck: {fail:?} {ended:?}: {:?}", r.lines);
    c.check_stopped("pmfw stuck")?;
    Ok(())
}
ktest::ktest!("gx.hw.boot_fail_stages", Safe, fail_stages);

/// After the stages: the facts, the proof, the jobs, the node.
fn fail_after() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    for (what, fault, want) in [("facts", Fault::Device, Fail::Device), ("proof", Fault::ProofMemory, Fail::Proof), ("jobs", Fault::Jobs, Fail::Proof), ("node", Fault::NodeTaken, Fail::Node)] {
        let mut c = FakeBoot::new(&d);
        c.fault = fault;
        let count = drm::device::devices().len();
        let r = boot::run(&c, DEADLINE);
        let (fail, ended) = failed_at(&r).map_err(|e| format!("{what}: {e}"))?;
        kassert!(fail == want && ended == StopEnd::Stopped, "{what}: {fail:?} {ended:?}: {:?}", r.lines);
        kassert!(c.slot.minor().is_none() && drm::device::devices().len() == count, "{what}: a node stayed");
        c.check_stopped(what)?;
    }
    Ok(())
}
ktest::ktest!("gx.hw.boot_fail_after", Safe, fail_after);

/// The bound: past it no node comes, whichever part took the time, and the chip is stopped. The stick's wait counts, and
/// a stick that never comes touches nothing. A `gx_bringup` over MCP keeps the chip.
fn bound() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    for (what, chain, proof) in [("stages", BOUND_NS + 1, 0), ("proof", 1, BOUND_NS)] {
        let mut c = FakeBoot::new(&d);
        (c.chain_ns, c.proof_ns) = (chain, proof);
        let r = boot::run(&c, DEADLINE);
        let (fail, ended) = failed_at(&r).map_err(|e| format!("{what}: {e}"))?;
        kassert!(fail == Fail::Late && ended == StopEnd::Stopped && c.slot.minor().is_none(), "{what}: {fail:?} {ended:?} {:?}", r.lines);
        c.check_stopped(what)?;
    }
    // Exactly at the bound is still in time.
    let mut c = FakeBoot::new(&d);
    (c.chain_ns, c.proof_ns) = (BOUND_NS, 0);
    let r = boot::run(&c, DEADLINE);
    kassert!(matches!(r.outcome, Outcome::Up(_)), "at the bound: {:?} {:?}", r.outcome, r.lines);
    kassert!(c.stop(&mut Vec::new()) == Ok(true), "stop");
    // The stick comes late: its wait is taken from the same bound.
    let c = FakeBoot::new(&d);
    let polls = (BOUND_NS / boot::STICK_POLL_NS - 1) as u32;
    c.no_stick.set(polls);
    let r = boot::run(&c, DEADLINE);
    let (fail, ended) = failed_at(&r)?;
    kassert!(fail == Fail::Late && ended == StopEnd::Stopped && c.backends.get() == 1, "late stick: {fail:?} {ended:?} {:?}", r.lines);
    // No stick: nothing touched, nothing to stop.
    let c = FakeBoot::new(&d);
    c.no_stick.set(u32::MAX);
    let r = boot::run(&c, DEADLINE);
    let (fail, ended) = failed_at(&r)?;
    kassert!(fail == Fail::NoStick && ended == StopEnd::NotNeeded && c.backends.get() == 0 && c.stops.get() == 0, "no stick: {fail:?} {ended:?}");
    kassert!(c.pass() > DEADLINE && c.pass() <= DEADLINE + boot::STICK_POLL_NS, "no stick: ended at {}", c.pass());
    // A call over MCP holds the gate: not ours to start or to stop.
    let c = FakeBoot::new(&d);
    let held = c.gate.enter().ok_or("an idle gate")?;
    let r = boot::run(&c, DEADLINE);
    let (fail, ended) = failed_at(&r)?;
    kassert!(fail == Fail::Busy && ended == StopEnd::NotNeeded && c.backends.get() == 0 && c.stops.get() == 0, "busy: {fail:?} {ended:?}");
    drop(held);
    Ok(())
}
ktest::ktest!("gx.hw.boot_bound", Safe, bound);

/// One run of `Until::Backend` on the owner as a stop left it: the classifier with the note it reads across a Reload.
#[inline(never)]
fn again(d: &Discovery, owner: &mut Owner, bo: &mut BoManager, note: &StopNote) -> Result<(Box<Chain>, classify::Start), String> {
    let opts = ListOpts::PSP13_0_4;
    let (fw, _) = fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), &opts).map_err(|e| format!("{e:?}"))?;
    let list = fw.load_list(&opts).map_err(|e| format!("{e:?}"))?;
    let f = owner.hw().hw();
    let st = classify::classify_with(d, &mut |b| f.reg_read(b), Some(note));
    let sys = f.sdma.sys_alloc();
    let c = amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, SIZE);
    let inp = Inputs {
        addrs: f.a,
        carve: Carve { mc_base: f.fb_mc, phys_base: f.fb_phys },
        start: st.start,
        until: Until::Backend,
        fw: &fw,
        list: &list,
        autoload: opts.autoload,
        keep: true,
        t: Timeouts::LINUX,
        gfx: GfxInputs::from_discovery(d),
        gmc: c.and_then(|c| crate::gpucore::gmc::GmcInputs::new(d, c)),
        sdma: c.and_then(|c| sdma::SdmaInputs::new(d, c, sdma::burst_nop(&fw))).map(|x| sdma::SdmaInputs { sys: Some(&sys), ..x }),
        gfx_ring: c.and_then(|c| gfxring::GfxRingInputs::new(d, c)),
        mes: c.and_then(|c| mes::MesInputs::new(d, c, fw.mes_uc_starts())),
        vm: crate::gpucore::vmring::VmAddrs::from_discovery(d),
        unload_first: pspbringup::unload_first(st.start, note.gc == GcFate::Unloaded),
    };
    Ok((pspbringup::run(owner, bo, &inp), st.start))
}

/// A chip brought up, stopped as a Reload's kernel leaves it and handed on by the note: the whole chain and the proof again,
/// three times. The next kernel's statics count no run, so no `again` is asked for.
fn after_reload() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    let (mut out, mut owner, mut bo, _) = one_variant(&d, SIZE, StartAs::Cold, Until::Backend, true, Hostile::None, &ListOpts::PSP13_0_4, &[], GOP, Variant::DEFAULT, &no_setup)?;
    kassert!(out.passed(), "first run");
    for round in 0..3 {
        let mut lines = Vec::new();
        kassert!(stop_chain(&d, &mut owner, &mut out, &mut lines), "round {round}: stop: {lines:?}");
        let p = out.kept.take().ok_or("not kept")?;
        p.free(&mut bo);
        let a = owner.hw().hw().a;
        let note = handoff::read_note(owner.hw(), &a, RingFate::Destroyed, GcFate::Unloaded, true, false);
        let (next, start) = again(&d, &mut owner, &mut bo, &note)?;
        kassert!(start == classify::Start::AfterBitsOs, "round {round}: start {start:?}");
        kassert!(next.passed(), "round {round}: {:?} {:?}", next.error, next.checks.iter().filter(|c| !c.pass).map(|c| (c.name, c.why.as_str())).collect::<Vec<_>>());
        out = next;
    }
    let rig = Rig::on_chain(&d, SIZE, &out, owner, bo)?;
    check::run(&rig.gpu)?;
    rig.finish()
}
ktest::ktest!("gx.hw.boot_after_reload", Safe, after_reload);

/// `hw::boot::spawn` as the kernel's call starts it, in the scenario's boot: the thread takes the hold, lets go at its end,
/// and the report stands. The state is the boot's own, so only this boot may play with it.
fn thread() -> ktest::TestResult {
    use crate::hw::boot::{Script, ScriptChip};
    for (script, up) in [(Script::Up, true), (Script::Fail, false)] {
        boot::spawn(ScriptChip { script, gate: ToolGate::new() }, BOUND_NS);
        kassert!(console::bootscreen::desktop_hold().is_some() || !boot::answer().contains("still running"), "no hold while it runs");
        let t0 = crate::hw::clock_ns();
        while console::bootscreen::desktop_hold().is_some() {
            kassert!(crate::hw::clock_ns().saturating_sub(t0) < BOUND_NS, "{script:?}: still held past the bound");
            sched::sleep_ns(10_000_000);
        }
        let a = boot::answer();
        let want = if up { "gx_bringup: PASS\nboot: up on renderD" } else { "gx_bringup: FAIL\nboot: failed at sdma, stop done" };
        kassert!(a.starts_with(want), "{script:?}: {a}");
    }
    Ok(())
}
ktest::ktest!("gx.hw.boot_thread", Run("gx/10-boot"), thread);

/// What the boot's run left, in `gx/15-boot-gpu`'s boots, after it ended: the desktop let go, and a render node only when it
/// came up. The failing boots have `drm=off`, so any device is one that the run left behind.
fn report() -> ktest::TestResult {
    kassert!(console::bootscreen::desktop_hold().is_none(), "the desktop is still held");
    let a = boot::answer();
    if a.starts_with("gx_bringup: PASS\nboot: up on renderD128") {
        kassert!(drm::device::by_render_minor(128).is_some(), "up, and no render node: {a}");
    } else if a.starts_with("gx_bringup: FAIL\nboot: failed at") {
        kassert!(a.contains(", stop done"), "not stopped: {a}");
        kassert!(drm::device::devices().is_empty(), "failed, and a render node stands: {a}");
    } else {
        return Err(format!("no end of the boot's run: {a}"));
    }
    Ok(())
}
ktest::ktest!("gx.hw.boot_report", Run("gx/15-boot-gpu"), report);

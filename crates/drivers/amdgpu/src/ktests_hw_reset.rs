//! GX-9a: the fake chain's full reset: the stop and the bring-up `psplive` runs on the chip (`stop_chain`, `reinit_chip`), on
//! the same fakes, with the switches a test wants of a chip that does not play along.

use crate::bo::BoManager;
use crate::gpucore::classify;
use crate::gpucore::disc::Discovery;
use crate::gpucore::fwload;
use crate::gpucore::gfxring::{self, GfxAddrs, GfxRingInputs};
use crate::gpucore::gfxup;
use crate::gpucore::gmc::GmcInputs;
use crate::gpucore::handoff::{self, GcFate, RingFate, StopNote, UnloadAddrs};
use crate::gpucore::mes::{self, MesAddrs, MesInputs};
use crate::gpucore::psp::{Carve, Hw, Psp, Timeouts, Until};
use crate::gpucore::pspbringup::{self, GfxInputs, Inputs};
use crate::gpucore::pspfake::{self, MapSource};
use crate::gpucore::sdma::{self, SdmaAddrs, SdmaInputs};
use crate::gpucore::smu::SmuTimeouts;
use crate::gpucore::ucode::ListOpts;
use crate::gpucore::vmring::{self, VmAddrs, VmInputs};
use crate::hw::chain::Reinit;
use crate::hw::live::HaltProof;
use crate::ipmod::psp13::Mailbox13;
use crate::ktests_bringup::Owner;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use ksync::SpinLock;

pub(crate) type Shared = Arc<SpinLock<Box<Owner>>>;

/// The switches and counts of a test's chain.
#[derive(Default)]
pub(crate) struct Knobs {
    /// This many re-inits fail at their first stage.
    pub reinit_fails: AtomicU32,
    /// The re-init says the picture moved (the fake's own DCN never does).
    pub picture_moves: AtomicBool,
    /// The re-init wipes what the backend holds in the carve-out.
    pub wipes_vram: AtomicBool,
    /// The step sits still until the test says so, for at most 20 s (a chain that does not answer in time).
    pub reinit_holds: AtomicBool,
    pub suspends: AtomicU32,
    pub mode2s: AtomicU32,
    pub reinits: AtomicU32,
    pub proven: AtomicU32,
    /// A re-init's thread has ended.
    pub reinit_ended: AtomicU32,
    /// A re-init's stop takes the chip's bad mood along (what a reset is for).
    pub heals: AtomicBool,
    /// The chain's next stop cannot halt an engine (the chain goes all the same, as in `stop_chain`).
    pub halt_fails: AtomicBool,
    /// `Chain::reset_abandoned`: the ladder went on without the step.
    pub abandoned: AtomicBool,
}

pub(crate) struct FakeReset {
    d: Discovery,
    size: u64,
    bos: Arc<SpinLock<BoManager>>,
    slabs: Arc<SpinLock<Vec<Range<u64>>>>,
    /// The chain's PSP while it runs. None: no chain to stop (a suspend's, or a rig that did not keep it).
    psp: SpinLock<Option<Psp<Mailbox13>>>,
    note: SpinLock<Option<StopNote>>,
    vmi: SpinLock<VmInputs>,
    proof: HaltProof,
    pub knobs: Knobs,
}

impl FakeReset {
    pub(crate) fn new(d: &Discovery, size: u64, bos: Arc<SpinLock<BoManager>>, slabs: Arc<SpinLock<Vec<Range<u64>>>>, vmi: VmInputs) -> FakeReset {
        FakeReset { d: d.clone(), size, bos, slabs, psp: SpinLock::named("gx.ktest.reset_psp", None), note: SpinLock::named("gx.ktest.reset_note", None), vmi: SpinLock::named("gx.ktest.reset_vmi", vmi), proof: HaltProof::new(), knobs: Knobs::default() }
    }

    pub(crate) fn keep_psp(&self, p: Option<Psp<Mailbox13>>) {
        *self.psp.lock() = p;
    }

    /// As `stop_chain`, in Linux's order. True: every engine halted, the PSP's rings down.
    fn stop(&self, o: &mut Owner, bo: &mut BoManager, lines: &mut Vec<String>) -> bool {
        let Some(mut p) = self.psp.lock().take() else {
            lines.push(String::from("fake reset: no chain kept"));
            return self.proof.proven();
        };
        let d = &self.d;
        let (Some(ma), Some(xa), Some(sa), Some(g)) = (MesAddrs::from_discovery(d), GfxAddrs::from_discovery(d), SdmaAddrs::from_discovery(d), GfxInputs::from_discovery(d)) else {
            lines.push(String::from("fake reset: no addresses"));
            return false;
        };
        let bo_mc = self.vmi.lock().mes.bo_mc;
        let st = mes::stop(o, &ma, Some(bo_mc), true);
        lines.extend(st.lines);
        let halted = gfxring::halt(o, &xa) && sdma::halt(o, &sa) && st.ok && !self.knobs.halt_fails.swap(false, Ordering::AcqRel);
        let u = UnloadAddrs { smu: g.smu, gc: g.gc };
        let (gc, _) = handoff::unload_gc(o, GcFate::Booted, Some(&u), SmuTimeouts::LINUX, false);
        let stopped = p.stop(o);
        lines.push(format!("fake reset: the stop: {stopped:?}, engines halted: {halted}"));
        let quiet = stopped.is_ok() && p.quiet();
        if quiet {
            p.free(bo);
        }
        let a = o.hw().hw().a;
        *self.note.lock() = Some(handoff::read_note(o.hw(), &a, RingFate::Destroyed, gc, true, false));
        self.proof.stop_ran(halted && quiet, true);
        halted && quiet
    }

    pub(crate) fn suspend(&self, owner: &Shared, lines: &mut Vec<String>) -> bool {
        self.knobs.suspends.fetch_add(1, Ordering::AcqRel);
        let mut bo = self.bos.lock();
        let mut o = owner.lock();
        self.stop(&mut o, &mut bo, lines)
    }

    pub(crate) fn mode2(&self, owner: &Shared, lines: &mut Vec<String>) -> Option<bool> {
        self.knobs.mode2s.fetch_add(1, Ordering::AcqRel);
        let g = GfxInputs::from_discovery(&self.d)?;
        let mut o = owner.lock();
        let r = gfxup::mode2(&mut **o, g.smu, &g.gc, &g.t);
        lines.extend(gfxup::render(&r));
        (r.error.is_none()).then(|| o.hw().total() == 0)
    }

    pub(crate) fn reinit(&self, owner: &Shared, lines: &mut Vec<String>) -> Option<Reinit> {
        let r = self.reinit_inner(owner, lines);
        self.knobs.reinit_ended.fetch_add(1, Ordering::AcqRel);
        r
    }

    fn reinit_inner(&self, owner: &Shared, lines: &mut Vec<String>) -> Option<Reinit> {
        self.knobs.reinits.fetch_add(1, Ordering::AcqRel);
        for _ in 0..4000 {
            if !self.knobs.reinit_holds.load(Ordering::Acquire) {
                break;
            }
            // Host tests have no second thread and never set it.
            #[cfg(not(test))]
            sched::sleep_ns(5_000_000);
        }
        if self.knobs.abandoned.load(Ordering::Acquire) {
            lines.push(String::from("fake reset: the ladder went on without this step"));
            return None;
        }
        let d = &self.d;
        let opts = ListOpts::PSP13_0_4;
        let (fw, _) = fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), &opts).ok()?;
        let list = fw.load_list(&opts).ok()?;
        let mut bo = self.bos.lock();
        let mut o = owner.lock();
        if !self.stop(&mut o, &mut bo, lines) {
            return None;
        }
        if self.knobs.heals.load(Ordering::Acquire) {
            o.hw().hw().mes.hostile = crate::gpucore::mesfake::MesHostile::None;
        }
        if self.knobs.reinit_fails.load(Ordering::Acquire) > 0 {
            self.knobs.reinit_fails.fetch_sub(1, Ordering::AcqRel);
            lines.push(String::from("fake reset: the first stage fails (the test's switch)"));
            return None;
        }
        let note = self.note.lock().clone();
        let f = o.hw().hw();
        let st = classify::classify_with(d, &mut |b| f.reg_read(b), note.as_ref());
        let sys = f.sdma.sys_alloc();
        let c = Carveout::new(f.fb_mc, f.fb_phys, self.size)?;
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
            gmc: GmcInputs::new(d, c),
            sdma: SdmaInputs::new(d, c, sdma::burst_nop(&fw)).map(|x| SdmaInputs { sys: Some(&sys), ..x }),
            gfx_ring: GfxRingInputs::new(d, c),
            mes: MesInputs::new(d, c, fw.mes_uc_starts()),
            vm: VmAddrs::from_discovery(d),
            unload_first: pspbringup::unload_first(st.start, note.is_some_and(|n| n.gc == GcFate::Unloaded)),
        };
        let mut out = pspbringup::run(&mut **o, &mut *bo, &inp);
        if !out.passed() {
            lines.push(format!("fake reset: the bring-up failed: {:?}", out.checks.iter().filter(|x| !x.pass).map(|x| (x.name, x.why.as_str())).collect::<Vec<_>>()));
            return None;
        }
        self.keep_psp(out.kept.take());
        let vmi = pspbringup::vm_inputs(VmAddrs::from_discovery(d)?, c, SdmaAddrs::from_discovery(d), &out)?;
        *self.vmi.lock() = vmi;
        let queues = match vmring::kept_queues(&mut **o, &vmi) {
            Ok(q) => q,
            Err(e) => {
                lines.push(format!("fake reset: a kept queue: {e:?}"));
                return None;
            }
        };
        if self.knobs.wipes_vram.load(Ordering::Acquire) {
            let f = o.hw().hw();
            for r in self.slabs.lock().iter() {
                f.wipe(c.mc_base() + r.start, r.end - r.start);
            }
        }
        lines.push(String::from("fake reset: the bring-up passed"));
        Some(Reinit { vmi, queues, display_equal: !self.knobs.picture_moves.load(Ordering::Acquire) && o.hw().total() == 0 })
    }
}

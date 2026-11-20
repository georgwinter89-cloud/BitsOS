//! Wave 44, item RG (piece 13): the stage `gcring`, the kernel rings' start once MES mapped them, as Linux's
//! `gfx_v12_0_hw_init` makes it after `kcq_resume` and `amdgpu_gfx_enable_kgq` (gfx_v12_0.c:3566-3583, :3852-3862),
//! the trace's section gc-ring (init.txt lines 9568-9603):
//!
//! | step | Linux | writes |
//! |---|---|---|
//! | gfx start | `gfx_v12_0_cp_gfx_start` :2699-2710 (the gfx enable skipped: `amdgpu_async_gfx_ring`) | CP_MAX_CONTEXT 7, CP_DEVICE_ID 1 |
//! | ring tests | `amdgpu_ring_test_helper` on gfx_0.0.0, then comp_1.0.0, comp_1.1.0, comp_1.0.1, comp_1.1.1 (:3571-3583), each `gfx_v12_0_ring_test_ring` :452-492 | SCRATCH_REG0 0xCAFEDEAD, then the ring's frame and its doorbell |
//! | interrupts | `amdgpu_irq_get` of the privileged register, privileged instruction and bad opcode faults (:3852-3862) | CP_INT_CNTL_RING0, CP_ME1_PIPE0/1_INT_CNTL, each read and written with its one bit |
//!
//! The queues' MQDs and their maps are `mesq`'s (MES's piece of system memory, bound by `mes`), which the stage
//! stands on: it writes no MQD and maps nothing. Each ring test is Linux's: the poison by MMIO, SET_UCONFIG_REG
//! of 0xDEADBEEF from the ring padded to its 256-dword frame, the write pointer's shadow and the doorbell
//! (`ringops`), SCRATCH_REG0 polled at most 100000 times 1 us apart. A test that runs out is Linux's -ETIMEDOUT:
//! the stage stops there with the register's last value, the write pointer and the read pointer's slot, and
//! after the comparison reads what shows a hung engine (`after_stop`: GRBM_STATUS and CP_STAT, reads only).
//! The section has no declared difference: no address BitsOS chooses reaches a register here.

use super::guard::{Allow, Guard};
use super::meskq::KernelQueue;
use super::messtage::{Up, q_stop};
use super::nb::Io;
use super::nbregs::NbRegs;
use super::session::Session;
use super::stage::{Stage, Stop, psp_stop, stop};
use crate::gpucore::psp::{self, Cond, Hw, LINUX_POLLS};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::gc12ring::{self as rg, f};
use rdna4_core::hubseq::Op;
use rdna4_core::mes12::{self, Kq};

/// Linux's order of the five tests: the gfx ring, then the compute rings (`mes12::KERNEL_QUEUES` holds the
/// compute queues first, as `mesq` maps them).
pub const TEST_ORDER: [usize; 5] = [4, 0, 1, 2, 3];

/// The ring's name as Linux gives it (`gfx_v12_0_gfx_ring_init` :987-1003, `compute_ring_init` :1005-1040).
pub fn ring_name(k: &Kq) -> String {
    format!("{}_{}.{}.{}", if k.gfx { "gfx" } else { "comp" }, k.me, k.pipe, k.queue)
}

/// The kernel rings in this boot.
#[derive(Clone, Debug, Default)]
pub struct State {
    pub tried: bool,
    pub passed: bool,
    /// Each queue's write pointer in dwords (`mes12::KERNEL_QUEUES`' order), as the driver keeps it.
    pub wptr: [u64; 5],
    /// Each queue's fence sequence (`amdgpu_fence_emit`'s `sync_seq`).
    pub seq: [u32; 5],
    /// The rungs (`ringrungs`): each one's name and whether it passed, the last call's.
    pub rungs: Vec<(&'static str, bool)>,
    /// The rungs' page for the IBs (a system page, kept for the boot) and where it is bound in the GART, while
    /// it is.
    pub ib_page: Option<u64>,
    pub ib_mc: Option<u64>,
    /// The end-of-pipe interrupts are on (the rungs set them, as Linux's fence driver does at its start).
    pub eop_on: bool,
    /// The Reload hook's word (`livering`, `ringoff`), set together with `tried`.
    pub reload: Option<super::sdmaoff::Written>,
}

impl State {
    pub const fn new() -> State {
        State { tried: false, passed: false, wptr: [0; 5], seq: [0; 5], rungs: Vec::new(), ib_page: None, ib_mc: None, eop_on: false, reload: None }
    }

    pub fn describe(&self) -> String {
        let rungs = if self.rungs.is_empty() { String::new() } else { format!(", rungs {}/{} passed", self.rungs.iter().filter(|r| r.1).count(), self.rungs.len()) };
        match (self.passed, self.tried) {
            (true, _) => format!("rings tested (write pointers {:?}){rungs}", self.wptr),
            (false, true) => String::from("rings stopped half-way (gcring does not run again in this boot)"),
            (false, false) => String::from("rings not tested"),
        }
    }
}

pub fn is_mine(stage: Stage) -> bool {
    matches!(stage, Stage::GcRing | Stage::RingTest)
}

/// The stage's own order: once in a boot, not after a run that wrote a register.
pub fn order(s: &State) -> Result<(), Stop> {
    if s.passed {
        return Err(stop("die Befehlsringe sind schon geprüft", "gcring ran in this boot: a second run is refused"));
    }
    if s.tried {
        return Err(stop("die Befehlsringe wurden in diesem Start schon angefasst: erst neu starten", "gcring stopped half-way in this boot after it wrote registers"));
    }
    Ok(())
}

/// What the stage `gcring` writes.
pub const WRITTEN: [rdna4_core::regmap::Reg; 6] = [rg::CP_MAX_CONTEXT, rg::CP_DEVICE_ID, rg::SCRATCH_REG0, rg::CP_INT_CNTL_RING0, rg::CP_ME1_PIPE0_INT_CNTL, rg::CP_ME1_PIPE1_INT_CNTL];

pub fn allow(stage: Stage, s: &Session, n: &NbRegs) -> Allow {
    let mut regs: Vec<u32> = WRITTEN.iter().filter_map(|r| n.byte(*r)).collect();
    let mut mem = Vec::new();
    if stage == Stage::RingTest {
        // The IB page's bind and unbind (the GART table and the flush in this boot's form) and the HDP flush
        // before a queue's map again.
        regs.extend(s.flush_written(n));
        regs.push(s.nb.hdp_flush(n.hdp_flush));
        mem.extend(s.nb.gart.as_ref().map(|g| g.table_mc()));
        // The stage reads the IH rings itself (`sdmaih`): an overflow's clear and a page fault's clear.
        regs.extend(super::sdmastage::irq_regs(n));
    }
    Allow { stage: stage.name(), regs, mem }
}

/// The guard of the rings' stages: the five kernel queues' doorbells and MES's piece (their rings and slots); the
/// rungs add MES's KIQ doorbell (the flush through MES, the unmap and map) and their IB page.
pub fn guarded<H: Hw>(g: Guard<H>, stage: Stage, s: &Session) -> Guard<H> {
    if !is_mine(stage) {
        return g;
    }
    let mut bells: Vec<u32> = mes12::KERNEL_QUEUES.iter().map(|k| k.doorbell).collect();
    let mut sys: Vec<Range<u64>> = s.mes.piece.map(|at| at..at + super::mesring::lay::PIECE).into_iter().collect();
    if stage == Stage::RingTest {
        // MES's two pipes (the unmap and map, the flush), the IH's doorbells and its read pointers' write-back,
        // which the reader the stage runs itself writes.
        bells.extend([mes12::Pipe::Sched.doorbell(), mes12::Pipe::Kiq.doorbell(), rdna4_core::ih7::DOORBELL_RING0, rdna4_core::ih7::DOORBELL_RING1]);
        sys.extend(s.rings.ib_page.map(|at| at..at + rdna4_core::pte12::PAGE));
        sys.extend(s.nb.ih.as_ref().map(|i| i.place.sys + 2 * u64::from(rdna4_core::ih7::RING_BYTES)).map(|wb| wb..wb + super::ihring::IhPlace::WB_BYTES));
        return g.with_host(alloc::vec![super::guard::HostAsk::Pages]).with_doorbells(bells).with_sys(sys);
    }
    g.with_doorbells(bells).with_sys(sys)
}

/// A register read and written with Linux's change; both values.
fn change(io: &mut Io, r: rdna4_core::regmap::Reg, f: fn(u32) -> u32) -> Result<(u32, u32), Stop> {
    let v = io.read(r)?;
    io.write(r, f(v))?;
    Ok((v, f(v)))
}

/// `gfx_v12_0_ring_test_ring` on kernel queue `i`; its write pointer moves on.
pub fn ring_test(io: &mut Io, up: &Up, i: usize, wptr: &mut u64, out: &mut Vec<String>) -> Result<(), Stop> {
    let q = KernelQueue::of(up, i).ok_or_else(|| stop("Reihenfolge stimmt nicht", "no such kernel queue"))?;
    let name = ring_name(&q.kq);
    let scratch = io.byte(rg::SCRATCH_REG0)?;
    io.write(rg::SCRATCH_REG0, amdgpu_fmt::gfx12::RING_TEST_POISON)?;
    let words = super::ringops::ring_test(scratch / 4).map_err(q_stop)?;
    *wptr = super::ringops::commit(io.card, &up.place, &q, *wptr, &words).map_err(q_stop)?;
    let hw = io.card.hw();
    let t0 = hw.now_us();
    match psp::wait_reads(hw, scratch, Cond::Masked { mask: u32::MAX, value: amdgpu_fmt::gfx12::RING_TEST_VALUE }, LINUX_POLLS, "SCRATCH_REG0 of a ring test") {
        Ok(_) => {
            out.push(format!("gcring: ring {name} test passed ({} us; doorbell dword {:#x}, write pointer {})", hw.now_us().saturating_sub(t0), q.kq.doorbell, *wptr));
            Ok(())
        }
        Err(psp::PspError::Timeout { last, .. }) => {
            let rptr = super::cpring::get_rptr(io.card, up.place.slot_sys(q.slots.rptr));
            klog::event!(Error, "gx", "rdna4.ring.test_failed", ring = name.as_str(), scratch = klog::Hex(u64::from(last)); "a kernel ring's test failed");
            let plain = if q.kq.gfx { "die Grafik-Warteschlange führt keinen Befehl aus" } else { "eine Rechen-Warteschlange führt keinen Befehl aus" };
            Err(stop(plain, format!("ring {name} test failed (-ETIMEDOUT): SCRATCH_REG0 still {last:#x} after Linux's {LINUX_POLLS} reads; the write pointer {}, the read pointer's slot {} (Linux's amdgpu_ring_test_helper logs it and hw_init fails)", *wptr, rptr.map_or(String::from("unreadable"), |r| format!("{r:#x}")))))
        }
        Err(e) => Err(psp_stop(e)),
    }
}

/// The stage `gcring`.
pub fn run(s: &mut Session, io: &mut Io, out: &mut Vec<String>) -> Result<(), Stop> {
    order(&s.rings)?;
    let Session { mes, rings, nb, .. } = s;
    // The chain's order (`in_order`, before any access) holds `mesq` passed, the kernel queues mapped.
    let up = mes.up.as_ref().ok_or_else(|| stop("zuerst die Warteschlangen beim Planer anmelden", "MES does not run in this boot"))?;
    rings.tried = true;
    if let Some(w) = &rings.reload {
        w.set();
    }
    io.write(rg::CP_MAX_CONTEXT, rg::MAX_HW_CONTEXTS - 1)?;
    io.write(rg::CP_DEVICE_ID, rg::DEVICE_ID)?;
    out.push(format!("gcring: CP_MAX_CONTEXT {} (max_hw_contexts {} - 1), CP_DEVICE_ID {}", rg::MAX_HW_CONTEXTS - 1, rg::MAX_HW_CONTEXTS, rg::DEVICE_ID));
    for i in TEST_ORDER {
        ring_test(io, up, i, &mut rings.wptr[i], out)?;
    }
    // Linux registers the CP's sources in sw_init (gfx_v12_0.c:1472-1500), before it enables the faults: from
    // here on their entries are the kernel rings' in every boot, not only in one that reaches `ringtest`.
    if let Some(ih) = nb.ih.as_mut() {
        super::ringih::register(ih);
    }
    let mut said = Vec::new();
    for op in rg::FAULT_ENABLES {
        let Op::Modify(r, f) = op else { continue };
        let (was, now) = change(io, r, f)?;
        said.push(format!("{} {was:#x} to {now:#x}", r.name));
    }
    out.push(format!("gcring: the fault interrupts on (privileged register, privileged instruction, bad opcode): {}", said.join(", ")));
    rings.passed = true;
    Ok(())
}

/// After a stop that reached the card: what shows a hung engine, read once, outside the comparison (Linux's
/// error path reads nothing; `gfx_v12_0_is_idle` and the devcoredump list name these, gfx_v12_0.c:80-215, :3938).
pub fn after_stop(hw: &mut dyn Hw, n: &NbRegs, out: &mut Vec<String>) {
    let mut rd = |r| n.byte(r).and_then(|b| hw.reg_read(b));
    let (grbm, cp) = (rd(rg::GRBM_STATUS), rd(rg::CP_STAT));
    let busy = grbm.is_some_and(|v| f::GUI_ACTIVE.get(v) != 0);
    out.push(format!(
        "gcring: after the stop GRBM_STATUS {} ({}), CP_STAT {}",
        grbm.map_or(String::from("-"), |v| format!("{v:#x}")),
        if busy { "the graphics block is busy: an engine did not finish" } else { "idle" },
        cp.map_or(String::from("-"), |v| format!("{v:#x}"))
    ));
}

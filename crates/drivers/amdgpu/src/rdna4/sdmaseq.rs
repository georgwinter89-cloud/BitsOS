//! Wave 43: how the copy engines' two stages are held against Linux's trace (`linuxseq`).
//!
//! `sdma` against the section `sdma`: the only differences declared, each an address BitsOS chooses itself (held
//! to exactly the object the stage bound), and the bind of the memory they name (plan/spec/gpu-rdna4.md section 6). The host tests, the ktest and every
//! stage call's own `linuxseq` line use this one declaration (`chainseq`).
//!
//! `sdmatest` stands in no section: Linux's IB tests run from the delayed work two seconds after init
//! (amdgpu_device.c:3005-3011) and the rungs behind them are BitsOS's own. What the trace does have of it is
//! the TLB flush after a GART change (`amdgpu_gart_invalidate_tlb`, called after a bind and after an unbind
//! alike, amdgpu_gart.c:336 and amdgpu_ttm.c:1034). The stage binds and unbinds its test pages and touches no
//! other register, so every register access of a call must be a line of that unit, in its order
//! (`rungs_verdict`).
//!
//! The engines sit on GFXHUB, so both stages run only with that hub on (`sdmastage::start`); in Linux's order
//! they come after MES, so every bind and unbind of theirs flushes in Linux's form after MES (wave 44,
//! `mesflush`): an HDP flush of its own and per hub an HDP flush and a MES command, three HDP flushes on the
//! bus. The unit is `linuxseq::bind_flush_mes` and the bind's rule `Rule::BindMesBefore`; until wave 44 it was
//! the register form of both hubs' flush (`bind_flush_both`), which only MES's own bind keeps (`messeq`).

use super::linuxseq::compare::Compare;
use super::linuxseq::fold::{Bus, fold};
use super::linuxseq::rules::{Address, Rule};
use super::linuxseq::{Line, Section, bind_flush_mes};
use super::regs14::Regs14;
use crate::gpucore::trail::Access;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::{pte12, sdma7};

/// The engines' objects as the stage bound them (GART addresses): per engine its ring, its read pointer's slot
/// and its write pointer's poll slot. Each of the six address pairs must name exactly its own object, not merely
/// some place in the GART.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bound {
    pub ring: [u64; 2],
    pub rptr: [u64; 2],
    pub wptr: [u64; 2],
}

impl Bound {
    pub fn of(p: &super::sdmaq::Place, slots: &[super::sdmaq::Slots; 2]) -> Bound {
        Bound { ring: [p.ring_mc(0), p.ring_mc(1)], rptr: slots.map(|x| p.slot_mc(x.rptr)), wptr: slots.map(|x| p.slot_mc(x.wptr)) }
    }
}

fn pair(lo: &'static str, hi: &'static str, shift: u8, align: u64, at: u64, bytes: u64, why: &'static str) -> Rule {
    Rule::Address(Address { lo, hi: Some(hi), shift, keep: 0, align, within: at..at + bytes, bytes, why })
}

/// Seven rules: one bind, and per engine its ring, its write-pointer poll slot and its read-pointer slot, each held
/// to the object the stage bound (`Bound`). While the stage bound nothing in this boot only the bind's rule: an
/// address written then has no rule and differs. The doorbell offsets and NBIF's doorbell entry are Linux's fixed
/// numbers and have none.
pub fn rules(bound: Option<Bound>) -> Vec<Rule> {
    let bind = Rule::BindMesBefore {
        reg: "SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_LO",
        why: "Linux bound the engines' rings, slots and IB pool while the blocks set up; BitsOS binds them in one bind right before it first names them, when MES runs, so the flush is Linux's form after MES: an HDP flush of its own and per hub an HDP flush and a MES command",
    };
    let Some(b) = bound else { return alloc::vec![bind] };
    let ring = u64::from(sdma7::RING_BYTES);
    alloc::vec![
        bind,
        pair("SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_LO", "SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_HI", 0, 8, b.wptr[0], 8, "engine 0's write-pointer poll slot, which BitsOS took in its write-back page"),
        pair("SDMA0_QUEUE0_RB_RPTR_ADDR_LO", "SDMA0_QUEUE0_RB_RPTR_ADDR_HI", 0, 8, b.rptr[0], 8, "engine 0's read-pointer slot, which BitsOS took in its write-back page"),
        pair("SDMA0_QUEUE0_RB_BASE", "SDMA0_QUEUE0_RB_BASE_HI", 8, pte12::PAGE, b.ring[0], ring, "engine 0's ring, where BitsOS bound it in the GART"),
        pair("SDMA1_QUEUE0_RB_WPTR_POLL_ADDR_LO", "SDMA1_QUEUE0_RB_WPTR_POLL_ADDR_HI", 0, 8, b.wptr[1], 8, "engine 1's write-pointer poll slot, which BitsOS took in its write-back page"),
        pair("SDMA1_QUEUE0_RB_RPTR_ADDR_LO", "SDMA1_QUEUE0_RB_RPTR_ADDR_HI", 0, 8, b.rptr[1], 8, "engine 1's read-pointer slot, which BitsOS took in its write-back page"),
        pair("SDMA1_QUEUE0_RB_BASE", "SDMA1_QUEUE0_RB_BASE_HI", 8, pte12::PAGE, b.ring[1], ring, "engine 1's ring, where BitsOS bound it in the GART"),
    ]
}

/// Why the rungs themselves are compared with nothing: said in the stage's `linuxseq` line.
const NO_SECTION: &str = "the rungs themselves stand in no section (Linux's IB tests run from the delayed work 2 s after init, amdgpu_device.c:3005-3011)";

/// How many whole flushes the call's register accesses are, and how many answers of the card were not the
/// trace's; or the first difference. `whole`: the call passed, so no flush may be cut short.
fn flushes(trail: &[Access], lost: u64, regs: &Regs14, whole: bool) -> Result<(usize, usize), String> {
    let unit = bind_flush_mes()?;
    if lost != 0 {
        return Err(format!("the stage's trail lost {lost} accesses: it proves no sequence"));
    }
    let did = fold(trail, &Bus::of(regs))?;
    let times = did.len().div_ceil(unit.len());
    if times == 0 {
        return Ok((0, 0));
    }
    let lines: Vec<Line> = (0..times).flat_map(|_| unit.iter().copied()).collect();
    let sec = Section { name: "mes", head: "", lines };
    let mut c = Compare::new(&sec, &[]).map_err(|e| format!("{e}"))?.answers_kept();
    c.feed(&did).map_err(|e| format!("{e}"))?;
    let answers = c.answers().len();
    if whole {
        c.finish().map_err(|e| format!("{e}"))?;
    }
    Ok((times, answers))
}

/// One call of `sdmatest` held against Linux's trace, as the last technical line of its answer. `None` for a
/// call that ended before its first register access (refused, or stopped in a rung before the test pages'
/// bind): nothing of such a call is in the trace, as with a chain stage that was refused before any access.
///
/// `irq`: the registers of Linux's interrupt handlers (`sdmastage::irq_regs`). The stage reads the IH rings
/// itself, so an overflow or a page fault that arrives while it waits is handled inside the call; those
/// accesses are the handlers', are taken out before the comparison, and the line says how many there were.
pub fn rungs_verdict(pass: bool, trail: &[Access], lost: u64, regs: &Regs14, irq: &[u32]) -> Option<String> {
    rungs_judge(pass, trail, lost, regs, irq).map(|v| v.line)
}

/// `rungs_verdict`'s typed twin (`chainseq::Verdict`), for the measurement run's stop rule.
pub fn rungs_judge(pass: bool, trail: &[Access], lost: u64, regs: &Regs14, irq: &[u32]) -> Option<super::chainseq::Verdict> {
    use super::chainseq::{Said, Verdict};
    if !pass && trail.is_empty() && lost == 0 {
        return None;
    }
    let handlers = |a: &Access| matches!(a, Access::Read { byte, .. } | Access::Write { byte, .. } if irq.contains(byte));
    let own: Vec<Access> = trail.iter().copied().filter(|a| !handlers(a)).collect();
    let handled = match trail.len() - own.len() {
        0 => String::new(),
        n => format!("; {n} accesses of Linux's interrupt handlers (an IH ring's overflow clear, a page fault's status and its clear) came while the stage read the IH rings and are not compared"),
    };
    let found = match flushes(&own, lost, regs, pass) {
        // A call that passed unbound its test pages: without that flush the card's walker still finds them.
        Ok((0, _)) if pass => Err(String::from("the call passed with no register access: the TLB flush after the test pages' unbind is missing")),
        other => other,
    };
    Some(match found {
        Ok((n, answers)) => {
            let (scope, said) = if pass { ("equal", Said::Equal) } else { ("the same as Linux's as far as the stage came", Said::AsFar) };
            let card = if answers == 0 { String::new() } else { format!("; {answers} answers of the card are not the trace's") };
            Verdict { section: "mes", said, answers, line: format!("linuxseq mes: {scope}: the call's only register accesses are {n} of the TLB flush after a GART change once MES runs (the test pages' bind, their unbind: the HDP flush of its own and one per hub, each hub's invalidation a MES command), line for line{card}{handled}; {NO_SECTION}") }
        }
        Err(e) => {
            klog::event!(Warn, "gx", "rdna4.linuxseq.differs", stage = "sdmatest", section = "mes", what = e.as_str(); "a stage's accesses are not Linux's");
            Verdict { section: "mes", said: Said::Differs, answers: 0, line: format!("linuxseq mes: differs: {e}{handled}; {NO_SECTION}") }
        }
    })
}

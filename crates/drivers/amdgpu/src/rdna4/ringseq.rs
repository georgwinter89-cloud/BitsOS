//! Wave 44, item RG: how the rings' two stages are held against Linux's trace (`linuxseq`).
//!
//! `gcring` against the section gc-ring: no declared difference (`chainseq::rules` gives it none). No address
//! BitsOS chooses reaches a register there: the rings, slots and MQDs were named to MES by memory (`mesq`), the
//! doorbells are BAR2 writes, which the trace does not hold.
//!
//! `ringtest` stands in no section: Linux's IB tests run from the delayed work two seconds after init
//! (amdgpu_device.c:3005-3011) and the rungs behind them are BitsOS's own. Its register accesses are each a form
//! the trace has, in the rungs' order, and are held against them line for line (`rungs_judge`):
//!
//! - the end-of-pipe enables of `amdgpu_fence_driver_hw_init`: the trace's section late, init.txt 9817-9822
//!   (three reads and writes), with what gc-ring left as the reads (Linux's late section writes CPC_INT_CNTL
//!   under each compute pipe's selection first, init.txt 9811-9816, which this stage does not, so the compute
//!   pipes read 0x01800000 here, not late's 0x05000000) and Linux's change of each as the writes; only in the
//!   first call of a boot;
//! - the TLB flush after the IB page's bind and after its unbind, once MES runs (`linuxseq::bind_flush_mes`);
//! - the HDP flush of `amdgpu_gfx_mes_enable_kcq` before comp_1.0.0's map again (section mes's HDP line) and
//!   the ring test after it (gc-ring's first test: the poison and its wait).

use super::chainseq::{Said, Verdict};
use super::linuxseq::compare::Compare;
use super::linuxseq::fold::{Bus, fold};
use super::linuxseq::{Line, Section, What, bind_flush_mes, section};
use super::regs14::Regs14;
use crate::gpucore::trail::Access;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::gc12ring as rg;

const NO_SECTION: &str = "the rungs themselves stand in no section (Linux's IB tests run from the delayed work 2 s after init, amdgpu_device.c:3005-3011)";

/// Where the trace's section late holds the three end-of-pipe enables (init.txt 9817-9822); the kernel does not
/// carry that section, a host test holds these lines to it.
pub const LATE_EOP_AT: u32 = 9817;

/// The end-of-pipe enables as the rungs make them: the three registers in late's order (the gfx pipe, MEC 1's
/// two), read with what gc-ring left in them, written with Linux's change.
pub fn eop_unit() -> Result<Vec<Line<'static>>, String> {
    let ring = section("gc-ring")?;
    let mut out = Vec::new();
    for (k, reg) in [rg::CP_INT_CNTL_RING0, rg::CP_ME1_PIPE0_INT_CNTL, rg::CP_ME1_PIPE1_INT_CNTL].into_iter().enumerate() {
        let last = ring.lines.iter().rev().find(|l| matches!(l.what, What::Write { name, .. } if name == reg.name)).copied().ok_or("gc-ring.txt does not write an interrupt control")?;
        let What::Write { name, dword, value } = last.what else { continue };
        let init = LATE_EOP_AT + 2 * k as u32;
        out.push(Line { at: 0, init, what: What::Read { name, dword, value, times: 1 } });
        out.push(Line { at: 0, init: init + 1, what: What::Write { name, dword, value: rg::eop_on(value) } });
    }
    Ok(out)
}

/// The HDP flush before a map and one ring test after it.
fn remap_unit() -> Result<Vec<Line<'static>>, String> {
    let mes = section("mes")?;
    let hdp = mes.lines.iter().find(|l| matches!(l.what, What::Hdp { .. })).copied().ok_or("mes.txt holds no HDP flush")?;
    let ring = section("gc-ring")?;
    let at = ring.lines.iter().position(|l| matches!(l.what, What::Write { name: "SCRATCH_REG0", .. })).ok_or("gc-ring.txt holds no ring test")?;
    let test = ring.lines.get(at..at + 2).filter(|t| matches!(t[1].what, What::Wait { .. })).ok_or("gc-ring.txt's ring test has no wait")?;
    Ok([hdp].into_iter().chain(test.iter().copied()).collect())
}

/// What a whole call of `ringtest` makes, in order: the enables (`eop`: not yet on in this boot), the bind's
/// flush, the remap's flush and test, the unbind's flush.
pub fn expected(eop: bool) -> Result<Vec<Line<'static>>, String> {
    let flush = bind_flush_mes()?;
    let mut v = if eop { eop_unit()? } else { Vec::new() };
    v.extend(flush.iter().copied());
    v.extend(remap_unit()?);
    v.extend(flush);
    Ok(v)
}

/// One call of `ringtest` held against Linux's forms, as the last technical line of its answer. `None` for a call
/// refused before any access. `irq`: the registers of Linux's interrupt handlers, taken out as for `sdmatest`.
pub fn rungs_judge(pass: bool, trail: &[Access], lost: u64, regs: &Regs14, irq: &[u32]) -> Option<Verdict> {
    if !pass && trail.is_empty() && lost == 0 {
        return None;
    }
    let handlers = |a: &Access| matches!(a, Access::Read { byte, .. } | Access::Write { byte, .. } if irq.contains(byte));
    let own: Vec<Access> = trail.iter().copied().filter(|a| !handlers(a)).collect();
    let handled = match trail.len() - own.len() {
        0 => String::new(),
        n => format!("; {n} accesses of Linux's interrupt handlers came while the stage read the IH rings and are not compared"),
    };
    // The enables come first, and only in the call that set them: a call that begins with a read of the gfx
    // pipe's interrupt control is that call.
    let mut eop = false;
    let found = (|| -> Result<usize, String> {
        if lost != 0 {
            return Err(format!("the stage's trail lost {lost} accesses: it proves no sequence"));
        }
        let did = fold(&own, &Bus::of(regs))?;
        let first = eop_unit()?.first().map(|l| match l.what { What::Read { dword, .. } => dword, _ => 0 });
        eop = matches!(did.first(), Some(super::linuxseq::fold::Did::Read { dword, .. }) if Some(*dword) == first);
        let lines = expected(eop)?;
        let sec = Section { name: "gc-ring", head: "", lines };
        let mut c = Compare::new(&sec, &[]).map_err(|e| format!("{e}"))?.answers_kept();
        c.feed(&did).map_err(|e| format!("{e}"))?;
        let answers = c.answers().len();
        if pass {
            c.finish().map_err(|e| format!("{e}"))?;
        }
        Ok(answers)
    })();
    let forms = if eop { "the end-of-pipe enables of Linux's fence driver (late's three, init.txt 9817-9822, over what gc-ring left), " } else { "" };
    Some(match found {
        Ok(answers) => {
            let (scope, said) = if pass { ("equal", Said::Equal) } else { ("the same as Linux's as far as the stage came", Said::AsFar) };
            let card = if answers == 0 { String::new() } else { format!("; {answers} answers of the card are not the trace's") };
            Verdict { section: "gc-ring", said, answers, line: format!("linuxseq gc-ring: {scope}: the call's register accesses are Linux's forms, line for line: {forms}the TLB flush through MES after the IB page's bind and after its unbind, the HDP flush and the ring test of comp_1.0.0 mapped again{card}{handled}; {NO_SECTION}") }
        }
        Err(e) => {
            klog::event!(Warn, "gx", "rdna4.linuxseq.differs", stage = "ringtest", section = "gc-ring", what = e.as_str(); "a stage's accesses are not Linux's");
            Verdict { section: "gc-ring", said: Said::Differs, answers: 0, line: format!("linuxseq gc-ring: differs: {e}{handled}; {NO_SECTION}") }
        }
    })
}

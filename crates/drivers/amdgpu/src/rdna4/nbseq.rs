//! Wave 42: how the stages `nbif`, `gmc` and `ih` are held against Linux's trace (`linuxseq`): the sections and the
//! only differences declared, each an address BitsOS chooses itself (plan/spec/gpu-rdna4.md section 6). The host
//! tests and the ktest use the same declarations.

use super::linuxseq::compare::{Compare, Diff};
use super::linuxseq::fold::Bus;
use super::linuxseq::rules::{Address, Rule};
use super::linuxseq::section;
use super::regs14::Regs14;
use super::stage::{Done, Stage};
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::{ih7, layout, pte12};

/// The section of Linux's trace a stage of this wave must equal.
pub fn section_of(stage: Stage) -> Option<&'static str> {
    match stage {
        Stage::Nbif => Some("nbif"),
        Stage::Gmc => Some("mmhub"),
        Stage::Ih => Some("ih"),
        _ => None,
    }
}

fn pair(lo: &'static str, hi: Option<&'static str>, shift: u8, keep: u32, align: u64, within: core::ops::Range<u64>, bytes: u64, why: &'static str) -> Rule {
    Rule::Address(Address { lo, hi, shift, keep, align, within, bytes, why })
}

/// The declared differences of a stage's section. `vram_bytes`: the card's memory (a VRAM offset must lie in it).
pub fn rules(stage: Stage, vram_bytes: u64) -> Vec<Rule> {
    let gart = 0..layout::GART_BYTES;
    let ring = u64::from(ih7::RING_BYTES);
    match stage {
        // Nothing in `nbif` is BitsOS's choice: the remap's place is Linux's fixed one.
        Stage::Gmc => alloc::vec![
            pair("MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32", Some("MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32"), 0, 1, pte12::PAGE, 0..vram_bytes, layout::GART_BYTES / pte12::PAGE * 8, "the GART table lies where BitsOS's VRAM manager put it (a VRAM offset, with Linux's valid bit)"),
            pair("MMMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_LSB", Some("MMMC_VM_SYSTEM_APERTURE_DEFAULT_ADDR_MSB"), 12, 0, pte12::PAGE, 0..vram_bytes, pte12::PAGE, "the default page lies where BitsOS's VRAM manager put it (a VRAM offset)"),
            pair("MMVM_L2_PROTECTION_FAULT_DEFAULT_ADDR_LO32", Some("MMVM_L2_PROTECTION_FAULT_DEFAULT_ADDR_HI32"), 12, 0, pte12::PAGE, 0..1 << layout::DMA_BITS, pte12::PAGE, "the dummy page is a system page of BitsOS's, below the card's 44 bits"),
        ],
        Stage::Ih => alloc::vec![
            pair("BIF_BX0_INTERRUPT_CNTL2", None, 8, 0, pte12::PAGE, 0..1 << 40, pte12::PAGE, "the dummy page is a system page of BitsOS's; the register holds 40 bits of it"),
            Rule::BindBefore { reg: "IH_RB_BASE", why: "Linux bound its rings while the later blocks set up; BitsOS binds them right before it names them" },
            pair("IH_RB_BASE", Some("IH_RB_BASE_HI"), 8, 0, pte12::PAGE, gart.clone(), ring, "ring 0 lies where BitsOS bound it in the GART"),
            pair("IH_RB_BASE_RING1", Some("IH_RB_BASE_HI_RING1"), 8, 0, pte12::PAGE, gart.clone(), ring, "ring 1 lies where BitsOS bound it in the GART"),
            pair("IH_RB_WPTR_ADDR_LO", Some("IH_RB_WPTR_ADDR_HI"), 0, 0, 4, gart, 4, "the write-back slot lies where BitsOS bound it in the GART"),
        ],
        _ => Vec::new(),
    }
}

/// A stage's trail against its whole section: `Ok` or the first difference in plain words.
pub fn equal(d: &Done, regs: &Regs14, vram_bytes: u64) -> Result<(), String> {
    let name = section_of(d.stage).ok_or("no section for this stage")?;
    let sec = section(name)?;
    let rules = rules(d.stage, vram_bytes);
    let text = |e: Diff| alloc::format!("{e}");
    let mut c = Compare::new(&sec, &rules).map_err(text)?;
    c.feed_trail(&d.trail, d.trail_lost, &Bus::of(regs)).map_err(text)?;
    c.finish().map_err(text)
}

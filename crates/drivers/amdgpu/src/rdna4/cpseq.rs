//! Wave 44, item CP: the stage `gccp` against the trace's section gc-cp (init.txt lines 9362-9501). One declared
//! difference: the clear-state buffer's address in RLC_CSIB_ADDR_LO/HI is where BitsOS's VRAM manager put it
//! (Linux's lay at MC 0x83_daafc000). It is read from the manager's own record by name and size, as the PMFW's
//! tables are (`pmfw::placed`), so another free page, or a buffer the stage did not take from the manager, differs.
//! Every other access, the 21 reads of CP_STAT's wait as one wait (the generator's rule), is Linux's.

use super::cpstage::CSB;
use super::linuxseq::compare::{Compare, Diff};
use super::linuxseq::fold::Bus;
use super::linuxseq::rules::{Address, Rule};
use super::regs14::Regs14;
use super::stage::Done;
use super::vram::{Bump, Layout};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::{clearstate12, pte12};

/// The clear-state buffer's MC address as the VRAM manager's record holds it.
pub fn placed(l: &Layout, b: &Bump) -> Option<u64> {
    b.taken.iter().find(|(w, r)| *w == CSB && r.end - r.start == pte12::PAGE).map(|(_, r)| l.mc(r.start))
}

/// The section's declared rules: the buffer's address, page-aligned, exactly the manager's page, with the two
/// low bits of the low register Linux's (0: `& 0xfffffffc`, gfx_v12_0.c:1944).
pub fn linux_rules(at: u64) -> Vec<Rule> {
    alloc::vec![Rule::Address(Address {
        lo: "RLC_CSIB_ADDR_LO",
        hi: Some("RLC_CSIB_ADDR_HI"),
        shift: 0,
        keep: 0b11,
        align: pte12::PAGE,
        within: at..at + pte12::PAGE,
        bytes: clearstate12::BYTES,
        why: "the clear-state buffer lies where BitsOS's VRAM manager put it; Linux's lay at MC 0x83daafc000",
    })]
}

/// `gccp`'s trail against the whole section.
pub fn equal(d: &Done, regs: &Regs14, l: &Layout, b: &Bump) -> Result<(), String> {
    let sec = super::linuxseq::section("gc-cp")?;
    let at = placed(l, b).ok_or("the VRAM manager's record holds no clear-state buffer")?;
    let text = |e: Diff| format!("{e}");
    let rules = linux_rules(at);
    let mut c = Compare::new(&sec, &rules).map_err(text)?;
    c.feed_trail(&d.trail, d.trail_lost, &Bus::of(regs)).map_err(text)?;
    c.finish().map_err(text)
}

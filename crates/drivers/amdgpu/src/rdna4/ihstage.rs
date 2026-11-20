//! Wave 42 (piece 11): the stage `ih`, IH 7.0's two rings. Trace section `ih` (init.txt lines 624-670).
//!
//! What Linux did in `ih_v7_0_sw_init` (ih_v7_0.c:585-630) comes first, as far as the card's host side sees it:
//! the rings' memory (two 256 KiB buffers of system pages reached through the GART, `amdgpu_ih_ring_init`
//! amdgpu_ih.c:42-115 with `use_bus_addr` false, and the write-back page) and the one MSI vector
//! (`amdgpu_irq_init` amdgpu_irq.c:307-350). Then `ih_v7_0_irq_init` (:310-424) register by register. Linux bound
//! its rings into the GART while the later blocks set up (section `sw-init`); BitsOS binds them here, right before
//! the first register that names them, with Linux's flush after a bind (`linuxseq`'s `Rule::BindBefore`).
//!
//! Where it differs from Linux, none of it visible in the register trace: the PCI core's enable of the function
//! (Linux's `pci_enable_device` at probe and `pci_set_master` at :399) comes BEFORE the first register write (and
//! after the rings' pages are there, so a machine with no memory for them is left unchanged), since
//! the PCI core hands out an MSI vector only for a function it enabled for sending; on this card it changes the
//! command register from 0x0006 to I/O decode and INTx off besides, as Linux's own probe did ("enabling device
//! (0006 -> 0007)"), and bus master is on already. No bridge is ever written (one that does not forward ends the
//! stage).

use super::ihring::{IhPlace, IhUp};
use super::nb::{Io, State, pages_of};
use super::policy;
use super::stage::{Stop, stop};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::ih7::{self, Ring, f};
use rdna4_core::nbreg;
use rdna4_core::regmap::Reg;

/// The system memory the stage asks for in one piece: both rings and the write-back page.
pub const HOST_BYTES: u64 = 2 * ih7::RING_BYTES as u64 + IhPlace::WB_BYTES;

/// The stage's own order check: once in a boot (that `gmc` passed before it is the chain's, `stage::in_order`;
/// the kernel's glue asks both before it prepares the PCI side).
pub fn order(s: &State) -> Result<(), Stop> {
    if s.ih.is_some() {
        return Err(stop("die Meldungen der Karte sind schon eingeschaltet", "ih ran in this boot: a second run is refused"));
    }
    Ok(())
}

/// `ih_v7_0_toggle_ring_interrupts` :127-168.
fn toggle(io: &mut Io, ring: Ring, on: bool) -> Result<(), Stop> {
    let (cntl, rptr, wptr) = match ring {
        Ring::Ring0 => (ih7::RB_CNTL, ih7::RB_RPTR, ih7::RB_WPTR),
        Ring::Ring1 => (ih7::RB_CNTL_RING1, ih7::RB_RPTR_RING1, ih7::RB_WPTR_RING1),
    };
    let v = io.read(cntl)?;
    io.write(cntl, ih7::toggled(ring, v, on))?;
    if !on {
        io.write(rptr, 0)?;
        io.write(wptr, 0)?;
    }
    Ok(())
}

fn modify(io: &mut Io, r: Reg, f: impl Fn(u32) -> u32) -> Result<(), Stop> {
    let v = io.read(r)?;
    io.write(r, f(v))
}

pub fn run(s: &mut State, io: &mut Io, out: &mut Vec<String>) -> Result<(), Stop> {
    order(s)?;
    let Some(gart) = s.gart.as_mut() else { return Err(stop("Reihenfolge stimmt nicht", "no GART")) };
    // The host side first: every refusal here leaves the card's registers untouched. What only asks comes before
    // what changes (the rings' pages are taken before the function is enabled), so a no changes nothing at all;
    // `hostprep::prepare` makes the same checks in the same order on the card.
    let survey = io.card.dma_survey();
    policy::may_send(io.driven, survey).map_err(|why| stop("der Rechner lässt die Karte nicht an den Arbeitsspeicher", why))?;
    io.card.bridges_forward().map_err(|why| stop("ein Bauteil zwischen Karte und Prozessor leitet nichts weiter", why))?;
    let was = io.card.bus_master().ok_or_else(|| stop("die Karte nennt ihren Zustand nicht", "the command register does not read"))?;
    let ring = u64::from(ih7::RING_BYTES);
    let bytes = HOST_BYTES;
    let sys = io.card.pages(bytes).ok_or_else(|| stop("kein Arbeitsspeicher für die Karte frei", "no pages for the IH rings"))?;
    let enabled = io.card.enable_dma().map_err(|why| stop("die Karte darf nicht senden", why))?;
    out.push(format!("ih: the function is enabled for sending (bus master was {} before): {enabled}", if was { "on, as the firmware left it" } else { "off" }));
    let msi = io.card.msi().map_err(|why| stop("die Karte bekommt keine Meldeleitung", why))?;
    out.push(format!("ih: {msi}"));
    // ih_v7_0_irq_init: "disable irqs", then NBIF's interrupt control with the dummy page.
    toggle(io, Ring::Ring0, false)?;
    toggle(io, Ring::Ring1, false)?;
    io.write(nbreg::INTERRUPT_CNTL2, (gart.dummy >> 8) as u32)?;
    modify(io, nbreg::INTERRUPT_CNTL, ih7::interrupt_cntl)?;
    // The rings and the write-back page into the GART in one bind, with Linux's flush after a bind.
    let f = io.flush()?;
    let mc = gart.map(io.card.hw(), &f, "IH rings", &pages_of(sys, bytes), out).map_err(|e| stop("der Speicher für die Meldungen lässt sich nicht einbinden", format!("{e:?}")))?;
    let place = IhPlace { sys, ring_mc: [mc, mc + ring], wb_mc: mc + 2 * ring };
    out.push(format!("ih: rings at mc {:#x} and {:#x}, write-back at mc {:#x} (system pages at {sys:#x})", place.ring_mc[0], place.ring_mc[1], place.wb_mc));
    // ih_v7_0_enable_ring :237-280, ring 0 then ring 1.
    io.write(ih7::RB_BASE, (place.ring_mc[0] >> 8) as u32)?;
    io.write(ih7::RB_BASE_HI, (place.ring_mc[0] >> 40) as u32 & 0xFF)?;
    modify(io, ih7::RB_CNTL, |v| ih7::rb_cntl(Ring::Ring0, v, true))?;
    io.write(ih7::RB_WPTR_ADDR_LO, place.wptr_mc(Ring::Ring0) as u32)?;
    io.write(ih7::RB_WPTR_ADDR_HI, (place.wptr_mc(Ring::Ring0) >> 32) as u32 & 0xFFFF)?;
    io.write(ih7::RB_WPTR, 0)?;
    io.write(ih7::RB_RPTR, 0)?;
    io.write(ih7::DOORBELL_RPTR, ih7::doorbell_rptr(ih7::DOORBELL_RING0))?;
    io.write(ih7::RB_BASE_RING1, (place.ring_mc[1] >> 8) as u32)?;
    io.write(ih7::RB_BASE_HI_RING1, (place.ring_mc[1] >> 40) as u32 & 0xFF)?;
    modify(io, ih7::RB_CNTL_RING1, |v| ih7::rb_cntl(Ring::Ring1, v, true))?;
    io.write(ih7::RB_WPTR_RING1, 0)?;
    io.write(ih7::RB_RPTR_RING1, 0)?;
    io.write(ih7::DOORBELL_RPTR_RING1, ih7::doorbell_rptr(ih7::DOORBELL_RING1))?;
    // "update doorbell range for ih ring 0", the storm client, the flood control, the MSI storm delay.
    modify(io, nbreg::DOORBELL_ENTRY_1, ih7::doorbell_range)?;
    modify(io, ih7::STORM_CLIENT_LIST_CNTL, |v| f::CLIENT18_IS_STORM_CLIENT.put(v, 1))?;
    modify(io, ih7::INT_FLOOD_CNTL, |v| f::FLOOD_CNTL_ENABLE.put(v, 1))?;
    modify(io, ih7::MSI_STORM_CTRL, |v| f::MSI_STORM_DELAY.put(v, 3))?;
    // "Redirect the interrupts to IH RB1 for dGPU".
    modify(io, ih7::RING1_CLIENT_CFG_INDEX, |v| f::CFG_INDEX.put(v, 0))?;
    modify(io, ih7::RING1_CLIENT_CFG_DATA, ih7::ring1_client_cfg)?;
    // "enable interrupts", then force_update_wptr_for_self_int(adev, 0, 8, true) :92-125.
    toggle(io, Ring::Ring0, true)?;
    toggle(io, Ring::Ring1, true)?;
    let cntl2 = io.read(ih7::CNTL2)?;
    let ring1 = io.read(ih7::RB_CNTL_RING1)?;
    let (cntl2, ring1) = ih7::self_int(cntl2, ring1);
    io.write(ih7::RB_CNTL_RING1, ring1)?;
    io.write(ih7::CNTL2, cntl2)?;
    s.ih = Some(IhUp::new(place, msi));
    Ok(())
}

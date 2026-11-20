//! Wave 42 (piece 6): the stage `nbif`, Linux's `soc24_common_hw_init` (soc24.c:437-458) on this card, in its
//! order: `soc24_program_aspm` writes nothing here (:224-231, and the trace shows no ASPM write);
//! `nbif_v6_3_1_init_registers` (nbif_v6_3_1.c:480-487), `nbif_v6_3_1_remap_hdp_registers` (:97-110),
//! `df_v4_15_hw_init` (df_v4_15.c:29-40), `nbif_v6_3_1_enable_doorbell_aperture` (:311-316). Trace section `nbif`
//! (init.txt lines 9-16); nothing in it is an address BitsOS chooses, so no rule.
//!
//! DF's write: Linux makes it only with `have_atomics_support` (df_v4_15.c:29-40), which it gets on this card
//! from `pci_enable_atomic_ops_to_root` (amdgpu_device.c:4018-4021, drivers/pci/pci.c:3747-3826; `lspci` before
//! and after shows `AtomicOpsCtl: ReqEn-` turn to `ReqEn+`). The pair belongs together: with DF's bits set the
//! card no longer handles its atomics locally, so they must be allowed out. So the stage asks the machine's side
//! for exactly that first (`host::Host::atomics`: reads of the bridges, one write of the card's OWN Device Control
//! 2), and a path that does not grant it ends the stage before any register write: the machine is then not the
//! one whose trace the stage follows. Linux does this at device init, before any block; configuration space is
//! not in the register trace.
//!
//! A run that stops after the remap has moved the flush for the rest of the boot (`State::remapped`) but did not
//! pass (`State::nbif`): `gmc` refuses, and `nbif` runs again from the start. Every step is a read with Linux's
//! one change or a fixed value, so a second run writes what the first wrote.

use super::nb::{Io, State};
use super::stage::{Stop, stop};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::nbreg;

/// The stage's own order check: once in a boot (the chain's first; the kernel's glue asks before it prepares the
/// PCI side).
pub fn order(s: &State) -> Result<(), Stop> {
    if s.nbif {
        return Err(stop("der Anschluss der Karte ist schon eingerichtet", "nbif ran in this boot: a second run is refused"));
    }
    Ok(())
}

pub fn run(s: &mut State, io: &mut Io, out: &mut Vec<String>) -> Result<(), Stop> {
    order(s)?;
    if s.remapped {
        out.push(String::from("nbif: an earlier run stopped after the HDP remap; the whole sequence runs again"));
    }
    // The machine's side first: a no leaves the card's registers untouched.
    let atomics = io.card.atomics().map_err(|why| stop("der Weg zum Prozessor lässt eine Einstellung der Karte nicht zu", format!("AtomicOps to the root port: {why}")))?;
    out.push(format!("nbif: {atomics}"));
    let (strap2, df) = (io.n.strap2, io.n.df);
    let v = io.read_far(strap2)?;
    io.write_far(strap2, nbreg::strap2(v))?;
    out.push(format!("nbif: RCC_DEV0_EPF2_STRAP2 {v:#010x} (index pair at {strap2:#x}), STRAP_NO_SOFT_RESET_DEV0_F2 cleared"));
    io.write(nbreg::REMAP_HDP_MEM_FLUSH, nbreg::REMAPPED_MEM_FLUSH_BYTE)?;
    // From here every flush of this boot goes to the remapped place (the memory flush is the one a flush
    // writes), whatever this stage still does.
    s.remapped = true;
    io.write(nbreg::REMAP_HDP_REG_FLUSH, nbreg::REMAPPED_REG_FLUSH_BYTE)?;
    out.push(format!("nbif: HDP flush registers remapped to BAR5 bytes {:#x} and {:#x}", nbreg::REMAPPED_MEM_FLUSH_BYTE, nbreg::REMAPPED_REG_FLUSH_BYTE));
    let v = io.read_far(df)?;
    io.write_far(df, nbreg::df_ncs_config1(v))?;
    out.push(format!("nbif: DF NCSConfigurationRegister1 {v:#010x} -> {:#010x} (index pair at {df:#x})", nbreg::df_ncs_config1(v)));
    let v = io.read(nbreg::DOORBELL_APER_EN)?;
    io.write(nbreg::DOORBELL_APER_EN, nbreg::BIF_DOORBELL_APER_EN.put(v, 1))?;
    out.push(format!("nbif: doorbell aperture {v:#x} -> on"));
    // Passed only now: `gmc` and `ih` build on the aperture too (the IH's doorbells).
    s.nbif = true;
    Ok(())
}

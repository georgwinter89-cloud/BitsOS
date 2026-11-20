//! Wave 42, the model's rules that cross the blocks: what holds between NBIF, GMC and IH on one side and the
//! secure processor on the other. Linux turns to the secure processor after the common block, GMC and IH
//! (amdgpu_device.c:2397, :2406 and :2469 before `amdgpu_device_fw_loading` :2473), so the model's bootloader takes
//! nothing before they are started; and what GMC holds in VRAM (the GART table, the default page) is no memory a
//! driver may hand the secure processor, which writes into its buffers.
//!
//! Wave 43: the graphics block has power only once the power controller switched its features on (Linux's order of
//! blocks: smu_v14_0 before gfx_v12_0; the trace: EnableAllSmuFeatures at init.txt 96.27 s, the first GC write at
//! 96.56 s). One answer says so, `Model::gc_powered`, which is the power controller's model itself (`fakepmfw`:
//! EnableAllSmuFeatures was accepted); `fakegc`, `fakegchub` and the SDMA engines' model ask it and refuse every
//! write of their sections until it holds AND the RLC's autoload is complete. No test sets it: a test that needs
//! the block's power runs the stage `pmfw`.

use super::fakemodel::{MC_BASE, Model};
use super::fakenb::FakeNb;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::nbreg::{self, member};
use rdna4_core::{hub41, pte12};

impl FakeNb {
    /// What the three blocks hold in the card's VRAM (VRAM offsets): the GART table and the default page, once
    /// `gmc` named them to the hub. No buffer of another block may lie there.
    pub fn vram_held(&self) -> Vec<(&'static str, Range<u64>)> {
        let mut held = Vec::new();
        if let Some(t) = self.table_range() {
            held.push(("the GART table", t));
        }
        let (lsb, msb) = (self.b(hub41::SYSTEM_APERTURE_DEFAULT_ADDR_LSB), self.b(member(hub41::SYSTEM_APERTURE_DEFAULT_ADDR_LSB, 1, 1)));
        if self.context0 && let (Some(lo), Some(hi)) = (self.vals.get(&lsb), self.vals.get(&msb)) {
            let at = (u64::from(*hi) << 44 | u64::from(*lo) << 12).saturating_sub(u64::from(self.val(hub41::FB_OFFSET)) << 24);
            held.push(("the default page", at..at + pte12::PAGE));
        }
        held
    }

    /// The card as a kernel before this one left it for a Reload: the HDP flush's remap stays in force (nothing
    /// takes it back), whatever else that kernel started.
    pub(super) fn left_remapped(&mut self) {
        self.remapped = [true; 2];
        for (r, v) in [(nbreg::REMAP_HDP_MEM_FLUSH, nbreg::REMAPPED_MEM_FLUSH_BYTE), (nbreg::REMAP_HDP_REG_FLUSH, nbreg::REMAPPED_REG_FLUSH_BYTE)] {
            if let Some(b) = self.n.byte(r) {
                self.vals.insert(b, v);
            }
        }
    }

    /// Tests: what the cold card's remap register holds before `nbif` writes it (Linux writes it without reading
    /// it, init.txt line 11, so no trace shows it). The flush's remapped place works only while it holds Linux's
    /// byte.
    pub fn remap_holds(&mut self, v: u32) {
        if let Some(b) = self.n.byte(nbreg::REMAP_HDP_MEM_FLUSH) {
            self.vals.insert(b, v);
        }
        self.remapped[0] = v == nbreg::REMAPPED_MEM_FLUSH_BYTE;
    }

    /// GMC and IH are started as Linux has them when it turns to the secure processor (amdgpu_device.c:2406 and
    /// :2469 before :2473): context 0 on, both rings on.
    pub fn started(&self) -> bool {
        self.context0 && self.ih_on == [true, true]
    }
}

impl Model {
    /// The power controller's features are on (EnableAllSmuFeatures was accepted). Linux starts the graphics
    /// block, GFXHUB and SDMA only after that (the trace's order of sections: smu, then gc-boot, gc-hub, sdma),
    /// and neither may a driver of this model: the models of GFX, GFXHUB and SDMA ask this before they take a
    /// write of those sections. An order gate, not a statement that the block is dead before: Linux itself
    /// reads and writes GC:RLC_SPM_MC_CNTL long before section smu (init.txt lines 316-317, in sw-init), so a
    /// model must not hold every GC register to it.
    pub fn gc_powered(&self) -> bool {
        self.pmfw.enabled
    }

    /// Memory a driver may hand the secure processor: inside VRAM and outside what GMC holds there (the GART
    /// table and the default page). The secure processor writes into its buffers; in the table that would rewrite
    /// the card's map of system memory.
    pub(super) fn psp_may_have(&self, mc: u64, bytes: u64) -> bool {
        let Some(off) = mc.checked_sub(MC_BASE) else { return false };
        self.in_vram(mc, bytes) && !self.nb.vram_held().iter().any(|(_, r)| off < r.end && r.start < off.saturating_add(bytes))
    }
}

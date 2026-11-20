//! Wave 42: the model's IH 7.0 beyond its registers (`fakenb`): what must stand before a ring starts, the two
//! doorbells, the card's side of an interrupt (the entry into the ring's memory through the GART, the write
//! pointer, its write-back, the message on the MSI vector with RPTR_REARM), and the rings as a reader reaches them.

use super::fakemodel::Model;
use super::fakenb::{FakeNb, idx};
use super::host::Host;
use alloc::format;
use alloc::string::String;
use rdna4_core::ih7::{self, Iv, Ring, f};
use rdna4_core::{hub41, layout, nbreg, pte12};

impl Model {
    pub(super) fn ring_may_start(&self, ring: Ring, cntl: u32) -> Result<(), String> {
        let i = idx(ring);
        if self.nb.ring_set[i] != 7 {
            return Err(format!("IH ring {i} enabled before its base and size were written"));
        }
        let (mc, bytes) = (self.nb.ring_mc(ring), 4u64 << f::RB_SIZE.get(cntl));
        if f::MC_SPACE.get(cntl) != ih7::MC_SPACE_GPU_VA || mc.checked_add(bytes).is_none_or(|e| e > layout::GART_BYTES) {
            return Err(format!("IH ring {i} at mc {mc:#x} with {bytes:#x} bytes: not inside the GART"));
        }
        // Every page, as Linux binds every page of a ring (amdgpu_gart_map, amdgpu_gart.c:354-374).
        if let Some(page) = (0..bytes.div_ceil(pte12::PAGE)).find(|p| self.gart_walk(mc + p * pte12::PAGE).is_none()) {
            return Err(format!("IH ring {i} at mc {mc:#x}: its pages are not bound in the GART (page {page}, as the card's walker sees the table now)"));
        }
        if !self.nb.bus_master {
            return Err(format!("IH ring {i} enabled while the card may not send (bus master off)"));
        }
        Ok(())
    }

    /// The reader's doorbell: an enabled IH doorbell through an open aperture, or refused.
    pub(super) fn nb_doorbell(&mut self, index_dw: u32, v: u32) -> bool {
        let (ring, reg) = match index_dw {
            ih7::DOORBELL_RING0 => (Ring::Ring0, ih7::DOORBELL_RPTR),
            ih7::DOORBELL_RING1 => (Ring::Ring1, ih7::DOORBELL_RPTR_RING1),
            _ => return self.refuse(format!("doorbell {index_dw:#x}: not an IH doorbell")),
        };
        let open = self.nb.val(nbreg::DOORBELL_APER_EN) & 1 != 0 && nbreg::PORT1_ENABLE.get(self.nb.val(nbreg::DOORBELL_ENTRY_1)) != 0;
        if !open || self.nb.val(reg) != ih7::doorbell_rptr(index_dw) {
            return self.refuse(format!("doorbell {index_dw:#x} rung before the aperture, the doorbell entry and the ring's doorbell were set up"));
        }
        self.nb.rptr[idx(ring)] = v;
        // RPTR_REARM: a read pointer update lets the next message out; entries still waiting send it at once.
        self.nb.armed = true;
        self.message();
        true
    }

    fn message(&mut self) {
        let c = self.nb.val(ih7::RB_CNTL);
        if f::ENABLE_INTR.get(c) != 0 && self.nb.msi_on && self.nb.armed && self.nb.wptr[0] & ih7::PTR_MASK != self.nb.rptr[0] {
            self.nb.msi_sent += 1;
            self.nb.armed = f::RPTR_REARM.get(c) == 0;
        }
    }

    /// The card raises an interrupt: the entry into the ring's memory through the GART, the write pointer on, its
    /// write-back (ring 0), the message.
    pub fn raise(&mut self, ring: Ring, iv: &Iv) -> Result<(), String> {
        let i = idx(ring);
        if !self.nb.ih_on[i] {
            return Err(format!("ring {i} is off"));
        }
        let cntl = self.nb.val(FakeNb::ring_regs(ring)[0]);
        let at = self.nb.wptr[i] & ih7::PTR_MASK;
        let mut bytes = [0u8; 32];
        for (k, d) in iv.encode().iter().enumerate() {
            bytes[k * 4..k * 4 + 4].copy_from_slice(&d.to_le_bytes());
        }
        let sys = self.gart_walk(self.nb.ring_mc(ring) + u64::from(at)).ok_or("the ring's page is not bound")?;
        self.nb.sys_poke(sys, &bytes);
        let next = (at + ih7::ENTRY_BYTES) & ih7::PTR_MASK;
        // The write pointer running into the read pointer: the oldest entry is lost and the overflow bit says so.
        let over = next == self.nb.rptr[i] && f::WPTR_OVERFLOW_ENABLE.get(cntl) != 0;
        self.nb.wptr[i] = next | u32::from(over || self.nb.wptr[i] & 1 != 0);
        if ring == Ring::Ring0 && f::WPTR_WRITEBACK_ENABLE.get(cntl) != 0 {
            let wb = u64::from(self.nb.val(ih7::RB_WPTR_ADDR_HI)) << 32 | u64::from(self.nb.val(ih7::RB_WPTR_ADDR_LO));
            let sys = self.gart_walk(wb).ok_or("the write-back's page is not bound")?;
            let w = self.nb.wptr[0];
            self.nb.sys_poke(sys, &w.to_le_bytes());
        }
        if ring == Ring::Ring0 {
            self.message();
        }
        Ok(())
    }
}

/// The rings of the model as the reader reaches them (`ihring::IhIo`): the model's system memory, its two
/// registers per ring, its doorbells, and the card itself for the thermal taker (wave 44).
pub struct ModelIo<'a> {
    pub m: &'a mut Model,
    /// The system address of the rings' piece (`ihring::IhPlace::sys`).
    pub sys: u64,
}

impl ModelIo<'_> {
    fn reg(&self, ring: Ring, cntl: bool) -> u32 {
        let [c, _, _, _, w] = FakeNb::ring_regs(ring);
        self.m.nb.b(if cntl { c } else { w })
    }
}

impl super::ihring::IhIo for ModelIo<'_> {
    fn mem32(&mut self, off: u64) -> Option<u32> {
        let mut b = [0u8; 4];
        self.m.sys_read(self.sys + off, &mut b).then(|| u32::from_le_bytes(b))
    }
    fn set_mem32(&mut self, off: u64, v: u32) -> bool {
        self.m.sys_write(self.sys + off, &v.to_le_bytes())
    }
    fn wptr_reg(&mut self, ring: Ring) -> Option<u32> {
        let b = self.reg(ring, false);
        crate::gpucore::psp::Hw::reg_read(self.m, b)
    }
    fn cntl(&mut self, ring: Ring) -> Option<u32> {
        let b = self.reg(ring, true);
        crate::gpucore::psp::Hw::reg_read(self.m, b)
    }
    fn set_cntl(&mut self, ring: Ring, v: u32) -> bool {
        let b = self.reg(ring, true);
        crate::gpucore::psp::Hw::reg_write(self.m, b, v)
    }
    fn doorbell(&mut self, ring: Ring, rptr: u32) -> bool {
        crate::gpucore::psp::Hw::doorbell_write32(self.m, if ring == Ring::Ring0 { ih7::DOORBELL_RING0 } else { ih7::DOORBELL_RING1 }, rptr)
    }
    fn fault_status(&mut self, hub: hub41::Kind) -> Option<u32> {
        let b = self.m.nb.b(hub41::of(hub).fault_status);
        crate::gpucore::psp::Hw::reg_read(self.m, b)
    }
    fn fault_clear(&mut self, hub: hub41::Kind) -> bool {
        let b = self.m.nb.b(hub41::of(hub).fault_cntl);
        crate::gpucore::psp::Hw::reg_read(self.m, b).is_some_and(|v| crate::gpucore::psp::Hw::reg_write(self.m, b, hub41::fault_clear(v)))
    }
    fn now_us(&mut self) -> u64 {
        crate::gpucore::psp::Hw::now_us(self.m)
    }
    // The model is the card: the thermal taker reaches it as the IH thread reaches the real one.
    fn thermal_hw(&mut self) -> Option<&mut dyn crate::gpucore::psp::Hw> {
        Some(self.m)
    }
}

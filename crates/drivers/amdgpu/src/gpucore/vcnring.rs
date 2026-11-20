//! GX-8v: VCN 0's unified ring as the CPU drives it (vcn_v4_0.c:1748-1797): packets into the ring in the carve-out,
//! the commit's padding to 64 dwords, the write pointer into its shadow slot and then VCN's doorbell (WDOORBELL32 at
//! dword 0x312) or, without the doorbell aperture, UVD_RB_WPTR; the read pointer is UVD_RB_RPTR. The hardware's
//! pointers are masked to the ring (the capture: 0x140 after five frames); this queue keeps 64-bit counters and
//! rebuilds the read pointer's counter from the register. The stage (`vcn`) and the job path (`hw::vcn`) both use it.

use super::gmc;
use super::psp::Hw;
use super::vcn::{VcnAddrs, VcnError};
use crate::ipmod::vcn4::{self, R};
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::ring::{Kind, Ring, RingError};

/// One VCN ring: where it lies, the CPU's copy, the pointers.
pub struct VcnQueue {
    pub a: VcnAddrs,
    pub ring_mc: u64,
    /// `ring->wptr_cpu_addr`: the shadow Linux writes before the doorbell.
    pub wptr_mc: u64,
    pub doorbell: bool,
    ring: Ring,
    shadow: Vec<u32>,
    /// What the last read of UVD_RB_RPTR gave, as a counter.
    rptr: u64,
}

fn dwords(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|d| d.to_le_bytes()).collect()
}

impl VcnQueue {
    /// A queue whose pointers start at `start` (a counter; the registers hold it masked).
    pub fn new(a: VcnAddrs, ring_mc: u64, wptr_mc: u64, start: u64, doorbell: bool) -> Result<VcnQueue, VcnError> {
        let ring = Ring::new(Kind::Vcn, vcn4::RING_DW, start).map_err(VcnError::Ring)?;
        Ok(VcnQueue { a, ring_mc, wptr_mc, doorbell, ring, shadow: vec![0; vcn4::RING_DW as usize], rptr: start })
    }

    pub fn wptr(&self) -> u64 {
        self.ring.wptr()
    }

    pub fn mask(&self) -> u64 {
        self.ring.mask()
    }

    /// UVD_RB_RPTR as a counter: the register is masked to the ring, so the counter is the write pointer minus the
    /// masked distance. `None`: unreadable, all ones (VCN gated), or outside the ring.
    pub fn rptr(&mut self, hw: &mut dyn Hw) -> Option<u64> {
        let v = u64::from(self.a.byte(R::RbRptr).and_then(|b| hw.reg_read(b)).filter(|&v| v != u32::MAX)?);
        if v > self.mask() {
            return None;
        }
        let w = self.ring.wptr();
        let back = (w.wrapping_sub(v)) & self.mask();
        let r = w.wrapping_sub(back);
        self.rptr = r;
        Some(r)
    }

    /// The ring's memory zeroed (no stale packet of an earlier run may look valid) and the shadow at the start.
    pub fn clear(&mut self, hw: &mut dyn Hw) -> Result<(), VcnError> {
        mem(hw, self.ring_mc, &vec![0u8; (vcn4::RING_DW * 4) as usize])?;
        mem(hw, self.wptr_mc, &((self.ring.wptr() & self.mask()) as u32).to_le_bytes())?;
        hw.store_barrier();
        Ok(())
    }

    /// Packets into the ring, the commit, the shadow, a store barrier, then the doorbell (or UVD_RB_WPTR). Returns
    /// the new write pointer (a counter).
    pub fn submit(&mut self, hw: &mut dyn Hw, ndw: u64, build: impl FnOnce(&mut Ring) -> Result<(), RingError>) -> Result<u64, VcnError> {
        let rptr = self.rptr(hw).unwrap_or(self.rptr);
        self.ring.begin(ndw, rptr).map_err(VcnError::Ring)?;
        if let Err(e) = build(&mut self.ring) {
            self.ring.undo();
            return Err(VcnError::Ring(e));
        }
        let old = self.ring.wptr();
        let new = self.ring.commit(&mut self.shadow[..]).map_err(VcnError::Ring)?;
        let (size, mask) = (self.ring.size_dw(), self.ring.mask());
        let mut p = old;
        while p != new {
            let i = p & mask;
            let n = (new - p).min(size - i);
            mem(hw, self.ring_mc + i * 4, &dwords(&self.shadow[i as usize..(i + n) as usize]))?;
            p += n;
        }
        hw.store_barrier();
        let hwp = (new & mask) as u32;
        mem(hw, self.wptr_mc, &hwp.to_le_bytes())?;
        hw.store_barrier();
        if self.doorbell {
            if !hw.doorbell_write32(vcn4::DOORBELL_DW, hwp) {
                return Err(VcnError::NoDoorbell);
            }
        } else {
            let b = self.a.byte(R::RbWptr).ok_or(VcnError::NoAddress)?;
            if !hw.reg_write(b, hwp) {
                return Err(VcnError::Write { byte: b });
            }
        }
        Ok(new)
    }

    /// Polls until `done`, at most `us`: whether it came, and how many polls it took.
    pub fn wait(&mut self, hw: &mut dyn Hw, us: u64, done: &mut dyn FnMut(&mut dyn Hw, &mut VcnQueue) -> bool) -> (bool, u32) {
        let mut polls = 0;
        let ok = gmc::poll(hw, us, &mut polls, &mut |h| done(h, self));
        (ok, polls)
    }

    /// Whether the read pointer reached `wptr` (a counter).
    pub fn reached(&mut self, hw: &mut dyn Hw, wptr: u64) -> bool {
        self.rptr(hw) == Some(wptr)
    }
}

pub(crate) fn mem(hw: &mut dyn Hw, mc: u64, data: &[u8]) -> Result<(), VcnError> {
    if hw.mem_write(mc, data) { Ok(()) } else { Err(VcnError::Mem { mc }) }
}

pub(crate) fn rd32(hw: &mut dyn Hw, mc: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    hw.mem_read(mc, &mut b).then(|| u32::from_le_bytes(b))
}

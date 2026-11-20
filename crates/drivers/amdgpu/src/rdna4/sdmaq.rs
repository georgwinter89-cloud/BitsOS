//! Wave 43 (piece 12): one SDMA queue as the CPU drives it, Linux's ring side for this engine: `amdgpu_ring_alloc`,
//! `amdgpu_ring_commit` (amdgpu_ring.c:86-106, :169-190), `sdma_v7_0_ring_set_wptr` (sdma_v7_0.c:205-242: the write
//! pointer into its poll slot, then the 64-bit doorbell), the write-back slots of `amdgpu_ring_init`
//! (amdgpu_ring.c:264-320), and `amdgpu_ib_schedule`'s frame around an IB (amdgpu_ib.c:124-335 with no job).
//!
//! The ring, its padding and the packets are the 780M's code (`amdgpu_fmt::{ring, sdma, wb}`), with SDMA 7's two
//! forms from `amdgpu_fmt::sdma7`. What differs from the 780M's queue (`gpucore::sdma`) is where the memory lies:
//! Linux keeps this card's rings, slots and IBs in system pages behind the GART (amdgpu_ring.c:360-366,
//! amdgpu_wb.c:61-83, amdgpu_sa.c:54-55), so the CPU writes them through `host::Host` and names them to the
//! engine by their GART addresses.

use super::host::Card;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::ring::{Kind, Ring, RingError};
use amdgpu_fmt::sdma::{self as pkt, PollRegMem};
use amdgpu_fmt::wb::{Slot, WbError, WbPool};
use rdna4_core::pte12::PAGE;
use rdna4_core::sdma7;

/// The block's one piece of system memory, as byte offsets into it: both rings, the write-back page and the IB
/// page are bound into the GART by the stage `sdma`; the test pages behind them only while a rung uses them.
pub mod lay {
    use super::{PAGE, sdma7};
    pub const fn ring(i: u32) -> u64 {
        (i as u64) * sdma7::RING_BYTES as u64
    }
    pub const WB: u64 = sdma7::INSTANCES as u64 * sdma7::RING_BYTES as u64;
    pub const IB: u64 = WB + PAGE;
    /// What the stage binds.
    pub const BOUND: u64 = IB + PAGE;
    pub const TEST: u64 = BOUND;
    pub const TEST_BYTES: u64 = 4 * PAGE;
    pub const PIECE: u64 = TEST + TEST_BYTES;
}

/// Slots of the write-back page (32 bytes each, Linux's `amdgpu_wb_get`).
pub const WB_SLOTS: u32 = (PAGE / amdgpu_fmt::wb::SLOT_BYTES) as u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// The piece's system address and the GART address of its first page.
    pub sys: u64,
    pub mc: u64,
}

impl Place {
    pub fn ring_mc(&self, i: u32) -> u64 {
        self.mc + lay::ring(i)
    }
    pub fn wb_mc(&self) -> u64 {
        self.mc + lay::WB
    }
    pub fn ib_mc(&self) -> u64 {
        self.mc + lay::IB
    }
    /// A slot as the engine names it and as the CPU reaches it.
    pub fn slot_mc(&self, s: Slot) -> u64 {
        s.gpu_addr(self.wb_mc())
    }
    pub fn slot_sys(&self, s: Slot) -> u64 {
        s.gpu_addr(self.sys + lay::WB)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QStop {
    Ring(RingError),
    Slots(WbError),
    /// System memory at this address did not take a write or give a read.
    Mem { at: u64 },
    /// The doorbell write did not reach the card.
    Doorbell { index_dw: u32 },
}

/// A ring's five slots in the order `amdgpu_ring_init` takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slots {
    pub rptr: Slot,
    pub wptr: Slot,
    pub fence: Slot,
    pub trail: Slot,
    pub cond_exe: Slot,
}

#[derive(Clone, Debug)]
pub struct Queue {
    pub me: u32,
    ring: Ring,
    shadow: Vec<u32>,
    /// The engine's read pointer as last read from its slot (dwords).
    rptr_dw: u64,
    pub slots: Slots,
    /// `fence_drv.sync_seq`.
    pub seq: u32,
    /// Doorbell writes that reached the card: (dword index, value).
    pub rung: Vec<(u32, u64)>,
}

pub fn sys_write(card: &mut dyn Card, at: u64, data: &[u8]) -> Result<(), QStop> {
    if card.sys_write(at, data) { Ok(()) } else { Err(QStop::Mem { at }) }
}

pub fn sys_read32(card: &mut dyn Card, at: u64) -> Result<u32, QStop> {
    let mut b = [0u8; 4];
    if card.sys_read(at, &mut b) { Ok(u32::from_le_bytes(b)) } else { Err(QStop::Mem { at }) }
}

pub fn sys_read64(card: &mut dyn Card, at: u64) -> Result<u64, QStop> {
    let mut b = [0u8; 8];
    if card.sys_read(at, &mut b) { Ok(u64::from_le_bytes(b)) } else { Err(QStop::Mem { at }) }
}

fn bytes_of(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|d| d.to_le_bytes()).collect()
}

impl Queue {
    /// Engine `me`'s queue 0 with its five slots from `pool`. `burst_nop`: the firmware takes the counted NOP
    /// (`amdgpu_sdma.c:182-183`).
    pub fn new(me: u32, burst_nop: bool, pool: &mut WbPool) -> Result<Queue, QStop> {
        let mut get = || pool.get().map_err(QStop::Slots);
        let slots = Slots { rptr: get()?, wptr: get()?, fence: get()?, trail: get()?, cond_exe: get()? };
        let ring = Ring::new(Kind::Sdma { burst_nop }, u64::from(sdma7::RING_DW), 0).map_err(QStop::Ring)?;
        Ok(Queue { me, ring, shadow: vec![0; sdma7::RING_DW as usize], rptr_dw: 0, slots, seq: 0, rung: Vec::new() })
    }

    pub fn kind(&self) -> Kind {
        self.ring.kind()
    }

    pub fn wptr(&self) -> u64 {
        self.ring.wptr()
    }

    /// The ring cleared with NOPs (`amdgpu_ring_clear_ring`, amdgpu_ring.h:488-491), its slots at 0 and the
    /// conditional-execution slot at 1 ("always set cond_exec_polling to CONTINUE", amdgpu_ring.c:320).
    pub fn init_memory(&self, card: &mut dyn Card, p: &Place) -> Result<(), QStop> {
        sys_write(card, p.sys + lay::ring(self.me), &bytes_of(&vec![pkt::NOP; sdma7::RING_DW as usize]))?;
        for s in [self.slots.rptr, self.slots.wptr, self.slots.fence, self.slots.trail] {
            sys_write(card, p.slot_sys(s), &0u64.to_le_bytes())?;
        }
        sys_write(card, p.slot_sys(self.slots.cond_exe), &1u64.to_le_bytes())
    }

    /// The engine's read pointer from its write-back slot (`sdma_v7_0_ring_get_rptr`: 64 bits, in bytes).
    pub fn read_rptr(&mut self, card: &mut dyn Card, p: &Place) -> Result<u64, QStop> {
        let hw = sys_read64(card, p.slot_sys(self.slots.rptr))?;
        self.rptr_dw = self.ring.kind().ptr_from_hw(hw);
        Ok(hw)
    }

    /// `words` onto the ring and the commit: the pad to the fetch size, the ring's memory, `mb()`, the write
    /// pointer into its poll slot, the doorbell. The new write pointer in bytes, as the doorbell took it.
    pub fn submit(&mut self, card: &mut dyn Card, p: &Place, words: &[u32]) -> Result<u64, QStop> {
        self.ring.begin(words.len() as u64, self.rptr_dw).map_err(QStop::Ring)?;
        if let Err(e) = self.ring.write(words) {
            self.ring.undo();
            return Err(QStop::Ring(e));
        }
        let old = self.ring.wptr();
        let new = self.ring.commit(&mut self.shadow).map_err(QStop::Ring)?;
        let base = p.sys + lay::ring(self.me);
        for (at, n) in crate::gpucore::sdma::ring_spans(old, new, self.ring.size_dw()) {
            sys_write(card, base + at * 4, &bytes_of(&self.shadow[at as usize..(at + n) as usize]))?;
        }
        card.hw().store_barrier();
        let hw = self.ring.kind().hw_ptr(new);
        sys_write(card, p.slot_sys(self.slots.wptr), &hw.to_le_bytes())?;
        let index_dw = sdma7::doorbell_dw(self.me);
        if !card.hw().doorbell_write64(index_dw, hw) {
            return Err(QStop::Doorbell { index_dw });
        }
        self.rung.push((index_dw, hw));
        Ok(hw)
    }

    /// The ring's frame around an IB of `ib_dw` dwords at `ib_mc`, as `amdgpu_ib_schedule` emits it with no job:
    /// the conditional execution on the ring's slot, the HDP flush from the ring, the IB in VMID 0, the fence
    /// with its trap; the conditional's count patched to everything behind it. The fence's sequence number.
    pub fn frame(&mut self, p: &Place, hdp_flush: (u32, u32), ib: Option<(u64, u32)>) -> Result<(Vec<u32>, u32), QStop> {
        let enc = |e| QStop::Ring(RingError::from(e));
        let mut tail = Vec::new();
        let (done, req) = hdp_flush;
        let bit = sdma7::hdp_flush_bit(self.me);
        // sdma_v7_0_ring_emit_hdp_flush: func 3 is "==".
        let flush = PollRegMem { hdp_flush: true, mem: false, function: 3, addr_lo: done << 2, addr_hi: req << 2, reference: bit, mask: bit, interval: sdma7::HDP_FLUSH_INTERVAL, retry: sdma7::HDP_FLUSH_RETRY };
        pkt::poll_regmem(&mut tail, &flush).map_err(enc)?;
        if let Some((ib_mc, ib_dw)) = ib {
            // sdma_v7_0_ring_emit_ib: the IB packet ends on an 8-dword boundary. The conditional's five dwords
            // come before the tail.
            let at = self.ring.wptr() + 5 + tail.len() as u64;
            self.ring.kind().nops(&mut tail, 2u64.wrapping_sub(at) & 7);
            pkt::indirect(&mut tail, 0, ib_mc, ib_dw, 0).map_err(enc)?;
        }
        let seq = self.seq.wrapping_add(1);
        pkt::emit_fence(&mut tail, p.slot_mc(self.slots.fence), u64::from(seq), false, true).map_err(enc)?;
        let mut words = Vec::new();
        // amdgpu_ring_patch_cond_exec: the dwords from the count to the last one written.
        pkt::cond_exe(&mut words, p.slot_mc(self.slots.cond_exe), 1, tail.len() as u32).map_err(enc)?;
        words.extend_from_slice(&tail);
        self.seq = seq;
        Ok((words, seq))
    }

    /// The fence slot as it reads now (`amdgpu_fence_read`).
    pub fn read_fence(&self, card: &mut dyn Card, p: &Place) -> Result<u32, QStop> {
        sys_read32(card, p.slot_sys(self.slots.fence))
    }
}

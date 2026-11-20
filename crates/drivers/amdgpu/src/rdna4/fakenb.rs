//! Wave 42: the model's faces of NBIF 6.3.1 with DF, MMHUB 4.1.0 and IH 7.0, and the machine's side of the card
//! (`host::Host`: the DMAR survey, bus master, the MSI vector, system pages). Like `fakemodel` it answers as the
//! real card answered Linux (the read values of plan/hw/gaming-pc-gpu-2026-11-18's trace) and refuses, and keeps,
//! what Linux would not do:
//!
//! - the index pair aimed at anything but the two registers of the common hw_init (and the SMU's flags), or
//!   either written with anything but Linux's one change of what it holds;
//! - DF's NCSConfigurationRegister1 written while AtomicOp requester enable is off in the card's Device Control 2
//!   (Linux writes it only with `have_atomics_support`, df_v4_15.c:29-40);
//! - an HDP flush through the remapped place before the remap, or through the register itself after it;
//! - context 0 enabled before its page table base and range were written;
//! - an HDP flush after entries of the named GART table were written, with no store barrier between them
//!   (Linux's `mb()` in `amdgpu_gart_invalidate_tlb`, amdgpu_gart.c:499);
//! - an invalidation request without the engine's semaphore, or before context 0 is on (an acknowledge stands
//!   only until the semaphore is released: a wait for it with no request of its own runs out);
//! - a system address handed to the card (the fault's default page, the interrupt control's dummy page, a GART
//!   entry) before the DMAR survey was asked and said yes; bus master before it; a GART entry above 44 bits,
//!   whenever it was written (`fakegart`);
//! - an IH ring enabled before its base and size were written, with its base outside the GART or with any of its
//!   pages not bound as the card's walker sees the table (a bind without Linux's HDP flush and invalidation after
//!   it is not seen, `fakegart`), or while the card may not send; a doorbell that is not an enabled IH doorbell
//!   (`fakeih`).
//!
//! `fakehost` is the machine's side, `fakegart` the table as the card's walker sees it, `fakeih` the card's side
//! of the rings (it writes entries where the driver told it to) and the rings as a reader reaches them.

use super::fakemodel::Model;
use super::nbregs::NbRegs;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::ih7::{self, Ring, f};
use rdna4_core::nbreg::{self, member};
use rdna4_core::{hub41, pte12};

/// Where the model's system pages begin: above 4 GiB, so a 32-bit slip shows.
pub const SYS_BASE: u64 = 0x4_5678_0000;

pub struct FakeNb {
    pub n: NbRegs,
    /// Every register of the three blocks the model knows, by BAR5 byte, with what it reads now.
    pub(super) vals: BTreeMap<u32, u32>,
    pub strap2: u32,
    pub df: u32,
    pub remapped: [bool; 2],
    /// CONTEXT0's base, start and end halves written so far (six bits).
    pub(super) ctx0: u8,
    pub context0: bool,
    pub(super) sem_held: bool,
    pub(super) acked: bool,
    pub tlb_flushes: u32,
    /// Tests: the semaphore or the acknowledge never comes.
    pub sem_never: bool,
    pub ack_never: bool,
    /// VRAM written since the last HDP flush, and the table entries the card's walker does not see yet (`fakegart`).
    pub(super) vram_dirty: bool,
    pub(super) pending: Vec<super::fakegart::Pending>,
    /// Store barriers that reached the card's side (`Hw::store_barrier`), and whether entries of the named table
    /// were written since the last one: Linux's `mb()` comes before the flush after a bind (amdgpu_gart.c:499).
    pub barriers: u32,
    pub(super) table_unfenced: bool,
    // The host side.
    /// What `pci_enable_atomic_ops_to_root` finds on the way to the root port, and AtomicOp requester enable in
    /// the card's own Device Control 2 (off at the firmware's handover: `lspci` before Linux's start).
    pub atomics_path: Result<(), &'static str>,
    pub atomics_req: bool,
    pub survey: Result<(), &'static str>,
    pub survey_asked: bool,
    pub bus_master: bool,
    pub bridges: Result<(), &'static str>,
    pub msi_on: bool,
    /// Tests: the PCI core has no vector.
    pub msi_refused: bool,
    /// Tests: the machine has no memory for the card.
    pub pages_refused: bool,
    /// Host-side changes that happened, in order.
    pub host_changes: Vec<&'static str>,
    pub(super) sys: BTreeMap<u64, Box<[u8; 4096]>>,
    pub sys_given: Vec<Range<u64>>,
    pub next_page: u64,
    // IH: per ring, what was written before the enable (base, base hi, a control with a size), and the pointers.
    pub(super) ring_set: [u8; 2],
    pub ih_on: [bool; 2],
    pub wptr: [u32; 2],
    pub rptr: [u32; 2],
    pub(super) armed: bool,
    /// Messages the card sent on its MSI vector.
    pub msi_sent: u64,
}

pub(super) fn idx(ring: Ring) -> usize {
    match ring {
        Ring::Ring0 => 0,
        Ring::Ring1 => 1,
    }
}

impl FakeNb {
    pub fn new(n: NbRegs) -> FakeNb {
        let mut vals = BTreeMap::new();
        let mut put = |r, v: u32| {
            if let Some(b) = n.byte(r) {
                vals.insert(b, v);
            }
        };
        // What the card answered Linux at a cold start (init.txt lines 15-204 and 624-667).
        hub41::gart_enable_regs(&hub41::MM, |r| put(r, 0));
        for (r, v) in [
            (nbreg::BIOS_SCRATCH_4, 0xAB91),
            (nbreg::DOORBELL_APER_EN, 0),
            (nbreg::REMAP_HDP_MEM_FLUSH, 0),
            (nbreg::REMAP_HDP_REG_FLUSH, 0),
            (nbreg::INTERRUPT_CNTL, 0x0001_0000),
            (nbreg::INTERRUPT_CNTL2, 0),
            (nbreg::DOORBELL_ENTRY_1, 0),
            (hub41::FB_OFFSET, 0),
            (hub41::FAULT_CNTL2, 0x000A_0000),
            (hub41::MX_L1_TLB_CNTL, 0x0000_0501),
            (hub41::L2_CNTL, 0x0008_0602),
            (hub41::FAULT_CNTL, 0x3FFF_FFFC),
            (hub41::BANK_SELECT_RESERVED_CID2, 0x1010_5C17),
            (member(hub41::INVALIDATE_ENG0_SEM, hub41::GART_ENGINE, 1), 0),
            (member(hub41::INVALIDATE_ENG0_REQ, hub41::GART_ENGINE, 1), 0),
            (member(hub41::INVALIDATE_ENG0_ACK, hub41::GART_ENGINE, 1), 0),
            (ih7::RB_CNTL, 0x4061_0000),
            (ih7::RB_CNTL_RING1, 0x4041_0000),
            (ih7::CNTL2, 0x8),
        ] {
            put(r, v);
        }
        for i in 0..=hub41::USER_CONTEXTS {
            put(member(hub41::CONTEXT0_CNTL, i, 1), 0x03FF_FC00);
        }
        for r in [
            ih7::RB_RPTR, ih7::RB_WPTR, ih7::RB_BASE, ih7::RB_BASE_HI, ih7::RB_WPTR_ADDR_LO, ih7::RB_WPTR_ADDR_HI, ih7::DOORBELL_RPTR, ih7::RB_RPTR_RING1, ih7::RB_WPTR_RING1, ih7::RB_BASE_RING1,
            ih7::RB_BASE_HI_RING1, ih7::DOORBELL_RPTR_RING1, ih7::STORM_CLIENT_LIST_CNTL, ih7::INT_FLOOD_CNTL, ih7::MSI_STORM_CTRL, ih7::RING1_CLIENT_CFG_INDEX, ih7::RING1_CLIENT_CFG_DATA,
        ] {
            put(r, 0);
        }
        FakeNb {
            n,
            vals,
            strap2: 0x0313_2000,
            df: 0x0A40_0001,
            remapped: [false; 2],
            ctx0: 0,
            context0: false,
            sem_held: false,
            acked: false,
            tlb_flushes: 0,
            sem_never: false,
            ack_never: false,
            vram_dirty: false,
            pending: Vec::new(),
            barriers: 0,
            table_unfenced: false,
            atomics_path: Ok(()),
            atomics_req: false,
            pages_refused: false,
            survey: Ok(()),
            survey_asked: false,
            // The firmware hands the card over with bus master on (command register 0x0006).
            bus_master: true,
            bridges: Ok(()),
            msi_on: false,
            msi_refused: false,
            host_changes: Vec::new(),
            sys: BTreeMap::new(),
            sys_given: Vec::new(),
            next_page: SYS_BASE,
            ring_set: [0; 2],
            ih_on: [false; 2],
            wptr: [0; 2],
            rptr: [0; 2],
            armed: true,
            msi_sent: 0,
        }
    }

    pub(super) fn b(&self, r: rdna4_core::regmap::Reg) -> u32 {
        self.n.byte(r).unwrap_or(u32::MAX)
    }

    pub(super) fn val(&self, r: rdna4_core::regmap::Reg) -> u32 {
        self.vals.get(&self.b(r)).copied().unwrap_or(0)
    }

    /// The survey was asked and allows DMA.
    pub(super) fn dma_ok(&self) -> bool {
        self.survey_asked && self.survey.is_ok()
    }

    pub(super) fn in_sys(&self, at: u64, bytes: u64) -> bool {
        self.sys_given.iter().any(|r| at >= r.start && at.saturating_add(bytes) <= r.end)
    }

    pub(super) fn sys_peek(&self, at: u64, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            let a = at + i as u64;
            *b = self.sys.get(&(a >> 12)).map_or(0, |p| p[(a & 0xFFF) as usize]);
        }
    }

    pub(super) fn sys_poke(&mut self, at: u64, data: &[u8]) {
        for (i, b) in data.iter().enumerate() {
            let a = at + i as u64;
            self.sys.entry(a >> 12).or_insert_with(|| Box::new([0; 4096]))[(a & 0xFFF) as usize] = *b;
        }
    }

    /// The GART table's VRAM offset, once CONTEXT0's base was written.
    pub fn table(&self) -> Option<u64> {
        (self.ctx0 & 3 == 3).then(|| (u64::from(self.val(member(hub41::CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32, 1, 1))) << 32 | u64::from(self.val(hub41::CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32))) & pte12::ADDR_MASK)
    }

    pub(super) fn ring_regs(ring: Ring) -> [rdna4_core::regmap::Reg; 5] {
        match ring {
            Ring::Ring0 => [ih7::RB_CNTL, ih7::RB_BASE, ih7::RB_BASE_HI, ih7::RB_RPTR, ih7::RB_WPTR],
            Ring::Ring1 => [ih7::RB_CNTL_RING1, ih7::RB_BASE_RING1, ih7::RB_BASE_HI_RING1, ih7::RB_RPTR_RING1, ih7::RB_WPTR_RING1],
        }
    }

    /// A ring's base as an MC address.
    pub fn ring_mc(&self, ring: Ring) -> u64 {
        let [_, base, hi, ..] = Self::ring_regs(ring);
        (u64::from(self.val(hi)) << 32 | u64::from(self.val(base))) << 8
    }
}

impl Model {
    /// A read of a register of the three blocks, or `None` for any other byte.
    pub(super) fn nb_read(&mut self, byte: u32) -> Option<u32> {
        let nb = &mut self.nb;
        if byte == nb.n.data && self.index == nb.n.strap2 {
            return Some(nb.strap2);
        }
        if byte == nb.n.data && self.index == nb.n.df {
            return Some(nb.df);
        }
        let eng = |first| nb.n.byte(member(first, hub41::GART_ENGINE, 1));
        if Some(byte) == eng(hub41::INVALIDATE_ENG0_SEM) {
            // "a read return value of 1 means semaphore acquire" (gmc_v12_0.c:240).
            nb.sem_held |= !nb.sem_never;
            return Some(u32::from(!nb.sem_never));
        }
        if Some(byte) == eng(hub41::INVALIDATE_ENG0_ACK) {
            return Some(if nb.acked && !nb.ack_never { 0x0001_0001 } else { 0 });
        }
        for (i, ring) in [Ring::Ring0, Ring::Ring1].into_iter().enumerate() {
            let [_, _, _, rptr, wptr] = FakeNb::ring_regs(ring);
            if byte == nb.b(rptr) {
                return Some(nb.rptr[i]);
            }
            if byte == nb.b(wptr) {
                return Some(nb.wptr[i]);
            }
        }
        nb.vals.get(&byte).copied()
    }

    /// A write to a register of the three blocks: `Some(reached)`, or `None` for any other byte.
    pub(super) fn nb_write(&mut self, byte: u32, v: u32) -> Option<bool> {
        let n = self.nb.n.clone();
        let is = |r| n.byte(r) == Some(byte);
        if byte == n.index && (v == n.strap2 || v == n.df) {
            self.index = v;
            return Some(true);
        }
        if byte == n.data && self.index == n.strap2 {
            if v != nbreg::strap2(self.nb.strap2) {
                return Some(self.refuse(format!("RCC_DEV0_EPF2_STRAP2 written {v:#x}: Linux clears STRAP_NO_SOFT_RESET_DEV0_F2 of {:#x} and nothing else", self.nb.strap2)));
            }
            self.nb.strap2 = v;
            return Some(true);
        }
        if byte == n.data && self.index == n.df {
            if !self.nb.atomics_req {
                return Some(self.refuse(String::from("DF NCSConfigurationRegister1 written with AtomicOp requester enable off: Linux writes it only with have_atomics_support (df_v4_15.c:29-40)")));
            }
            if v != nbreg::df_ncs_config1(self.nb.df) {
                return Some(self.refuse(format!("DF NCSConfigurationRegister1 written {v:#x}: Linux sets DisIntAtomicsLclProcessing of {:#x} and nothing else", self.nb.df)));
            }
            self.nb.df = v;
            return Some(true);
        }
        if byte == nbreg::REMAPPED_MEM_FLUSH_BYTE {
            if !self.nb.remapped[0] {
                return Some(self.refuse(String::from("an HDP flush through the remapped place before the remap was written")));
            }
            if v != 0 {
                return Some(self.refuse(format!("HDP flush written {v:#x}")));
            }
            if self.nb.table_unfenced {
                return Some(self.refuse(String::from("an HDP flush after GART entries were written with no store barrier between them: Linux's mb() comes first (amdgpu_gart.c:499)")));
            }
            self.hdp_flushed();
            return Some(true);
        }
        if byte == n.hdp_flush && self.nb.remapped[0] {
            return Some(self.refuse(String::from("an HDP flush through the register itself after the remap: Linux flushes at BAR5 byte 0x7f000")));
        }
        if !self.nb.vals.contains_key(&byte) {
            return None;
        }
        for (i, (r, want)) in [(nbreg::REMAP_HDP_MEM_FLUSH, nbreg::REMAPPED_MEM_FLUSH_BYTE), (nbreg::REMAP_HDP_REG_FLUSH, nbreg::REMAPPED_REG_FLUSH_BYTE)].into_iter().enumerate() {
            if is(r) {
                if v != want {
                    return Some(self.refuse(format!("{} written {v:#x}: Linux's remap is {want:#x}", r.name)));
                }
                self.nb.remapped[i] = true;
            }
        }
        // A system address: only after the survey said the card may reach system memory.
        if (is(hub41::FAULT_DEFAULT_ADDR_LO32) || is(nbreg::INTERRUPT_CNTL2)) && !self.nb.dma_ok() {
            return Some(self.refuse(String::from("a system address handed to the card before the DMAR survey allowed it")));
        }
        let ctx0 = [hub41::CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32, hub41::CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32, hub41::CONTEXT0_PAGE_TABLE_START_ADDR_LO32, member(hub41::CONTEXT0_PAGE_TABLE_START_ADDR_LO32, 1, 1), hub41::CONTEXT0_PAGE_TABLE_END_ADDR_LO32, member(hub41::CONTEXT0_PAGE_TABLE_END_ADDR_LO32, 1, 1)];
        if let Some(i) = ctx0.iter().position(|r| is(*r)) {
            self.nb.ctx0 |= 1 << i;
            if i < 2 && self.nb.ctx0 & 3 == 3 {
                // Stored first: the table's place is read from the registers.
                self.nb.vals.insert(byte, v);
                self.nb.table_named();
            }
        }
        if is(hub41::CONTEXT0_CNTL) && hub41::f::ENABLE_CONTEXT.get(v) != 0 {
            if self.nb.ctx0 != 0x3F {
                return Some(self.refuse(String::from("context 0 enabled before its page table base and range were written")));
            }
            if let Some(why) = self.table_refused() {
                return Some(self.refuse(why));
            }
            if !self.nb.context0 {
                // A context that starts has no translation cached: what reached VRAM is what the walker finds.
                self.nb.tlb_emptied();
            }
            self.nb.context0 = true;
        }
        let eng = |first| n.byte(member(first, hub41::GART_ENGINE, 1)) == Some(byte);
        if eng(hub41::INVALIDATE_ENG0_SEM) {
            if v != 0 {
                return Some(self.refuse(format!("the invalidation semaphore written {v:#x}")));
            }
            // The acknowledge answered the request made under this semaphore (gmc_v12_0.c:255-263): with the
            // release it is gone, so the next flush must make its own request before its wait can end.
            (self.nb.sem_held, self.nb.acked) = (false, false);
            return Some(true);
        }
        if eng(hub41::INVALIDATE_ENG0_REQ) {
            if !self.nb.sem_held && !self.nb.sem_never {
                return Some(self.refuse(String::from("an invalidation request without the engine's semaphore")));
            }
            if !self.nb.context0 {
                return Some(self.refuse(String::from("an invalidation request before context 0 is on")));
            }
            if v != hub41::invalidate_req(0) {
                return Some(self.refuse(format!("invalidation request {v:#x}: Linux flushes VMID 0 with {:#x}", hub41::invalidate_req(0))));
            }
            (self.nb.acked, self.nb.tlb_flushes) = (true, self.nb.tlb_flushes + 1);
            self.nb.tlb_emptied();
            return Some(true);
        }
        let mut stored = v;
        if is(hub41::BANK_SELECT_RESERVED_CID2) {
            // The private invalidation's bit clears itself (the trace reads 0x10105c17 back).
            stored &= !hub41::RESERVED_CACHE_PRIVATE_INVALIDATION;
        }
        for (i, ring) in [Ring::Ring0, Ring::Ring1].into_iter().enumerate() {
            let [cntl, base, hi, rptr, wptr] = FakeNb::ring_regs(ring);
            if is(base) {
                self.nb.ring_set[i] |= 1;
            }
            if is(hi) {
                self.nb.ring_set[i] |= 2;
            }
            if is(rptr) {
                self.nb.rptr[i] = v;
            }
            if is(wptr) {
                self.nb.wptr[i] = v;
            }
            if is(cntl) {
                if f::RB_SIZE.get(v) != 0 {
                    self.nb.ring_set[i] |= 4;
                }
                let on = f::RB_ENABLE.get(v) != 0;
                if on && !self.nb.ih_on[i] {
                    if let Err(why) = self.ring_may_start(ring, v) {
                        return Some(self.refuse(why));
                    }
                }
                self.nb.ih_on[i] = on;
                if f::WPTR_OVERFLOW_CLEAR.get(v) != 0 {
                    self.nb.wptr[i] &= !f::RB_OVERFLOW.mask;
                }
                // The clear bit reads back 0; ring 1 has no write-back bit (osssys_7_0_0_sh_mask.h).
                stored &= !f::WPTR_OVERFLOW_CLEAR.mask;
                if ring == Ring::Ring1 {
                    stored &= !f::WPTR_WRITEBACK_ENABLE.mask;
                }
            }
        }
        self.nb.vals.insert(byte, stored);
        Some(true)
    }
}

//! Wave 43: the model's two SDMA 7.0.1 engines as their registers show them. Like the rest of the model it
//! answers reads as the real card answered Linux (init.txt lines 9604-9671 and 9823-9826) and refuses, and keeps,
//! what Linux would not do:
//!
//! - any write to a register of the block, or a doorbell, before the GC block has power and its autoload is
//!   complete (`Model::gc_awake`, the one answer the graphics block's model gives too: the power controller's
//!   features are on, `fakechain`, and the secure processor took AUTOLOAD_RLC, which follows the load of the
//!   engines' firmware, type 71). Linux's first write here follows both (the trace's order of sections);
//! - a ring's enable on a halted engine;
//! - a ring's enable before its base, its size and all four pointers were written since its control was last
//!   written without the enable (Linux's first write of a start), with its
//!   base outside the GART, with a page of the ring, its read-pointer slot or its poll slot not bound as the
//!   card's walker sees the table (a bind without Linux's HDP flush and invalidation is not seen, `fakegart`), or
//!   while the card may not send;
//! - a ring's enable with its ring, its read-pointer slot or its poll slot overlapping one another or those of
//!   the other engine while that one runs (Linux gives each its own memory);
//! - a pointer written while the ring runs; a write pointer below the one it holds without MINOR_PTR_UPDATE
//!   ("before programing wptr to a less value, need set minor_ptr_update first", sdma_v7_0.c:534);
//! - NBIF's doorbell entry for the engines written with anything but Linux's value for what it holds;
//! - a doorbell before the aperture, NBIF's entry (enabled, the index inside its range) and the queue's own
//!   doorbell register were set up, at an index no queue was given, with the ring off, or with a value the
//!   write-pointer poll slot does not hold (Linux writes the slot first, :226-230);
//! - a register of the block that a driver only reads.
//!
//! What the engines do with a ring is `fakesdmaeng`.

use super::fakemodel::Model;
use super::sdmaregs::SdmaRegs;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::sim::Engine;
use rdna4_core::regmap::Reg;
use rdna4_core::sdma7::{self, f};
use rdna4_core::{layout, nbreg, pte12};

/// What an idle engine's status reads in the model: idle with an empty ring. The real card's value is in no
/// trace (plan/spec/gpu-rdna4.md section 7); a halted or busy engine reads without IDLE.
pub const STATUS_IDLE: u32 = f::IDLE.mask | f::RB_EMPTY.mask;
/// `SDMA0_CNTL` as the cold card holds it (init.txt line 9823): the trap, PIO done acknowledge, the MES interrupt
/// and the page fault interrupt enabled.
pub const CNTL_COLD: u32 = 0x0000_2441;

// What must be written before a ring's enable, as bits of `One::set` (the size is in the enabling write).
const BASE: u8 = 1;
const BASE_HI: u8 = 2;
const RPTR: u8 = 4;
const RPTR_HI: u8 = 8;
const WPTR: u8 = 16;
const WPTR_HI: u8 = 32;
const ALL_SET: u8 = 63;

/// One engine.
pub struct One {
    /// Registers by their offset in `sdma7` (engine 0's).
    vals: BTreeMap<u32, u32>,
    set: u8,
    minor: bool,
    pub on: bool,
    /// The pointers in the registers' form (bytes).
    pub rptr: u64,
    pub wptr: u64,
    pub(super) eng: Option<Engine>,
    /// Why the engine stopped at a packet, if it did: it stays stopped until its ring is switched off.
    pub halted: Option<String>,
    /// Traps the engine raised, and those that found the trap disabled or the IH off.
    pub traps: u64,
    pub traps_lost: u64,
}

impl One {
    fn new() -> One {
        let mut vals = BTreeMap::new();
        for r in sdma7::ALL {
            vals.insert(r.offset, 0);
        }
        for (r, v) in [(sdma7::RB_CNTL, 0x0004_0800), (sdma7::IB_CNTL, 0x100), (sdma7::UTCL1_CNTL, 0x3B00_0288), (sdma7::UTCL1_PAGE, 0x000C_EC20), (sdma7::CNTL, CNTL_COLD)] {
            vals.insert(r.offset, v);
        }
        One { vals, set: 0, minor: false, on: false, rptr: 0, wptr: 0, eng: None, halted: None, traps: 0, traps_lost: 0 }
    }

    pub fn val(&self, r: Reg) -> u32 {
        self.vals.get(&r.offset).copied().unwrap_or(0)
    }

    /// A 64-bit address held by a low and a high register.
    pub(super) fn addr(&self, lo: Reg, hi: Reg) -> u64 {
        u64::from(self.val(hi)) << 32 | u64::from(self.val(lo))
    }

    pub(super) fn ring_mc(&self) -> u64 {
        self.addr(sdma7::RB_BASE, sdma7::RB_BASE_HI) << 8
    }
}

pub struct FakeSdma {
    pub r: SdmaRegs,
    pub e: [One; 2],
    /// NBIF's doorbell entry for the engines and the in-ring HDP flush's done bits.
    pub entry2: u32,
    pub(super) hdp_done: u32,
    /// Every doorbell that reached an engine: (dword index, value).
    pub doorbells: Vec<(u32, u64)>,
    /// Accesses of an engine the hub refused: (address, write).
    pub hub_faults: Vec<(u64, bool)>,
    /// HDP flushes the engines asked for from their rings.
    pub ring_flushes: u32,
    /// VRAM the CPU wrote since the last HDP flush, with the bytes from before: what an engine still reads there.
    pub(super) stale: Vec<(u64, Vec<u8>)>,
    /// Tests: this engine never runs its ring (a ring test that runs out).
    pub dead: [bool; 2],
    /// Tests: the engines get their data wrong in this way (`fakesdmaeng::Spoil`).
    pub spoil: Option<super::fakesdmaeng::Spoil>,
}

impl FakeSdma {
    pub fn new(r: SdmaRegs) -> FakeSdma {
        FakeSdma { r, e: [One::new(), One::new()], entry2: 0, hdp_done: 0, doorbells: Vec::new(), hub_faults: Vec::new(), ring_flushes: 0, stale: Vec::new(), dead: [false; 2], spoil: None }
    }

    /// An engine runs: its ring is on and may be read by the card.
    pub fn any_on(&self) -> bool {
        self.e.iter().any(|e| e.on)
    }
}

fn name(i: u32, r: Reg) -> String {
    format!("SDMA{i}{}", r.name.strip_prefix("SDMA0").unwrap_or(r.name))
}

impl Model {
    /// A read of a register of the block or of NBIF's doorbell entry for it; `None` for any other byte.
    pub(super) fn sdma_read(&mut self, byte: u32) -> Option<u32> {
        if self.sdma.r.nbif(sdma7::DOORBELL_ENTRY_2) == Some(byte) {
            return Some(self.sdma.entry2);
        }
        let (i, r) = self.sdma.r.reg_at(byte)?;
        let e = &self.sdma.e[i as usize];
        let is = |x: Reg| x.offset == r.offset;
        Some(match () {
            _ if is(sdma7::RB_RPTR) || is(sdma7::RB_RPTR_FETCH) => e.rptr as u32,
            _ if is(sdma7::RB_RPTR_HI) || is(sdma7::RB_RPTR_FETCH_HI) => (e.rptr >> 32) as u32,
            _ if is(sdma7::RB_WPTR) => e.wptr as u32,
            _ if is(sdma7::RB_WPTR_HI) => (e.wptr >> 32) as u32,
            _ if is(sdma7::STATUS_REG) => {
                if e.halted.is_some() || (e.on && e.rptr != e.wptr) { 0 } else { STATUS_IDLE }
            }
            _ => e.val(r),
        })
    }

    /// A write to a register of the block or to NBIF's doorbell entry for it: `Some(reached)`, or `None` for any
    /// other byte.
    pub(super) fn sdma_write(&mut self, byte: u32, v: u32) -> Option<bool> {
        let entry = self.sdma.r.nbif(sdma7::DOORBELL_ENTRY_2) == Some(byte);
        let at = self.sdma.r.reg_at(byte);
        if !entry && at.is_none() {
            return None;
        }
        if !self.gc_awake() {
            let what = at.map_or(String::from(sdma7::DOORBELL_ENTRY_2.name), |(i, r)| name(i, r));
            // NBIF has power of its own; its entry is refused here only as part of the engines' start.
            return Some(self.refuse(format!("{what} written {v:#x} while the GC block has no power or its autoload is not complete (the power controller's start and the autoload come first)")));
        }
        if entry {
            let want = sdma7::doorbell_range(self.sdma.entry2);
            if v != want {
                return Some(self.refuse(format!("{} written {v:#x}: Linux writes {want:#x} for what it holds", sdma7::DOORBELL_ENTRY_2.name)));
            }
            self.sdma.entry2 = v;
            return Some(true);
        }
        let (i, r) = at?;
        Some(match self.sdma_reg_write(i as usize, r, v) {
            Ok(()) => true,
            Err(why) => self.refuse(format!("{} written {v:#x}: {why}", name(i, r))),
        })
    }

    fn sdma_reg_write(&mut self, i: usize, r: Reg, v: u32) -> Result<(), String> {
        let is = |x: Reg| x.offset == r.offset;
        if is(sdma7::RB_CNTL) {
            let on = f::RB_ENABLE.get(v) != 0;
            if !on {
                // Written off (Linux's first write of a start, sdma_v7_0.c:485-494): what a start needs is
                // counted from here, and a stopped engine is one no more.
                let e = &mut self.sdma.e[i];
                (e.eng, e.set, e.halted) = (None, 0, None);
            }
            if on && !self.sdma.e[i].on {
                self.sdma_may_start(i, v)?;
                let e = &self.sdma.e[i];
                let (base, dw, start) = (e.ring_mc(), 1u64 << f::RB_SIZE.get(v), e.rptr >> 2);
                self.sdma.e[i].eng = Some(super::fakesdmaeng::engine(base, dw, start).map_err(|e| format!("the ring is none an engine takes: {e:?}"))?);
            }
            self.sdma.e[i].on = on;
        }
        let e = &mut self.sdma.e[i];
        let pointer = [(sdma7::RB_RPTR, RPTR), (sdma7::RB_RPTR_HI, RPTR_HI), (sdma7::RB_WPTR, WPTR), (sdma7::RB_WPTR_HI, WPTR_HI)].into_iter().find(|(x, _)| is(*x));
        if let Some((_, bit)) = pointer {
            if e.on {
                return Err(String::from("a ring pointer written while the ring runs"));
            }
            let hi = bit == RPTR_HI || bit == WPTR_HI;
            let old = if bit & (RPTR | RPTR_HI) != 0 { e.rptr } else { e.wptr };
            let new = if hi { u64::from(v) << 32 | old & 0xFFFF_FFFF } else { old & !0xFFFF_FFFF | u64::from(v) };
            if bit & (WPTR | WPTR_HI) != 0 {
                if new < e.wptr && !e.minor {
                    return Err(format!("a write pointer below the {:#x} it holds without MINOR_PTR_UPDATE (sdma_v7_0.c:534)", e.wptr));
                }
                e.wptr = new;
            } else {
                e.rptr = new;
            }
            e.set |= bit;
            return Ok(());
        }
        let read_only = [sdma7::UCODE_REV, sdma7::RB_RPTR_FETCH, sdma7::RB_RPTR_FETCH_HI, sdma7::STATUS_REG, sdma7::UTCL1_RD_STATUS, sdma7::UTCL1_WR_STATUS, sdma7::INT_STATUS];
        if read_only.into_iter().any(is) {
            return Err(String::from("a register a driver only reads"));
        }
        if is(sdma7::RB_BASE) {
            e.set |= BASE;
        }
        if is(sdma7::RB_BASE_HI) {
            e.set |= BASE_HI;
        }
        if is(sdma7::MINOR_PTR_UPDATE) {
            e.minor = f::MINOR_PTR_UPDATE_ENABLE.get(v) != 0;
        }
        e.vals.insert(r.offset, v);
        Ok(())
    }

    /// Whether engine `i`'s ring may be switched on with this control value.
    fn sdma_may_start(&mut self, i: usize, cntl: u32) -> Result<(), String> {
        let e = &self.sdma.e[i];
        if f::HALT.get(e.val(sdma7::MCU_CNTL)) != 0 {
            return Err(String::from("the ring enabled on a halted engine (Linux unhalts first, sdma_v7_0.c:586-595)"));
        }
        // The size comes with the enabling write itself, as Linux writes it (:594).
        if e.set != ALL_SET || f::RB_SIZE.get(cntl) == 0 {
            return Err(String::from("the ring enabled before its base, its size and its four pointers were written since it was last off"));
        }
        let (mc, bytes) = (e.ring_mc(), 4u64 << f::RB_SIZE.get(cntl));
        if mc.checked_add(bytes).is_none_or(|end| end > layout::GART_BYTES) {
            return Err(format!("the ring at mc {mc:#x} with {bytes:#x} bytes: not inside the GART"));
        }
        // Every page, as Linux binds every page of a ring (amdgpu_gart_map, amdgpu_gart.c:354-374).
        if let Some(page) = (0..bytes.div_ceil(pte12::PAGE)).find(|p| self.gc_hub_walk(mc + p * pte12::PAGE).is_none()) {
            return Err(format!("the ring at mc {mc:#x}: its pages are not bound in the GART (page {page}, as the card's walker sees the table now)"));
        }
        let slots = [(f::RPTR_WRITEBACK_ENABLE, sdma7::RB_RPTR_ADDR_LO, sdma7::RB_RPTR_ADDR_HI, "read-pointer slot"), (f::MCU_WPTR_POLL_ENABLE, sdma7::RB_WPTR_POLL_ADDR_LO, sdma7::RB_WPTR_POLL_ADDR_HI, "write-pointer poll slot")];
        for (field, lo, hi, what) in slots {
            let at = e.addr(lo, hi);
            if field.get(cntl) != 0 && self.gc_hub_walk(at).is_none() {
                return Err(format!("the ring enabled with its {what} at mc {at:#x} not bound in the GART"));
            }
        }
        if !self.nb.bus_master {
            return Err(String::from("the ring enabled while the card may not send (bus master off)"));
        }
        // Linux gives every ring its own pages and every slot its own place in the write-back page
        // (amdgpu_ring_init, amdgpu_ring.c:264-320 and :360-366): two engines on one ring would each fetch the
        // other's packets, and a slot inside a ring is overwritten by the next submission.
        let mine = self.sdma_claims(i, Some(cntl));
        let others: Vec<_> = (0..self.sdma.e.len()).filter(|o| *o != i && self.sdma.e[*o].on).flat_map(|o| self.sdma_claims(o, None)).collect();
        let clash = mine.iter().enumerate().find_map(|(k, a)| mine[k + 1..].iter().chain(&others).find(|b| a.1.start < b.1.end && b.1.start < a.1.end).map(|b| (a, b)));
        if let Some((a, b)) = clash {
            return Err(format!("the ring enabled with {} at mc {:#x} overlapping {} at mc {:#x}: Linux gives each ring and each slot its own memory", a.0, a.1.start, b.0, b.1.start));
        }
        Ok(())
    }

    /// The memory engine `i` is told to use, by its registers: its ring, and the slots its control enables.
    /// `cntl`: the control value being written (an engine that runs is asked by the one it holds).
    fn sdma_claims(&self, i: usize, cntl: Option<u32>) -> Vec<(String, core::ops::Range<u64>)> {
        let e = &self.sdma.e[i];
        let cntl = cntl.unwrap_or(e.val(sdma7::RB_CNTL));
        let mut out = alloc::vec![(format!("SDMA{i}'s ring"), e.ring_mc()..e.ring_mc() + (4u64 << f::RB_SIZE.get(cntl)))];
        let slots = [(f::RPTR_WRITEBACK_ENABLE, sdma7::RB_RPTR_ADDR_LO, sdma7::RB_RPTR_ADDR_HI, "read-pointer slot"), (f::MCU_WPTR_POLL_ENABLE, sdma7::RB_WPTR_POLL_ADDR_LO, sdma7::RB_WPTR_POLL_ADDR_HI, "write-pointer poll slot")];
        for (field, lo, hi, what) in slots {
            if field.get(cntl) != 0 {
                let at = e.addr(lo, hi);
                out.push((format!("SDMA{i}'s {what}"), at..at + 8));
            }
        }
        out
    }

    /// A 64-bit doorbell write (`WDOORBELL64`): an enabled SDMA doorbell through an open aperture and NBIF's
    /// entry, with the value the poll slot holds, or refused. The engine then runs its ring.
    pub(super) fn sdma_doorbell(&mut self, index_dw: u32, v: u64) -> bool {
        self.deepest = self.deepest.min(super::fakemodel::stack_at());
        if !self.gc_awake() {
            return self.refuse(format!("doorbell {index_dw:#x} rung while the GC block has no power or its autoload is not complete"));
        }
        let entry = self.sdma.entry2;
        let (first, size) = (sdma7::PORT2_RANGE_OFFSET.get(entry), sdma7::PORT2_RANGE_SIZE.get(entry));
        let open = self.nb.val(nbreg::DOORBELL_APER_EN) & 1 != 0 && sdma7::PORT2_ENABLE.get(entry) != 0 && (first..first + size).contains(&index_dw);
        if !open {
            return self.refuse(format!("doorbell {index_dw:#x} rung before the aperture and NBIF's doorbell entry for the engines were set up to cover it"));
        }
        let own = |e: &One| f::DOORBELL_ENABLE.get(e.val(sdma7::DOORBELL)) != 0 && f::DOORBELL_OFFSET.get(e.val(sdma7::DOORBELL_OFFSET)) == index_dw;
        let Some(i) = self.sdma.e.iter().position(own) else {
            return self.refuse(format!("doorbell {index_dw:#x}: no engine's queue has this doorbell enabled"));
        };
        if !self.sdma.e[i].on {
            return self.refuse(format!("doorbell {index_dw:#x} rung with engine {i}'s ring off"));
        }
        if v & 3 != 0 {
            return self.refuse(format!("doorbell {index_dw:#x} written {v:#x}: no write pointer in bytes"));
        }
        let e = &self.sdma.e[i];
        if f::MCU_WPTR_POLL_ENABLE.get(e.val(sdma7::RB_CNTL)) != 0 {
            let at = e.addr(sdma7::RB_WPTR_POLL_ADDR_LO, sdma7::RB_WPTR_POLL_ADDR_HI);
            let mut b = [0u8; 8];
            let held = self.gc_hub_walk(at).map(|sys| {
                self.nb.sys_peek(sys, &mut b);
                u64::from_le_bytes(b)
            });
            if held != Some(v) {
                return self.refuse(format!("doorbell {index_dw:#x} written {v:#x} while engine {i}'s write-pointer poll slot holds {held:x?}: Linux writes the slot first (sdma_v7_0.c:226-230)"));
            }
        }
        self.sdma.doorbells.push((index_dw, v));
        self.sdma.e[i].wptr = v;
        self.sdma_run(i);
        true
    }
}

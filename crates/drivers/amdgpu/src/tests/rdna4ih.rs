//! Wave 42: the IH rings read as Linux reads them (`rdna4::ihring`), on the model whose card side writes the
//! entries where the `ih` stage told it to: through the GART into the system pages, the write pointer into its
//! write-back slot. Entries of the 18.11. trace come out as Linux printed them and are counted per source, since
//! no block handles one yet; the read pointer goes back by slot and doorbell; an overflow, the self-interrupt for
//! ring 1, a pointer off the grid, memory that does not read and a ring that never empties.

use super::rdna4fix::*;
use super::rdna4nb::up;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::ihring::{IhIo, IhPlace, Pass, Reader};
use crate::rdna4::stage::Session;
use rdna4_core::ih7::{self, Iv, Ring};
use std::vec::Vec;

fn ih_up() -> (Session, Model, u64) {
    let rig = Rig::new();
    let mut s = Session::default();
    let (_, m) = up(&rig, &mut s, model());
    let sys = s.nb.ih.as_ref().unwrap().place.sys;
    (s, m, sys)
}

fn iv(client_id: u8, src_id: u8, ring_id: u8, timestamp: u64, d0: u32) -> Iv {
    Iv { client_id, src_id, ring_id, timestamp, src_data: [d0, 0, 0, 0], ..Iv::default() }
}

/// The first entries of the trace (trace-load.txt lines 62122, 108353, 108354, 108357, 108358, 153873, 154371).
fn seen() -> [Iv; 7] {
    [
        iv(4, 9, 0, 25_997_017_620, 6),
        iv(20, 181, 5, 26_027_898_750, 0),
        iv(20, 181, 20, 26_027_901_895, 0),
        iv(10, 49, 16, 26_030_656_633, 0),
        iv(16, 119, 0, 26_030_680_255, 0),
        iv(4, 79, 0, 26_097_363_931, 0x038A_942C),
        iv(4, 87, 0, 26_098_435_588, 0x039A_EE10),
    ]
}

#[test]
fn entries_arrive_where_the_stage_said_and_are_counted_per_source() {
    let (mut s, mut m, sys) = ih_up();
    let reader = &mut s.nb.ih.as_mut().unwrap().reader;
    // Nothing yet: a look reads nothing and still writes the read pointer back, as Linux's does.
    assert_eq!(reader.process(&mut ModelIo { m: &mut m, sys }), [Pass::default(); 2]);
    for e in seen() {
        m.raise(Ring::Ring0, &e).unwrap();
    }
    // One message for the first entry; the next only after the read pointer moved (RPTR_REARM).
    assert_eq!((m.nb.msi_sent, m.nb.wptr[0]), (1, 7 * 32));
    let passes = reader.process(&mut ModelIo { m: &mut m, sys });
    assert_eq!(passes, [Pass { entries: 7, more: false }, Pass::default()]);
    assert_eq!(reader.entries, [7, 0]);
    // Nobody handles a source yet: each is counted, none dropped.
    let counted: Vec<((u8, u8), u64)> = reader.unclaimed.iter().map(|(k, v)| (*k, *v)).collect();
    assert_eq!(counted, [((4, 9), 1), ((4, 79), 1), ((4, 87), 1), ((10, 49), 1), ((16, 119), 1), ((20, 181), 2)]);
    assert_eq!(reader.unclaimed.values().sum::<u64>(), 7);
    // The read pointer: in its slot of the write-back page and through ring 0's doorbell.
    assert_eq!((reader.rptr(Ring::Ring0), m.nb.rptr[0]), (7 * 32, 7 * 32));
    let mut io = ModelIo { m: &mut m, sys };
    assert_eq!((io.mem32(IhPlace::rptr_at(Ring::Ring0)), io.mem32(IhPlace::wptr_at(Ring::Ring0))), (Some(7 * 32), Some(7 * 32)));
    // The entries lie in ring 0's memory as eight dwords each, Linux's layout.
    assert_eq!(io.mem32(IhPlace::ring_at(Ring::Ring0)), Some(4 | 9 << 8));
    assert_eq!(io.mem32(IhPlace::ring_at(Ring::Ring0) + 6 * 32 + 16), Some(0x039A_EE10));
    // Re-armed by the doorbell: the next entry sends a message again, and a look finds exactly it.
    m.raise(Ring::Ring0, &iv(4, 87, 0, 1, 2)).unwrap();
    assert_eq!(m.nb.msi_sent, 2);
    assert_eq!(reader.process(&mut ModelIo { m: &mut m, sys })[0].entries, 1);
    assert_eq!(reader.unclaimed[&(4, 87)], 2);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    assert!(reader.describe().contains("no handler for: client 4 source 9: 1, client 4 source 79: 1, client 4 source 87: 2"), "{}", reader.describe());
}

#[test]
fn more_entries_than_one_batch_are_read_in_linuxs_batches() {
    let (mut s, mut m, sys) = ih_up();
    let reader = &mut s.nb.ih.as_mut().unwrap().reader;
    for i in 0..100u32 {
        m.raise(Ring::Ring0, &iv(10, 49, 0, u64::from(i), i)).unwrap();
    }
    assert_eq!(reader.process(&mut ModelIo { m: &mut m, sys })[0], Pass { entries: 100, more: false });
    assert_eq!((reader.unclaimed[&(10, 49)], m.nb.rptr[0]), (100, 100 * 32));
    // Linux moves the read pointer every 31 entries (AMDGPU_IH_MAX_NUM_IVS - 1): 31, 62, 93, 100.
    assert_eq!(m.nb.msi_sent, 1 + 3, "each doorbell with entries still waiting lets the next message out");
}

#[test]
fn an_overflow_goes_on_behind_the_write_pointer_and_clears_the_bit() {
    let (mut s, mut m, sys) = ih_up();
    let reader = &mut s.nb.ih.as_mut().unwrap().reader;
    let slots = ih7::RING_BYTES / 32;
    // The card fills the whole ring while nobody reads: the last entry runs into the read pointer.
    for i in 0..slots {
        m.raise(Ring::Ring0, &iv(10, 49, 0, u64::from(i), i)).unwrap();
    }
    assert_eq!(m.nb.wptr[0], 1, "the write pointer wrapped to 0 with RB_OVERFLOW");
    let writes = m.writes.len();
    let p = reader.process(&mut ModelIo { m: &mut m, sys })[0];
    assert_eq!(reader.overflows, 1);
    // "start parsing interrupt from the last not overwritten vector (wptr + 32)": every entry but the oldest.
    assert_eq!((p, reader.entries[0]), (Pass { entries: slots - 1, more: false }, u64::from(slots - 1)));
    assert_eq!((reader.rptr(Ring::Ring0), m.nb.rptr[0], m.nb.wptr[0]), (0, 0, 0));
    // The clear bit set and unset at once, ring 0's control only (ih_v7_0.c:480-487).
    let cntl: Vec<u32> = m.writes[writes..].iter().map(|w| w.1).collect();
    assert_eq!(cntl, [0xC033_0121, 0x4033_0121]);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    // The next entry is read as usual.
    m.raise(Ring::Ring0, &iv(4, 9, 0, 0, 0)).unwrap();
    assert_eq!(reader.process(&mut ModelIo { m: &mut m, sys })[0].entries, 1);
    assert_eq!(reader.overflows, 1);
}

#[test]
fn the_self_interrupt_brings_ring_1s_write_pointer() {
    let (mut s, mut m, sys) = ih_up();
    let reader = &mut s.nb.ih.as_mut().unwrap().reader;
    // Client 10 source 49 on ring 1: no block handles it (client 10 source 0, which `ih` redirects there, is
    // GFXHUB's page fault and has its taker: `rdna4fault`).
    for i in 0..3u32 {
        m.raise(Ring::Ring1, &iv(10, 49, 0, u64::from(i), 0x1000 + i)).unwrap();
    }
    // Without the IH's own entry nobody looks at ring 1 (it has no write-back from the card).
    assert_eq!(reader.process(&mut ModelIo { m: &mut m, sys }), [Pass::default(); 2]);
    // Client 0, source 0, ring 1, the write pointer in the first data word (ih_v7_0_self_irq).
    m.raise(Ring::Ring0, &Iv { ring_id: 1, src_data: [3 * 32, 0, 0, 0], ..Iv::default() }).unwrap();
    let passes = reader.process(&mut ModelIo { m: &mut m, sys });
    assert_eq!(passes, [Pass { entries: 1, more: false }, Pass { entries: 3, more: false }]);
    assert_eq!((reader.self_irqs, reader.entries, reader.unclaimed.get(&(10, 49)).copied()), (1, [1, 3], Some(3)));
    assert_eq!(reader.faults.count, [0, 0]);
    assert!(!reader.unclaimed.contains_key(&(0, 0)), "the IH's own entry has its handler");
    assert_eq!(m.nb.rptr, [32, 3 * 32], "each ring's doorbell");
    // A self-interrupt that names another ring is counted and moves nothing.
    m.raise(Ring::Ring0, &Iv { ring_id: 2, src_data: [0x40, 0, 0, 0], ..Iv::default() }).unwrap();
    assert_eq!(reader.process(&mut ModelIo { m: &mut m, sys }), [Pass { entries: 1, more: false }, Pass::default()]);
    assert_eq!(reader.self_irqs, 2);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}

/// A ring whose parts fail or lie on purpose.
struct Odd {
    wptr: u32,
    step: u32,
    mem_fails: bool,
    doorbells: Vec<u32>,
}

impl IhIo for Odd {
    fn mem32(&mut self, off: u64) -> Option<u32> {
        if off == IhPlace::wptr_at(Ring::Ring0) {
            let w = self.wptr;
            self.wptr = (self.wptr + self.step) & ih7::PTR_MASK;
            return Some(w);
        }
        (!self.mem_fails).then_some(0x0000_090A)
    }
    fn set_mem32(&mut self, _: u64, _: u32) -> bool {
        true
    }
    fn wptr_reg(&mut self, _: Ring) -> Option<u32> {
        None
    }
    fn cntl(&mut self, _: Ring) -> Option<u32> {
        None
    }
    fn set_cntl(&mut self, _: Ring, _: u32) -> bool {
        false
    }
    fn doorbell(&mut self, _: Ring, rptr: u32) -> bool {
        self.doorbells.push(rptr);
        true
    }
    fn fault_status(&mut self, _: rdna4_core::hub41::Kind) -> Option<u32> {
        None
    }
    fn fault_clear(&mut self, _: rdna4_core::hub41::Kind) -> bool {
        false
    }
    fn now_us(&mut self) -> u64 {
        0
    }
}

#[test]
fn a_ring_that_misbehaves_is_bounded_cut_down_or_left_alone() {
    // A write pointer off the 32-byte grid is cut down, never followed into the middle of an entry.
    let mut r = Reader::default();
    let mut io = Odd { wptr: 0x46, step: 0, mem_fails: false, doorbells: Vec::new() };
    assert_eq!(r.process(&mut io)[0], Pass { entries: 2, more: false });
    assert_eq!((r.rptr(Ring::Ring0), &io.doorbells[..]), (0x40, &[0x40][..]));
    assert_eq!(r.unclaimed[&(0x0A, 0x09)], 2);
    // A ring that never empties ends the pass at its bound and says there is more.
    let mut r = Reader::default();
    let mut io = Odd { wptr: 32, step: 31 * 32, mem_fails: false, doorbells: Vec::new() };
    let p = r.process(&mut io)[0];
    assert!(p.more && p.entries >= 2 * ih7::RING_BYTES / 32 && p.entries < 2 * ih7::RING_BYTES / 32 + 32, "{p:?}");
    // Entries that do not read: the pass ends there, counted, the read pointer where it was.
    let mut r = Reader::default();
    let mut io = Odd { wptr: 64, step: 0, mem_fails: true, doorbells: Vec::new() };
    assert_eq!(r.process(&mut io)[0], Pass::default());
    assert_eq!((r.bad_reads, r.rptr(Ring::Ring0), io.doorbells.len()), (1, 0, 0));
    // An overflow whose register does not read: the ring is left alone.
    let mut r = Reader::default();
    let mut io = Odd { wptr: 0x41, step: 0, mem_fails: false, doorbells: Vec::new() };
    assert_eq!(r.process(&mut io)[0], Pass::default());
    assert_eq!((r.bad_reads, r.overflows, io.doorbells.len()), (1, 0, 0));
}

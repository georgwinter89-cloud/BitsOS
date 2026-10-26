//! PC-3 host tests: the MSI and MSI-X capability logic over the fake configuration space and a fake table.

use super::fake::{Fake, FakeFn};
use crate::access::{ConfigAccess, Loc};
use crate::msi::{self, MsiLayout, MsixLayout, Plan, Table};
use hal::msi::{compose, Message};
use std::sync::Mutex;
use std::vec::Vec;

const AT: Loc = Loc::new(0, 3, 0, 0);

/// A function with an MSI capability at 0x50 (control as given) and an MSI-X capability at 0x70 (`size`
/// entries, table in BAR3 at 0, PBA in BAR3 at 0x2000), like the I226 (spec/net.md §2.1).
fn device(msi_ctrl: u16, size: u16) -> Fake {
    let fake = Fake::new();
    let mut f = FakeFn::new(0x8086, 0x125C, 0x02_00_00, 0);
    f.cap(0x50, 0x05, 0x70).w16(0x52, msi_ctrl);
    f.cap(0x70, 0x11, 0).w16(0x72, size - 1).w32(0x74, 3).w32(0x78, 3 | 0x2000);
    f.cmd(0x6);
    fake.add_at(AT, f);
    fake
}

/// MSI-X table as memory: dwords plus a log of (offset, value, entry masked when written).
struct FakeTable {
    mem: Mutex<Vec<u32>>,
    log: Mutex<Vec<(usize, u32, bool)>>,
}

impl FakeTable {
    fn new(entries: usize) -> FakeTable {
        FakeTable { mem: Mutex::new(std::vec![0; entries * 4]), log: Mutex::new(Vec::new()) }
    }
    fn ctrl(&self, i: usize) -> u32 {
        self.mem.lock().unwrap()[i * 4 + 3]
    }
}

impl Table for FakeTable {
    fn read32(&self, off: usize) -> Option<u32> {
        self.mem.lock().unwrap().get(off / 4).copied()
    }
    fn write32(&self, off: usize, v: u32) -> bool {
        let mut m = self.mem.lock().unwrap();
        let Some(d) = m.get_mut(off / 4) else { return false };
        *d = v;
        let masked = m[(off / 16) * 4 + 3] & 1 != 0;
        self.log.lock().unwrap().push((off, v, masked));
        true
    }
}

#[test]
fn msi_layouts() {
    let f = device(0x0000, 1);
    let l = MsiLayout::read(&f, AT, 0x50).unwrap();
    assert_eq!(l, MsiLayout { cap: 0x50, is64: false, maskbit: false, capable_log2: 0 });
    assert_eq!((l.data_off(), l.mask_off(), l.pending_off()), (0x58, None, None));
    // 64-bit, per-vector masks, 32 messages capable (MMC = 5).
    let f = device(0x0180 | 5 << 1, 1);
    let l = MsiLayout::read(&f, AT, 0x50).unwrap();
    assert_eq!(l, MsiLayout { cap: 0x50, is64: true, maskbit: true, capable_log2: 5 });
    assert_eq!((l.data_off(), l.mask_off(), l.pending_off()), (0x5C, Some(0x60), Some(0x64)));
    assert_eq!(l.capable(), 32);
    // Reserved MMC encodings 6/7 read as 32.
    let f = device(0x0100 | 7 << 1, 1);
    let l = MsiLayout::read(&f, AT, 0x50).unwrap();
    assert_eq!((l.capable_log2, l.data_off(), l.mask_off()), (5, 0x58, Some(0x5C)));
    let empty = Fake::new();
    assert_eq!(MsiLayout::read(&empty, AT, 0x50), None, "nothing answers");
}

#[test]
fn msix_layout() {
    let f = device(0, 5);
    let l = MsixLayout::read(&f, AT, 0x70).unwrap();
    assert_eq!(l, MsixLayout { cap: 0x70, size: 5, table_bir: 3, table_offset: 0, pba_bir: 3, pba_offset: 0x2000 });
    assert_eq!((l.table_bytes(), l.pba_bytes()), (80, 8));
    let f = device(0, 2048);
    let l = MsixLayout::read(&f, AT, 0x70).unwrap();
    assert_eq!((l.size, l.table_bytes(), l.pba_bytes()), (2048, 32768, 256));
}

#[test]
fn block_sizes_and_plans() {
    assert_eq!(msi::msi_block_log2(1, 5), 0);
    assert_eq!(msi::msi_block_log2(3, 5), 1);
    assert_eq!(msi::msi_block_log2(4, 5), 2);
    assert_eq!(msi::msi_block_log2(64, 5), 5);
    assert_eq!(msi::msi_block_log2(8, 2), 2);
    assert_eq!(msi::msi_block_log2(0, 3), 0);
    let all = (true, true, true);
    assert_eq!(msi::plan(1, 4, Some((5, true)), Some(0), all), Some(Plan::MsiX { n: 4 }));
    assert_eq!(msi::plan(1, 8, Some((5, true)), Some(0), all), Some(Plan::MsiX { n: 5 }));
    assert_eq!(msi::plan(6, 8, Some((5, true)), Some(3), all), Some(Plan::Msi { block_log2: 3 }), "MSI-X too small for min");
    assert_eq!(msi::plan(1, 4, Some((5, false)), Some(0), all), Some(Plan::Msi { block_log2: 0 }), "table outside its BAR");
    assert_eq!(msi::plan(2, 4, None, Some(0), all), Some(Plan::Polled), "single MSI below min");
    assert_eq!(msi::plan(2, 4, None, Some(0), (true, true, false)), None);
    assert_eq!(msi::plan(1, 1, Some((5, true)), Some(0), (false, true, true)), Some(Plan::Msi { block_log2: 0 }));
    assert_eq!(msi::plan(1, 1, None, None, (true, true, true)), Some(Plan::Polled));
    assert_eq!(msi::plan(0, 0, Some((1, true)), None, all), Some(Plan::MsiX { n: 1 }), "0 asks for one");
}

#[test]
fn aligned_runs() {
    assert_eq!(msi::aligned_run(&[0x31, 0x32, 0x33, 0x34, 0x35], 4), None, "0x34 needs 0x36 and 0x37");
    assert_eq!(msi::aligned_run(&[0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37], 4), Some(0x34));
    assert_eq!(msi::aligned_run(&[0x30, 0x31, 0x33], 2), Some(0x30));
    assert_eq!(msi::aligned_run(&[0x31, 0x32], 2), None, "0x32/0x33 incomplete, 0x30 missing");
    assert_eq!(msi::aligned_run(&[0x45], 1), Some(0x45));
    assert_eq!(msi::aligned_run(&[0xFE, 0xFF], 2), Some(0xFE));
    assert_eq!(msi::aligned_run(&[], 1), None);
}

#[test]
fn msi_program_keeps_read_only_bits_and_header() {
    // 64-bit with masks, 4 capable; the header dword's id/next and the RO bits must survive.
    let f = device(0x0180 | 2 << 1, 1);
    let l = MsiLayout::read(&f, AT, 0x50).unwrap();
    let m = compose(2, 0x44).unwrap();
    let ctrl = msi::msi_program(&f, AT, &l, m, 2, 0b0100);
    assert_eq!(ctrl & 0xFFFF, 0x7005, "capability id and next pointer kept");
    assert_eq!(ctrl >> 16 & 0x18E, 0x180 | 2 << 1, "64-bit, mask, MMC kept");
    assert_eq!(msi::msi_state(&f, AT, &l), (true, 2));
    assert_eq!(msi::msi_read_message(&f, AT, &l), m);
    assert_eq!(msi::msi_read_mask(&f, AT, &l), Some(0b0100));
    // Enable came last: every write before it saw MSI off.
    let w = f.writes.lock().unwrap().clone();
    let enable_at = w.iter().rposition(|x| x.1 == 0x50 && x.2 & msi::MSI_ENABLE != 0).unwrap();
    assert_eq!(enable_at, w.len() - 1, "enable is the last write: {w:x?}");
    assert!(w[..enable_at].iter().all(|x| x.1 != 0x50 || x.2 & msi::MSI_ENABLE == 0));
    // The data dword's upper half (extended message data) is kept.
    f.fns.lock().unwrap().get_mut(&AT).unwrap().w16(0x5E, 0xABCD);
    msi::msi_write_message(&f, AT, &l, compose(1, 0x50).unwrap());
    assert_eq!(f.read32(AT, 0x5C), 0xABCD_0050);
    assert!(msi::msi_mask_one(&f, AT, &l, 0, true));
    assert_eq!(msi::msi_read_mask(&f, AT, &l), Some(0b0101));
    assert!(msi::msi_mask_one(&f, AT, &l, 2, false));
    assert_eq!(msi::msi_read_mask(&f, AT, &l), Some(0b0001));
    msi::msi_disable(&f, AT, &l);
    assert_eq!(msi::msi_state(&f, AT, &l), (false, 0));
    assert_eq!(f.read32(AT, 0x50) >> 16 & 0x1FE, 0x180 | 2 << 1, "MME back to one, the rest kept");
}

#[test]
fn msi_without_masks() {
    let f = device(0x0000, 1);
    let l = MsiLayout::read(&f, AT, 0x50).unwrap();
    let m = compose(0, 0x30).unwrap();
    msi::msi_program(&f, AT, &l, m, 0, 0);
    assert_eq!(msi::msi_state(&f, AT, &l), (true, 0));
    assert_eq!(f.read32(AT, 0x54), 0xFEE0_0000);
    assert_eq!(f.read32(AT, 0x58) & 0xFFFF, 0x30);
    assert!(!msi::msi_mask_one(&f, AT, &l, 0, true), "no mask bits");
    assert_eq!(msi::msi_read_mask(&f, AT, &l), None);
    // Nothing was written past the 32-bit form's data register (0x5C would be the next capability's space).
    assert!(f.writes.lock().unwrap().iter().all(|w| w.1 <= 0x58));
    assert!(msi::msi_set_enable(&f, AT, &l, false));
    assert!(!msi::msi_set_enable(&f, AT, &l, true));
}

#[test]
fn msix_program_order_and_masks() {
    let f = device(0, 5);
    let l = MsixLayout::read(&f, AT, 0x70).unwrap();
    let t = FakeTable::new(5);
    // Stale entries a previous kernel left unmasked.
    for i in 0..5 {
        t.mem.lock().unwrap()[i * 4] = 0xDEAD_0000;
    }
    let entries = [(0u16, compose(0, 0x30).unwrap(), false), (1, compose(1, 0x31).unwrap(), false), (2, compose(2, 0x32).unwrap(), true)];
    assert!(msi::msix_program(&f, AT, &l, &t, &entries));
    assert_eq!(msi::msix_state(&f, AT, &l), (true, false));
    for (i, m, masked) in entries {
        assert_eq!(msi::msix_read_entry(&t, i), Some((m, masked)), "entry {i}");
    }
    assert!(t.ctrl(3) & 1 != 0 && t.ctrl(4) & 1 != 0, "unused entries stay masked");
    // Every address/data write happened while its entry was masked.
    for (off, _, masked) in t.log.lock().unwrap().iter() {
        if off % 16 != 12 {
            assert!(*masked, "entry dword {off:#x} written unmasked");
        }
    }
    // The function mask was on while the entries changed and is off at the end; the header kept.
    let w = f.writes.lock().unwrap().clone();
    assert!(w.first().is_some_and(|x| x.1 == 0x70 && x.2 & msi::MSIX_ENABLE != 0 && x.2 & msi::MSIX_FUNCTION_MASK != 0));
    assert!(w.last().is_some_and(|x| x.1 == 0x70 && x.2 & msi::MSIX_FUNCTION_MASK == 0));
    assert_eq!(f.read32(AT, 0x70) & 0x07FF_FFFF, 0x0004_0011, "id, next and table size kept");

    // Re-aiming entry 1 keeps it masked while it changes and unmasks it after.
    t.log.lock().unwrap().clear();
    assert!(msi::msix_write_entry(&t, 1, compose(3, 0x40).unwrap(), false));
    assert!(t.log.lock().unwrap().iter().filter(|x| x.0 % 16 != 12).all(|x| x.2));
    assert_eq!(msi::msix_read_entry(&t, 1), Some((compose(3, 0x40).unwrap(), false)));
    assert!(msi::msix_mask_entry(&t, 1, true));
    assert_eq!(msi::msix_read_entry(&t, 1).map(|e| e.1), Some(true));
    assert!(!msi::msix_write_entry(&t, 9, compose(0, 0x30).unwrap(), false), "past the table");

    msi::msix_disable(&f, AT, &l, Some(&t), &[0, 1, 2]);
    assert_eq!(msi::msix_state(&f, AT, &l), (false, true));
    assert!((0..3).all(|i| t.ctrl(i) & 1 != 0));
}

#[test]
fn msix_program_failure_leaves_it_off() {
    let f = device(0, 5);
    let l = MsixLayout::read(&f, AT, 0x70).unwrap();
    // A table mapping shorter than the capability says (2 of 5 entries).
    let t = FakeTable::new(2);
    assert!(!msi::msix_program(&f, AT, &l, &t, &[(0, compose(0, 0x30).unwrap(), false)]));
    assert!(!msi::msix_state(&f, AT, &l).0);
}

#[test]
fn pending_bits() {
    let pba = FakeTable::new(1);
    pba.mem.lock().unwrap()[0] = 0b10;
    pba.mem.lock().unwrap()[1] = 1;
    assert_eq!(msi::msix_pending(&pba, 0), Some(false));
    assert_eq!(msi::msix_pending(&pba, 1), Some(true));
    assert_eq!(msi::msix_pending(&pba, 32), Some(true));
    assert_eq!(msi::msix_pending(&pba, 200), None);
}

#[test]
fn intx_goes_off() {
    let f = device(0, 1);
    assert_eq!(msi::intx_off(&f, AT), 0x6);
    assert_eq!(f.read32(AT, 4) & 0xFFFF, 0x406);
    let n = f.writes.lock().unwrap().len();
    msi::intx_off(&f, AT);
    assert_eq!(f.writes.lock().unwrap().len(), n, "already off: nothing written");
}

#[test]
fn message_round_trip() {
    let m = Message { address: 0xFEE0_5000, data: 0x77 };
    assert_eq!(m.decode(), Some((5, 0x77)));
}

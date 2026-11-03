//! GART builder: any window size, Linux's entry values, refusals before the first write.

use std::vec::Vec;

use crate::gart::{Gart, GartError, GartImage, GartPage, GartSink};
use crate::pte::Target;
use super::whole as cv;

const SYS: GartPage = GartPage { target: Target::System { snooped: true }, writeable: true };

struct Log(Vec<(u64, u64)>);
impl GartSink for Log {
    fn write(&mut self, index: u64, value: u64) {
        self.0.push((index, value));
    }
}

#[test]
fn window_sizes_are_arguments() {
    // The 780M under Linux: 512 MiB at 0x7FFF_0000_0000, 131072 entries, 1 MiB table.
    let g = Gart::new(0x7FFF_0000_0000, 512 << 20, 0x1000, cv()).unwrap();
    assert_eq!((g.entries(), g.table_bytes()), (131_072, 1 << 20));
    let big = Gart::new(0x7000_0000_0000, 8 << 30, 0x1000, cv()).unwrap();
    assert_eq!(big.entries(), 2 << 20);
    assert_eq!(Gart::new(0, 3 * 4096, 0, cv()).unwrap().entries(), 3);
    for (s, n, d) in [(0, 0, 0), (0x800, 4096, 0), (0, 4097, 0), (0, 4096, 0x10), ((1 << 48) - 4096, 8192, 0)] {
        assert_eq!(Gart::new(s, n, d, cv()), Err(GartError::BadWindow), "{s:#x} {n:#x} {d:#x}");
    }
}

#[test]
fn entries_match_linux_values() {
    // VALID|SYSTEM|SNOOPED|EXEC|READ|WRITE + MTYPE UC: amdgpu_ttm_tt_pte_flags with gart_pte_flags.
    assert_eq!(Gart::entry(0x1_2345_6000, SYS).unwrap(), 0x0003_0001_2345_6077);
    let ro_local = GartPage { target: Target::Local, writeable: false };
    assert_eq!(Gart::entry(0x6_6000_0000, ro_local).unwrap(), 0x0003_0006_6000_0031);
    assert_eq!(Gart::entry(0x1001, SYS), Err(GartError::BadPhys(0x1001)));
}

#[test]
fn map_translate_unmap() {
    let g = Gart::new(0x7FFF_0000_0000, 64 << 10, 0xD000, cv()).unwrap();
    let mut t = GartImage::new(&g);
    let mc = g.map(&mut t, 0x3000, &[0x10_0000, 0x5_0000], SYS).unwrap();
    assert_eq!(mc, 0x7FFF_0000_3000);
    assert_eq!(g.translate(&t, mc + 0x10).unwrap(), Some((0x10_0010, Target::System { snooped: true })));
    assert_eq!(g.translate(&t, mc + 0x1FFF).unwrap(), Some((0x5_0FFF, Target::System { snooped: true })));
    assert_eq!(g.translate(&t, mc + 0x2000).unwrap(), None);
    assert_eq!(g.translate(&t, 0x7FFF_0001_0000).unwrap(), None, "past the window");
    g.unmap(&mut t, 0x3000, 2).unwrap();
    assert_eq!(t.0[3], 0xD000, "dummy page, flags 0");
    assert_eq!(g.translate(&t, mc).unwrap(), None);
}

#[test]
fn refusals_write_nothing() {
    let g = Gart::new(0, 16 * 4096, 0, cv()).unwrap();
    let mut log = Log(Vec::new());
    assert_eq!(g.map(&mut log, 15 * 4096, &[0x1000, 0x2000], SYS), Err(GartError::OutOfWindow(15 * 4096)));
    assert_eq!(g.map(&mut log, 0x800, &[0x1000], SYS), Err(GartError::OutOfWindow(0x800)));
    assert_eq!(g.map(&mut log, 0, &[0x1000, 1 << 48], SYS), Err(GartError::BadPhys(1 << 48)));
    assert_eq!(g.unmap(&mut log, 0, u64::MAX), Err(GartError::OutOfWindow(0)));
    assert!(log.0.is_empty());
    g.clear(&mut log);
    assert_eq!(log.0.len(), 16);
}

#[test]
fn context0_base_matches_a_linux_register_trace_of_the_780m() {
    // amdgpu under Linux 7.1 on a 780M with a 512 MiB carve-out: MMMC_VM_FB_OFFSET read 0x840, VRAM at MC
    // 0x80_0000_0000, GART table at MC 0x80_1FD0_0000; both hubs got CONTEXT0_PAGE_TABLE_BASE
    // LO32 0x5FD0_0001, HI32 0x8.
    let c = crate::addr::Carveout::new(0x80_0000_0000, 0x840 << 24, 512 << 20).unwrap();
    let phys = c.mc_to_phys(0x80_1FD0_0000).unwrap();
    let base = crate::pte::pd_addr(crate::store::TableAddr { phys, target: Target::Local }).unwrap();
    assert_eq!((base as u32, (base >> 32) as u32), (0x5FD0_0001, 0x8));
    assert_eq!(c.phys_to_mc(phys), Some(0x80_1FD0_0000));
    assert_eq!(c.mc_to_phys(0x80_2000_0000), None, "one past the carve-out");
    assert!(c.contains_phys(phys, 1 << 20) && !c.contains_phys(phys, 4 << 20));
}

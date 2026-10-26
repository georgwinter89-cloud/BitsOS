//! PC-5: the raw-tool guards over the Mini-PC world (pure; the accesses themselves are ktests).

use super::fake::{Fake, FakeFn};
use super::world::{minipc, GPU, I226, NVME0, WLAN, WLAN_BRIDGE};
use super::{keys_off, ALL};
use crate::node::Window;
use crate::raw::{self, DecodeState, Refusal, MMIO_MAX};
use crate::{build, ConfigAccess, Loc, Node};

fn at(t: (u8, u8, u8)) -> Loc {
    Loc::new(0, t.0, t.1, t.2)
}

fn world() -> (Fake, alloc::vec::Vec<Node>) {
    let w = minipc();
    let n = build(&w, &ALL, keys_off);
    (w, n)
}

fn node(nodes: &[Node], l: Loc) -> (usize, &Node) {
    let i = nodes.iter().position(|n| n.loc == l).unwrap();
    (i, &nodes[i])
}

#[test]
fn function_names() {
    assert_eq!(raw::parse_loc("03:00.0"), Some(Loc::new(0, 3, 0, 0)));
    assert_eq!(raw::parse_loc(" c7:00.6 "), Some(Loc::new(0, 0xC7, 0, 6)));
    assert_eq!(raw::parse_loc("0001:C9:1f.7"), Some(Loc::new(1, 0xC9, 0x1F, 7)));
    for bad in ["", "3", "03:00", "03:20.0", "03:00.8", "100:00.0", "x:00.0", "03:00.0.1", "1:2:3:4.0", "03:-1.0", "03:00.", "00000:00:00.0"] {
        assert_eq!(raw::parse_loc(bad), None, "{bad}");
    }
}

#[test]
fn configuration_reads() {
    let (_, nodes) = world();
    let (_, nic) = node(&nodes, at(I226));
    assert_eq!(raw::cfg_read_check(nic, 4096, 0x1C8, 4, 1), Ok(()));
    assert_eq!(raw::cfg_read_check(nic, 4096, 0, 4, 1024), Ok(()), "the whole space");
    assert_eq!(raw::cfg_read_check(nic, 4096, 0, 4, 1025), Err(Refusal::OutOfRange));
    assert_eq!(raw::cfg_read_check(nic, 4096, 0xFFE, 2, 1), Ok(()));
    assert_eq!(raw::cfg_read_check(nic, 4096, 0xFFE, 4, 1), Err(Refusal::Unaligned));
    assert_eq!(raw::cfg_read_check(nic, 256, 0x100, 4, 1), Err(Refusal::OutOfRange), "0xCF8 reaches 256 bytes");
    assert_eq!(raw::cfg_read_check(nic, 4096, 0, 8, 1), Err(Refusal::BadWidth));
    assert_eq!(raw::cfg_read_check(nic, 4096, 0, 4, 0), Err(Refusal::OutOfRange));
    assert_eq!(raw::cfg_read_check(nic, 4096, u64::MAX - 3, 4, 1), Err(Refusal::OutOfRange));
    assert_eq!(raw::cfg_read_check(nic, 4096, 0, 4, u64::MAX), Err(Refusal::OutOfRange));
    // Mass storage may be read (only writes and mappings are refused).
    let (_, nvme) = node(&nodes, at(NVME0));
    assert_eq!(raw::cfg_read_check(nvme, 4096, 0, 4, 64), Ok(()));
    // Untouched: the header only (PC-4 check list: the WLAN's command register).
    let (_, wl) = node(&nodes, at(WLAN));
    assert_eq!(raw::cfg_read_check(wl, 256, 4, 2, 1), Ok(()));
    assert_eq!(raw::cfg_read_check(wl, 256, 0, 4, 16), Ok(()));
    assert_eq!(raw::cfg_read_check(wl, 256, 0x3C, 4, 2), Err(Refusal::Untouched("wlan")));
    assert_eq!(raw::cfg_read_check(wl, 256, 0x50, 4, 1), Err(Refusal::Untouched("wlan")), "its MSI capability");
    assert_eq!(raw::cfg_read_check(wl, 256, 0x100, 4, 1), Err(Refusal::OutOfRange));
}

#[test]
fn configuration_writes() {
    let (_, nodes) = world();
    let w = |l: Loc, off, width, narrow| {
        let (i, n) = node(&nodes, l);
        raw::cfg_write_check(&nodes, i, n.space, off, width, narrow)
    };
    assert_eq!(w(at(I226), 0x3C, 1, true), Ok(()));
    assert_eq!(w(at(I226), 0x04, 4, false), Ok(()));
    assert_eq!(w(at(I226), 0x3C, 1, false), Err(Refusal::NarrowWrite), "0xCF8 moves dwords");
    assert_eq!(w(at(I226), 0x3D, 2, true), Err(Refusal::Unaligned));
    assert_eq!(w(at(I226), 0x1000, 4, true), Err(Refusal::OutOfRange));
    assert_eq!(w(at(NVME0), 0x04, 2, true), Err(Refusal::Storage));
    assert_eq!(w(at(NVME0), 0x2000, 4, true), Err(Refusal::Storage), "the rule comes before the range");
    assert_eq!(w(Loc::new(0, 0, 1, 1), 0x04, 2, true), Err(Refusal::StoragePath), "root port above NVMe 01:00.0");
    assert_eq!(w(Loc::new(0, 0, 2, 4), 0x18, 4, true), Err(Refusal::StoragePath), "root port above NVMe 05:00.0");
    assert_eq!(w(Loc::new(0, 0, 2, 1), 0x04, 2, true), Ok(()), "root port above the I226");
    assert_eq!(w(at(WLAN), 0x04, 2, true), Err(Refusal::Untouched("wlan")));
    assert_eq!(w(at(WLAN_BRIDGE), 0x04, 2, true), Err(Refusal::Untouched("wlan")));
    assert_eq!(w(Loc::new(0, 0, 0, 0), 0x04, 2, true), Ok(()), "the host bridge is no bridge in the tree");
    assert!(raw::storage_below(&nodes, node(&nodes, Loc::new(0, 0, 1, 1)).0));
    assert!(!raw::storage_below(&nodes, node(&nodes, Loc::new(0, 0, 8, 1)).0), "the GPU's bridge");
}

#[test]
fn addresses_and_interrupts_are_the_pci_cores() {
    let (_, nodes) = world();
    let w = |l: Loc, off, width| {
        let (i, n) = node(&nodes, l);
        raw::cfg_write_check(&nodes, i, n.space, off, width, true)
    };
    let nic = at(I226);
    // Type 0: BAR0-5 and the ROM BAR, at every width that touches them.
    for (off, width) in [(0x10u64, 4u8), (0x1C, 4), (0x24, 4), (0x27, 1), (0x30, 4), (0x32, 2)] {
        assert_eq!(w(nic, off, width), Err(Refusal::Addresses), "{off:#x}/{width}");
    }
    for (off, width) in [(0x0Cu64, 4u8), (0x28, 4), (0x2C, 4), (0x34, 1), (0x3C, 1)] {
        assert_eq!(w(nic, off, width), Ok(()), "{off:#x}/{width}");
    }
    // The I226: PM 0x40 (free), MSI 0x50 (64-bit, per-vector mask: 0x18 bytes), MSI-X 0x70 (0x0C bytes).
    assert_eq!(w(nic, 0x44, 2), Ok(()), "PMCSR");
    for off in [0x50u64, 0x52, 0x54, 0x58, 0x5C, 0x60, 0x64, 0x70, 0x72, 0x78] {
        assert_eq!(w(nic, off, 2), Err(Refusal::Interrupts), "{off:#x}");
    }
    assert_eq!(w(nic, 0x68, 4), Ok(()), "past MSI (0x50 + 0x18)");
    assert_eq!(w(nic, 0x7C, 4), Ok(()), "past MSI-X");
    assert_eq!(w(nic, 0xB0, 2), Ok(()), "Link Control");
    // Type 1 (the I226's root port): BARs, bus numbers, windows, ROM; secondary status and bridge control free.
    let port = Loc::new(0, 0, 2, 1);
    for (off, width) in [(0x10u64, 4u8), (0x18, 4), (0x19, 1), (0x1C, 2), (0x20, 4), (0x24, 4), (0x28, 4), (0x2C, 4), (0x30, 4), (0x38, 4)] {
        assert_eq!(w(port, off, width), Err(Refusal::Addresses), "{off:#x}/{width}");
    }
    assert_eq!(w(port, 0x1E, 2), Ok(()), "secondary status");
    assert_eq!(w(port, 0x3E, 2), Ok(()), "bridge control");
    // The Radeon: its BARs cannot be moved onto another function's (R8c through config space).
    assert_eq!(w(at(GPU), 0x24, 4), Err(Refusal::Addresses), "BAR5");
    assert!(Refusal::Addresses.is_rule() && Refusal::Interrupts.is_rule());
}

#[test]
fn sriov_and_resizable_bar_up_to_the_next_capability() {
    let (_, mut nodes) = world();
    let (i, _) = node(&nodes, at(I226));
    let info = nodes[i].ext_caps[0].info;
    let mk = |offset, id| crate::caps::ExtCap { offset, id, version: 1, info };
    let (sriov, rebar) = (mk(0x200, crate::caps::ext::SRIOV), mk(0x260, crate::caps::ext::REBAR));
    nodes[i].ext_caps.extend([sriov, rebar, mk(0x280, crate::caps::ext::LTR)]);
    let n = &nodes[i];
    let w = |off| raw::cfg_write_check(&nodes, i, n.space, off, 4, true);
    assert_eq!(w(0x208), Err(Refusal::Addresses), "VF enable");
    assert_eq!(w(0x23C), Err(Refusal::Addresses), "VF BAR5");
    assert_eq!(w(0x240), Ok(()), "past SR-IOV");
    assert_eq!(w(0x268), Err(Refusal::Addresses), "Resizable BAR control");
    assert_eq!(w(0x27C), Err(Refusal::Addresses));
    assert_eq!(w(0x284), Ok(()), "LTR right after a short Resizable BAR capability");
}

#[test]
fn bus_master_goes_off_but_never_on() {
    let m = 4u32;
    assert_eq!(raw::cmd_write_check(0x0010_0002, 4, 0x0006), Err(Refusal::BusMaster), "word write");
    assert_eq!(raw::cmd_write_check(0x0010_0002, 4, 0x0010_0006), Err(Refusal::BusMaster), "dword write");
    assert_eq!(raw::cmd_write_check(0x0010_0002, 4, m), Err(Refusal::BusMaster), "byte write");
    assert_eq!(raw::cmd_write_check(0x0010_0006, 4, 0x0006), Ok(()), "already on");
    assert_eq!(raw::cmd_write_check(0x0010_0006, 4, 0x0002), Ok(()), "off");
    assert_eq!(raw::cmd_write_check(0x0010_0002, 4, 0x0000), Ok(()), "decoding off");
    assert_eq!(raw::cmd_write_check(0x0010_0002, 0x3C, 0xFF), Ok(()), "not the command register");
    assert_eq!(raw::cmd_write_check(0x0010_0002, 6, 0xFFFF), Ok(()), "the status half");
    assert!(Refusal::BusMaster.is_rule());
}

#[test]
fn memory_is_reached_only_where_it_is_decoded() {
    let (_, mut nodes) = world();
    let (i, _) = node(&nodes, at(I226));
    let port = node(&nodes, Loc::new(0, 0, 2, 1)).0;
    assert_eq!(crate::node::ancestors(&nodes, i), [port]);
    let win = Window { start: 0xDCA0_0000, end: 0xDCBF_FFFF };
    nodes[port].bridge.as_mut().unwrap().mem = Some(win);
    let s = raw::mmio_check(&nodes[i], 0, 0x8, 4, 1, true).unwrap();
    let st = |command, d_state, bridge_cmd| DecodeState { command, d_state, bridges: alloc::vec![(port, bridge_cmd)] };
    assert_eq!(raw::decode_check(&nodes, &s, &st(0x6, Some(0), 0x7)), Ok(()));
    assert_eq!(raw::decode_check(&nodes, &s, &st(0x6, None, 0x6)), Ok(()), "no PM capability");
    assert_eq!(raw::decode_check(&nodes, &s, &st(0x4, Some(0), 0x7)), Err(Refusal::DecodeOff), "function not decoding");
    assert_eq!(raw::decode_check(&nodes, &s, &st(0x6, Some(3), 0x7)), Err(Refusal::NotD0));
    assert_eq!(raw::decode_check(&nodes, &s, &st(0x6, Some(0), 0x5)), Err(Refusal::DecodeOff), "port not forwarding memory");
    assert_eq!(raw::decode_check(&nodes, &s, &st(0xFFFF, Some(0), 0x7)), Err(Refusal::NotFound), "gone");
    assert_eq!(raw::decode_check(&nodes, &s, &st(0x6, Some(0), 0xFFFF)), Err(Refusal::DecodeOff), "port gone");
    // Outside every window of the port: the address would go elsewhere.
    nodes[port].bridge.as_mut().unwrap().mem = Some(Window { start: 0xDCB0_0000, end: 0xDCBF_FFFF });
    assert_eq!(raw::decode_check(&nodes, &s, &st(0x6, Some(0), 0x7)), Err(Refusal::DecodeOff));
    nodes[port].bridge.as_mut().unwrap().prefetch = Some(win);
    assert_eq!(raw::decode_check(&nodes, &s, &st(0x6, Some(0), 0x7)), Ok(()), "the prefetchable window counts");
    nodes[port].bridge.as_mut().unwrap().prefetch = None;
    nodes[port].class = 0x06_04_01;
    assert_eq!(raw::decode_check(&nodes, &s, &st(0x6, Some(0), 0x7)), Ok(()), "subtractive decode forwards everything");
    // On bus 0 there is no bridge to ask.
    let (h, _) = node(&nodes, at(GPU));
    let gs = raw::mmio_check(&nodes[h], 5, 0, 4, 1, false).unwrap();
    assert_eq!(raw::decode_check(&nodes, &gs, &DecodeState { command: 0x7, d_state: None, bridges: alloc::vec![] }), Ok(()));
    assert!(Refusal::DecodeOff.is_rule() && Refusal::NotD0.is_rule());
}

#[test]
fn memory_bars() {
    let (_, nodes) = world();
    let (_, nic) = node(&nodes, at(I226));
    // BAR0 1 MiB at 0xDCA00000, BAR3 16 KiB with the MSI-X table (5 entries at 0) and the PBA at 0x2000.
    let s = raw::mmio_check(nic, 0, 0x8, 4, 1, true).unwrap();
    assert_eq!((s.bar_phys, s.bar_size, s.phys(), s.len), (0xDCA0_0000, 0x10_0000, 0xDCA0_0008, 4));
    assert_eq!(raw::mmio_check(nic, 0, 0xF_FFF8, 8, 1, false).map(|s| s.len), Ok(8));
    assert_eq!(raw::mmio_check(nic, 0, 0xF_FFFC, 8, 1, false), Err(Refusal::Unaligned));
    assert_eq!(raw::mmio_check(nic, 0, 0x10_0000, 4, 1, false), Err(Refusal::OutOfRange));
    assert_eq!(raw::mmio_check(nic, 0, 0, 4, MMIO_MAX / 4, false).map(|s| s.len), Ok(MMIO_MAX));
    assert_eq!(raw::mmio_check(nic, 0, 0, 4, MMIO_MAX / 4 + 1, false), Err(Refusal::OutOfRange));
    assert_eq!(raw::mmio_check(nic, 0, 0, 16, 1, false), Err(Refusal::BadWidth));
    assert_eq!(raw::mmio_check(nic, 0, u64::MAX - 7, 8, 1, false), Err(Refusal::OutOfRange));
    assert_eq!(raw::mmio_check(nic, 1, 0, 4, 1, false), Err(Refusal::NoBar));
    assert_eq!(raw::mmio_check(nic, 6, 0, 4, 1, false), Err(Refusal::NoBar));
    assert_eq!(raw::mmio_check(nic, 3, 0x10, 4, 1, true), Err(Refusal::MsixTable));
    assert_eq!(raw::mmio_check(nic, 3, 0x4C, 4, 1, true), Err(Refusal::MsixTable), "last word of entry 4");
    assert!(raw::mmio_check(nic, 3, 0x50, 4, 1, true).is_ok(), "past the table");
    assert_eq!(raw::mmio_check(nic, 3, 0x2000, 8, 1, true), Err(Refusal::MsixTable), "the PBA");
    assert!(raw::mmio_check(nic, 3, 0x10, 4, 1, false).is_ok(), "the table may be read");
    let (_, nvme) = node(&nodes, at(NVME0));
    assert_eq!(raw::mmio_check(nvme, 0, 0, 4, 1, false), Err(Refusal::Storage), "not even a read");
    let (_, wl) = node(&nodes, at(WLAN));
    assert_eq!(raw::mmio_check(wl, 2, 0, 4, 1, false), Err(Refusal::Untouched("wlan")));
    let (_, hb) = node(&nodes, Loc::new(0, 0, 0, 0));
    assert_eq!(raw::mmio_check(hb, 0, 0, 4, 1, false), Err(Refusal::NoBar));
}

#[test]
fn the_radeon() {
    let (_, nodes) = world();
    let (_, gpu) = node(&nodes, at(GPU));
    assert!(raw::is_amd_display(gpu));
    // BAR5: DCN registers below 0x80000 (OTG0 master update lock 0x14128, SMU mailbox 0x58A0C), no switch (R8c).
    for off in [0x14128u64, 0x58A0C, 0x0, 0x7FFFC] {
        assert!(raw::mmio_check(gpu, 5, off, 4, 1, false).is_ok(), "read: {off:#x}");
        assert!(raw::mmio_check(gpu, 5, off, 4, 1, true).is_ok(), "write: {off:#x}");
    }
    // Graphics memory and doorbells too.
    assert!(raw::mmio_check(gpu, 0, 0x1000, 8, 1, true).is_ok());
    assert!(raw::mmio_check(gpu, 2, 0, 4, 1, true).is_ok());
    assert!(raw::mmio_check(gpu, 0, 0x1000, 8, 512, false).is_ok());
    // Index/data pairs: never.
    for (off, w) in [(0x38u64, 4u8), (0x3C, 4), (0x3E, 2), (0x48, 4), (0x50, 4), (0x54, 4), (0x77, 1)] {
        assert_eq!(raw::mmio_check(gpu, 5, off, w, 1, false), Err(Refusal::IndexPair), "{off:#x}");
        assert_eq!(raw::mmio_check(gpu, 5, off, w, 1, true), Err(Refusal::IndexPair), "{off:#x}");
    }
    assert_eq!(raw::mmio_check(gpu, 5, 0x30, 4, 3, false), Err(Refusal::IndexPair), "a run into the pair");
    assert!(raw::mmio_check(gpu, 5, 0x40, 4, 2, false).is_ok(), "0x40..0x48 lies between");
    assert!(raw::mmio_check(gpu, 5, 0x78, 4, 1, false).is_ok(), "right after the RSMU alias");
    assert_eq!(raw::mmio_check(gpu, 4, 0, 1, 1, false), Err(Refusal::IoBar));
    // Another vendor's display controller (QEMU's VGA) is not the Radeon: its pair offsets are ordinary registers.
    let mut vga = gpu.clone();
    vga.vendor = 0x1234;
    assert!(raw::mmio_check(&vga, 5, 0x38, 4, 1, true).is_ok());
}

#[test]
fn rule_refusals_are_rules() {
    for r in [Refusal::Untouched("wlan"), Refusal::Storage, Refusal::StoragePath, Refusal::IndexPair, Refusal::MsixTable, Refusal::NarrowWrite] {
        assert!(r.is_rule(), "{r:?}");
    }
    for r in [Refusal::NotFound, Refusal::BadWidth, Refusal::Unaligned, Refusal::OutOfRange, Refusal::NoBar, Refusal::IoBar, Refusal::NotSized] {
        assert!(!r.is_rule(), "{r:?}");
    }
}

#[test]
fn values_out_of_a_dword_and_narrow_writes() {
    assert_eq!(raw::extract(0x1234_5678, 0, 4), 0x1234_5678);
    assert_eq!(raw::extract(0x1234_5678, 2, 2), 0x1234);
    assert_eq!(raw::extract(0x1234_5678, 1, 1), 0x56);
    assert_eq!(raw::extract(0x1234_5678, 7, 1), 0x12);
    // The fake's ECAM writes one byte without touching the status half next to it.
    let w = Fake::new();
    let mut f = FakeFn::new(0x1234, 0x5678, 0x02_00_00, 0);
    f.w32(0x04, 0xF900_0006).w32(0x3C, 0x0000_010B);
    let l = Loc::new(0, 1, 0, 0);
    w.add_at(l, f);
    assert!(w.narrow_writes() && w.write_narrow(l, 0x3C, 1, 0x5A));
    assert_eq!(w.read32(l, 0x3C), 0x0000_015A);
    assert!(w.write_narrow(l, 0x04, 2, 0x0007));
    assert_eq!(w.read32(l, 0x04), 0xF900_0007, "status kept");
    assert!(!Fake::legacy().narrow_writes());
}

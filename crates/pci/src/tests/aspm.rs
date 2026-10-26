//! PC-5 over the fake space: ASPM L1.2 off on the Mini-PC's I226 link (device first, then the port, only the
//! L1.2 bits, nothing else written), the fallback without ECAM, refusals, and the PCIe control words the
//! saved copy now keeps (Slot, Root, Device 2, Link 2).

use super::fake::{root_port, Fake, FakeFn};
use super::world::{minipc, I226, NVME0, WLAN, WLAN_BRIDGE};
use super::{keys_off, no_keys, ALL};
use crate::aspm::{self, AspmError, Method, L1_2};
use crate::caps::ext;
use crate::enable::{self, Change, PcieRegs};
use crate::{build, ConfigAccess, Loc};

const PORT: Loc = Loc::new(0, 0, 2, 1);
const NIC: Loc = Loc::new(0, I226.0, I226.1, I226.2);
/// The world's I226: L1SS at 0x1C0 (Control 1 = 0x5: ASPM L1.2 + PCI-PM L1.2), PCIe at 0xA0.
const NIC_CTL1: u16 = 0x1C8;
const PORT_CTL1: u16 = 0x108;

/// The Mini-PC world with an L1SS capability (all four enables on) on the I226's root port 00:02.1, and ASPM
/// L0s+L1 plus common clock on in both Link Controls.
fn world() -> Fake {
    let w = minipc();
    {
        let mut fns = w.fns.lock().unwrap();
        fns.get_mut(&PORT).unwrap().ext(0x100, ext::L1SS, 1, 0).w32(PORT_CTL1, 0xF).w16(0x58 + 0x10, 0x43);
        fns.get_mut(&NIC).unwrap().w16(0xA0 + 0x10, 0x43);
    }
    w
}

fn idx(nodes: &[crate::Node], at: Loc) -> usize {
    nodes.iter().position(|n| n.loc == at).unwrap()
}

#[test]
fn l1_2_off_on_both_ends_device_first_and_nothing_else() {
    let w = world();
    let nodes = build(&w, &ALL, keys_off);
    w.writes.lock().unwrap().clear();
    let r = aspm::l12_off(&w, &nodes, idx(&nodes, NIC), false).unwrap();
    assert_eq!((r.method, r.device, r.port, r.verified), (Method::L1ss, NIC, PORT, true));
    assert_eq!(r.ends.iter().map(|e| (e.at, e.offset, e.before, e.after)).collect::<alloc::vec::Vec<_>>(), [
        (NIC, NIC_CTL1, 0x5, 0x0),
        (PORT, PORT_CTL1, 0xF, 0xF & !L1_2),
    ]);
    // ASPM L1 off on the link (device, then port), the L1SS enables (device first: PCIe 6.0 §5.5.4), ASPM L1
    // back as it was (port, then device); nothing else written.
    assert_eq!(*w.writes.lock().unwrap(), [
        (NIC, 0xB0, 0x41),
        (PORT, 0x68, 0x41),
        (NIC, NIC_CTL1, 0x0),
        (PORT, PORT_CTL1, 0xA),
        (PORT, 0x68, 0x43),
        (NIC, 0xB0, 0x43),
    ]);
    assert_eq!(r.held.iter().map(|e| (e.at, e.offset, e.before, e.after)).collect::<alloc::vec::Vec<_>>(), [(NIC, 0xB0, 0x43, 0x41), (PORT, 0x68, 0x43, 0x41)]);
    // L1.1 and ASPM L1 stay.
    assert_eq!(w.read32(PORT, PORT_CTL1) & 0xA, 0xA);
    assert_eq!(crate::access::read16(&w, NIC, 0xB0), 0x43);
    w.writes.lock().unwrap().clear();
    let again = aspm::l12_off(&w, &nodes, idx(&nodes, NIC), false).unwrap();
    assert!(!again.changed() && again.verified);
    assert!(w.writes.lock().unwrap().is_empty(), "idempotent: {:?}", w.writes.lock().unwrap());
}

#[test]
fn with_aspm_l1_off_already_only_the_l1ss_enables_are_written() {
    let w = world();
    {
        let mut fns = w.fns.lock().unwrap();
        fns.get_mut(&NIC).unwrap().w16(0xB0, 0x41);
        fns.get_mut(&PORT).unwrap().w16(0x68, 0x40);
    }
    let nodes = build(&w, &ALL, keys_off);
    w.writes.lock().unwrap().clear();
    let r = aspm::l12_off(&w, &nodes, idx(&nodes, NIC), false).unwrap();
    assert!(r.held.is_empty() && r.verified);
    assert_eq!(*w.writes.lock().unwrap(), [(NIC, NIC_CTL1, 0x0), (PORT, PORT_CTL1, 0xA)]);
}

#[test]
fn only_the_port_has_l1_2_on() {
    // The device's enables are clear already, the port's not: the L1 hold still covers the change.
    let w = world();
    w.fns.lock().unwrap().get_mut(&NIC).unwrap().w32(NIC_CTL1, 0);
    let nodes = build(&w, &ALL, keys_off);
    w.writes.lock().unwrap().clear();
    let r = aspm::l12_off(&w, &nodes, idx(&nodes, NIC), false).unwrap();
    assert_eq!(r.held.len(), 2);
    assert_eq!(*w.writes.lock().unwrap(), [(NIC, 0xB0, 0x41), (PORT, 0x68, 0x41), (PORT, PORT_CTL1, 0xA), (PORT, 0x68, 0x43), (NIC, 0xB0, 0x43)]);
}

#[test]
fn a_device_gone_before_the_hold_leaves_the_port_alone() {
    // The hold starts at the device: it fails there, and the port's ASPM L1 is never touched.
    let w = world();
    let nodes = build(&w, &ALL, keys_off);
    w.fns.lock().unwrap().remove(&NIC);
    w.writes.lock().unwrap().clear();
    assert_eq!(aspm::l12_off(&w, &nodes, idx(&nodes, NIC), false), Err(AspmError::Gone(NIC)));
    assert!(w.writes.lock().unwrap().is_empty());
    assert_eq!(crate::access::read16(&w, PORT, 0x68), 0x43);
}

#[test]
fn a_port_without_l1ss_means_the_link_never_enters_l1_2() {
    let w = minipc();
    let nodes = build(&w, &ALL, keys_off);
    w.writes.lock().unwrap().clear();
    let r = aspm::l12_off(&w, &nodes, idx(&nodes, NIC), false).unwrap();
    assert_eq!((r.method, r.ends.len(), r.verified), (Method::NotSupported, 0, true));
    assert!(w.writes.lock().unwrap().is_empty());
}

#[test]
fn without_ecam_aspm_l1_goes_off_on_the_link() {
    let w = Fake::legacy();
    let port = Loc::new(0, 0, 1, 0);
    let mut rp = root_port(0, 1, 1);
    rp.w16(0x58 + 0x10, 0x43);
    w.add_at(port, rp);
    let mut nic = FakeFn::new(0x8086, 0x125C, 0x02_00_00, 0);
    nic.pcie(0xA0, 0, 0).w16(0xB0, 0x43).w16(0xB2, 0x1011);
    w.add(1, 0, 0, nic);
    let nodes = build(&w, &[crate::enumerate::Root { segment: 0, bus_start: 0, bus_end: 1 }], no_keys);
    let dev = Loc::new(0, 1, 0, 0);
    w.writes.lock().unwrap().clear();
    let r = aspm::l12_off(&w, &nodes, idx(&nodes, dev), false).unwrap();
    assert_eq!((r.method, r.verified), (Method::AspmL1Off, true));
    // Device first, then the port; L0s (bit 0) and common clock (bit 6) stay; Link Status written 0.
    assert_eq!(*w.writes.lock().unwrap(), [(dev, 0xB0, 0x41), (port, 0x68, 0x41)]);
    assert_eq!(r.ends.iter().map(|e| (e.before, e.after)).collect::<alloc::vec::Vec<_>>(), [(0x43, 0x41), (0x43, 0x41)]);
}

#[test]
fn l1ss_of_a_multi_function_device_is_function_0s() {
    let w = world();
    // A second function on the I226 (multi-function header): L1SS lives in function 0.
    {
        let mut fns = w.fns.lock().unwrap();
        fns.get_mut(&NIC).unwrap().w8(0x0E, 0x80);
    }
    let mut f1 = FakeFn::new(0x8086, 0x125C, 0x02_00_00, 0);
    f1.pcie(0xA0, 0, 0).ext(0x100, ext::L1SS, 1, 0).w32(0x108, 0x5);
    w.add(I226.0, 0, 1, f1);
    let nodes = build(&w, &ALL, keys_off);
    let fn1 = Loc::new(0, I226.0, 0, 1);
    let r = aspm::l12_off(&w, &nodes, idx(&nodes, fn1), false).unwrap();
    assert_eq!((r.device, r.ends[0].at), (NIC, NIC));
    assert_eq!(w.read32(fn1, 0x108), 0x5, "function 1's copy is not the link's");
    assert_eq!(w.read32(NIC, NIC_CTL1) & L1_2, 0);
}

#[test]
fn refusals_before_any_write() {
    let w = world();
    let nodes = build(&w, &ALL, keys_off);
    w.writes.lock().unwrap().clear();
    let at = |t: (u8, u8, u8)| idx(&nodes, Loc::new(0, t.0, t.1, t.2));
    assert_eq!(aspm::l12_off(&w, &nodes, at(WLAN), false), Err(AspmError::Untouched("wlan")));
    assert_eq!(aspm::l12_off(&w, &nodes, at(WLAN_BRIDGE), false), Err(AspmError::Untouched("wlan")));
    assert_eq!(aspm::l12_off(&w, &nodes, at(NVME0), false), Err(AspmError::Storage));
    assert_eq!(aspm::l12_off(&w, &nodes, idx(&nodes, PORT), false), Err(AspmError::NoLink), "a root port on bus 0");
    assert_eq!(aspm::l12_off(&w, &nodes, 9999, false), Err(AspmError::NotScanned));
    assert!(w.writes.lock().unwrap().is_empty());
    // The NVMe driver may: its root port has no L1SS in this world.
    assert_eq!(aspm::l12_off(&w, &nodes, at(NVME0), true).map(|r| r.method), Ok(Method::NotSupported));
    // A device behind an untouched port is refused too.
    let mut un = nodes.clone();
    let p = idx(&un, PORT);
    un[p].untouched = Some("test");
    assert_eq!(aspm::l12_off(&w, &un, idx(&un, NIC), false), Err(AspmError::Untouched("test")));
}

#[test]
fn a_device_gone_reads_all_ones() {
    let w = world();
    let nodes = build(&w, &ALL, keys_off);
    w.fns.lock().unwrap().remove(&NIC);
    assert_eq!(aspm::l12_off(&w, &nodes, idx(&nodes, NIC), false), Err(AspmError::Gone(NIC)));
}

#[test]
fn the_i226_family_is_on_the_list() {
    // Linux igc_hw.h: I226_LM 0x125B, I226_V 0x125C, I226_IT 0x125D, I226_BLANK_NVM 0x125F, I226_LMVP 0x5503,
    // I226_K 0x3102 (checked against the alias list of the installed igc.ko).
    for d in [0x125B, 0x125C, 0x125D, 0x125F, 0x5503, 0x3102] {
        assert!(aspm::needs_l12_off(0x8086, d), "{d:#x}");
    }
    assert_eq!(aspm::L12_OFF.len(), 6);
    assert!(!aspm::needs_l12_off(0x8086, 0x5504), "no igc ID");
    assert!(!aspm::needs_l12_off(0x8086, 0x125E), "I221-V");
    assert!(!aspm::needs_l12_off(0x8086, 0x3100), "I225-K");
    assert!(!aspm::needs_l12_off(0x8086, 0x10C9), "igb (QEMU)");
    assert!(!aspm::needs_l12_off(0x8086, 0x15F3), "I225");
    assert!(!aspm::needs_l12_off(0x10EC, 0x125C));
}

#[test]
fn which_pcie_registers_exist() {
    let r = |flags| PcieRegs::from_flags(flags);
    assert_eq!(r(0x0002), PcieRegs { link: true, slot: false, root: false, v2: true }, "endpoint v2");
    assert_eq!(r(0x0001), PcieRegs { link: true, slot: false, root: false, v2: false }, "endpoint v1: no 2-registers");
    assert_eq!(r(0x0042), PcieRegs { link: true, slot: false, root: true, v2: true }, "root port without a slot");
    assert_eq!(r(0x0142), PcieRegs { link: true, slot: true, root: true, v2: true }, "root port with a slot");
    assert_eq!(r(0x0162), PcieRegs { link: true, slot: true, root: false, v2: true }, "downstream port with a slot");
    assert_eq!(r(0x0152), PcieRegs { link: true, slot: false, root: false, v2: true }, "upstream port: never a slot");
    assert_eq!(r(0x0172), PcieRegs { link: true, slot: false, root: false, v2: true }, "PCIe-to-PCI bridge: no PCIe slot");
    assert_eq!(r(0x0182), PcieRegs { link: true, slot: true, root: false, v2: true }, "PCI-to-PCIe bridge with a slot");
    assert_eq!(r(0x0092), PcieRegs { link: false, slot: false, root: false, v2: true }, "integrated endpoint: no link");
    assert_eq!(r(0x00A2), PcieRegs { link: false, slot: false, root: true, v2: true }, "event collector: root control");
}

#[test]
fn slot_root_and_2_registers_are_saved_and_restored() {
    let w = Fake::new();
    // A root port with a slot (flags 0x0142), Device Control 2 = 0x0405 (LTR on, timeout range 5), Link Control
    // 2 = 0x0003 (target 8 GT/s), Slot Control = 0x01F1, Root Control = 0x0008.
    let mut rp = root_port(0, 1, 1);
    rp.w16(0x58 + 2, 0x0142).w16(0x58 + 0x28, 0x0405).w16(0x58 + 0x30, 0x0003).w16(0x58 + 0x18, 0x01F1).w16(0x58 + 0x1C, 0x0008);
    w.add_at(PORT, rp);
    let saved = enable::save(&w, PORT, 0x58);
    let p = saved.pcie.unwrap();
    assert_eq!((p.slot_ctl, p.root_ctl, p.dev_ctl2, p.link_ctl2), (Some(0x01F1), Some(0x0008), Some(0x0405), Some(0x0003)));
    // A reset put them back to 0 (status halves set: they must be written 0, never 1).
    {
        let mut fns = w.fns.lock().unwrap();
        let f = fns.get_mut(&PORT).unwrap();
        for o in [0x28u16, 0x30, 0x18, 0x1C] {
            f.w32(0x58 + o, 0xFFFF_0000);
        }
    }
    w.writes.lock().unwrap().clear();
    let ch = enable::restore(&w, &saved, false, false).unwrap();
    assert_eq!(ch, [
        Change::Restored { at: PORT, offset: 0x58 + 0x18, was: 0, now: 0x01F1 },
        Change::Restored { at: PORT, offset: 0x58 + 0x1C, was: 0, now: 0x0008 },
        Change::Restored { at: PORT, offset: 0x58 + 0x28, was: 0, now: 0x0405 },
        Change::Restored { at: PORT, offset: 0x58 + 0x30, was: 0, now: 0x0003 },
    ], "Linux's order: slot, root, device 2, link 2");
    assert!(w.writes.lock().unwrap().iter().all(|x| x.2 >> 16 == 0), "a status half written as 1: {:?}", w.writes.lock().unwrap());
    assert!(enable::restore(&w, &saved, false, false).unwrap().is_empty(), "second restore writes nothing");
}

#[test]
fn registers_a_function_lacks_are_never_touched() {
    // A version-1 endpoint: no Device/Link Control 2 (Linux: not implemented), no slot, no root control.
    let w = Fake::new();
    let mut f = FakeFn::new(0x1234, 0x5678, 0x02_00_00, 0);
    f.cap(0x40, 0x10, 0).w16(0x42, 0x0001).w32(0x40 + 0x28, 0xDEAD).w32(0x40 + 0x30, 0xBEEF);
    let at = Loc::new(0, 1, 0, 0);
    w.add_at(at, f);
    let saved = enable::save(&w, at, 0x40);
    let p = saved.pcie.unwrap();
    assert_eq!((p.link_ctl, p.slot_ctl, p.root_ctl, p.dev_ctl2, p.link_ctl2), (Some(0), None, None, None, None));
    w.fns.lock().unwrap().get_mut(&at).unwrap().w32(0x40 + 0x28, 0x1111);
    w.writes.lock().unwrap().clear();
    assert!(enable::restore(&w, &saved, false, false).unwrap().is_empty());
    assert!(w.writes.lock().unwrap().is_empty());
}

//! PC-1 ktests. All but `pc.ecam.q35_window` are `Safe`: they read configuration space and compare with
//! what the scan recorded; the only write attempts go to untouched functions and are refused before they
//! reach the device. Tests whose hardware is missing say `skip` (the topology ones need the extra QEMU
//! devices of `host/tests/pc/10-topology.sh`; on the Mini-PC they skip).
//!
//! Two test drivers are registered with QEMU-only IDs (ICH9 SMBus and AHCI), so on real hardware they
//! match nothing.

use crate::access::{ConfigAccess, PortCf8};
use crate::caps::{ext, id, CapInfo};
use crate::enumerate::{enumerate, Root};
use crate::node::{ancestors, Bar, Node};
use crate::{DriverEntry, Match, PciFunction};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("pc.scan.tree", Safe, scan_tree);
ktest!("pc.scan.ecam", Safe, scan_ecam);
ktest!("pc.ecam.q35_window", QemuOnly, q35_window);
ktest!("pc.bars.restored", Safe, bars_restored);
ktest!("pc.untouched.key_off", Safe, untouched_key_off);
ktest!("pc.untouched.wlan", Safe, untouched_wlan);
ktest!("pc.storage.nvme_only", Safe, storage_nvme_only);
ktest!("pc.guard.storage_bar", Safe, guard_storage_bar);
ktest!("pc.topo.multifunction", Safe, topo_multifunction);
ktest!("pc.topo.root_port", Safe, topo_root_port);
ktest!("pc.topo.bridge_chain", Safe, topo_bridge_chain);
ktest!("pc.cf8.writer", Safe, cf8_writer_owned);

const ICH9_SMBUS: (u16, u16) = (0x8086, 0x2930);
const ICH9_AHCI: (u16, u16) = (0x8086, 0x2922);
const Q35_MCH: (u16, u16) = (0x8086, 0x29C0);
const IGB: (u16, u16) = (0x8086, 0x10C9);
const PCI_TESTDEV: (u16, u16) = (0x1B36, 0x0005);

static UNTOUCHED_PROBES: AtomicU32 = AtomicU32::new(0);
static STORAGE_PROBES: AtomicU32 = AtomicU32::new(0);

fn untouched_probe(_: &PciFunction) -> Result<(), String> {
    UNTOUCHED_PROBES.fetch_add(1, Ordering::SeqCst);
    Ok(())
}
fn storage_probe(_: &PciFunction) -> Result<(), String> {
    STORAGE_PROBES.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

crate::pci_driver!(DriverEntry {
    name: "ktest-untouched",
    matches: &[Match::Id { vendor: ICH9_SMBUS.0, device: ICH9_SMBUS.1 }],
    opt_in_key: Some("ktest_untouched"),
    probe: untouched_probe,
});
crate::pci_driver!(DriverEntry {
    name: "ktest-storage",
    matches: &[Match::Id { vendor: ICH9_AHCI.0, device: ICH9_AHCI.1 }],
    opt_in_key: None,
    probe: storage_probe,
});

fn find(nodes: &[Node], ids: (u16, u16)) -> Option<usize> {
    nodes.iter().position(|n| (n.vendor, n.device) == ids)
}

/// The tree is consistent: no absent vendor, nothing twice, every function inside its bridge's bus range,
/// a host bridge at 00:00.0, and the text for hw_info.
fn scan_tree() -> R {
    let nodes = crate::tree();
    kassert!(!nodes.is_empty(), "scan found nothing");
    let mut locs: Vec<_> = nodes.iter().map(|n| n.loc).collect();
    locs.sort();
    locs.dedup();
    kassert!(locs.len() == nodes.len(), "a function listed twice");
    for (i, n) in nodes.iter().enumerate() {
        kassert!(n.vendor != 0 && n.vendor != 0xFFFF, "{} has vendor {:#x}", n.loc, n.vendor);
        if let Some(p) = n.parent {
            kassert!(p < i, "{} listed before its bridge", n.loc);
            let b = nodes[p].bridge.ok_or("parent is not a bridge")?;
            kassert!((b.secondary..=b.subordinate).contains(&n.loc.bus), "{} outside {}'s bus range", n.loc, nodes[p].loc);
        }
    }
    let hb = nodes.iter().find(|n| n.loc == crate::Loc::new(0, 0, 0, 0)).ok_or("no 00:00.0")?;
    kassert!(hb.class >> 8 == 0x0600, "00:00.0 is class {:#x}, not a host bridge", hb.class);
    kassert!(crate::devices().len() == nodes.len());
    let lines = crate::tree_lines();
    kassert!(lines.len() > nodes.len(), "tree text too short");
    kassert!(lines[0].starts_with(&format!("pci: {} functions", nodes.len())), "{}", lines[0]);
    // The hw_info text in the log, for the desk's comparison with HARDWARE-MINIPC.md.
    for l in &lines {
        klog::event!(Info, "pci", "tree.line", text = l.as_str());
    }
    Ok(())
}

/// ECAM is in use whenever the firmware has an MCFG, and every PCIe function then reaches 4 KiB.
fn scan_ecam() -> R {
    let Some(t) = acpi::tables() else {
        return ktest::skip("acpi::init was not called (desk change request CP-2/1): PCI runs on the 0xCF8 fallback");
    };
    if t.mcfg.is_empty() {
        kassert!(crate::method() == "cf8");
        return ktest::skip("no MCFG on this machine");
    }
    kassert!(crate::method() == "ecam", "MCFG present but method {}", crate::method());
    for n in crate::tree() {
        // A conventional function (q35's MCH, the ICH9 functions) has 256 bytes even on ECAM (Linux rule).
        let want = if n.pcie.is_some() { 0x1000 } else { 0x100 };
        kassert!(n.space == want, "{} reaches {} bytes, expected {}", n.loc, n.space, want);
        if n.pcie.is_some() {
            kassert!(n.function().read32_ext(0x100).is_some(), "{}: extended space unreadable", n.loc);
        }
    }
    Ok(())
}

/// q35's own ECAM window (PCIEXBAR in the MCH, 00:00.0 offset 0x60) walked next to 0xCF8: the same tree,
/// the same first 64 bytes of every function. Proves the ECAM path in the gate even before the kernel
/// reads ACPI. Reads only; maps the window with `hal::mmio`.
fn q35_window() -> R {
    let cf8 = PortCf8;
    let hb = crate::Loc::new(0, 0, 0, 0);
    let id = cf8.read32(hb, 0);
    if (id as u16, (id >> 16) as u16) != Q35_MCH {
        return ktest::skip("not a q35 machine");
    }
    let lo = cf8.read32(hb, 0x60);
    let hi = cf8.read32(hb, 0x64);
    if lo & 1 == 0 {
        return ktest::skip("PCIEXBAR disabled");
    }
    let (buses, mask): (u16, u64) = match (lo >> 1) & 3 {
        0 => (256, 0xF_F000_0000),
        1 => (128, 0xF_F800_0000),
        2 => (64, 0xF_FC00_0000),
        _ => return Err("PCIEXBAR length field reserved".into()),
    };
    let base = ((hi as u64) << 32 | lo as u64) & mask;
    let last = (buses - 1) as u8;
    let space = crate::ecam::EcamSpace::new(&[acpi::mcfg::Ecam { base, segment: 0, bus_start: 0, bus_end: last }]);
    let roots = [Root { segment: 0, bus_start: 0, bus_end: last }];
    let (a, b) = {
        let _g = crate::CFG_LOCK.lock();
        (enumerate(&space, &roots), enumerate(&cf8, &roots))
    };
    kassert!(a.len() == b.len(), "ECAM found {} functions, 0xCF8 {}", a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        kassert!(x.loc == y.loc && x.vendor == y.vendor && x.device == y.device && x.class == y.class, "{} differs", x.loc);
        kassert!(x.bridge.map(|b| (b.secondary, b.subordinate)) == y.bridge.map(|b| (b.secondary, b.subordinate)));
        kassert!(x.space == if x.pcie.is_some() { 0x1000 } else { 0x100 } && y.space == 0x100, "{} space {}/{}", x.loc, x.space, y.space);
        let _g = crate::CFG_LOCK.lock();
        for off in (0..64u16).step_by(4).filter(|&o| o != 4) {
            kassert!(space.read32(x.loc, off) == cf8.read32(x.loc, off), "{} offset {:#x} differs", x.loc, off);
        }
    }
    let ext_total: usize = a.iter().map(|n| n.ext_caps.len()).sum();
    klog::event!(Info, "pci", "ktest.q35_window", base = klog::Hex(base), buses = buses, functions = a.len(), ext_caps = ext_total);
    Ok(())
}

/// After sizing, every BAR register holds the address the scan recorded (the all-ones probe was undone).
fn bars_restored() -> R {
    let mut checked = 0;
    for n in crate::tree() {
        let Some(bars) = n.bars else { continue };
        let f = n.function();
        for (i, b) in bars.iter().enumerate() {
            let Some(b) = b else { continue };
            let off = 0x10 + 4 * i as u8;
            let now = match b {
                Bar::Mem { is64: true, .. } => (f.read32(off + 4) as u64) << 32 | (f.read32(off) & !0xF) as u64,
                Bar::Mem { .. } => (f.read32(off) & !0xF) as u64,
                Bar::Io { .. } => (f.read32(off) & !3) as u64,
            };
            kassert!(now == b.start(), "{} BAR{} reads {:#x}, scan saw {:#x}", n.loc, i, now, b.start());
            checked += 1;
        }
    }
    kassert!(checked > 0, "no BAR anywhere");
    Ok(())
}

/// A function matched by an opt-in driver whose key is off: no sizing, no probe, writes refused, command
/// register as the firmware left it.
fn untouched_key_off() -> R {
    let nodes = crate::tree();
    let Some(i) = find(&nodes, ICH9_SMBUS) else { return ktest::skip("no ICH9 SMBus (QEMU q35 only)") };
    let n = &nodes[i];
    kassert!(n.untouched == Some("ktest_untouched"), "untouched = {:?}", n.untouched);
    kassert!(n.bars.is_none(), "an untouched function was sized");
    let f = n.function();
    kassert!(f.bar(4).is_none());
    kassert!(!f.write32_ext(4, n.command_at_scan as u32 | 0x7), "write to an untouched function went through");
    kassert!(f.command() == n.command_at_scan, "command changed: {:#x} -> {:#x}", n.command_at_scan, f.command());
    kassert!(UNTOUCHED_PROBES.load(Ordering::SeqCst) == 0, "opt-in driver probed without its key");
    Ok(())
}

/// R8d: every wireless controller (and a bridge above only it) is untouched without `wlan=on`, its command
/// register unchanged since the scan. On the Mini-PC: 04:00.0 and 00:02.2.
fn untouched_wlan() -> R {
    let nodes = crate::tree();
    let is_wlan = |n: &Node| crate::optin::CORE_OPT_IN.iter().any(|(m, _)| m.hits(n.vendor, n.device, n.class));
    if !nodes.iter().any(is_wlan) {
        return ktest::skip("no wireless controller");
    }
    if crate::registry::opted_in("wlan") {
        return ktest::skip("wlan=on: the WLAN may be touched");
    }
    for (i, n) in nodes.iter().enumerate() {
        if is_wlan(n) {
            kassert!(n.untouched == Some("wlan"), "{} not untouched", n.loc);
            // Its bridge, if it serves only the WLAN, is untouched too.
            if let Some(&p) = ancestors(&nodes, i).first() {
                let only = nodes.iter().filter(|c| c.parent == Some(p)).all(|c| c.untouched.is_some());
                kassert!(!only || nodes[p].untouched == Some("wlan"), "bridge {} above the WLAN touched", nodes[p].loc);
            }
        }
        if n.untouched == Some("wlan") {
            kassert!(n.bars.is_none(), "{} sized", n.loc);
            let now = n.function().command();
            kassert!(now == n.command_at_scan, "{} command {:#x} -> {:#x}", n.loc, n.command_at_scan, now);
        }
    }
    Ok(())
}

/// R8a: a mass-storage function binds only to the NVMe driver. QEMU's AHCI (class 01 06) is matched by a
/// test driver that must never be probed.
fn storage_nvme_only() -> R {
    let nodes = crate::tree();
    let Some(i) = find(&nodes, ICH9_AHCI) else { return ktest::skip("no ICH9 AHCI (QEMU q35 only)") };
    kassert!(nodes[i].is_storage());
    kassert!(STORAGE_PROBES.load(Ordering::SeqCst) == 0, "a non-NVMe driver was bound to mass storage");
    kassert!(!crate::optin::may_bind("ktest-storage", &nodes[i]) && crate::optin::may_bind("nvme", &nodes[i]));
    Ok(())
}

/// CP-2's register guard now sees PC's device list: a mass-storage BAR is refused.
fn guard_storage_bar() -> R {
    if acpi::tables().is_none() {
        return ktest::skip("acpi::init was not called (desk change request CP-2/1): the guard has no memory map");
    }
    let nodes = crate::tree();
    let bar = nodes
        .iter()
        .filter(|n| n.is_storage())
        .filter_map(|n| n.bars)
        .flat_map(|b| b.into_iter().flatten())
        .find(|b| matches!(b, Bar::Mem { .. }) && b.assigned());
    let Some(bar) = bar else { return ktest::skip("no mass-storage memory BAR") };
    let r = acpi::check_mem(bar.start(), 4);
    kassert!(r == Err(acpi::guard::Refusal::StorageBar), "guard said {:?} for storage BAR {:#x}", r, bar.start());
    Ok(())
}

/// A multi-function device: function 0 has header bit 7 and further functions were found (q35's 00:1f).
fn topo_multifunction() -> R {
    let nodes = crate::tree();
    let same_dev = |a: &Node, b: &Node| (a.loc.segment, a.loc.bus, a.loc.dev) == (b.loc.segment, b.loc.bus, b.loc.dev);
    let Some(f0) = nodes.iter().find(|n| n.loc.func == 0 && n.multifunction() && nodes.iter().any(|m| m.loc.func != 0 && same_dev(m, n))) else {
        return ktest::skip("no multi-function device");
    };
    for m in nodes.iter().filter(|m| same_dev(m, f0)) {
        kassert!(m.parent == f0.parent, "{} and {} have different parents", m.loc, f0.loc);
    }
    Ok(())
}

/// `-device pcie-root-port,id=rp1 -device igb,bus=rp1`: the igb below a root port, its BARs and MSI-X.
fn topo_root_port() -> R {
    let nodes = crate::tree();
    let Some(i) = find(&nodes, IGB) else { return ktest::skip("no igb card (host/tests/pc/10-topology.sh adds one)") };
    let n = &nodes[i];
    let p = n.parent.ok_or("igb on a root bus, not below a root port")?;
    let rp = &nodes[p];
    kassert!(rp.pcie.is_some_and(|x| x.is_root_port()), "{} is not a PCIe root port", rp.loc);
    kassert!(rp.bridge.is_some_and(|b| b.secondary == n.loc.bus), "bus numbers");
    kassert!(n.loc.dev == 0, "below a root port only device 0 exists");
    let bars = n.bars.ok_or("igb not sized")?;
    let b0 = bars[0].ok_or("igb BAR0 missing")?;
    kassert!(matches!(b0, Bar::Mem { .. }) && b0.size() == 128 << 10 && b0.assigned(), "igb BAR0 {:?}", b0);
    let msix = n.cap(id::MSIX).ok_or("igb has no MSI-X")?;
    let CapInfo::MsiX { table_bir, .. } = msix.info else { return Err("MSI-X not decoded".into()) };
    kassert!(bars[table_bir as usize].is_some(), "MSI-X table BAR{} missing", table_bir);
    kassert!(n.problems.is_empty(), "{:?}", n.problems);
    kassert!(n.pcie.is_some_and(|x| x.port_type == 0), "igb is not a PCIe endpoint");
    if crate::method() == "ecam" {
        kassert!(n.ext_cap(ext::AER).is_some(), "igb: no AER in the extended list {:?}", n.ext_caps.iter().map(|c| c.id).collect::<Vec<_>>());
    }
    Ok(())
}

/// A pci-bridge chain with QEMU's pci-testdev at the end (`membar` = a 64-bit prefetchable BAR2).
fn topo_bridge_chain() -> R {
    let nodes = crate::tree();
    let Some(i) = find(&nodes, PCI_TESTDEV) else { return ktest::skip("no pci-testdev (host/tests/pc/10-topology.sh adds one)") };
    let chain = ancestors(&nodes, i);
    kassert!(chain.len() >= 3, "pci-testdev only {} bridges deep", chain.len());
    for w in chain.windows(2) {
        let (lower, upper) = (nodes[w[0]].bridge.ok_or("not a bridge")?, nodes[w[1]].bridge.ok_or("not a bridge")?);
        kassert!(upper.secondary < lower.secondary && lower.subordinate <= upper.subordinate, "bus ranges not nested");
    }
    let bars = nodes[i].bars.ok_or("pci-testdev not sized")?;
    let b2 = bars[2].ok_or("no BAR2 (membar)")?;
    let Bar::Mem { phys, size, prefetch, is64 } = b2 else { return Err("BAR2 is not memory".into()) };
    kassert!(is64 && prefetch, "BAR2 {:?}", b2);
    kassert!(size == 1 << 30, "BAR2 size {:#x}", size);
    kassert!(phys >= 1 << 32, "BAR2 at {:#x}, below 4 GiB", phys);
    klog::event!(Info, "pci", "ktest.chain", depth = chain.len(), bar2 = klog::Hex(phys));
    Ok(())
}

/// hal's single 0xCF8 writer belongs to this crate after `scan`: nobody else can claim one (R8).
fn cf8_writer_owned() -> R {
    kassert!(crate::access::cf8_writer().is_some(), "the pci crate has no 0xCF8 writer");
    kassert!(hal::portio::PciConfigWriter::claim().is_none(), "a second 0xCF8 writer could be claimed");
    Ok(())
}

//! PC-2 ktests. The ones that clear bus master on bridges, put a function into D3hot or fake a stale MSI
//! are `QemuOnly` and work only on QEMU devices no driver is bound to (e1000e, pci-testdev; added by
//! `host/tests/pc/20-enable.sh`; an igb only while igc does not bind it). OVMF leaves bridge bus master on
//! and QEMU does not model it, so they clear it first and check that `enable_device` sets it again and logs
//! each bridge (spec/usb.md §8). The `Safe` ones only read, or call `enable_device` where it must refuse
//! before any access; they run on the Mini-PC.

use crate::enable::{CMD_INTX_OFF, CMD_MASTER, CMD_MEM};
use crate::node::{Bar, Node};
use crate::{caps, EnableError, Loc, PciFunction};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("pc.enable.bridges", QemuOnly, enable_bridges);
ktest!("pc.enable.wake", QemuOnly, enable_wake);
ktest!("pc.enable.msi_stale", QemuOnly, enable_msi_stale);
ktest!("pc.enable.restore", QemuOnly, enable_restore);
ktest!("pc.enable.refused", Safe, enable_refused);
ktest!("pc.enable.dma_path", Safe, enable_dma_path);

const IGB: (u16, u16) = (0x8086, 0x10C9);
/// 82574L: PM, MSI, MSI-X and PCIe like the igb, and no BitsOS driver matches it.
const E1000E: (u16, u16) = (0x8086, 0x10D3);
const PCI_TESTDEV: (u16, u16) = (0x1B36, 0x0005);
const ICH9_SMBUS: (u16, u16) = (0x8086, 0x2930);

/// QEMU test devices with no driver bound (these tests switch bus master off and put the function into
/// D3hot, which a running driver must never see; igc binds the igb since NW-2).
fn spare(ids: &[(u16, u16)]) -> Vec<Node> {
    crate::tree()
        .into_iter()
        .filter(|n| ids.contains(&(n.vendor, n.device)) && n.untouched.is_none() && crate::device::driver_of_loc(n.loc).is_none())
        .collect()
}

/// `pci` events of this name since `seq`, as their rendered field text.
fn events(seq: u64, name: &str) -> Vec<String> {
    klog::read_since(seq, usize::MAX).into_iter().filter(|r| r.subsystem == "pci" && r.name == name).map(|r| r.text).collect()
}

fn has_field(text: &str, key: &str, value: &str) -> bool {
    let want = format!("{key}={value}");
    text.split_whitespace().any(|w| w == want)
}

/// Bus master cleared on every bridge above a test device, then `enable_device(true)` sets it on the whole
/// path, root first, one `bridge.bus_master` event per bridge naming bridge and device; a second call
/// changes and logs nothing; `disable_device` leaves the device's bus master off and the bridges on.
fn enable_bridges() -> R {
    let targets: Vec<Node> = spare(&[PCI_TESTDEV, E1000E, IGB]).into_iter().filter(|n| n.parent.is_some()).collect();
    if targets.is_empty() {
        return ktest::skip("no driverless e1000e, igb or pci-testdev behind a bridge (host/tests/pc/20-enable.sh adds them)");
    }
    for n in targets {
        let f = n.function();
        let fname = format!("{}", n.loc);
        let path = f.upstream_bridges();
        kassert!(!path.is_empty(), "{fname}: no bridge above");
        let orig = f.command();
        for b in &path {
            b.set_command(b.command() & !CMD_MASTER);
            kassert!(b.command() & CMD_MASTER == 0, "{}: bus master did not clear", b.loc());
        }
        f.set_command(orig & !CMD_MASTER);
        let seq = klog::next_seq();
        let r = f.enable_device(true).map_err(|e| format!("{fname}: {e}"))?;
        let root_first: Vec<Loc> = path.iter().rev().map(PciFunction::loc).collect();
        kassert!(r.bridges_bus_master == root_first, "{fname}: switched {:?}, path {:?}", r.bridges_bus_master, root_first);
        let logged = events(seq, "bridge.bus_master");
        for b in &path {
            kassert!(b.command() & CMD_MASTER != 0, "{fname}: bridge {} still without bus master", b.loc());
            let bl = format!("{}", b.loc());
            kassert!(logged.iter().any(|t| has_field(t, "bridge", &bl) && has_field(t, "device", &fname)), "{fname}: no bridge.bus_master event for {bl}: {:?}", logged);
        }
        kassert!(logged.len() == path.len(), "{fname}: {} bridge events for {} bridges", logged.len(), path.len());
        let cmd = f.command();
        kassert!(cmd & (CMD_MASTER | CMD_INTX_OFF) == CMD_MASTER | CMD_INTX_OFF, "{fname}: command {cmd:#x}");
        let needs_mem = n.bars.iter().flatten().flatten().any(|b| matches!(b, Bar::Mem { .. }) && b.assigned());
        kassert!(!needs_mem || cmd & CMD_MEM != 0, "{fname}: memory decoding off");
        kassert!(r.command == cmd, "{fname}: reported {:#x}, reads {cmd:#x}", r.command);
        kassert!(crate::dma_enabled().contains(&n.loc), "{fname}: not recorded as a DMA function");

        let seq = klog::next_seq();
        let again = f.enable_device(true).map_err(|e| format!("{fname} again: {e}"))?;
        kassert!(again.bridges_bus_master.is_empty(), "{fname}: second enable switched {:?}", again.bridges_bus_master);
        kassert!(events(seq, "bridge.bus_master").is_empty() && events(seq, "enable").is_empty(), "{fname}: second enable logged changes");

        let d = f.disable_device();
        kassert!(d.bus_master_was_on, "{fname}: disable found bus master off");
        kassert!(f.command() & CMD_MASTER == 0, "{fname}: bus master still on after disable_device");
        kassert!(f.command() & CMD_MEM == cmd & CMD_MEM, "{fname}: disable changed memory decoding");
        for b in &path {
            kassert!(b.command() & CMD_MASTER != 0, "{fname}: disable switched bridge {} off", b.loc());
        }
        kassert!(!crate::dma_enabled().contains(&n.loc), "{fname}: still recorded as DMA after disable");
        klog::event!(Info, "pci", "ktest.bridges", function = fname.as_str(), bridges = path.len());
        f.set_command(orig);
    }
    Ok(())
}

/// D3hot -> D0: a test device put into D3hot through PMCSR comes back with `enable_device` (event
/// `pm.wake`), its BARs as the scan saw them.
fn enable_wake() -> R {
    let mut tried = 0;
    for n in spare(&[E1000E, IGB, PCI_TESTDEV]) {
        let Some(pm) = n.cap(caps::id::PM) else { continue };
        let f = n.function();
        let fname = format!("{}", n.loc);
        let off = pm.offset + 4;
        let orig = f.command();
        let csr = f.read32(off);
        f.write32(off, (csr & 0xFFFF_0000) | 3);
        if f.read16(off) & 3 != 3 {
            klog::event!(Info, "pci", "ktest.wake_skipped", function = fname.as_str(); "PMCSR does not accept D3hot here");
            continue;
        }
        tried += 1;
        let seq = klog::next_seq();
        let r = f.enable_device(false).map_err(|e| format!("{fname}: {e}"))?;
        kassert!(r.woke_from == Some(3), "{fname}: woke_from {:?}", r.woke_from);
        kassert!(f.read16(off) & 3 == 0, "{fname}: not in D0");
        let ev = events(seq, "pm.wake");
        kassert!(ev.iter().any(|t| has_field(t, "function", &fname) && has_field(t, "from", "D3")), "{fname}: no pm.wake event: {:?}", ev);
        for (i, b) in n.bars.iter().flat_map(|b| b.iter().enumerate()) {
            let Some(Bar::Mem { phys, is64, .. }) = *b else { continue };
            let off = 0x10 + 4 * i as u8;
            let hi = if is64 { (f.read32(off + 4) as u64) << 32 } else { 0 };
            let now = hi | (f.read32(off) & !0xF) as u64;
            kassert!(now == phys, "{fname}: BAR{i} {now:#x} after the wake, scan saw {phys:#x}");
        }
        f.set_command(orig);
    }
    if tried == 0 {
        return ktest::skip("no test device whose PMCSR accepts D3hot");
    }
    Ok(())
}

/// MSI and MSI-X enables left on (as a previous kernel would) are cleared by `enable_device`; bus master
/// stays off throughout, so nothing can be delivered.
fn enable_msi_stale() -> R {
    let Some(n) = spare(&[E1000E, IGB]).into_iter().next() else { return ktest::skip("no driverless e1000e or igb (host/tests/pc/20-enable.sh adds one)") };
    let f = n.function();
    let fname = format!("{}", n.loc);
    let orig = f.command();
    // An earlier ktest may have enabled it: `disable_device` makes the next enable a first one again (stale
    // enables are checked only then; afterwards they are the driver's own vectors).
    let _ = f.disable_device();
    f.set_command(orig & !CMD_MASTER);
    let mut set = 0;
    for (id, bit) in [(caps::id::MSI, 1u32 << 16), (caps::id::MSIX, 1 << 31)] {
        if let Some(off) = f.find_capability(id) {
            f.write32(off, f.read32(off) | bit);
            kassert!(f.read32(off) & bit != 0, "{fname}: cap {id:#x} enable did not stick");
            set += 1;
        }
    }
    if set == 0 {
        f.set_command(orig);
        return ktest::skip("test card without MSI/MSI-X capability");
    }
    let seq = klog::next_seq();
    let r = f.enable_device(false).map_err(|e| format!("{fname}: {e}"))?;
    kassert!(r.msi_cleared, "{fname}: nothing reported cleared");
    for (id, bit) in [(caps::id::MSI, 1u32 << 16), (caps::id::MSIX, 1 << 31)] {
        if let Some(off) = f.find_capability(id) {
            kassert!(f.read32(off) & bit == 0, "{fname}: cap {id:#x} still enabled");
        }
    }
    let n_ev = events(seq, "msi.stale_cleared").len();
    kassert!(n_ev == set, "{fname}: {n_ev} events for {set} enables");
    kassert!(f.command() & CMD_MASTER == 0, "{fname}: bus master on without dma");
    // The driver sets its MSI up (here: the enable bit, bus master stays off so nothing is sent) and calls
    // enable_device again: its vector stays; disable_device switches it off.
    if let Some(off) = f.find_capability(caps::id::MSI) {
        f.write32(off, f.read32(off) | 1 << 16);
        let seq = klog::next_seq();
        let again = f.enable_device(false).map_err(|e| format!("{fname} again: {e}"))?;
        kassert!(!again.msi_cleared && f.read32(off) & 1 << 16 != 0, "{fname}: a later enable cleared the driver's MSI");
        kassert!(events(seq, "msi.stale_cleared").is_empty(), "{fname}: stale MSI logged on a later enable");
        kassert!(f.disable_device().msi_was_on && f.read32(off) & 1 << 16 == 0, "{fname}: disable_device left MSI on");
    }
    f.set_command(orig);
    Ok(())
}

/// After a "retrain" (decoding off, BAR0 read 0), `restore_config` writes the saved header back: BAR0 as
/// the scan saw it, the command register as after the enable, each dword logged.
fn enable_restore() -> R {
    let Some(n) = spare(&[E1000E, IGB, PCI_TESTDEV]).into_iter().find(|n| n.bars.is_some_and(|b| matches!(b[0], Some(Bar::Mem { .. })))) else {
        return ktest::skip("no test device with a memory BAR0");
    };
    let f = n.function();
    let fname = format!("{}", n.loc);
    let orig = f.command();
    let r = f.enable_device(false).map_err(|e| format!("{fname}: {e}"))?;
    let bar0 = f.read32(0x10);
    f.set_command(r.command & !(CMD_MEM | 1));
    f.write32(0x10, 0);
    kassert!(f.read32(0x10) & !0xF == 0, "{fname}: BAR0 did not clear");
    let seq = klog::next_seq();
    let k = f.restore_config().map_err(|e| format!("{fname}: {e}"))?;
    kassert!(k >= 2, "{fname}: {k} dwords restored, expected BAR0 and the command register");
    kassert!(f.read32(0x10) == bar0, "{fname}: BAR0 {:#x}, was {bar0:#x}", f.read32(0x10));
    kassert!(f.command() == r.command, "{fname}: command {:#x}, was {:#x}", f.command(), r.command);
    let ev = events(seq, "config.restored");
    kassert!(ev.iter().any(|t| has_field(t, "function", &fname) && has_field(t, "offset", "0x10")), "{fname}: no config.restored for BAR0: {:?}", ev);
    kassert!(f.restore_config() == Ok(0), "{fname}: second restore wrote something");
    f.set_command(orig);
    Ok(())
}

/// R8d and R8a: an untouched function (QEMU: the ICH9 SMBus with a test key) and mass storage without the
/// NVMe driver are refused before any configuration access; their command registers stay as they were.
/// On the Mini-PC this calls `enable_device` on the NVMe disks and checks the refusal (nothing is written).
fn enable_refused() -> R {
    let mut checked = 0;
    for n in crate::tree() {
        let untouched = n.untouched.is_some() && (n.vendor, n.device) == ICH9_SMBUS;
        let storage = n.is_storage() && n.untouched.is_none() && crate::device::driver_of_loc(n.loc) != Some(crate::optin::STORAGE_DRIVER);
        if !untouched && !storage {
            continue;
        }
        let f = n.function();
        let fname = format!("{}", n.loc);
        let before = f.command();
        let seq = klog::next_seq();
        let r = f.enable_device(true);
        let want = if untouched { EnableError::Untouched("ktest_untouched") } else { EnableError::Storage };
        kassert!(r == Err(want), "{fname}: {:?}", r);
        kassert!(f.command() == before, "{fname}: command {before:#x} -> {:#x}", f.command());
        kassert!(events(seq, "enable.refused").iter().any(|t| has_field(t, "function", &fname)), "{fname}: no enable.refused event");
        if untouched {
            let d = f.disable_device();
            kassert!(d == crate::Disabled { refused: true, ..crate::Disabled::default() }, "{fname}: disable touched an untouched function: {d:?}");
            kassert!(f.command() == before, "{fname}: disable changed the command register");
        }
        checked += 1;
    }
    if checked == 0 {
        return ktest::skip("no untouched test function and no mass storage");
    }
    Ok(())
}

/// Every function a driver enabled for DMA has bus master on, and so has every bridge above it (the
/// Mini-PC: the I226's root port 00:02.1 once the igc driver ran). Reads only.
fn enable_dma_path() -> R {
    let locs = crate::dma_enabled();
    if locs.is_empty() {
        return ktest::skip("no function enabled for DMA yet");
    }
    let devs = crate::devices();
    for at in locs {
        let Some(f) = devs.iter().find(|d| d.loc() == at) else { return Err(format!("{at} not in the tree")) };
        kassert!(f.command() & CMD_MASTER != 0, "{at}: bus master off although enabled for DMA");
        for b in f.upstream_bridges() {
            kassert!(b.command() & CMD_MASTER != 0, "{at}: bridge {} has bus master off", b.loc());
        }
        let st = f.dma_status();
        kassert!(st.iter().all(|s| s.bus_master()), "{at}: {:?}", st);
        klog::event!(Info, "pci", "ktest.dma_path", function = format!("{at}"), bridges = st.len() - 1);
    }
    Ok(())
}

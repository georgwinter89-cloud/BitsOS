//! PC-3 ktests. The QEMU-only ones need devices no BitsOS driver binds, added by `host/tests/pc/50-msi.sh`:
//! QEMU's `edu` (MSI, one message, no mask bits; an MMIO write raises it), an `e1000e` (82574L: MSI-X with 5
//! entries in BAR3, and MSI; causes forced through ICS reach the vector IVAR names), `pci-testdev` (no MSI at
//! all: the polled fallback) and `virtio-rng-pci` (virtio MSI-X: table programming, masking, affinity). In the
//! plain gate they skip. The `Safe` ones only read or are refused before any access; they run on the Mini-PC
//! (`pc.msi.sets` checks every live set, e.g. igc's after NW-4) and may run several times per boot.

use crate::irq::{self, Affinity, IrqMode, IrqRequest, IrqSet, StormLimit, Wake};
use crate::msi::{self, MsiLayout, MsixLayout, Table};
use crate::node::{Bar, Node};
use crate::{device, enable, Loc, PciFunction};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("pc.msi.compose", Safe, msi_compose);
ktest!("pc.msi.sets", Safe, msi_sets);
ktest!("pc.msi.refused", Safe, msi_refused);
ktest!("pc.msi.edu", QemuOnly, msi_edu);
ktest!("pc.msi.edu_storm", QemuOnly, msi_edu_storm);
ktest!("pc.msi.msix", QemuOnly, msi_msix);
ktest!("pc.msi.e1000e_msi", QemuOnly, msi_e1000e_msi);
ktest!("pc.msi.fake_storm", QemuOnly, msi_fake_storm);
ktest!("pc.msi.disable_device", QemuOnly, msi_disable_device);
ktest!("pc.msi.restore", QemuOnly, msi_restore);
ktest!("pc.msi.polled", QemuOnly, msi_polled);
ktest!("pc.msi.virtio", QemuOnly, msi_virtio);

const EDU: (u16, u16) = (0x1234, 0x11E8);
const E1000E: (u16, u16) = (0x8086, 0x10D3);
const PCI_TESTDEV: (u16, u16) = (0x1B36, 0x0005);
const VIRTIO_VENDOR: u16 = 0x1AF4;

/// edu registers (QEMU `hw/misc/edu.c`): identification, interrupt status, raise, acknowledge.
const EDU_ID: usize = 0x00;
const EDU_STATUS: usize = 0x24;
const EDU_RAISE: usize = 0x60;
const EDU_ACK: usize = 0x64;

/// e1000e registers and causes (82574L datasheet §10.2.4; QEMU `e1000e_core.c`): ICR (write 1 to clear), ICS
/// (set causes), IMS (enable), IMC (disable), IVAR (cause -> MSI-X vector, 4 bits each with a valid bit),
/// EIAC (causes cleared automatically after their MSI-X message, so the next ICS raises again).
const E_ICR: usize = 0xC0;
const E_ICS: usize = 0xC8;
const E_IMS: usize = 0xD0;
const E_IMC: usize = 0xD8;
const E_EIAC: usize = 0xDC;
const E_IVAR: usize = 0xE4;
const RXQ0: u32 = 1 << 20;
const RXQ1: u32 = 1 << 21;
const TXQ0: u32 = 1 << 22;
/// RXQ0 -> vector 0, RXQ1 -> vector 1, TXQ0 -> vector 2.
const IVAR_3: u32 = 0x8 | 0x9 << 4 | 0xA << 8;
const CAUSES: [u32; 3] = [RXQ0, RXQ1, TXQ0];

const SECOND: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

fn spare(pick: impl Fn(&Node) -> bool) -> Vec<Node> {
    crate::tree().into_iter().filter(|n| pick(n) && n.untouched.is_none() && device::driver_of_loc(n.loc).is_none()).collect()
}

fn spare_ids(id: (u16, u16)) -> Option<Node> {
    spare(|n| (n.vendor, n.device) == id).into_iter().next()
}

fn events(seq: u64, name: &str) -> Vec<String> {
    klog::read_since(seq, usize::MAX).into_iter().filter(|r| r.subsystem == "pci" && r.name == name).map(|r| r.text).collect()
}

/// Polls `f` every millisecond until it holds or `ns` passed.
fn until(ns: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = time::now_ns().saturating_add(ns);
    loop {
        if f() {
            return true;
        }
        if time::now_ns() > end {
            return false;
        }
        sched::sleep_ns(MS);
    }
}

fn bar0(f: &PciFunction) -> Result<IoMem, String> {
    match f.bar(0) {
        Some(Bar::Mem { phys, size, .. }) => {
            hal::mmio::map_device(phys, size.min(0x2_0000) as usize, hal::paging::CacheType::Uncached).map_err(|e| format!("{e:?}"))
        }
        b => Err(format!("{}: BAR0 not memory: {b:?}", f.loc())),
    }
}

fn w(io: &IoMem, off: usize, v: u32) {
    let _ = io.write32(off, v);
}

fn r(io: &IoMem, off: usize) -> u32 {
    io.read32(off).unwrap_or(u32::MAX)
}

/// The MSI-X table through the PCI core's mapping cache (the Safe check reruns without mapping again).
fn table(f: &PciFunction, l: &MsixLayout) -> Result<alloc::sync::Arc<IoMem>, String> {
    match f.bar(l.table_bir) {
        Some(Bar::Mem { phys, .. }) => irq::map_cached(phys + l.table_offset as u64, l.table_bytes()).ok_or_else(|| String::from("table not mappable")),
        b => Err(format!("table BAR {b:?}")),
    }
}

/// The set's state checked against the device (shared by the Safe check and the QEMU tests).
fn check_device(at: Loc, mode: IrqMode, n: usize) -> R {
    let f = crate::function(at).ok_or("function gone")?;
    kassert!(f.command() & enable::CMD_INTX_OFF != 0, "{at}: INTx not off ({:#x})", f.command());
    let stats = irq::sets().into_iter().find(|s| s.0 == at).ok_or_else(|| format!("{at}: no live set"))?.3;
    kassert!(stats.len() == n, "{at}: {} indices, want {n}", stats.len());
    match mode {
        IrqMode::MsiX => {
            let l = MsixLayout::read(&device::Locked, at, f.msix_cap).ok_or("no MSI-X")?;
            kassert!(msi::msix_state(&device::Locked, at, &l) == (true, false), "{at}: MSI-X not enabled/unmasked");
            if f.msi_cap != 0 {
                let m = MsiLayout::read(&device::Locked, at, f.msi_cap).ok_or("no MSI")?;
                kassert!(!msi::msi_state(&device::Locked, at, &m).0, "{at}: MSI and MSI-X both on");
            }
            let io = table(&f, &l)?;
            for (i, s) in stats.iter().enumerate() {
                let v = s.vector.ok_or_else(|| format!("{at} #{i}: no vector"))?;
                kassert!(hal::vectors::is_registered(v) && hal::msi::is_bound(v), "{at} #{i}: {v:?} not registered/bound");
                let (m, masked) = msi::msix_read_entry(&*io, i as u16).ok_or("table unreadable")?;
                kassert!(m == hal::msi::message(v).map_err(|e| format!("{e}"))?, "{at} #{i}: entry {m:x?} is not {v:?}");
                kassert!(masked == (s.masked || s.stormed), "{at} #{i}: entry masked={masked}, record masked={} stormed={}", s.masked, s.stormed);
            }
        }
        IrqMode::Msi => {
            let l = MsiLayout::read(&device::Locked, at, f.msi_cap).ok_or("no MSI")?;
            let (on, mme) = msi::msi_state(&device::Locked, at, &l);
            kassert!(on || stats.iter().any(|s| s.stormed), "{at}: MSI off");
            kassert!(1usize << mme == n, "{at}: {} messages enabled, {n} vectors", 1 << mme);
            let v0 = stats[0].vector.ok_or("no vector")?;
            kassert!(msi::msi_read_message(&device::Locked, at, &l) == hal::msi::message(v0).map_err(|e| format!("{e}"))?, "{at}: MSI message");
            for (i, s) in stats.iter().enumerate() {
                let v = s.vector.ok_or("no vector")?;
                kassert!(v.cpu == v0.cpu && v.vector as usize == v0.vector as usize + i, "{at} #{i}: block not contiguous: {v:?}");
                kassert!(hal::vectors::is_registered(v) && hal::msi::is_bound(v), "{at} #{i}: not registered/bound");
            }
        }
        IrqMode::Polled => kassert!(stats.iter().all(|s| s.vector.is_none()), "{at}: polled with vectors"),
    }
    Ok(())
}

/// Teardown check: MSI and MSI-X off, the vectors given back, no live set.
fn check_gone(at: Loc, vs: &[hal::vectors::Vector]) -> R {
    let f = crate::function(at).ok_or("function gone")?;
    if f.msix_cap != 0 {
        let l = MsixLayout::read(&device::Locked, at, f.msix_cap).ok_or("no MSI-X")?;
        kassert!(!msi::msix_state(&device::Locked, at, &l).0, "{at}: MSI-X still on");
    }
    if f.msi_cap != 0 {
        let l = MsiLayout::read(&device::Locked, at, f.msi_cap).ok_or("no MSI")?;
        kassert!(!msi::msi_state(&device::Locked, at, &l).0, "{at}: MSI still on");
    }
    for v in vs {
        kassert!(!hal::vectors::is_registered(*v) && !hal::msi::is_bound(*v), "{at}: {v:?} still allocated");
    }
    kassert!(irq::sets().iter().all(|s| s.0 != at), "{at}: set still listed");
    kassert!(irq::first_vector(at).is_none(), "{at}: sysfs irq still set");
    Ok(())
}

fn vectors_of(set: &IrqSet) -> Vec<hal::vectors::Vector> {
    (0..set.len()).filter_map(|i| set.vector(i)).collect()
}

/// Every online CPU has an APIC ID an MSI can name; messages decode to it.
fn msi_compose() -> R {
    let n = hal::percpu::cpu_count() as u32;
    let mut reachable = 0;
    for c in 0..n {
        let Some(id) = hal::msi::destination(c) else { continue };
        if id > 0xFF {
            kassert!(hal::msi::compose(id, 0x40).is_err(), "cpu {c}: APIC ID {id} accepted");
            continue;
        }
        reachable += 1;
        let m = hal::msi::compose(id, 0x40).map_err(|e| format!("{e}"))?;
        kassert!(m.decode() == Some((id, 0x40)), "cpu {c}: {m:x?}");
        kassert!(m.address >> 20 == 0xFEE && m.address_hi() == 0, "cpu {c}: {m:x?}");
    }
    kassert!(reachable > 0, "no CPU reachable by MSI");
    Ok(())
}

/// Every live set (igc's after NW-4, xHCI's after US-4) matches its device: enables, entries, INTx off,
/// vectors registered and bound on their CPUs. Reads only.
fn msi_sets() -> R {
    let all = irq::sets();
    if all.is_empty() {
        return ktest::skip("no MSI/MSI-X set allocated (drivers still polled)");
    }
    for (at, name, mode, stats) in all {
        check_device(at, mode, stats.len()).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

/// Mass storage (R8a) and functions without `enable_device(true)` are refused before any access.
fn msi_refused() -> R {
    let mut tried = 0;
    for n in crate::tree().into_iter().filter(|n| n.untouched.is_none()) {
        let f = n.function();
        let storage = n.is_storage() && device::driver_of_loc(n.loc) != Some(crate::optin::STORAGE_DRIVER);
        let idle = !n.is_storage()
            && !n.is_bridge()
            && !device::has_record(n.loc)
            && (n.cap(crate::caps::id::MSI).is_some() || n.cap(crate::caps::id::MSIX).is_some());
        if !storage && !idle {
            continue;
        }
        let before = f.read32(4);
        let caps_before = [f.msi_cap, f.msix_cap].map(|c| if c != 0 { f.read32(c) } else { 0 });
        let r = f.alloc_irqs(&IrqRequest::new("ktest-refused", 1, 1));
        let want = if storage { irq::IrqError::Storage } else { irq::IrqError::NotEnabled };
        kassert!(r.as_ref().err() == Some(&want), "{}: {:?}, want {want:?}", n.loc, r.as_ref().err());
        kassert!(f.read32(4) == before, "{}: command/status changed", n.loc);
        kassert!([f.msi_cap, f.msix_cap].map(|c| if c != 0 { f.read32(c) } else { 0 }) == caps_before, "{}: MSI capability changed", n.loc);
        tried += 1;
        if tried >= 4 {
            break;
        }
    }
    if tried == 0 {
        return ktest::skip("no mass storage function and no idle MSI-capable function");
    }
    // Untouched functions (R8d), if this machine has one (the Mini-PC's WLAN).
    for n in crate::tree().into_iter().filter(|n| n.untouched.is_some()) {
        let r = n.function().alloc_irqs(&IrqRequest::new("ktest-refused", 1, 1));
        kassert!(matches!(r, Err(irq::IrqError::Untouched(_))), "{}: untouched not refused", n.loc);
    }
    Ok(())
}

/// edu: one MSI, raised by an MMIO write; the handle's thread wakes; mask in software; teardown.
fn msi_edu() -> R {
    let Some(n) = spare_ids(EDU) else { return ktest::skip("no driverless edu (host/tests/pc/50-msi.sh adds one)") };
    let f = n.function();
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let io = bar0(&f)?;
    kassert!(r(&io, EDU_ID) & 0xFF == 0xED, "edu id {:#x}", r(&io, EDU_ID));
    let seq = klog::next_seq();
    let set = f.alloc_irqs(&IrqRequest::new("ktest-edu", 1, 1)).map_err(|e| format!("{e}"))?;
    kassert!(set.mode() == IrqMode::Msi && set.len() == 1, "mode {:?} len {}", set.mode(), set.len());
    check_device(n.loc, IrqMode::Msi, 1)?;
    kassert!(!events(seq, "msi.alloc").is_empty(), "no msi.alloc event");
    let v = set.vector(0).ok_or("no vector")?;
    kassert!(irq::first_vector(n.loc) == Some(v), "sysfs irq");
    let h = set.handle(0).ok_or("no handle")?;
    let at_vector = hal::vectors::count(v);
    for k in 1..=5u64 {
        w(&io, EDU_RAISE, 1);
        let got = h.wait(SECOND);
        kassert!(got == Wake::Irq, "raise {k}: {got:?}");
        w(&io, EDU_ACK, r(&io, EDU_STATUS));
        kassert!(set.count(0) >= k, "raise {k}: count {}", set.count(0));
    }
    kassert!(hal::vectors::count(v) >= at_vector + 5, "vector {v:?} counted {}", hal::vectors::count(v) - at_vector);
    kassert!(h.wait(20 * MS) == Wake::Timeout, "a wake without an interrupt");
    set.mask(0).map_err(|e| format!("{e}"))?;
    w(&io, EDU_RAISE, 1);
    kassert!(until(SECOND, || set.stats(0).is_some_and(|s| s.suppressed >= 1)), "masked delivery not counted as suppressed");
    w(&io, EDU_ACK, r(&io, EDU_STATUS));
    kassert!(h.wait(20 * MS) == Wake::Timeout, "masked index woke its thread");
    set.unmask(0).map_err(|e| format!("{e}"))?;
    kassert!(h.wait(SECOND) == Wake::Irq, "unmask did not wake the owner once");
    let vs = vectors_of(&set);
    drop(set);
    check_gone(n.loc, &vs)?;
    kassert!(!events(seq, "msi.free").is_empty(), "no msi.free event");
    Ok(())
}

/// edu storm: far more raises than the limit allows; the guard switches MSI off (no mask bits), logs,
/// holds, switches it on again; the owner's waits turn into `Poll` meanwhile.
fn msi_edu_storm() -> R {
    let Some(n) = spare_ids(EDU) else { return ktest::skip("no driverless edu (host/tests/pc/50-msi.sh adds one)") };
    let f = n.function();
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let io = bar0(&f)?;
    let seq = klog::next_seq();
    let req = IrqRequest::new("ktest-storm", 1, 1).storm(Some(StormLimit { per_window: 20, window_ns: 10 * MS })).poll_ns(5 * MS);
    let set = f.alloc_irqs(&req).map_err(|e| format!("{e}"))?;
    let h = set.handle(0).ok_or("no handle")?;
    for _ in 0..2000 {
        w(&io, EDU_RAISE, 1);
    }
    w(&io, EDU_ACK, r(&io, EDU_STATUS));
    kassert!(until(SECOND, || set.stats(0).is_some_and(|s| s.storms == 1)), "no storm marked: {:?}", set.stats(0));
    kassert!(until(2 * SECOND, || !events(seq, "msi.storm").is_empty()), "no msi.storm event");
    let l = MsiLayout::read(&device::Locked, n.loc, f.msi_cap).ok_or("no MSI")?;
    let off_during = !msi::msi_state(&device::Locked, n.loc, &l).0;
    let _ = h.wait(0);
    let during = h.wait(SECOND);
    kassert!(until(5 * SECOND, || !events(seq, "msi.storm_end").is_empty()), "no msi.storm_end event");
    kassert!(off_during, "MSI still on during the hold");
    kassert!(during != Wake::Timeout, "no poll wake during the storm");
    kassert!(msi::msi_state(&device::Locked, n.loc, &l).0, "MSI not back on after the hold");
    kassert!(set.stats(0).is_some_and(|s| !s.stormed && s.suppressed > 0), "{:?}", set.stats(0));
    let text = events(seq, "msi.storm").join(" ");
    kassert!(text.contains("masked=msi_off") && text.contains("driver=ktest-storm"), "storm event: {text}");
    // Delivered normally again.
    let _ = h.wait(0);
    let before = set.count(0);
    w(&io, EDU_RAISE, 1);
    kassert!(h.wait(SECOND) == Wake::Irq && set.count(0) > before, "no delivery after the storm");
    w(&io, EDU_ACK, r(&io, EDU_STATUS));
    let vs = vectors_of(&set);
    drop(set);
    check_gone(n.loc, &vs)
}

/// The e1000e's registers set so that ICS of `CAUSES[i]` raises MSI-X vector i (or the MSI).
fn e1000e_causes(io: &IoMem) {
    w(io, E_IMC, u32::MAX);
    w(io, E_IVAR, IVAR_3);
    w(io, E_EIAC, RXQ0 | RXQ1 | TXQ0);
    w(io, E_ICR, u32::MAX);
    w(io, E_IMS, RXQ0 | RXQ1 | TXQ0);
}

/// ICR cleared and the causes enabled again before each raise: the 82574 may auto-mask a cause after its
/// MSI-X message (EIAME/IAM), and only a newly raised cause sends one.
fn e1000e_raise(io: &IoMem, i: usize) {
    w(io, E_ICR, u32::MAX);
    w(io, E_IMS, RXQ0 | RXQ1 | TXQ0);
    w(io, E_ICS, CAUSES[i]);
}

fn e1000e_quiet(io: &IoMem) {
    w(io, E_IMC, u32::MAX);
    w(io, E_ICR, u32::MAX);
}

/// e1000e MSI-X: three entries spread over the CPUs, each delivered on its own vector and CPU; masking holds
/// the message pending in the PBA until the unmask; affinity re-aims an entry; teardown.
fn msi_msix() -> R {
    let Some(n) = spare_ids(E1000E) else { return ktest::skip("no driverless e1000e (host/tests/pc/50-msi.sh adds one)") };
    let f = n.function();
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let io = bar0(&f)?;
    let seq = klog::next_seq();
    let set = f.alloc_irqs(&IrqRequest::new("ktest-msix", 3, 3).modes(true, false, false).affinity(Affinity::Spread)).map_err(|e| format!("{e}"))?;
    kassert!(set.mode() == IrqMode::MsiX && set.len() == 3, "mode {:?} len {}", set.mode(), set.len());
    check_device(n.loc, IrqMode::MsiX, 3)?;
    let cpus: Vec<u32> = (0..3).filter_map(|i| set.vector(i)).map(|v| v.cpu).collect();
    let online = hal::percpu::cpu_count();
    if online >= 3 {
        kassert!(cpus[0] != cpus[1] && cpus[1] != cpus[2] && cpus[0] != cpus[2], "not spread: {cpus:?}");
    }
    e1000e_causes(&io);
    for i in 0..3 {
        let h = set.handle(i).ok_or("no handle")?;
        let _ = h.wait(0);
        let counts: Vec<u64> = (0..3).map(|k| set.vector(k).map_or(0, hal::vectors::count)).collect();
        e1000e_raise(&io, i);
        kassert!(h.wait(SECOND) == Wake::Irq, "#{i}: no wake (count {})", set.count(i));
        // The raised index's vector counted on its CPU. (QEMU's e1000e sometimes sends an earlier cause once
        // more when IMS is written again, so the other indices are not required to stay quiet here; each line
        // only ever counts its own vectors, checked below.)
        let after: Vec<u64> = (0..3).map(|k| set.vector(k).map_or(0, hal::vectors::count)).collect();
        kassert!(after[i] > counts[i], "#{i}: vector {:?} did not count: before {counts:?} after {after:?}", set.vector(i));
    }
    for k in 0..3 {
        let v = set.vector(k).ok_or("no vector")?;
        kassert!(set.count(k) == hal::vectors::count(v), "#{k}: line counted {}, its vector {}", set.count(k), hal::vectors::count(v));
    }
    // Masked: the message waits in the PBA and comes on unmask.
    let h0 = set.handle(0).ok_or("no handle")?;
    let _ = h0.wait(0);
    set.mask(0).map_err(|e| format!("{e}"))?;
    let c0 = set.count(0);
    e1000e_raise(&io, 0);
    kassert!(h0.wait(50 * MS) == Wake::Timeout && set.count(0) == c0, "masked entry delivered");
    kassert!(set.pending(0) == Some(true), "no pending bit while masked: {:?}", set.pending(0));
    set.unmask(0).map_err(|e| format!("{e}"))?;
    kassert!(until(SECOND, || set.count(0) > c0), "pending message not delivered on unmask");
    kassert!(h0.wait(SECOND) == Wake::Irq, "no wake after unmask");
    // Affinity: index 1 to another CPU (or the same one again with one CPU).
    let old = set.vector(1).ok_or("no vector")?;
    let target = if online > 1 { (old.cpu + 1) % online as u32 } else { old.cpu };
    let new = set.set_affinity(1, target).map_err(|e| format!("{e}"))?;
    kassert!(new.cpu == target && set.vector(1) == Some(new), "moved to {new:?}");
    kassert!(!hal::vectors::is_registered(old) || old == new, "old vector {old:?} still allocated");
    check_device(n.loc, IrqMode::MsiX, 3)?;
    let h1 = set.handle(1).ok_or("no handle")?;
    // The move wakes the owner once (a message posted to the old vector found no line).
    kassert!(h1.wait(0) == Wake::Irq, "no wake after the affinity change");
    let spread = |v: u8| (0..online as u32).map(|c| hal::vectors::count(hal::vectors::Vector { cpu: c, vector: v })).collect::<Vec<_>>();
    let (ub, around) = (hal::msi::unbound_count(), spread(new.vector));
    e1000e_raise(&io, 1);
    let got = h1.wait(SECOND);
    kassert!(got == Wake::Irq && hal::vectors::count(new) >= 1, "no delivery on the new vector {new:?}: {got:?}, unbound +{}, counts of {:#x} per CPU {:?} -> {:?}, apic ids {:?}, entry {:x?}",
        hal::msi::unbound_count() - ub, new.vector, around, spread(new.vector),
        (0..online as u32).map(hal::msi::destination).collect::<Vec<_>>(), msi::msix_read_entry(&*table(&f, &MsixLayout::read(&device::Locked, n.loc, f.msix_cap).ok_or("x")?)?, 1));
    kassert!(!events(seq, "msi.affinity").is_empty(), "no msi.affinity event");
    e1000e_quiet(&io);
    let vs = vectors_of(&set);
    drop(set);
    check_gone(n.loc, &vs)
}

/// e1000e with MSI only: one message, delivered.
fn msi_e1000e_msi() -> R {
    let Some(n) = spare_ids(E1000E) else { return ktest::skip("no driverless e1000e (host/tests/pc/50-msi.sh adds one)") };
    let f = n.function();
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let io = bar0(&f)?;
    let set = f.alloc_irqs(&IrqRequest::new("ktest-msi", 1, 4).modes(false, true, false)).map_err(|e| format!("{e}"))?;
    kassert!(set.mode() == IrqMode::Msi, "mode {:?}", set.mode());
    check_device(n.loc, IrqMode::Msi, set.len())?;
    e1000e_causes(&io);
    let h = set.handle(0).ok_or("no handle")?;
    for k in 1..=3u64 {
        e1000e_raise(&io, 0);
        kassert!(h.wait(SECOND) == Wake::Irq, "raise {k}: no wake");
        kassert!(set.count(0) >= k, "raise {k}: count {}", set.count(0));
    }
    e1000e_quiet(&io);
    let vs = vectors_of(&set);
    drop(set);
    check_gone(n.loc, &vs)
}

/// A storm without the device's help: fixed IPIs at the vector of an MSI-X entry. The entry is masked in the
/// hard-IRQ path at once, the guard logs, holds and unmasks.
fn msi_fake_storm() -> R {
    let msix_spare = || spare(|n| n.vendor == VIRTIO_VENDOR && n.cap(crate::caps::id::MSIX).is_some()).into_iter().next();
    let Some(n) = spare_ids(E1000E).or_else(msix_spare) else {
        return ktest::skip("no driverless MSI-X function (host/tests/pc/50-msi.sh adds one)");
    };
    let f = n.function();
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let seq = klog::next_seq();
    let req = IrqRequest::new("ktest-fake-storm", 1, 1)
        .modes(true, false, false)
        .affinity(Affinity::Cpu(0))
        .storm(Some(StormLimit { per_window: 50, window_ns: 10 * MS }));
    let set = f.alloc_irqs(&req).map_err(|e| format!("{e}"))?;
    let v = set.vector(0).ok_or("no vector")?;
    let id = hal::msi::destination(v.cpu).ok_or("no APIC ID")?;
    let l = MsixLayout::read(&device::Locked, n.loc, f.msix_cap).ok_or("no MSI-X")?;
    let t = table(&f, &l)?;
    for _ in 0..500 {
        hal::apic::send_ipi(hal::apic::IpiDest::Physical(id), hal::apic::IpiKind::Fixed(v.vector)).map_err(|e| format!("{e:?}"))?;
    }
    kassert!(until(SECOND, || set.stats(0).is_some_and(|s| s.storms == 1)), "no storm after 500 IPIs: {:?}", set.stats(0));
    let masked_by_irq = msi::msix_read_entry(&*t, 0).is_some_and(|e| e.1);
    kassert!(until(2 * SECOND, || !events(seq, "msi.storm").is_empty()), "no msi.storm event");
    kassert!(until(5 * SECOND, || !events(seq, "msi.storm_end").is_empty()), "no msi.storm_end event");
    kassert!(masked_by_irq, "entry not masked by the hard-IRQ path");
    kassert!(msi::msix_read_entry(&*t, 0).is_some_and(|e| !e.1), "entry still masked after the hold");
    let s = set.stats(0).ok_or("no stats")?;
    // IPIs to a vector still pending in IRR merge, so fewer than 500 arrive; more than the limit did.
    kassert!(!s.stormed && s.storms == 1 && s.suppressed > 0 && s.deliveries > 50, "{s:?}");
    let vs = vectors_of(&set);
    drop(set);
    check_gone(n.loc, &vs)
}

/// `disable_device` (Quiesce, remove) switches MSI-X off and frees the vectors; the driver's set answers
/// `Released`; enable again, allocate again.
fn msi_disable_device() -> R {
    let Some(n) = spare_ids(E1000E) else { return ktest::skip("no driverless e1000e (host/tests/pc/50-msi.sh adds one)") };
    let f = n.function();
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let set = f.alloc_irqs(&IrqRequest::new("ktest-quiesce", 2, 2).modes(true, false, false)).map_err(|e| format!("{e}"))?;
    let vs = vectors_of(&set);
    kassert!(vs.len() == 2, "{vs:?}");
    let d = f.disable_device();
    kassert!(d.msix_was_on && !d.incomplete && !d.refused, "{d:?}");
    kassert!(set.is_released(), "set not released");
    for v in &vs {
        kassert!(!hal::vectors::is_registered(*v), "{v:?} still allocated after disable_device");
    }
    kassert!(set.mask(0) == Err(irq::IrqError::Released), "mask after release");
    kassert!(set.wait(0, 5 * SECOND) == Ok(Wake::Poll), "a released set must answer Poll, not block");
    kassert!(f.alloc_irqs(&IrqRequest::new("ktest-quiesce", 1, 1)).err() == Some(irq::IrqError::NotEnabled), "allocation after disable");
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let again = f.alloc_irqs(&IrqRequest::new("ktest-quiesce", 2, 2).modes(true, false, false)).map_err(|e| format!("again: {e}"))?;
    check_device(n.loc, IrqMode::MsiX, 2)?;
    drop(set);
    check_device(n.loc, IrqMode::MsiX, 2).map_err(|e| format!("dropping the released set touched the new one: {e}"))?;
    let vs2 = vectors_of(&again);
    drop(again);
    check_gone(n.loc, &vs2)
}

/// `restore_config` after a lost MSI-X state (control cleared, entries zeroed as after a function reset):
/// written back, delivered again.
fn msi_restore() -> R {
    let Some(n) = spare_ids(E1000E) else { return ktest::skip("no driverless e1000e (host/tests/pc/50-msi.sh adds one)") };
    let f = n.function();
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let io = bar0(&f)?;
    let set = f.alloc_irqs(&IrqRequest::new("ktest-restore", 2, 2).modes(true, false, false)).map_err(|e| format!("{e}"))?;
    let l = MsixLayout::read(&device::Locked, n.loc, f.msix_cap).ok_or("no MSI-X")?;
    let t = table(&f, &l)?;
    msi::msix_control(&device::Locked, n.loc, &l, false, false);
    for i in 0..l.size as usize {
        for d in 0..3 {
            let _ = Table::write32(&*t, i * 16 + d * 4, 0);
        }
        let _ = Table::write32(&*t, i * 16 + 12, 1);
    }
    let seq = klog::next_seq();
    let written = f.restore_config().map_err(|e| format!("{e}"))?;
    kassert!(written >= 1, "restore wrote nothing");
    kassert!(!events(seq, "msi.restored").is_empty(), "no msi.restored event");
    check_device(n.loc, IrqMode::MsiX, 2)?;
    kassert!(f.restore_config().map_err(|e| format!("{e}"))? == 0, "second restore wrote again");
    e1000e_causes(&io);
    let h = set.handle(1).ok_or("no handle")?;
    let _ = h.wait(0);
    e1000e_raise(&io, 1);
    kassert!(h.wait(SECOND) == Wake::Irq, "no delivery after restore");
    e1000e_quiet(&io);
    let vs = vectors_of(&set);
    drop(set);
    check_gone(n.loc, &vs)
}

/// No MSI capability: the polled fallback (no vector, INTx off, `wait` gives `Poll` every `poll_ns`); with
/// polling not allowed the request is refused.
fn msi_polled() -> R {
    let Some(n) = spare_ids(PCI_TESTDEV) else { return ktest::skip("no driverless pci-testdev") };
    let f = n.function();
    kassert!(f.msi_cap == 0 && f.msix_cap == 0, "pci-testdev has MSI now?");
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    kassert!(f.alloc_irqs(&IrqRequest::new("ktest-polled", 1, 1).modes(true, true, false)).err() == Some(irq::IrqError::NoVectors), "not refused");
    let set = f.alloc_irqs(&IrqRequest::new("ktest-polled", 2, 4).poll_ns(2 * MS)).map_err(|e| format!("{e}"))?;
    kassert!(set.mode() == IrqMode::Polled && set.len() == 2 && set.vector(0).is_none(), "mode {:?} len {}", set.mode(), set.len());
    check_device(n.loc, IrqMode::Polled, 2)?;
    let t0 = time::now_ns();
    kassert!(set.wait(1, SECOND) == Ok(Wake::Poll), "polled wait");
    kassert!(time::now_ns() - t0 < 500 * MS, "polled wait took {} ms", (time::now_ns() - t0) / MS);
    kassert!(set.set_affinity(0, 0) == Err(irq::IrqError::Polled), "affinity on a polled set");
    drop(set);
    kassert!(irq::sets().iter().all(|s| s.0 != n.loc), "set still listed");
    Ok(())
}

/// virtio MSI-X (a virtio-rng no driver binds; the gate's virtio-net belongs to NW's driver): every entry
/// programmed, masks and affinity visible in the table, teardown.
fn msi_virtio() -> R {
    let Some(n) = spare(|n| n.vendor == VIRTIO_VENDOR && n.cap(crate::caps::id::MSIX).is_some()).into_iter().next() else {
        return ktest::skip("no driverless virtio function with MSI-X (host/tests/pc/50-msi.sh adds virtio-rng-pci)");
    };
    let f = n.function();
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let l = MsixLayout::read(&device::Locked, n.loc, f.msix_cap).ok_or("no MSI-X")?;
    let want = l.size.min(2);
    let set = f.alloc_irqs(&IrqRequest::new("ktest-virtio", 1, want).modes(true, false, false)).map_err(|e| format!("{e}"))?;
    kassert!(set.mode() == IrqMode::MsiX && set.len() == want as usize, "mode {:?} len {}", set.mode(), set.len());
    check_device(n.loc, IrqMode::MsiX, want as usize)?;
    let t = table(&f, &l)?;
    set.mask(0).map_err(|e| format!("{e}"))?;
    kassert!(msi::msix_read_entry(&*t, 0).is_some_and(|e| e.1), "mask not in the table");
    check_device(n.loc, IrqMode::MsiX, want as usize)?;
    set.unmask(0).map_err(|e| format!("{e}"))?;
    kassert!(msi::msix_read_entry(&*t, 0).is_some_and(|e| !e.1), "unmask not in the table");
    let target = (hal::percpu::cpu_count() as u32).saturating_sub(1);
    let v = set.set_affinity(0, target).map_err(|e| format!("{e}"))?;
    kassert!(msi::msix_read_entry(&*t, 0).map(|e| e.0) == hal::msi::message(v).ok(), "entry not re-aimed");
    check_device(n.loc, IrqMode::MsiX, want as usize)?;
    let vs = vectors_of(&set);
    drop(set);
    check_gone(n.loc, &vs)
}

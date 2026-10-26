//! PC-5 ktests in the pci crate: the ASPM helper on a small in-kernel fake configuration space (an I226 below
//! a root port, both with L1 PM Substates; the same world on a 256-byte back end for the fallback), the
//! L1.2 state of real I226 links after boot, the extended saved PCIe control words, and the raw-tool guards
//! on the real tree plus a made-up Radeon. The tool-level tests are in `crates/mcp/src/tools/pc/ktests.rs`.

use crate::access::{ConfigAccess, Loc, EXT_SPACE, STD_SPACE};
use crate::aspm::{self, AspmError, Method, L1_2};
use crate::caps::ext;
use crate::enumerate::Root;
use crate::node::{Bar, Node};
use crate::raw::{self, Refusal};
use crate::{enable, NODES};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;
use ksync::SpinLock;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("pc.aspm.fake", Safe, aspm_fake);
ktest!("pc.aspm.i226", Safe, aspm_i226);
ktest!("pc.aspm.kernel_path", QemuOnly, aspm_kernel_path);
ktest!("pc.pcie.saved_regs", Safe, saved_regs);
ktest!("pc.pcie.restore_ctl2", QemuOnly, restore_ctl2);
ktest!("pc.raw.guards", Safe, raw_guards);

/// Configuration space as dwords per function; all ones where nothing answers.
struct Fake {
    fns: SpinLock<BTreeMap<Loc, Vec<u32>>>,
    space: u16,
}

impl Fake {
    fn new(space: u16) -> Fake {
        Fake { fns: SpinLock::named("pci.ktest_fake", BTreeMap::new()), space }
    }
    fn add(&self, at: Loc, set: &[(u16, u32)]) {
        let mut d = alloc::vec![0u32; EXT_SPACE as usize / 4];
        for &(o, v) in set {
            d[o as usize / 4] = v;
        }
        self.fns.lock().insert(at, d);
    }
    fn get(&self, at: Loc, off: u16) -> u32 {
        self.fns.lock().get(&at).map_or(u32::MAX, |d| d[off as usize / 4])
    }
}

impl ConfigAccess for Fake {
    fn read32(&self, at: Loc, off: u16) -> u32 {
        if off >= self.space {
            return u32::MAX;
        }
        self.get(at, off)
    }
    fn write32(&self, at: Loc, off: u16, v: u32) -> bool {
        if off >= self.space {
            return false;
        }
        if let Some(d) = self.fns.lock().get_mut(&at) {
            // Link Status / status halves are write-1-to-clear in hardware; the fake keeps what is written.
            d[off as usize / 4] = v;
        }
        true
    }
    fn space_size(&self, _segment: u16, _bus: u8) -> u16 {
        self.space
    }
    fn name(&self) -> &'static str {
        "fake"
    }
}

const PORT: Loc = Loc::new(0, 0, 1, 0);
const NIC: Loc = Loc::new(0, 1, 0, 0);
/// L1SS Control 1 as the firmware left it: all four enables on (port), ASPM + PCI-PM L1.2 on (NIC).
const PORT_CTL1: u32 = 0xF;
const NIC_CTL1: u32 = 0x5;

/// A root port (PCIe cap at 0x40, version 2, root port; Link Control ASPM L1 on) above an I226-V (PCIe cap
/// at 0xA0, endpoint; Link Control ASPM L1 on), both with an L1SS capability at 0x100.
fn world(space: u16) -> Fake {
    let f = Fake::new(space);
    let pcie = |port_type: u32| 0x10 | (2 | port_type << 4) << 16;
    f.add(PORT, &[
        (0x00, 0x14EE_1022),
        (0x04, 0x0010_0407),
        (0x08, 0x0604_0000),
        (0x0C, 0x0001_0000),
        (0x18, 0x0001_0100),
        (0x34, 0x40),
        (0x40, pcie(4)),
        (0x50, 0x2),
        (0x100, ext::L1SS as u32 | 1 << 16),
        (0x108, PORT_CTL1),
    ]);
    f.add(NIC, &[
        (0x00, 0x125C_8086),
        (0x04, 0x0010_0006),
        (0x08, 0x0200_0000),
        (0x34, 0xA0),
        (0xA0, pcie(0)),
        (0xB0, 0x2),
        (0x100, ext::L1SS as u32 | 1 << 16),
        (0x108, NIC_CTL1),
    ]);
    f
}

fn nodes(f: &Fake) -> Vec<Node> {
    crate::enumerate::enumerate(f, &[Root { segment: 0, bus_start: 0, bus_end: 1 }])
}

/// The helper on the fake: L1.2 off on both ends (device first), L1.1 and ASPM L1 untouched, a second call
/// writes nothing; without ECAM the fallback switches ASPM L1 off in both Link Controls; untouched and
/// storage refused before any write.
fn aspm_fake() -> R {
    let f = world(EXT_SPACE);
    let ns = nodes(&f);
    let nic = ns.iter().position(|n| n.loc == NIC).ok_or("fake NIC not walked")?;
    kassert!(ns[nic].ext_cap(ext::L1SS).is_some(), "fake NIC has no L1SS in the walk: {:?}", ns[nic].ext_caps);
    let r = aspm::l12_off(&f, &ns, nic, false).map_err(|e| format!("{e}"))?;
    kassert!(r.method == Method::L1ss && r.device == NIC && r.port == PORT && r.verified, "{r:?}");
    kassert!(r.ends.len() == 2 && r.ends[0].at == NIC && r.ends[1].at == PORT, "device first: {:?}", r.ends);
    kassert!(f.get(NIC, 0x108) == NIC_CTL1 & !L1_2, "NIC ctl1 {:#x}", f.get(NIC, 0x108));
    kassert!(f.get(PORT, 0x108) == PORT_CTL1 & !L1_2, "port ctl1 {:#x} (L1.1 must stay)", f.get(PORT, 0x108));
    kassert!(f.get(NIC, 0xB0) == 0x2 && f.get(PORT, 0x50) == 0x2, "ASPM L1 in Link Control must be back on");
    kassert!(r.held.len() == 2 && r.held[0].at == NIC && r.held[1].at == PORT && r.held.iter().all(|e| e.before == 0x2 && e.after == 0),
        "ASPM L1 held off on the link while the L1SS enables changed (device, then port): {:?}", r.held);
    let again = aspm::l12_off(&f, &ns, nic, false).map_err(|e| format!("{e}"))?;
    kassert!(!again.changed() && again.verified, "second call changed something: {again:?}");

    let legacy = world(STD_SPACE);
    let ln = nodes(&legacy);
    let i = ln.iter().position(|n| n.loc == NIC).ok_or("legacy NIC not walked")?;
    let r = aspm::l12_off(&legacy, &ln, i, false).map_err(|e| format!("{e}"))?;
    kassert!(r.method == Method::AspmL1Off && r.verified && r.changed(), "{r:?}");
    kassert!(legacy.get(NIC, 0xB0) == 0 && legacy.get(PORT, 0x50) == 0, "ASPM L1 still on without ECAM");

    let mut un = ns.clone();
    un[nic].untouched = Some("wlan");
    kassert!(aspm::l12_off(&f, &un, nic, false) == Err(AspmError::Untouched("wlan")));
    let mut st = ns.clone();
    st[nic].class = 0x01_08_02;
    kassert!(aspm::l12_off(&f, &st, nic, false) == Err(AspmError::Storage));
    let port = ns.iter().position(|n| n.loc == PORT).ok_or("no port")?;
    kassert!(aspm::l12_off(&f, &ns, port, false) == Err(AspmError::NoLink), "a root port on bus 0 has no port above it");
    Ok(())
}

/// After boot every I226 its driver enabled has L1.2 off on both ends of its link (the Mini-PC's 03:00.0
/// behind 00:02.1). Reads only.
fn aspm_i226() -> R {
    let ns = NODES.lock().clone();
    let mut seen = 0;
    for (i, n) in ns.iter().enumerate().filter(|(_, n)| aspm::needs_l12_off(n.vendor, n.device) && crate::device::is_enabled(n.loc)) {
        let Some(p) = n.parent.map(|p| &ns[p]) else { continue };
        let (Some(d), Some(pc)) = (n.ext_cap(ext::L1SS), p.ext_cap(ext::L1SS)) else {
            klog::event!(Info, "pci", "ktest.aspm", function = format!("{}", n.loc), l1ss = false);
            continue;
        };
        let f = n.function();
        let dv = f.read32_ext(d.offset + aspm::L1SS_CTL1).ok_or("device ctl1 unreadable")?;
        let pv = p.function().read32_ext(pc.offset + aspm::L1SS_CTL1).ok_or("port ctl1 unreadable")?;
        klog::event!(Info, "pci", "ktest.aspm", function = format!("{}", n.loc), port = format!("{}", p.loc),
            device_ctl1 = klog::Hex(dv as u64), port_ctl1 = klog::Hex(pv as u64));
        kassert!(dv & L1_2 == 0 && pv & L1_2 == 0, "{} (index {i}): L1.2 still on (device {dv:#x}, port {pv:#x})", n.loc);
        seen += 1;
    }
    if seen == 0 {
        return ktest::skip("no enabled I226 with L1 PM Substates on both ends of its link");
    }
    Ok(())
}

/// `PciFunction::aspm_l12_off` on q35: a PCIe card behind a root port (QEMU's ports have no L1 PM Substates:
/// "the link never enters L1.2", nothing written), and the refusals with their events (the untouched MCH,
/// the AHCI). QEMU only: it may write Link Control of a card no driver uses.
fn aspm_kernel_path() -> R {
    let ns = NODES.lock().clone();
    let mut done = 0;
    if let Some(n) = ns.iter().find(|n| {
        n.untouched.is_none() && !n.is_storage() && n.pcie.is_some_and(|p| p.port_type == 0) && n.parent.is_some_and(|p| ns[p].pcie.is_some_and(|x| x.is_root_port()))
    }) {
        let r = n.function().aspm_l12_off().map_err(|e| format!("{}: {e}", n.loc))?;
        let want = if n.ext_cap(ext::L1SS).is_some() && n.parent.is_some_and(|p| ns[p].ext_cap(ext::L1SS).is_some()) { Method::L1ss } else { Method::NotSupported };
        kassert!(r.method == want && r.verified, "{}: {r:?}", n.loc);
        let again = n.function().aspm_l12_off().map_err(|e| format!("{e}"))?;
        kassert!(!again.changed(), "{}: second call changed something", n.loc);
        done += 1;
    }
    for n in ns.iter().filter(|n| n.untouched.is_some()) {
        kassert!(matches!(n.function().aspm_l12_off(), Err(AspmError::Untouched(_))), "{}: untouched not refused", n.loc);
        done += 1;
    }
    for n in ns.iter().filter(|n| n.is_storage()) {
        kassert!(n.function().aspm_l12_off() == Err(AspmError::Storage), "{}: storage not refused", n.loc);
        done += 1;
    }
    if done == 0 {
        return ktest::skip("no PCIe card behind a root port, no untouched and no storage function");
    }
    Ok(())
}

/// The saved copy names exactly the PCIe control registers each function has (Linux's rules): Link Control
/// unless integrated endpoint / event collector, Root Control on root ports, the "2" registers from
/// capability version 2. Reads only (`enable::save` over the locked back end).
fn saved_regs() -> R {
    let ns = NODES.lock().clone();
    let mut checked = 0;
    for n in ns.iter().filter(|n| n.untouched.is_none()) {
        let Some(cap) = n.cap(crate::caps::id::PCIE) else { continue };
        let s = enable::save(&crate::device::Locked, n.loc, cap.offset);
        let p = s.pcie.ok_or_else(|| format!("{}: no PCIe words saved", n.loc))?;
        let flags = n.function().read16(cap.offset + 2);
        let regs = enable::PcieRegs::from_flags(flags);
        kassert!(p.link_ctl.is_some() == regs.link && p.root_ctl.is_some() == regs.root && p.slot_ctl.is_some() == regs.slot, "{}: {p:?} flags {flags:#x}", n.loc);
        kassert!(p.dev_ctl2.is_some() == regs.v2 && p.link_ctl2.is_some() == (regs.v2 && regs.link), "{}: {p:?} flags {flags:#x}", n.loc);
        let pi = n.pcie.ok_or("pcie info")?;
        kassert!(!pi.is_root_port() || p.root_ctl.is_some(), "{}: root port without Root Control", n.loc);
        checked += 1;
    }
    if checked == 0 {
        return ktest::skip("no PCIe function on this machine");
    }
    Ok(())
}

/// A control word lost behind the driver's back comes back with `restore_config`, and a second restore writes
/// nothing: Device Control 2 of an enabled PCIe function (the completion timeout value, or bit 15
/// End-End TLP Prefix Blocking, the one QEMU lets software write), and Slot Control of a root port with a slot
/// (the attention indicator bits, which only light a LED). QEMU only: it changes live registers.
fn restore_ctl2() -> R {
    let ns = NODES.lock().clone();
    let pcie_off = |n: &Node| n.cap(crate::caps::id::PCIE).map(|c| c.offset as u16);
    let mut done = 0;
    let dev = ns.iter().find(|n| {
        n.untouched.is_none() && !n.is_storage() && crate::device::is_enabled(n.loc) && pcie_off(n).is_some() && n.pcie.is_some_and(|p| p.version >= 2)
    });
    if let Some(n) = dev {
        let o = pcie_off(n).ok_or("cap")? + 0x28;
        let flips: [&dyn Fn(u32) -> u32; 2] = [&|v| (v & !0xF) | if v & 0xF == 0x5 { 0x6 } else { 0x5 }, &|v| v ^ 0x8000];
        done += lost_and_restored(n, o, "dev_ctl2", &flips)?;
    }
    let slot = ns.iter().find(|n| {
        n.untouched.is_none()
            && n.pcie.is_some_and(|p| p.is_root_port())
            && pcie_off(n).is_some_and(|o| n.function().read16(o as u8 + 2) & (1 << 8) != 0)
            && !raw::storage_below(&ns, ns.iter().position(|x| x.loc == n.loc).unwrap_or(0))
    });
    if let Some(n) = slot {
        let o = pcie_off(n).ok_or("cap")? + 0x18;
        let flips: [&dyn Fn(u32) -> u32; 1] = [&|v| (v & !0xC0) | if v & 0xC0 == 0x40 { 0x80 } else { 0x40 }];
        done += lost_and_restored(n, o, "slot_ctl", &flips)?;
    }
    if done == 0 {
        return ktest::skip("no PCIe function with a writable Device Control 2 and no root port with a slot");
    }
    Ok(())
}

/// Saves, changes the word at `o` with the first flip the function takes, restores, compares. 0: none took.
fn lost_and_restored(n: &Node, o: u16, what: &str, flips: &[&dyn Fn(u32) -> u32]) -> Result<usize, alloc::string::String> {
    let f = n.function();
    f.save_config().ok_or("save_config refused")?;
    let before = f.read32_ext(o).ok_or("unreadable")? & 0xFFFF;
    for flip in flips {
        kassert!(f.write32_ext(o, flip(before) & 0xFFFF), "{}: write refused", n.loc);
        let lost = f.read32_ext(o).ok_or("unreadable")? & 0xFFFF;
        if lost == before {
            continue;
        }
        let restored = f.restore_config().map_err(|e| format!("restore: {e}"))?;
        let now = f.read32_ext(o).ok_or("unreadable")? & 0xFFFF;
        kassert!(now == before, "{} {o:#x}: {now:#x} after restore, saved {before:#x}", n.loc);
        kassert!(restored >= 1, "{}: restore reported nothing", n.loc);
        kassert!(f.restore_config() == Ok(0), "{}: second restore wrote something", n.loc);
        klog::event!(Info, "pci", "ktest.restore_ctl", function = format!("{}", n.loc), register = what, offset = klog::Hex(o as u64), value = klog::Hex(before as u64));
        return Ok(1);
    }
    Ok(0)
}

fn amd_gpu() -> Node {
    let bars: crate::node::Bars = [
        Some(Bar::Mem { phys: 0x7C_0000_0000, size: 0x1000_0000, prefetch: true, is64: true }),
        None,
        Some(Bar::Mem { phys: 0x7C_1000_0000, size: 0x20_0000, prefetch: true, is64: true }),
        None,
        Some(Bar::Io { port: 0xE000, size: 0x100 }),
        Some(Bar::Mem { phys: 0xDC50_0000, size: 0x8_0000, prefetch: false, is64: false }),
    ];
    Node {
        loc: Loc::new(0, 0xC7, 0, 0),
        vendor: 0x1002,
        device: 0x1900,
        class: 0x03_00_00,
        revision: 0xD5,
        header_type: 0x80,
        subsystem: (0, 0),
        command_at_scan: 7,
        status_at_scan: 0x10,
        interrupt_pin: 1,
        parent: None,
        space: EXT_SPACE,
        bridge: None,
        caps: Vec::new(),
        ext_caps: Vec::new(),
        pcie: None,
        bars: Some(bars),
        untouched: None,
        problems: Vec::new(),
    }
}

/// The raw-tool guards (pure, no access): every class 01 function refused for config writes and memory;
/// every untouched function read only in its header and never written; a bridge above mass storage never
/// written; the Radeon's (made up, plus the real one when there is one) index pairs never touched.
fn raw_guards() -> R {
    let ns = NODES.lock().clone();
    for (i, n) in ns.iter().enumerate() {
        let space = raw::cfg_space(n, crate::access().space_size(n.loc.segment, n.loc.bus));
        if n.is_storage() {
            kassert!(raw::cfg_write_check(&ns, i, space, 0, 4, true) == Err(Refusal::Storage), "{}: storage write", n.loc);
            for b in 0..6 {
                kassert!(raw::mmio_check(n, b, 0, 4, 1, false) == Err(Refusal::Storage), "{}: storage mmio", n.loc);
            }
        }
        if let Some(k) = n.untouched {
            kassert!(raw::cfg_read_check(n, space, 4, 2, 1).is_ok(), "{}: header read of an untouched function", n.loc);
            kassert!(raw::cfg_read_check(n, space, raw::HEADER as u64, 4, 1) == Err(Refusal::Untouched(k)), "{}: read past the header", n.loc);
            kassert!(raw::cfg_write_check(&ns, i, space, 4, 2, true) == Err(Refusal::Untouched(k)), "{}: untouched write", n.loc);
            kassert!(raw::mmio_check(n, 0, 0, 4, 1, false) == Err(Refusal::Untouched(k)), "{}: untouched mmio", n.loc);
        }
        if n.is_bridge() && n.untouched.is_none() && raw::storage_below(&ns, i) {
            kassert!(raw::cfg_write_check(&ns, i, space, 4, 2, true) == Err(Refusal::StoragePath), "{}: bridge above storage", n.loc);
        }
        if raw::is_amd_display(n) {
            dcn_rules(n)?;
        }
        if n.untouched.is_none() && !n.is_storage() && !(n.is_bridge() && raw::storage_below(&ns, i)) {
            kassert!(raw::cfg_write_check(&ns, i, space, 0x10, 4, true) == Err(Refusal::Addresses), "{}: BAR0 write", n.loc);
            for c in n.caps.iter().filter(|c| c.id == crate::caps::id::MSI || c.id == crate::caps::id::MSIX) {
                kassert!(raw::cfg_write_check(&ns, i, space, c.offset as u64 + 2, 2, true) == Err(Refusal::Interrupts), "{}: MSI control", n.loc);
            }
        }
    }
    let gpu = amd_gpu();
    kassert!(raw::cfg_write_check(core::slice::from_ref(&gpu), 0, gpu.space, 0x24, 4, true) == Err(Refusal::Addresses), "the Radeon's BAR5 never moves");
    dcn_rules(&gpu)
}

fn dcn_rules(gpu: &Node) -> R {
    let lock = 0x14128; // OTG0 master update lock (spec/gpu.md §2.4 X1)
    kassert!(raw::mmio_check(gpu, 5, lock, 4, 1, false).is_ok(), "{}: DCN read", gpu.loc);
    kassert!(raw::mmio_check(gpu, 5, lock, 4, 1, true).is_ok(), "{}: DCN write (no switch, R8c)", gpu.loc);
    for off in [0x38u64, 0x3C, 0x48, 0x54, 0x74] {
        kassert!(raw::mmio_check(gpu, 5, off, 4, 1, false) == Err(Refusal::IndexPair), "{}: index pair {off:#x}", gpu.loc);
    }
    kassert!(raw::mmio_check(gpu, 5, 0x30, 8, 2, false) == Err(Refusal::IndexPair), "a read running into the pair");
    kassert!(raw::mmio_check(gpu, 4, 0, 4, 1, false) == Err(Refusal::IoBar), "I/O BAR");
    Ok(())
}

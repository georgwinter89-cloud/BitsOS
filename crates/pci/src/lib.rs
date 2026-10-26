//! PCI and the driver registry (owner: PC; contract `C: PciFunction, scan, devices, pci_driver!` with `Match`
//! and `opt_in_key`, kept as G0 defined it and extended).
//!
//! PC-1: configuration access over ECAM from CP-2's MCFG (every segment and bus range; 0xCF8 only as the
//! fallback when there is no MCFG, with an event), a walk through the bridges as the firmware numbered them
//! (`enumerate`), multi-function devices, vendor 0xFFFF and 0x0000 both absent, both capability lists
//! (`caps`), BAR sizing with decoding off (`bars`), the device tree for `hw_info` (`tree`, `tree_lines`),
//! the untouched rule (R8d, `optin`) and mass storage only to the NVMe driver (R8a, `registry`).
//! PC-2: `enable_device(dma)` with D0, stale MSI cleared and bus master on every bridge up to the root, and
//! `disable_device` for `Quiesce` (`enable`, `device`). PC-4: one driver per function in a defined order,
//! probe/remove with the per-function record freed on remove (`bind`), a rescan that never re-sizes BARs of
//! functions already in the tree (`rescan`), mass storage written only for `nvme` also on the raw write
//! paths, the driver in the hw_info tree and a `/sys/bus/pci`-like view (`sysfs`). PC-5: ASPM L1.2 off for
//! a link (`aspm`; `enable_device` does it for the I226 family), Device Control 2, Link Control 2, Slot and
//! Root Control in the saved copy, and the guards and accesses behind the raw MCP tools `pci_read`,
//! `pci_write`, `mmio_read`, `mmio_write` (`raw`; the handlers sit in `crates/mcp/src/tools/pc.rs`). PC-3:
//! MSI/MSI-X on CP's vector allocator (`PciFunction::alloc_irqs` -> `IrqSet`: MSI-X, MSI or polled, INTx off,
//! affinity, storm guard, IRQ -> thread wake; `msi` is the capability logic, `irq` the kernel side, `hal::msi`
//! the message and the per-vector dispatch).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod access;
pub mod aspm;
mod aspm_kernel;
pub mod bars;
pub mod bind;
pub mod caps;
pub mod device;
pub mod ecam;
pub mod enable;
pub mod enumerate;
pub mod guardview;
pub mod irq;
pub mod msi;
pub mod node;
pub mod optin;
pub mod raw;
pub mod raw_kernel;
pub mod registry;
pub mod rescan;
pub mod sysfs;
pub mod tree;

mod ktest_bind;
mod ktest_enable;
mod ktest_msi;
mod ktest_pc5;
mod ktests;
#[cfg(test)]
mod tests;

pub use access::{ConfigAccess, Loc};
pub use aspm::{AspmError, L12Report};
pub use device::{dma_enabled, Enabled};
pub use enable::{Disabled, DmaStatus, EnableError, Saved};
pub use node::{Bar, Bars, BridgeInfo, Node, PcieInfo, Problem};
pub use bind::{bind_drivers, binding, bindings, probe, remove, BindError, BindState, Binding};
pub use irq::{Affinity, IrqError, IrqHandle, IrqHook, IrqMode, IrqRequest, IrqSet, IrqStats, StormLimit, Wake};
pub use registry::{set_config, DriverEntry, Match, DRIVERS};
pub use rescan::Rescan;

#[doc(hidden)]
pub use kreg as __kreg;

use access::PortCf8;
use alloc::string::String;
use alloc::vec::Vec;
use enumerate::Root;
use ksync::{Once, OnceRef, SpinLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Address {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl core::fmt::Display for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.dev, self.func)
    }
}

/// Serialises configuration access: the 0xCF8/0xCFC pair must never be split, and BAR sizing must not
/// interleave with anyone's access to the same function. Lock order: pci.cfg > pci.ecam > klog > frames.
static CFG_LOCK: SpinLock<()> = SpinLock::named("pci.cfg", ());

static PORT_CF8: PortCf8 = PortCf8;
/// The back end chosen by `scan` (ECAM when the firmware has an MCFG); 0xCF8 before that.
static ACCESS: OnceRef<dyn ConfigAccess> = OnceRef::new();
static ECAM: Once<ecam::EcamSpace> = Once::new();
static GUARD_VIEW: guardview::GuardView = guardview::GuardView;

/// The device tree of the last scan.
pub(crate) static NODES: SpinLock<Vec<Node>> = SpinLock::named("pci.devices", Vec::new());
static SCANNED: Once<()> = Once::new();
/// The root bus ranges `scan` walked (a rescan walks the same).
static ROOTS: Once<Vec<Root>> = Once::new();
/// One rescan at a time; a second caller gets `Busy` instead of waiting (B4).
static RESCANNING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn access() -> &'static dyn ConfigAccess {
    ACCESS.get().unwrap_or(&PORT_CF8)
}

/// How configuration space is reached: "ecam" or "cf8" (before `scan`: "cf8").
pub fn method() -> &'static str {
    access().name()
}

/// One PCI function (contract since G0; PC-1 adds `segment`, `subsystem`, the sized BARs and the untouched
/// key; nobody outside this crate builds one, `Node::function` does).
#[derive(Clone, Debug)]
pub struct PciFunction {
    pub addr: Address,
    pub vendor: u16,
    pub device: u16,
    /// class << 16 | subclass << 8 | prog-if
    pub class: u32,
    pub revision: u8,
    pub header_type: u8,
    pub segment: u16,
    pub subsystem: (u16, u16),
    bars: Option<Bars>,
    untouched: Option<&'static str>,
    /// Configuration space the function has (256 for a conventional function even on ECAM).
    space: u16,
    /// MSI and MSI-X capability offsets (0 = none), kept so `disable_device` needs no list walk or heap.
    msi_cap: u8,
    msix_cap: u8,
}

impl PciFunction {
    pub fn loc(&self) -> Loc {
        Loc::new(self.segment, self.addr.bus, self.addr.dev, self.addr.func)
    }

    pub fn read32(&self, off: u8) -> u32 {
        let _g = CFG_LOCK.lock();
        access().read32(self.loc(), off as u16)
    }
    /// Refused (with a Warn event) on a function left untouched by its opt-in key (R8d).
    pub fn write32(&self, off: u8, v: u32) {
        self.write32_ext(off as u16, v);
    }
    pub fn read16(&self, off: u8) -> u16 {
        (self.read32(off & !3) >> ((off & 2) * 8)) as u16
    }
    pub fn read8(&self, off: u8) -> u8 {
        (self.read32(off & !3) >> ((off & 3) * 8)) as u8
    }
    pub fn command(&self) -> u16 {
        self.read16(4)
    }
    /// Sets the command register's low 16 bits (status bits are write-1-to-clear, so they are written 0).
    pub fn set_command(&self, v: u16) {
        self.write32(4, v as u32);
    }
    pub fn class_code(&self) -> u8 {
        (self.class >> 16) as u8
    }

    /// Bytes of configuration space reachable: 4096 with ECAM for a PCIe function, 256 through 0xCF8 or for
    /// a conventional function.
    pub fn config_size(&self) -> u16 {
        access().space_size(self.segment, self.addr.bus).min(self.space)
    }
    /// A dword anywhere in the (extended) configuration space; `None` past `config_size`.
    pub fn read32_ext(&self, off: u16) -> Option<u32> {
        if off >= self.config_size() {
            return None;
        }
        let _g = CFG_LOCK.lock();
        Some(access().read32(self.loc(), off))
    }
    /// Writes a dword; false when refused or out of reach. Refused: a function left untouched by its key
    /// (R8d), and mass storage unless the NVMe driver is probing or bound to it (R8a; the same rule as
    /// `enable_device`, now also on this raw path and `set_command`).
    pub fn write32_ext(&self, off: u16, v: u32) -> bool {
        if let Some(key) = self.untouched {
            klog::event!(Warn, "pci", "write.refused", function = alloc::format!("{}", self.loc()), offset = klog::Hex(off as u64), key = key;
                "function is untouched by its key (R8d)");
            return false;
        }
        if self.class >> 16 == 0x01 && device::driver_of_loc(self.loc()) != Some(optin::STORAGE_DRIVER) {
            klog::event!(Warn, "pci", "write.refused", function = alloc::format!("{}", self.loc()), offset = klog::Hex(off as u64), reason = "storage";
                "mass storage is written only for the NVMe driver (R8a)");
            return false;
        }
        let ok = {
            let _g = CFG_LOCK.lock();
            access().write32(self.loc(), off, v)
        };
        if off & !3 == 4 && v & enable::CMD_MASTER as u32 == 0 {
            device::note_master_off(self.loc());
        }
        ok
    }

    /// Standard capability list (offset, id) pairs, loop- and pointer-checked.
    pub fn capabilities(&self) -> Vec<(u8, u8)> {
        let _g = CFG_LOCK.lock();
        let c = access();
        let status = access::read16(c, self.loc(), 6);
        caps::walk_std(c, self.loc(), status, &mut Vec::new())
    }
    /// Extended capability list (offset, id); empty without ECAM or for a conventional function.
    pub fn ext_capabilities(&self) -> Vec<(u16, u16)> {
        let space = self.config_size();
        let _g = CFG_LOCK.lock();
        caps::walk_ext(access(), self.loc(), space, &mut Vec::new()).into_iter().map(|(o, i, _)| (o, i)).collect()
    }
    pub fn find_capability(&self, id: u8) -> Option<u8> {
        self.capabilities().into_iter().find(|&(_, i)| i == id).map(|(o, _)| o)
    }
    pub fn find_ext_capability(&self, id: u16) -> Option<u16> {
        self.ext_capabilities().into_iter().find(|&(_, i)| i == id).map(|(o, _)| o)
    }

    /// BAR `i` with its size, as sized at scan time (sizing switches decoding off; it is not repeated while
    /// a driver runs). `None` for an empty or unassigned slot, the upper half of a 64-bit BAR, a BAR past the
    /// CPU's physical address bits, or an untouched function. A function the scan did not size (it was in
    /// D3hot) is sized here, once, when it is in D0 by then.
    pub fn bar(&self, i: u8) -> Option<Bar> {
        if i > 5 || self.untouched.is_some() {
            return None;
        }
        // `enable_device` may have sized it after a wake since this copy was made.
        let known = self.bars.or_else(|| NODES.lock().iter().find(|n| n.loc == self.loc()).and_then(|n| n.bars));
        let bars = match known {
            Some(b) => b,
            None => {
                let b = {
                    let _g = CFG_LOCK.lock();
                    let c = access();
                    if bars::power_state(c, self.loc()).is_some_and(|d| d != 0) {
                        return None;
                    }
                    let keep = self.class >> 8 == 0x0600;
                    bars::size_bars(c, self.loc(), self.header_type, keep, phys_limit(), &mut Vec::new())
                };
                let mut nodes = NODES.lock();
                if let Some(n) = nodes.iter_mut().find(|n| n.loc == self.loc()) {
                    n.bars = Some(b);
                }
                b
            }
        };
        bars[i as usize].filter(Bar::assigned)
    }
    /// All six slots as sized (`None` if the function was not sized: untouched, or still in D3hot).
    pub fn bars(&self) -> Option<Bars> {
        self.bars.or_else(|| NODES.lock().iter().find(|n| n.loc == self.loc()).and_then(|n| n.bars))
    }
    /// The CONFIG key that keeps this function untouched (R8d), if any.
    pub fn untouched(&self) -> Option<&'static str> {
        self.untouched
    }

    /// G0 name of `enable_device` (kept so callers compile; the result is logged by `enable_device`).
    /// New code calls `enable_device` and handles its error.
    pub fn enable(&self, dma: bool) {
        let _ = self.enable_device(dma);
    }

    /// Bus master off only (G0; quiesce-safe like `disable_device`, which also switches MSI/MSI-X off).
    pub fn disable_bus_master(&self) {
        let _ = self.disable_with(0, 0);
    }
}

impl Node {
    /// The contract view of this node. Crate-private: a `PciFunction` comes only from the scanned tree
    /// (`devices`), so nobody can forge one with the untouched key cleared.
    pub(crate) fn function(&self) -> PciFunction {
        PciFunction {
            addr: Address { bus: self.loc.bus, dev: self.loc.dev, func: self.loc.func },
            vendor: self.vendor,
            device: self.device,
            class: self.class,
            revision: self.revision,
            header_type: self.header_type,
            segment: self.loc.segment,
            subsystem: self.subsystem,
            bars: self.bars,
            untouched: self.untouched,
            space: self.space,
            msi_cap: self.cap(caps::id::MSI).map_or(0, |c| c.offset),
            msix_cap: self.cap(caps::id::MSIX).map_or(0, |c| c.offset),
        }
    }
}

/// The whole pipeline over any back end: walk, untouched marking, BAR sizing (touched functions only),
/// MSI-X placement check. Host tests run it on a fake configuration space; `scan` on the machine.
pub fn build(c: &dyn ConfigAccess, roots: &[Root], key_for: impl Fn(&Node) -> Option<&'static str>) -> Vec<Node> {
    finish(c, enumerate::enumerate(c, roots), key_for, hal::mmio::phys_limit(0))
}

/// Everything after the walk: untouched marking, BAR sizing (not for untouched functions, not in D3hot),
/// MSI-X placement. `phys_limit`: first address past the CPU's physical address bits.
pub fn finish(c: &dyn ConfigAccess, mut nodes: Vec<Node>, key_for: impl Fn(&Node) -> Option<&'static str>, phys_limit: u64) -> Vec<Node> {
    optin::mark_untouched(&mut nodes, key_for);
    for n in nodes.iter_mut() {
        rescan::size_node(c, n, phys_limit);
    }
    nodes
}

/// Walks until no function answers "not ready" or the deadline passed (`rescan::crs_loop`). The lock is
/// held per walk only; between walks the thread sleeps (B4). Returns the tree and the functions still "not
/// ready" at the deadline (a rescan holds those instead of treating them as gone).
fn walk_until_ready(c: &dyn ConfigAccess, roots: &[Root]) -> (Vec<Node>, Vec<Loc>) {
    let mut walk = || {
        let _g = CFG_LOCK.lock();
        enumerate::walk(c, roots)
    };
    let mut wait = |ns: u64, not_ready: &[Loc]| {
        if let Some(first) = not_ready.first() {
            klog::event!(Debug, "pci", "crs.wait", functions = not_ready.len(), first = alloc::format!("{first}"), wait_ms = ns / 1_000_000;
                "functions answer 'not ready' (CRS): walking again shortly");
        }
        sched::sleep_ns(ns);
    };
    let r = rescan::crs_loop(&mut walk, &time::now_ns, time::tsc_hz() != 0, &mut wait);
    let waited_ms = r.waited_ns / 1_000_000;
    for at in &r.timed_out {
        klog::event!(Warn, "pci", "crs.timeout", function = alloc::format!("{at}"), waited_ms = waited_ms;
            "function still answers 'not ready' (vendor 0x0001): left out of the tree");
    }
    if r.waits > 0 && r.timed_out.is_empty() {
        klog::event!(Info, "pci", "crs.ready", waited_ms = waited_ms, walks = r.waits + 1);
    }
    (r.nodes, r.timed_out)
}

/// First physical address past the CPU's address bits (set by `scan`; 2^52 before).
static PHYS_LIMIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1 << 52);

fn phys_limit() -> u64 {
    PHYS_LIMIT.load(core::sync::atomic::Ordering::Acquire)
}

/// Picks the configuration back end from the MCFG and returns the root bus ranges to walk.
fn choose_access() -> (&'static dyn ConfigAccess, Vec<Root>) {
    let legacy = || -> (&'static dyn ConfigAccess, Vec<Root>) { (&PORT_CF8, alloc::vec![Root { segment: 0, bus_start: 0, bus_end: 255 }]) };
    let Some(t) = acpi::tables() else {
        klog::event!(Warn, "pci", "ecam.absent", reason = "acpi_not_read", fallback = "cf8";
            "ACPI tables were not read: PCI uses 0xCF8, no extended configuration space");
        return legacy();
    };
    if t.mcfg.is_empty() {
        klog::event!(Warn, "pci", "ecam.absent", reason = "no_mcfg", fallback = "cf8";
            "the firmware has no MCFG: PCI uses 0xCF8, no extended configuration space");
        return legacy();
    }
    let (usable, refused) = usable_windows(&t.mcfg, hal::cpu::facts().phys_addr_bits);
    for w in refused {
        klog::event!(Warn, "pci", "ecam.window_refused", segment = w.segment, base = klog::Hex(w.base), bus_start = w.bus_start, bus_end = w.bus_end;
            "MCFG entry past the CPU's physical address bits or not 1 MiB aligned");
    }
    let space = ECAM.get_or_init(|| ecam::EcamSpace::new(&usable));
    if space.windows().is_empty() {
        klog::event!(Warn, "pci", "ecam.absent", reason = "no_usable_window", fallback = "cf8");
        return legacy();
    }
    // Segment 0 can be checked against the legacy mechanism: the host bridge must read the same through both.
    if space.space_size(0, 0) != 0 {
        let at = Loc::new(0, 0, 0, 0);
        let (e, p) = {
            let _g = CFG_LOCK.lock();
            (space.read32(at, 0), PORT_CF8.read32(at, 0))
        };
        if e != p {
            klog::event!(Warn, "pci", "ecam.mismatch", ecam = klog::Hex(e as u64), cf8 = klog::Hex(p as u64), fallback = "cf8";
                "00:00.0 reads differently through ECAM and 0xCF8: ECAM not used");
            return legacy();
        }
    }
    for w in space.windows() {
        klog::event!(Info, "pci", "ecam.window", segment = w.segment, bus_start = w.bus_start, bus_end = w.bus_end, base = klog::Hex(w.base));
    }
    let roots = space.windows().iter().map(|w| Root { segment: w.segment, bus_start: w.bus_start, bus_end: w.bus_end }).collect();
    (space, roots)
}

/// MCFG entries the kernel can use (inside the CPU's physical address bits, 1 MiB aligned) and the rest.
/// CP-2's parser already dropped entries with address 0 or a backwards bus range.
pub fn usable_windows(mcfg: &[acpi::mcfg::Ecam], phys_bits: u32) -> (Vec<acpi::mcfg::Ecam>, Vec<acpi::mcfg::Ecam>) {
    let limit = if (1..64).contains(&phys_bits) { 1u64 << phys_bits } else { 1u64 << 52 };
    mcfg.iter().partition(|w| {
        let end = w.base.checked_add(((w.bus_end as u64) + 1) << 20);
        end.is_some_and(|e| e <= limit) && w.base & 0xF_FFFF == 0 && w.bus_start <= w.bus_end
    })
}

/// Scans every segment and bus range (once; a second call keeps the tree: BARs of running devices must not
/// be sized again). Returns the number of functions.
pub fn scan() -> usize {
    if SCANNED.get().is_some() {
        let n = NODES.lock().len();
        klog::event!(Warn, "pci", "scan.again", functions = n; "the tree was scanned already; kept");
        return n;
    }
    SCANNED.get_or_init(|| ());
    // hal's single 0xCF8 writer belongs to this crate: claim it before anyone else can.
    let _ = access::cf8_writer();
    let limit = hal::mmio::phys_limit(hal::cpu::facts().phys_addr_bits);
    PHYS_LIMIT.store(limit, core::sync::atomic::Ordering::Release);
    let (c, roots) = choose_access();
    ACCESS.set(c);
    let roots = ROOTS.get_or_init(|| roots);
    let (walked, _) = walk_until_ready(c, roots);
    let nodes = {
        let _g = CFG_LOCK.lock();
        finish(c, walked, |n| registry::untouched_key(n.vendor, n.device, n.class), limit)
    };
    log_tree(&nodes, c.name());
    let n = nodes.len();
    *NODES.lock() = nodes;
    acpi::set_pci_view(&GUARD_VIEW);
    n
}

fn log_tree(nodes: &[Node], method: &str) {
    for n in nodes {
        log_node(n, nodes);
    }
    let bridges = nodes.iter().filter(|n| n.is_bridge()).count();
    let untouched = nodes.iter().filter(|n| n.untouched.is_some()).count();
    let problems: usize = nodes.iter().map(|n| n.problems.len()).sum();
    klog::event!(Info, "pci", "scan", functions = nodes.len(), bridges = bridges, untouched = untouched, problems = problems, method = method);
}

/// The boot lines of one function (Linux's "pci 0000:00:1f.0: [8086:2918] type 00 class 0x060100").
fn log_node(n: &Node, nodes: &[Node]) {
    {
        let f = alloc::format!("{}", n.loc);
        let parent = n.parent.map(|p| alloc::format!("{}", nodes[p].loc)).unwrap_or_else(|| String::from("root"));
        klog::event!(Info, "pci", "function", function = f.as_str(), vendor = klog::Hex(n.vendor as u64), device = klog::Hex(n.device as u64),
            class = klog::Hex(n.class as u64), header = n.header_type & 0x7F, parent = parent.as_str());
        if let Some(b) = n.bridge {
            klog::event!(Info, "pci", "bridge", function = f.as_str(), primary = b.primary, secondary = b.secondary, subordinate = b.subordinate);
        }
        for (i, b) in n.bars.iter().flat_map(|b| b.iter().enumerate()) {
            let Some(b) = *b else { continue };
            let (kind, prefetch, is64) = match b {
                Bar::Mem { prefetch, is64, .. } => ("mem", prefetch, is64),
                Bar::Io { .. } => ("io", false, false),
            };
            klog::event!(Debug, "pci", "bar", function = f.as_str(), index = i, kind = kind, start = klog::Hex(b.start()), size = klog::Hex(b.size()),
                prefetch = prefetch, is64 = is64);
        }
        if let Some(k) = n.untouched {
            klog::event!(Info, "pci", "untouched", function = f.as_str(), key = k; "left untouched by its key (R8d: wlan=on etc. releases it; net_drivers=off holds network functions)");
        }
        for p in &n.problems {
            klog::event!(Warn, "pci", p.name(), function = f.as_str(), detail = alloc::format!("{p:?}"));
        }
    }
}

/// Every function of the last scan (contract).
pub fn devices() -> Vec<PciFunction> {
    NODES.lock().iter().map(Node::function).collect()
}

/// Reads one register of AMD's System Management Network through the root complex 00:00.0's index/data
/// pair (config 0x60/0x64), holding `pci.cfg` across both steps so no other configuration access splits the
/// pair (linux-6.6/arch/x86/kernel/amd_nb.c:174-215, `amd_smn_read` under `smn_mutex`). `None` when 00:00.0
/// is not AMD's root complex (QEMU's q35 has Intel's) or the index write fails. RB-7b CR1.
pub fn smn_read(addr: u32) -> Option<u32> {
    let root = Loc::new(0, 0, 0, 0);
    if function(root)?.vendor != 0x1022 {
        return None;
    }
    let _g = CFG_LOCK.lock();
    if !access().write32(root, 0x60, addr) {
        return None;
    }
    Some(access().read32(root, 0x64))
}

/// One function of the tree by its place.
pub fn function(at: Loc) -> Option<PciFunction> {
    NODES.lock().iter().find(|n| n.loc == at).map(Node::function)
}

/// ktests: holds the one-rescan flag as a running rescan would (false when one is running).
pub(crate) fn rescan_flag_try_hold() -> bool {
    use core::sync::atomic::Ordering;
    RESCANNING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

pub(crate) fn rescan_flag_release() {
    RESCANNING.store(false, core::sync::atomic::Ordering::Release);
}

struct RescanFlag;

impl Drop for RescanFlag {
    fn drop(&mut self) {
        RESCANNING.store(false, core::sync::atomic::Ordering::Release);
    }
}

/// Walks every root range again (after `scan`). Functions found again keep their node and BARs (never
/// sized again: decoding would go off under a running driver); new functions get the untouched rule, BAR
/// sizing and a driver; functions that are gone lose their driver (remove hook, no write to the absent
/// device) and their record. Sleeps between walks while functions answer "not ready" (B4). One rescan at a
/// time: a second caller gets `Busy` at once. May sleep; never call it with a lock held.
pub fn rescan() -> Result<Rescan, BindError> {
    use core::sync::atomic::Ordering;
    if SCANNED.get().is_none() {
        return Err(BindError::NotScanned);
    }
    if RESCANNING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        klog::event!(Info, "pci", "rescan.busy"; "a rescan is already running");
        return Err(BindError::Busy);
    }
    let _flag = RescanFlag;
    let c = access();
    let roots = ROOTS.get().cloned().unwrap_or_default();
    let (walked, not_ready) = walk_until_ready(c, &roots);
    let old = NODES.lock().clone();
    let m = {
        let _g = CFG_LOCK.lock();
        rescan::merge(c, &old, walked, &not_ready, |n| registry::untouched_key(n.vendor, n.device, n.class), phys_limit())
    };
    for h in &m.held {
        klog::event!(Warn, "pci", "rescan.uncertain", function = alloc::format!("{h}");
            "not ready at the deadline (or behind a function that was): kept as it was, driver stays");
    }
    for &i in &m.added {
        log_node(&m.nodes[i], &m.nodes);
    }
    let added: Vec<Loc> = m.added.iter().map(|&i| m.nodes[i].loc).collect();
    {
        let mut cur = NODES.lock();
        let mut nodes = m.nodes;
        // A function `enable_device` sized after a wake while we walked keeps those BARs.
        for n in nodes.iter_mut().filter(|n| n.bars.is_none()) {
            if let Some(now) = cur.iter().find(|c| rescan::same(c, n)) {
                n.bars = now.bars;
            }
        }
        *cur = nodes;
    }
    // After the tree no longer has them, so no new probe can claim them; a probe already running is marked
    // gone and removes again when it ends. Children before the bridge that carries them (Linux
    // `pci_stop_and_remove_bus_device`): the old tree is in walk order, parents first.
    for g in rescan::removal_order(&m.gone) {
        klog::event!(Warn, "pci", "function.gone", function = alloc::format!("{}", g.loc), vendor = klog::Hex(g.vendor as u64),
            device = klog::Hex(g.device as u64));
        bind::remove_gone(&g.function());
    }
    let bound = added.iter().filter(|&&at| bind::probe(at).is_ok()).count();
    let r = Rescan { gone: m.gone.iter().map(|g| g.loc).collect(), held: m.held, added, kept: m.kept, sized: m.sized, bound };
    klog::event!(Info, "pci", "rescan", added = r.added.len(), gone = r.gone.len(), held = r.held.len(), kept = r.kept, sized = r.sized,
        bound = r.bound);
    Ok(r)
}

/// The device tree of the last scan, with bridges, capabilities, BARs and problems (`hw_info`, sysfs).
pub fn tree() -> Vec<Node> {
    NODES.lock().clone()
}

/// The device tree as text lines for `hw_info`'s PCI section, with the driver bound to each function.
pub fn tree_lines() -> Vec<String> {
    // pci.devices and pci.state are both leaves: never one under the other.
    let nodes = NODES.lock().clone();
    tree::lines_with(&nodes, method(), &bind::driver_note)
}

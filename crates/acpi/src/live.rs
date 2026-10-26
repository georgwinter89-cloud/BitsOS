//! The kernel side: the tables read once at boot, kept for every lane, and the register guard other lanes
//! call before writing to an address a table names (CP-5's reboot and power-off, CP-3's I/O APICs, ...).

use crate::guard::{Guard, PciSnapshot, PciView, Refusal};
use crate::kmem::KernelMem;
use crate::kpci::PortPci;
use crate::memmap::MemMap;
use crate::sdt::{Gas, Space};
use crate::{parse, report, Acpi};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use hal::mmio::IoMem;
use klog::Hex;
use ksync::{IrqSpinLock, Once, OnceRef};

struct State {
    acpi: Acpi,
    map: MemMap,
    phys_bits: u32,
    /// PM1 control's SCI_EN bit as the firmware left it (`None`: no PM1 control or not readable). Part of the
    /// state so `tables()` and `sci_en_at_boot()` become visible together.
    sci_at_boot: Option<bool>,
}

static STATE: Once<State> = Once::new();
/// BootInfo's AP trampoline record, kept for CP-4's bring-up (`platform`), which runs as an initcall.
static TRAMPOLINE: Once<Option<bootinfo::Trampoline>> = Once::new();
static PCI_VIEW: OnceRef<dyn PciView> = OnceRef::new();
/// What the guard knows about PCI; replaced whole (never edited) so a check holds the lock only to clone.
static PCI: IrqSpinLock<Option<Arc<PciSnapshot>>> = IrqSpinLock::named("acpi.pci", None);

/// Reads the firmware's tables (kernel start, after `hal::mmio::init`, before the initcalls). A second call
/// changes nothing.
pub fn init(bi: &bootinfo::BootInfo<'_>) {
    if STATE.get().is_some() {
        klog::event!(Warn, "acpi", "init.again"; "tables were read already; kept");
        return;
    }
    let map = MemMap::from_bootinfo(bi);
    let phys_bits = hal::cpu::facts().phys_addr_bits;
    // The one 0xCF8 scan without PC's lock: one CPU, interrupts off, no initcall has run yet.
    let snap = pci_snapshot().unwrap_or_else(|| {
        let s = Arc::new(PciSnapshot::take(PCI_VIEW.get().unwrap_or(&PortPci)));
        *PCI.lock() = Some(s.clone());
        s
    });
    klog::event!(Debug, "acpi", "pci.snapshot", storage_bars = snap.storage_count().map(|n| n as i64).unwrap_or(-1));
    let acpi = match bi.rsdp() {
        Some(r) => {
            let mut mem = KernelMem::new(&map, phys_bits, &*snap);
            let a = parse::parse(&mut mem, r.phys);
            klog::event!(Debug, "acpi", "read", reserved_pages = mem.windows());
            a
        }
        None => {
            klog::event!(Warn, "acpi", "absent"; "BootInfo has no RSDP: no ACPI tables, reboot/power-off use the fallbacks");
            Acpi::default()
        }
    };
    // Read before anyone writes ACPI_ENABLE: CP-5 enables ACPI mode only when the firmware left it off.
    let sci = {
        let gd = Guard { map: &map, phys_bits, ecam: &acpi.mcfg, pci: &*snap };
        read_sci_en(&gd, &acpi)
    };
    let st = STATE.get_or_init(|| State { acpi, map, phys_bits, sci_at_boot: sci });
    report::log(&st.acpi, st.sci_at_boot);
    // CP-5: the HPET started and the TSC calibrated against it (the guard knows the map now), the RTC read.
    crate::clock::boot(&st.acpi, bi);
    TRAMPOLINE.get_or_init(|| bi.trampoline());
    // Every per-CPU container is sized for the CPUs the MADT lists before the first one is used (sched::init
    // comes after this); CP-4 starts them later.
    let cpus = st.acpi.madt.as_ref().map_or(1, |m| m.enabled_cpus());
    hal::percpu::set_possible(cpus);
    klog::event!(Debug, "acpi", "cpus.possible", cpus = hal::percpu::possible_cpus());
}

/// BootInfo's AP trampoline record (`None` before `init` or when bitsboot gave none).
pub fn trampoline() -> Option<bootinfo::Trampoline> {
    TRAMPOLINE.get().copied().flatten()
}

/// The tables, once `init` ran.
pub fn tables() -> Option<&'static Acpi> {
    STATE.get().map(|s| &s.acpi)
}

/// The firmware memory map `init` was given.
pub fn memory_map() -> Option<&'static MemMap> {
    STATE.get().map(|s| &s.map)
}

/// SCI_EN as the firmware handed over (Mini-PC: 0, legacy mode; spec/core.md §4).
pub fn sci_en_at_boot() -> Option<bool> {
    STATE.get().and_then(|s| s.sci_at_boot)
}

/// Lines for `hw_info`'s firmware section (MC registers the tool).
pub fn summary() -> alloc::vec::Vec<alloc::string::String> {
    match STATE.get() {
        Some(s) => report::summary(&s.acpi, sci_en_at_boot()),
        None => alloc::vec![alloc::string::String::from("acpi: not read (acpi::init was not called)")],
    }
}

/// PC hands over its own view of PCI (device list, real BAR sizes, its lock). First call wins; false if one
/// was set. The guard's snapshot is retaken through it at once.
pub fn set_pci_view(v: &'static dyn PciView) -> bool {
    if !PCI_VIEW.set(v) {
        return false;
    }
    refresh_pci();
    true
}

/// Retakes the guard's PCI snapshot through PC's view (after PC moved BARs or found new disk
/// controllers). False without a view: the boot scan through 0xCF8 is not repeated without PC's lock, and
/// the snapshot from boot stays.
pub fn refresh_pci() -> bool {
    let Some(v) = PCI_VIEW.get() else {
        klog::event!(Warn, "acpi", "pci.refresh_refused"; "no PCI view registered; the boot snapshot stays");
        return false;
    };
    let s = Arc::new(PciSnapshot::take(v));
    let n = s.storage_count();
    *PCI.lock() = Some(s);
    klog::event!(Info, "acpi", "pci.snapshot", storage_bars = n.map(|n| n as i64).unwrap_or(-1), from = "pci view");
    true
}

fn pci_snapshot() -> Option<Arc<PciSnapshot>> {
    PCI.lock().clone()
}

/// The guard over the boot state and the current PCI snapshot. Before `init` (or without a snapshot) it
/// knows no memory map and no PCI and refuses everything that needs them (fail closed).
fn with_guard<R>(f: impl FnOnce(&Guard<'_>) -> R) -> R {
    static EMPTY: Once<MemMap> = Once::new();
    let snap = pci_snapshot().unwrap_or_else(|| Arc::new(PciSnapshot::unknown()));
    let (map, bits, ecam): (&MemMap, u32, &[crate::mcfg::Ecam]) = match STATE.get() {
        Some(s) => (&s.map, s.phys_bits, &s.acpi.mcfg),
        None => (EMPTY.get_or_init(MemMap::default), 0, &[]),
    };
    f(&Guard { map, phys_bits: bits, ecam, pci: &*snap })
}

fn refused(r: Result<(), Refusal>, space: Space, addr: u64, bytes: u32) -> Result<(), Refusal> {
    if let Err(e) = r {
        klog::event!(Warn, "acpi", "guard.refused", space = space.name(), addr = Hex(addr), bytes = bytes, reason = e.name();
            "the register guard refused this address");
    }
    r
}

/// May `bytes` bytes be written to the register `g`? `allow_cf9`: only for the FADT reset register.
pub fn check_gas(g: &Gas, bytes: u32, allow_cf9: bool) -> Result<(), Refusal> {
    refused(with_guard(|gd| gd.check_gas(g, bytes, allow_cf9)), g.space, g.address, bytes)
}

pub fn check_io(port: u64, bytes: u32, allow_cf9: bool) -> Result<(), Refusal> {
    refused(with_guard(|gd| gd.check_io(port, bytes, allow_cf9)), Space::Io, port, bytes)
}

pub fn check_mem(addr: u64, bytes: u32) -> Result<(), Refusal> {
    refused(with_guard(|gd| gd.check_mem(addr, bytes)), Space::Memory, addr, bytes)
}

/// Reads a register a table names (I/O or memory space, 1-4 bytes, not across a page), after the guard
/// allowed that address. `None` when refused or not readable. Reads have side effects on some devices,
/// hence the guard.
pub fn read_reg(g: &Gas, bytes: u32) -> Option<u32> {
    with_guard(|gd| read_reg_with(gd, g, bytes))
}

fn read_reg_with(gd: &Guard<'_>, g: &Gas, bytes: u32) -> Option<u32> {
    gd.check_gas(g, bytes, false).ok()?;
    match (g.space, bytes) {
        (Space::Io, 1) => hal::portio::inb(g.address as u16).ok().map(u32::from),
        (Space::Io, 2) => hal::portio::inw(g.address as u16).ok().map(u32::from),
        (Space::Io, 4) => hal::portio::inl(g.address as u16).ok(),
        (Space::Memory, 1 | 2 | 4) => read_mem_reg(g.address, bytes),
        _ => None,
    }
}

const PAGE: u64 = 4096;

/// One uncached window per register page, mapped on first use and kept (G0's vmap never unmaps, so a
/// mapping per call would leak); bounded by the distinct pages the tables name.
static REG_WINDOWS: IrqSpinLock<BTreeMap<u64, IoMem>> = IrqSpinLock::named("acpi.reg_windows", BTreeMap::new());

fn read_mem_reg(addr: u64, bytes: u32) -> Option<u32> {
    let page = addr & !(PAGE - 1);
    let off = (addr - page) as usize;
    if off + bytes as usize > PAGE as usize {
        return None;
    }
    let read = |w: &IoMem| match bytes {
        1 => w.read8(off).ok().map(u32::from),
        2 => w.read16(off).ok().map(u32::from),
        _ => w.read32(off).ok(),
    };
    if let Some(w) = REG_WINDOWS.lock().get(&page) {
        return read(w);
    }
    // Mapped outside the lock (it allocates page-table frames); a racing mapper's window is kept, ours dropped.
    let w = match hal::mmio::map_device(page, PAGE as usize, hal::paging::CacheType::Uncached) {
        Ok(w) => w,
        Err(e) => {
            klog::event!(Warn, "acpi", "reg.map_failed", addr = Hex(addr), error = alloc::format!("{e:?}"));
            return None;
        }
    };
    let mut ws = REG_WINDOWS.lock();
    read(ws.entry(page).or_insert(w))
}

/// SCI_EN (bit 0 of PM1a control); `None` on hardware-reduced ACPI or without a readable PM1a control.
fn read_sci_en(gd: &Guard<'_>, a: &Acpi) -> Option<bool> {
    let f = a.fadt.as_ref()?;
    if f.hw_reduced() {
        return None;
    }
    let g = f.pm1a_cnt?;
    let bytes = if f.pm1_cnt_len >= 2 || g.access_bytes() >= 2 { 2 } else { 1 };
    read_reg_with(gd, &g, bytes).map(|v| v & 1 != 0)
}

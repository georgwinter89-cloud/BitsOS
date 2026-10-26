//! CP-3, kernel side: the interrupt controllers (spec/core.md §5.1-5.4, §11.10, §11.13).
//!
//! `init` is the one call the kernel makes (after `time::init` and `acpi::init`, see the CP-3 notes for the
//! exact place); it runs `setup` and then turns interrupts on. `setup` (idempotent, interrupts stay off):
//! 1. the BSP's LAPIC through `hal::apic::init_bsp`: x2APIC when CPUID offers it (`apic=xapic` in CONFIG.TXT
//!    keeps xAPIC), spurious vector 0xFF, LINT0 masked, NMI on the MADT's pin, LVT timer masked (CP-5);
//!    unmasked firmware LVTs (thermal, perf, CMCI, error, AMD extended) are left alone with their vector
//!    reserved, or masked when that vector is not a device vector (0x30-0xEF);
//! 2. both 8259s re-initialised to 0x20-0x2F and masked for good;
//! 3. every I/O APIC from the MADT (address through the register guard): all pins masked except SMI ones;
//! 4. the SCI and, where an 8042 exists, PS/2 keyboard and mouse programmed for the BSP on vectors from
//!    `hal::vectors` (`irq_plan`), **still masked**: a line is unmasked when its owner installs a handler
//!    (`set_handler`), so nothing fires that nobody can clear.
//!
//! The routed lines share one hard-IRQ handler (`route_irq`): it counts, runs the storm guard (more than 100
//! interrupts in 1 ms mask the pin; the owner polls or calls `rearm`, and installing a handler rearms too), then
//! calls the owner's `LineHandler`. A level line's source must be cleared by that handler; the EOI follows it
//! (`hal::vectors::dispatch`). Events from hard-IRQ context are deferred to the `cp.irq.events` poller
//! (`irq.first`, `irq.storm`).

use crate::ioapic::{Entry, IoApic, IoApicError, MaskReport};
use crate::irq_plan::{self, Line, PlanNote, Purpose, StormGuard};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hal::apic::{self, LapicConfig, LapicReport, LvtSnapshot, Mode};
use hal::vectors::{self, Vector};
use klog::Hex;
use ksync::{IrqSpinLock, Once};

/// V1's storm rule: more than 100 interrupts within 1 ms.
pub const STORM_WINDOW_NS: u64 = 1_000_000;
pub const STORM_LIMIT: u32 = 100;
/// Where the 8259s point after the re-init (reserved, below `vectors::FIRST_DYNAMIC`).
pub const PIC_BASE: u8 = 0x20;

/// The owner's part of a routed line (hard-IRQ context, see `hal::vectors`).
pub type LineHandler = fn(Purpose);

pub struct Route {
    pub purpose: Purpose,
    /// SCI_INT or the ISA IRQ.
    pub source: u32,
    pub line: Line,
    /// Index into `State::ioapics`.
    pub ioapic: usize,
    pub pin: u32,
    pub vector: Vector,
    pub dest: u32,
    handler: IrqSpinLock<Option<LineHandler>>,
    count: AtomicU64,
    storm: StormGuard,
    stormed: AtomicBool,
    told_first: AtomicBool,
    told_storm: AtomicBool,
}

impl Route {
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
    /// Whether an owner installed a handler (only then is the pin unmasked).
    pub fn has_handler(&self) -> bool {
        self.handler.lock().is_some()
    }
    pub fn stormed(&self) -> bool {
        self.stormed.load(Ordering::Acquire)
    }
}

pub struct IoApicInfo {
    pub apic: IoApic,
    pub mask: MaskReport,
}

/// What `setup` found and did.
pub struct State {
    pub lapic: LapicReport,
    /// The LVTs after the NMI pin was set from the MADT.
    pub lvt: LvtSnapshot,
    pub ioapics: Vec<IoApicInfo>,
    pub routes: Vec<Route>,
    pub notes: Vec<PlanNote>,
    /// MADT says 8259s exist (or no MADT): re-initialised and masked.
    pub pic_remapped: bool,
    pub pic_masks: (u8, u8),
    /// `acpi::init` had read the tables (no tables: LAPIC only, nothing routed).
    pub tables: bool,
    /// Firmware LVTs left unmasked, with the vector reserved for them.
    pub reserved_lvt: Vec<(&'static str, u8)>,
    /// Firmware LVTs CP masked because their vector is not a device vector (or belongs to someone else).
    pub masked_lvt: Vec<(&'static str, u8)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// The LAPIC cannot be used (no x2APIC and the xAPIC page could not be mapped).
    Lapic(apic::ApicError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqError {
    NotSetUp,
    /// This machine has no such line (e.g. PS/2 on the Mini-PC).
    NotRouted,
}

static STATE: Once<Result<State, SetupError>> = Once::new();

// Metrics (B6). The routed-line ones are bumped in hard-IRQ context (atomic adds); hal's per-CPU vector
// counts are copied over by the `cp.irq.events` poller.
klog::counter!(ROUTED_IRQS, "irq.routed_lines");
klog::counter!(STORMS, "irq.storms");
klog::counter!(DELIVERED, "irq.delivered");
klog::counter!(UNHANDLED, "irq.unhandled");
klog::counter!(APIC_SPURIOUS, "irq.apic_spurious");
static SYNCED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static POLLER: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The state once `setup` ran successfully.
pub fn state() -> Option<&'static State> {
    STATE.get().and_then(|r| r.as_ref().ok())
}

/// Whether `init` turned interrupts on (the kernel's hook ran).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn route(p: Purpose) -> Option<&'static Route> {
    state()?.routes.iter().find(|r| r.purpose == p)
}

/// Sets up the controllers and turns interrupts on for this CPU. The kernel calls it once, after `acpi::init`.
pub fn init(config: &dyn Fn(&str) -> Option<String>) -> Result<&'static State, SetupError> {
    // Before anything can fail: CP-4b's `smp=`/`cpus=` for the `cpu.smp` initcall.
    crate::cpu_keys::capture(config);
    // CP-5: tz=, lapic_timer=, hrtimer= before the boot CPU's timer starts.
    time::configure(config);
    let r = setup(config);
    match r {
        Ok(st) => {
            // CP-5: the boot CPU's LAPIC timer drives its hrtimer queue from here on (logs its own result).
            let _ = time::start_bsp();
            hal::cpu::enable_interrupts();
            if !ENABLED.swap(true, Ordering::AcqRel) {
                klog::event!(Info, "irq", "enabled", mode = st.lapic.mode.name(), routes = st.routes.len(), ioapics = st.ioapics.len());
            }
        }
        Err(e) => klog::event!(Error, "irq", "not_enabled", error = format!("{e:?}"); "interrupts stay off; drivers keep polling"),
    }
    r
}

/// Sets up LAPIC, 8259s and I/O APICs once (later calls return the first result). Interrupts stay as they are.
pub fn setup(config: &dyn Fn(&str) -> Option<String>) -> Result<&'static State, SetupError> {
    let st = STATE.get_or_init(|| build(config)).as_ref().map_err(|e| *e)?;
    if !POLLER.swap(true, Ordering::AcqRel) {
        vectors::register_poller("cp.irq.events", report_deferred);
    }
    Ok(st)
}

fn want_x2apic(config: &dyn Fn(&str) -> Option<String>) -> bool {
    match config("apic").as_deref().map(str::trim) {
        None | Some("") | Some("x2apic") => true,
        Some("xapic") => false,
        Some(other) => {
            klog::event!(Warn, "apic", "config.unknown", value = other; "apic= takes x2apic or xapic; x2apic used");
            true
        }
    }
}

fn build(config: &dyn Fn(&str) -> Option<String>) -> Result<State, SetupError> {
    let _irq = hal::cpu::IrqGuard::new();
    let want = want_x2apic(config);
    let mut lapic = apic::init_bsp(&LapicConfig { want_x2apic: want, nmi: None }).map_err(|e| {
        klog::event!(Error, "apic", "setup.failed", error = format!("{e:?}"); "the LAPIC cannot be used");
        SetupError::Lapic(e)
    })?;
    let acpi = crate::tables();
    let madt = acpi.and_then(|a| a.madt.as_ref());
    let mut notes = Vec::new();
    let nmi = irq_plan::nmi_pin(madt, lapic.id, &mut notes);
    if nmi != lapic.nmi {
        apic::set_nmi_pin(nmi);
        lapic.nmi = nmi;
    }
    let lvt = apic::lvt_snapshot().unwrap_or(lapic.after);
    log_lapic(&lapic, &lvt, want);
    let (reserved_lvt, masked_lvt) = reserve_firmware_lvts(&lvt);
    if let Some(m) = madt
        && m.lapic_address != lapic.base_phys
    {
        klog::event!(Warn, "apic", "base.mismatch", madt = Hex(m.lapic_address), msr = Hex(lapic.base_phys); "IA32_APIC_BASE is used");
    }

    let pic_remapped = madt.is_none_or(|m| m.has_8259());
    if pic_remapped {
        apic::remap_and_mask_pic(PIC_BASE);
    } else {
        apic::quiet_legacy_pic();
    }
    let pic_masks = apic::pic_masks();
    klog::event!(Info, "irq", "pic8259.masked", remapped = pic_remapped, base = Hex(PIC_BASE as u64), imr_master = Hex(pic_masks.0 as u64), imr_slave = Hex(pic_masks.1 as u64));

    let mut st = State { lapic, lvt, ioapics: Vec::new(), routes: Vec::new(), notes, pic_remapped, pic_masks, tables: acpi.is_some(), reserved_lvt, masked_lvt };
    let Some(acpi) = acpi else {
        klog::event!(Warn, "irq", "no_tables"; "acpi::init has not run: no I/O APIC set-up, nothing routed (LAPIC only)");
        return Ok(st);
    };
    if let Some(m) = madt {
        for a in &m.ioapics {
            if let Some(info) = open_ioapic(a.address as u64, a.id, a.gsi_base) {
                st.ioapics.push(info);
            }
        }
    }
    let plan = irq_plan::plan(acpi, hal::cpu::detect().hypervisor);
    st.notes.extend(plan.notes.iter().copied());
    for n in &st.notes {
        log_note(n);
    }
    let dest = st.lapic.id;
    if dest > 0xFF {
        klog::event!(Error, "irq", "dest.unreachable", apic_id = dest; "I/O APIC destinations are 8 bits without interrupt remapping; nothing routed");
        return Ok(st);
    }
    for p in &plan.routes {
        let Some((idx, pin)) = st.ioapics.iter().enumerate().find_map(|(i, a)| a.apic.pin_of(p.line.gsi).map(|pin| (i, pin))) else {
            klog::event!(Warn, "irq", "route.no_pin", purpose = p.purpose.name(), gsi = p.line.gsi; "no usable I/O APIC serves this GSI");
            continue;
        };
        let v = match vectors::alloc(route_irq, Some(0)) {
            Ok(v) => v,
            Err(e) => {
                klog::event!(Error, "irq", "route.no_vector", purpose = p.purpose.name(), error = format!("{e:?}"));
                continue;
            }
        };
        let a = &st.ioapics[idx].apic;
        a.set_entry(pin, Entry::fixed(v.vector, &p.line, dest as u8, true));
        klog::event!(Info, "irq", "route", purpose = p.purpose.name(), source = p.source, gsi = p.line.gsi, ioapic = a.id, pin = pin,
            vector = Hex(v.vector as u64), trigger = if p.line.level { "level" } else { "edge" }, polarity = if p.line.active_low { "low" } else { "high" }, dest = dest,
            masked = true; "unmasked when its owner installs a handler");
        st.routes.push(Route {
            purpose: p.purpose,
            source: p.source,
            line: p.line,
            ioapic: idx,
            pin,
            vector: v,
            dest,
            handler: IrqSpinLock::named("acpi.irq.route", None),
            count: AtomicU64::new(0),
            storm: StormGuard::new(STORM_WINDOW_NS, STORM_LIMIT),
            stormed: AtomicBool::new(false),
            told_first: AtomicBool::new(false),
            told_storm: AtomicBool::new(false),
        });
    }
    Ok(st)
}

fn log_lapic(l: &LapicReport, lvt: &LvtSnapshot, want: bool) {
    klog::event!(Info, "apic", "setup", mode = l.mode.name(), firmware_mode = l.firmware_mode.name(), cpuid_x2apic = l.cpuid_x2apic,
        id = l.id, base = Hex(l.base_phys), apic_base_msr = Hex(l.apic_base), version = Hex(l.version as u64), max_lvt = l.max_lvt,
        svr_was = Hex(l.before.svr as u64), svr = Hex(lvt.svr as u64), lint0 = Hex(lvt.lint0 as u64), lint1 = Hex(lvt.lint1 as u64),
        timer = Hex(lvt.timer as u64), error = Hex(lvt.error as u64), nmi_lint = l.nmi.lint, nmi_low = l.nmi.active_low,
        stale_isr_acked = l.stale_isr_acked, esr = Hex(l.esr as u64), eilvt_count = l.eilvt_count);
    if l.mode == Mode::XApic && want {
        klog::event!(Info, "apic", "fallback.xapic", reason = "cpuid has no x2apic"; "xAPIC through its MMIO page");
    }
    if l.kept_firmware_x2apic {
        klog::event!(Warn, "apic", "fallback.x2apic", reason = "firmware had x2apic on"; "apic=xapic ignored");
    }
    if l.xapic_map_failed {
        klog::event!(Warn, "apic", "fallback.x2apic", reason = "xapic page not mappable");
    }
    if l.stale_isr_acked > 0 {
        klog::event!(Warn, "apic", "stale_isr", acked = l.stale_isr_acked; "the firmware left vectors in service");
    }
}

/// Firmware LVTs (spec §5.1): an unmasked fixed-delivery one is left alone and its vector reserved, so no device
/// gets it. Its vector must be a device vector, though: in 0x10-0x1F it would enter an exception gate, at 0xFF it
/// would stay in service for good (dispatch never acknowledges 0xFF), in 0xF0-0xFE it would land in a system
/// handler (LAPIC timer, IPIs). Such an LVT, and one whose vector a device already holds, is masked instead
/// (Linux masks them all). Returns (reserved, masked).
fn reserve_firmware_lvts(lvt: &LvtSnapshot) -> (Vec<(&'static str, u8)>, Vec<(&'static str, u8)>) {
    const EILVT: [&str; apic::EILVT_REGS as usize] = ["eilvt0", "eilvt1", "eilvt2", "eilvt3"];
    let classic = [
        ("thermal", apic::REG_LVT_THERMAL, lvt.thermal),
        ("perf", apic::REG_LVT_PERF, lvt.perf),
        ("cmci", apic::REG_LVT_CMCI, lvt.cmci),
        ("error", apic::REG_LVT_ERROR, Some(lvt.error)),
    ];
    let ext = (0..apic::EILVT_REGS).map(|i| (EILVT[i as usize], apic::REG_EILVT0 + 0x10 * i, lvt.eilvt[i as usize]));
    let (mut reserved, mut masked) = (Vec::new(), Vec::new());
    for (name, reg, val) in classic.into_iter().chain(ext) {
        let Some(v) = val else { continue };
        if v & apic::LVT_MASKED != 0 || v & apic::DM_MASK != apic::DM_FIXED {
            continue;
        }
        let vec = v as u8;
        let keep = match vectors::reserve(0, vec) {
            Ok(()) => true,
            // Two firmware LVTs on one vector share the reservation.
            Err(vectors::VectorError::Taken) => reserved.iter().any(|&(_, r)| r == vec),
            Err(_) => false,
        };
        if keep {
            klog::event!(Warn, "apic", "lvt.unmasked", lvt = name, value = Hex(v as u64), vector = Hex(vec as u64), reserved = true;
                "left as the firmware set it");
            reserved.push((name, vec));
        } else {
            let done = apic::mask_lvt(reg);
            klog::event!(Warn, "apic", "lvt.masked", lvt = name, value = Hex(v as u64), vector = Hex(vec as u64), done = done;
                "the firmware's vector is not a free device vector (0x30-0xef)");
            masked.push((name, vec));
        }
    }
    (reserved, masked)
}

fn open_ioapic(addr: u64, id: u8, gsi_base: u32) -> Option<IoApicInfo> {
    // Up to 256 pins (version bits 16-23, plus one); a GSI base without room for them is a broken table.
    if gsi_base.checked_add(0xFF).is_none() {
        klog::event!(Error, "irq", "ioapic.bad_gsi_base", id = id, addr = Hex(addr), gsi_base = gsi_base; "GSI range would overflow; I/O APIC not used");
        return None;
    }
    // The guard checks registers (1-8 bytes): IOREGSEL and IOWIN, the two the driver touches.
    if let Err(e) = crate::check_mem(addr, 4).and_then(|_| crate::check_mem(addr + 0x10, 4)) {
        klog::event!(Error, "irq", "ioapic.refused", id = id, addr = Hex(addr), reason = e.name(); "the register guard refused the MADT's address");
        return None;
    }
    let a = match IoApic::open(addr, id, gsi_base) {
        Ok(a) => a,
        Err(IoApicError::Absent) => {
            klog::event!(Error, "irq", "ioapic.absent", id = id, addr = Hex(addr); "the version register reads all ones");
            return None;
        }
        Err(IoApicError::Map(e)) => {
            klog::event!(Error, "irq", "ioapic.map_failed", id = id, addr = Hex(addr), error = format!("{e:?}"));
            return None;
        }
    };
    let mask = a.mask_all();
    klog::event!(Info, "irq", "ioapic", id = id, hw_id = a.hw_id, addr = Hex(addr), gsi_base = gsi_base, pins = a.pins, version = Hex(a.version as u64),
        masked = mask.masked, smi_left = mask.smi_left, were_unmasked = mask.were_unmasked, remote_irr_cleared = mask.remote_irr_cleared);
    if a.hw_id != id & 0xF {
        klog::event!(Info, "irq", "ioapic.id_differs", madt = id, register = a.hw_id; "kept as the firmware set it");
    }
    Some(IoApicInfo { apic: a, mask })
}

fn log_note(n: &PlanNote) {
    match *n {
        PlanNote::NoMadt => klog::event!(Warn, "irq", "plan.no_madt"; "no MADT: nothing routed"),
        PlanNote::NoSci => klog::event!(Info, "irq", "plan.no_sci"; "no FADT or hardware-reduced ACPI"),
        PlanNote::No8042 => klog::event!(Info, "irq", "plan.no_8042"; "PS/2 not routed"),
        PlanNote::ReservedFlags { source, flags } => klog::event!(Warn, "irq", "plan.reserved_flags", source = source, flags = Hex(flags as u64); "defaults used"),
        PlanNote::SharedGsi { gsi, purpose } => klog::event!(Warn, "irq", "plan.shared_gsi", gsi = gsi, purpose = purpose.name(); "second user dropped"),
        PlanNote::NoIoApic { gsi, purpose } => klog::event!(Warn, "irq", "plan.no_ioapic", gsi = gsi, purpose = purpose.name()),
        PlanNote::BadNmiLint { lint } => klog::event!(Warn, "apic", "nmi.bad_lint", lint = lint; "LINT1 used"),
    }
}

/// The hard-IRQ handler of every routed line (interrupts off; no heap, no klog).
fn route_irq(vector: u8) {
    let Some(st) = state() else { return };
    let cpu = hal::percpu::this_cpu() as u32;
    let Some(r) = st.routes.iter().find(|r| r.vector.vector == vector && r.vector.cpu == cpu) else { return };
    r.count.fetch_add(1, Ordering::Relaxed);
    ROUTED_IRQS.inc();
    if r.storm.hit(irq_plan::storm_clock(time::now_ns(), hal::cpu::rdtsc)) {
        storm_mask(st, r);
        return;
    }
    let h = *r.handler.lock();
    if let Some(f) = h {
        f(r.purpose);
    }
}

fn storm_mask(st: &State, r: &Route) {
    st.ioapics[r.ioapic].apic.set_masked(r.pin, true);
    r.stormed.store(true, Ordering::Release);
    STORMS.inc();
}

/// Ktests: what the storm guard does on a storm, without needing 100 interrupts inside a millisecond (QEMU's
/// timing proves nothing; the counting itself is host-tested).
pub(crate) fn force_storm(p: Purpose) -> Result<(), IrqError> {
    let r = route_or_err(p)?;
    storm_mask(state().ok_or(IrqError::NotSetUp)?, r);
    Ok(())
}

/// Ktests: runs the deferred-event poller once.
pub(crate) fn poll_events() {
    report_deferred();
}

/// Installs the owner's handler of a routed line (IN for PS/2, CP-5/ACPI events for the SCI) and returns the
/// previous one. Installing unmasks the pin (and rearms a line the storm guard had masked); `None` masks it
/// again before the handler goes. For a level line (the SCI) the handler must clear the source, or the storm
/// guard masks it.
pub fn set_handler(p: Purpose, h: Option<LineHandler>) -> Result<Option<LineHandler>, IrqError> {
    let r = route_or_err(p)?;
    let st = state().ok_or(IrqError::NotSetUp)?;
    let a = &st.ioapics[r.ioapic].apic;
    if h.is_none() {
        a.set_masked(r.pin, true);
    }
    let old = core::mem::replace(&mut *r.handler.lock(), h);
    let was_stormed = h.is_some() && r.stormed();
    if h.is_some() {
        clear_storm(r);
        a.set_masked(r.pin, false);
    }
    klog::event!(Info, "irq", "handler.set", purpose = p.name(), installed = h.is_some(), masked = h.is_none(), rearmed = was_stormed);
    Ok(old)
}

fn clear_storm(r: &Route) {
    r.storm.reset();
    r.stormed.store(false, Ordering::Release);
    r.told_storm.store(false, Ordering::Release);
}

fn route_or_err(p: Purpose) -> Result<&'static Route, IrqError> {
    if state().is_none() {
        return Err(IrqError::NotSetUp);
    }
    route(p).ok_or(IrqError::NotRouted)
}

/// Clears the storm state of a line the storm guard masked (its owner fixed the cause) and unmasks it if an
/// owner's handler is installed; without one it stays masked. Logged.
pub fn rearm(p: Purpose) -> Result<(), IrqError> {
    let r = route_or_err(p)?;
    let st = state().ok_or(IrqError::NotSetUp)?;
    clear_storm(r);
    let owned = r.has_handler();
    if owned {
        st.ioapics[r.ioapic].apic.set_masked(r.pin, false);
    }
    klog::event!(Info, "irq", "rearm", purpose = p.name(), gsi = r.line.gsi, unmasked = owned);
    Ok(())
}

/// The deferred events of `route_irq` and the metric copy (a poller: thread context).
fn report_deferred() {
    let Some(st) = state() else { return };
    let mut now = [0u64; 3];
    for cpu in 0..hal::percpu::cpu_count() as u32 {
        if let Some(v) = vectors::stats(cpu) {
            now[0] += v.delivered;
            now[1] += v.unhandled;
            now[2] += v.apic_spurious;
        }
    }
    for (i, c) in [&DELIVERED, &UNHANDLED, &APIC_SPURIOUS].into_iter().enumerate() {
        let last = SYNCED[i].swap(now[i], Ordering::AcqRel);
        c.add(now[i].saturating_sub(last));
    }
    for r in &st.routes {
        let n = r.count();
        if n > 0 && !r.told_first.swap(true, Ordering::AcqRel) {
            klog::event!(Info, "irq", "first", purpose = r.purpose.name(), gsi = r.line.gsi, vector = Hex(r.vector.vector as u64));
        }
        if r.stormed() && !r.told_storm.swap(true, Ordering::AcqRel) {
            klog::event!(Warn, "irq", "storm", purpose = r.purpose.name(), gsi = r.line.gsi, count = n, limit_per_ms = STORM_LIMIT;
                "more than 100 interrupts in 1 ms: pin masked, the owner polls");
        }
    }
}

/// Text lines for `hw_info` (interrupts section) and the Mini-PC check list.
pub fn summary() -> Vec<String> {
    let mut out = Vec::new();
    let st = match STATE.get() {
        Some(Ok(st)) => st,
        Some(Err(e)) => return alloc::vec![format!("irq: set-up failed: {e:?}")],
        None => return alloc::vec![String::from("irq: not set up (acpi::irq::init was not called)")],
    };
    let l = &st.lapic;
    out.push(format!(
        "apic mode={} firmware={} cpuid_x2apic={} id={} base={:#x} version={:#x} max_lvt={} svr={:#x} lint0={:#x} lint1={:#x} timer={:#x} error={:#x} nmi=lint{}{} interrupts={}",
        l.mode.name(), l.firmware_mode.name(), l.cpuid_x2apic, l.id, l.base_phys, l.version, l.max_lvt, st.lvt.svr, st.lvt.lint0, st.lvt.lint1,
        st.lvt.timer, st.lvt.error, l.nmi.lint, if l.nmi.active_low { " low" } else { "" }, if enabled() { "on" } else { "off" }
    ));
    for (name, v) in &st.reserved_lvt {
        out.push(format!("apic lvt {name} unmasked by firmware, vector {v:#x} reserved"));
    }
    for (name, v) in &st.masked_lvt {
        out.push(format!("apic lvt {name} unmasked by firmware on vector {v:#x}, masked by CP"));
    }
    out.push(format!("pic8259 masked remapped={} imr={:#x}/{:#x}", st.pic_remapped, st.pic_masks.0, st.pic_masks.1));
    for a in &st.ioapics {
        let unmasked = (0..a.apic.pins).filter(|&p| a.apic.entry(p).is_some_and(|e| !e.masked())).count();
        out.push(format!(
            "ioapic id={} addr={:#x} gsi={}-{} pins={} version={:#x} smi={} unmasked_now={}",
            a.apic.id, a.apic.phys, a.apic.gsi_base, a.apic.gsi_base.saturating_add(a.apic.pins.saturating_sub(1)), a.apic.pins, a.apic.version, a.mask.smi_left, unmasked
        ));
    }
    for r in &st.routes {
        out.push(format!(
            "irq {} gsi={} ioapic={} pin={} vector={:#x} {} {} count={} stormed={} handler={}",
            r.purpose.name(), r.line.gsi, st.ioapics[r.ioapic].apic.id, r.pin, r.vector.vector,
            if r.line.level { "level" } else { "edge" }, if r.line.active_low { "low" } else { "high" }, r.count(), r.stormed(), r.handler.lock().is_some()
        ));
    }
    for cpu in 0..hal::percpu::cpu_count() as u32 {
        let v = vectors::stats(cpu).unwrap_or_default();
        out.push(format!(
            "vectors cpu={cpu} taken={} delivered={} unhandled={} apic_spurious={}",
            v.dynamic_taken, v.delivered, v.unhandled, v.apic_spurious
        ));
    }
    // CP-5: the clock and every CPU's LAPIC timer (the Mini-PC check list reads these).
    out.extend(time::summary());
    out
}

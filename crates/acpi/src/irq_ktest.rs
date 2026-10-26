//! CP-3 ktests. Every test first makes sure the controllers are set up (`irq::setup`, idempotent: the kernel's
//! `acpi::irq::init` hook may have run already) and opens interrupts only for a bounded wait (generous limits,
//! never a timing judgement). All but the 8042 loopback are `Safe`: they only use the BSP's own LAPIC
//! (self-IPIs, a software `int 0xff`) and the mask bit of a line CP routed itself.

use crate::ioapic::DM_SMI;
use crate::irq::{self, State};
use crate::irq_plan::Purpose;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use hal::apic::{self, IpiDest, IpiKind, Mode};
use hal::cpu::IrqGuard;
use hal::vectors::{self, Vector};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("cp.irq.alloc_free", Safe, alloc_free);
ktest!("cp.irq.lapic", Safe, lapic_config);
ktest!("cp.irq.self_ipi", Safe, self_ipi);
ktest!("cp.irq.ipi_physical", Safe, ipi_physical);
ktest!("cp.irq.unregistered", Safe, unregistered);
ktest!("cp.irq.spurious_vector", Safe, spurious_vector);
ktest!("cp.irq.system_vector", Safe, system_vector);
ktest!("cp.irq.pic_masked", Safe, pic_masked);
ktest!("cp.irq.ioapic_pins", Safe, ioapic_pins);
ktest!("cp.irq.route_dispatch", Safe, route_dispatch);
ktest!("cp.irq.storm_mask", Safe, storm_mask);
ktest!("cp.irq.owner_unmask", Safe, owner_unmask);
ktest!("cp.irq.ipi_refused", Safe, ipi_refused);
ktest!("cp.irq.firmware_lvts", Safe, firmware_lvts);
ktest!("cp.irq.ps2_loopback", QemuOnly, ps2_loopback);

fn ensure() -> Result<&'static State, String> {
    irq::setup(&|_| None).map_err(|e| format!("interrupt set-up failed: {e:?}"))
}

/// Opens interrupts until `cond` holds or a generous second passes; true if it held.
fn sti_until(cond: impl Fn() -> bool) -> bool {
    let deadline = time::now_ns().saturating_add(1_000_000_000);
    let mut spins = 0u64;
    hal::cpu::enable_interrupts();
    let mut ok = cond();
    while !ok && (time::now_ns() < deadline || spins < 1_000) {
        hal::cpu::spin_hint();
        spins += 1;
        ok = cond();
    }
    hal::cpu::disable_interrupts();
    ok
}

/// Frees a vector at the end of a test, whatever happens.
struct Owned(Vector);
impl Drop for Owned {
    fn drop(&mut self) {
        vectors::free(self.0);
    }
}

fn alloc_on_bsp(h: vectors::IrqHandler) -> Result<Owned, String> {
    vectors::alloc(h, Some(0)).map(Owned).map_err(|e| format!("alloc: {e:?}"))
}

static HITS: AtomicU32 = AtomicU32::new(0);
static SAW_VECTOR: AtomicU8 = AtomicU8::new(0);
static SAW_ISR: AtomicBool = AtomicBool::new(false);

fn counting(v: u8) {
    SAW_VECTOR.store(v, Ordering::Relaxed);
    SAW_ISR.store(apic::isr_bit(v), Ordering::Relaxed);
    HITS.fetch_add(1, Ordering::Release);
}

fn reset_hits() {
    HITS.store(0, Ordering::Release);
    SAW_VECTOR.store(0, Ordering::Relaxed);
    SAW_ISR.store(false, Ordering::Relaxed);
}

fn alloc_free() -> R {
    fn h(_: u8) {}
    let taken0 = vectors::stats(0).ok_or("no table for CPU 0")?.dynamic_taken;
    let a = vectors::alloc(h, Some(0)).map_err(|e| format!("{e:?}"))?;
    let b = vectors::alloc(h, None).map_err(|e| format!("{e:?}"))?;
    kassert!(a.cpu == 0 && b.cpu < hal::percpu::cpu_count() as u32);
    kassert!(a != b);
    for v in [a, b] {
        kassert!((vectors::FIRST_DYNAMIC..=vectors::LAST_DYNAMIC).contains(&v.vector), "{:#x} outside the dynamic range", v.vector);
        kassert!(vectors::is_registered(v));
    }
    kassert!(vectors::alloc(h, Some(hal::percpu::cpu_count() as u32)) == Err(vectors::VectorError::NoSuchCpu));
    vectors::free(a);
    kassert!(!vectors::is_registered(a));
    let again = vectors::alloc(h, Some(0)).map_err(|e| format!("{e:?}"))?;
    kassert!(again.vector <= a.vector, "the lowest free vector comes back first ({:#x} > {:#x})", again.vector, a.vector);
    vectors::free(again);
    vectors::free(b);
    // Fill CPU 0 to the last vector: 192 dynamic vectors per CPU is x86, not a limit of ours.
    let mut all = Vec::new();
    loop {
        match vectors::alloc(h, Some(0)) {
            Ok(v) => all.push(v),
            Err(vectors::VectorError::Exhausted) => break,
            Err(e) => {
                for v in all {
                    vectors::free(v);
                }
                return Err(format!("{e:?}"));
            }
        }
    }
    let n = all.len();
    for v in all {
        vectors::free(v);
    }
    kassert!(n as u32 + taken0 == 192, "filled {n} + {taken0} taken before != 192");
    kassert!(vectors::stats(0).map(|s| s.dynamic_taken) == Some(taken0), "every vector came back");
    kassert!(vectors::reserve(0, vectors::SPURIOUS_VECTOR) == Err(vectors::VectorError::NotInRange));
    kassert!(vectors::register_system(0x40, h) == Err(vectors::VectorError::NotInRange));
    Ok(())
}

fn lapic_config() -> R {
    let st = ensure()?;
    let l = &st.lapic;
    let base = apic::apic_base_msr();
    kassert!(apic::mode() == l.mode);
    kassert!(base & apic::APIC_BASE_EN != 0, "APIC globally disabled: {base:#x}");
    kassert!((base & apic::APIC_BASE_EXTD != 0) == (l.mode == Mode::X2Apic), "EXTD {base:#x} vs mode {:?}", l.mode);
    kassert!(base & apic::APIC_BASE_BSP != 0, "not marked BSP");
    if l.cpuid_x2apic && l.mode != Mode::X2Apic {
        // Only `apic=xapic` or a failed map may leave x2APIC off when CPUID offers it.
        kassert!(!l.kept_firmware_x2apic);
    }
    if !l.cpuid_x2apic {
        kassert!(l.mode == Mode::XApic, "no x2APIC in CPUID, mode {:?}", l.mode);
    }
    let svr = apic::read(apic::REG_SVR).ok_or("SVR unreadable")?;
    kassert!(svr & 0x1FF == 0x1FF, "SVR {svr:#x}: want enabled with vector 0xFF");
    kassert!(apic::read(apic::REG_TPR) == Some(0));
    let timer = apic::read(apic::REG_LVT_TIMER).ok_or("timer LVT unreadable")?;
    kassert!(timer as u8 == vectors::LAPIC_TIMER, "LVT timer {timer:#x}: vector 0xF0");
    // CP-5: a live timer is unmasked in the mode `time` chose; one not started (hrtimer=poll, failed) stays masked.
    match time::lapic::cpu_stats(hal::percpu::this_cpu()).filter(|s| s.live).and_then(|s| s.kind) {
        Some(k) => {
            let want = if k == time::lapic::Kind::TscDeadline { apic::timer::LVT_TSC_DEADLINE } else { apic::timer::LVT_ONESHOT };
            kassert!(timer & apic::LVT_MASKED == 0 && timer & apic::timer::LVT_MODE_MASK == want, "LVT timer {timer:#x}: live {k:?}, want unmasked mode {want:#x}");
        }
        None => kassert!(timer & apic::LVT_MASKED != 0, "LVT timer {timer:#x}: not started, want masked"),
    }
    let (l0, l1) = (apic::read(apic::REG_LVT_LINT0).ok_or("LINT0")?, apic::read(apic::REG_LVT_LINT1).ok_or("LINT1")?);
    let (nmi, other) = if l.nmi.lint == 0 { (l0, l1) } else { (l1, l0) };
    kassert!(nmi & apic::LVT_MASKED == 0 && nmi & apic::DM_MASK == apic::DM_NMI, "NMI LINT{} = {nmi:#x}", l.nmi.lint);
    kassert!(other & apic::LVT_MASKED != 0, "the other LINT {other:#x} must be masked");
    kassert!((nmi & apic::LVT_ACTIVE_LOW != 0) == l.nmi.active_low);
    // The APIC ID as CPUID reports it.
    let id = apic::id().ok_or("no id")?;
    kassert!(id == l.id);
    let cpuid_id = if l.mode == Mode::X2Apic && hal::cpu::cpuid(0, 0).eax >= 0xB { hal::cpu::cpuid(0xB, 0).edx } else { hal::cpu::cpuid(1, 0).ebx >> 24 };
    kassert!(id == cpuid_id, "APIC ID {id} vs CPUID {cpuid_id}");
    if let Some(m) = crate::tables().and_then(|a| a.madt.as_ref()) {
        kassert!(m.cpus.iter().any(|c| c.apic_id == id && c.enabled), "the MADT lists no enabled CPU with APIC ID {id}");
    }
    Ok(())
}

/// Sends `dest` an IPI on a fresh vector and checks: pending while interrupts are off, the handler runs once
/// on its vector with its ISR bit set (so the EOI comes after it), and the ISR bit is clear afterwards.
fn ipi_roundtrip(dest: IpiDest) -> R {
    ensure()?;
    let _irq = IrqGuard::new();
    let v = alloc_on_bsp(counting)?;
    let vec = v.0.vector;
    reset_hits();
    apic::send_ipi(dest, IpiKind::Fixed(vec)).map_err(|e| format!("send_ipi: {e:?}"))?;
    let mut pending = false;
    for _ in 0..100_000 {
        if apic::irr_bit(vec) {
            pending = true;
            break;
        }
        hal::cpu::spin_hint();
    }
    kassert!(HITS.load(Ordering::Acquire) == 0, "delivered while interrupts were off");
    kassert!(pending, "vector {vec:#x} never showed up in IRR");
    kassert!(sti_until(|| HITS.load(Ordering::Acquire) > 0), "handler of {vec:#x} did not run within 1 s");
    kassert!(HITS.load(Ordering::Acquire) == 1, "handler ran {} times", HITS.load(Ordering::Acquire));
    kassert!(SAW_VECTOR.load(Ordering::Relaxed) == vec);
    kassert!(SAW_ISR.load(Ordering::Relaxed), "ISR bit not set inside the handler (EOI must come after it)");
    kassert!(!apic::isr_bit(vec) && !apic::irr_bit(vec), "vector still in service or pending after the handler");
    kassert!(vectors::count(v.0) == 1, "count {}", vectors::count(v.0));
    Ok(())
}

fn self_ipi() -> R {
    ipi_roundtrip(IpiDest::SelfOnly)
}

fn ipi_physical() -> R {
    let id = ensure()?.lapic.id;
    ipi_roundtrip(IpiDest::Physical(id))
}

fn unregistered() -> R {
    ensure()?;
    let _irq = IrqGuard::new();
    let v = vectors::alloc(counting, Some(0)).map_err(|e| format!("{e:?}"))?;
    vectors::free(v);
    let before = vectors::spurious_count();
    let apic_before = vectors::apic_spurious_count();
    reset_hits();
    apic::send_ipi(IpiDest::SelfOnly, IpiKind::Fixed(v.vector)).map_err(|e| format!("{e:?}"))?;
    let ok = sti_until(|| vectors::spurious_count() > before);
    kassert!(ok, "an interrupt on a freed vector was not counted");
    kassert!(HITS.load(Ordering::Acquire) == 0, "the freed handler ran");
    kassert!(!apic::isr_bit(v.vector), "no EOI for the unhandled vector");
    kassert!(vectors::apic_spurious_count() == apic_before, "counted as an APIC spurious interrupt");
    Ok(())
}

static NESTED_STILL_IN_SERVICE: AtomicBool = AtomicBool::new(false);
static NESTED_SPURIOUS_SEEN: AtomicBool = AtomicBool::new(false);

fn nested_spurious(v: u8) {
    let before = vectors::apic_spurious_count();
    apic::soft_int_spurious();
    NESTED_SPURIOUS_SEEN.store(vectors::apic_spurious_count() == before + 1, Ordering::Relaxed);
    // Had the spurious path sent an EOI, it would have retired this vector.
    NESTED_STILL_IN_SERVICE.store(apic::isr_bit(v), Ordering::Relaxed);
    HITS.fetch_add(1, Ordering::Release);
}

fn spurious_vector() -> R {
    ensure()?;
    let _irq = IrqGuard::new();
    let svr = apic::read(apic::REG_SVR).ok_or("SVR unreadable")?;
    kassert!(svr as u8 == vectors::SPURIOUS_VECTOR, "spurious vector {:#x}", svr as u8);
    // Plain: counted, not as unhandled.
    let (sp, un) = (vectors::apic_spurious_count(), vectors::spurious_count());
    apic::soft_int_spurious();
    kassert!(vectors::apic_spurious_count() == sp + 1);
    kassert!(vectors::spurious_count() == un, "0xFF counted as an unhandled vector");
    // Nested inside a real interrupt: no EOI may come from the spurious path.
    let v = alloc_on_bsp(nested_spurious)?;
    reset_hits();
    NESTED_STILL_IN_SERVICE.store(false, Ordering::Relaxed);
    NESTED_SPURIOUS_SEEN.store(false, Ordering::Relaxed);
    apic::send_ipi(IpiDest::SelfOnly, IpiKind::Fixed(v.0.vector)).map_err(|e| format!("{e:?}"))?;
    kassert!(sti_until(|| HITS.load(Ordering::Acquire) > 0), "carrier interrupt did not arrive");
    kassert!(NESTED_SPURIOUS_SEEN.load(Ordering::Relaxed), "the int 0xff inside the handler was not counted");
    kassert!(NESTED_STILL_IN_SERVICE.load(Ordering::Relaxed), "the spurious path sent an EOI (it retired the carrier vector)");
    kassert!(!apic::isr_bit(v.0.vector), "carrier vector still in service after its own EOI");
    Ok(())
}

fn system_vector() -> R {
    ensure()?;
    const V: u8 = 0xFD;
    if vectors::register_system(V, counting).is_err() {
        return ktest::skip("system vector 0xFD has an owner");
    }
    let _irq = IrqGuard::new();
    reset_hits();
    let r = apic::send_ipi(IpiDest::SelfOnly, IpiKind::Fixed(V));
    let ok = r.is_ok() && sti_until(|| HITS.load(Ordering::Acquire) > 0);
    vectors::unregister_system(V);
    kassert!(r.is_ok(), "send_ipi: {r:?}");
    kassert!(ok, "the system handler did not run");
    kassert!(SAW_VECTOR.load(Ordering::Relaxed) == V && SAW_ISR.load(Ordering::Relaxed));
    kassert!(!apic::isr_bit(V));
    Ok(())
}

fn pic_masked() -> R {
    let st = ensure()?;
    let (m, s) = apic::pic_masks();
    kassert!((m, s) == (0xFF, 0xFF), "8259 masks {m:#x}/{s:#x}");
    kassert!(st.pic_masks == (0xFF, 0xFF));
    Ok(())
}

fn ioapic_pins() -> R {
    let st = ensure()?;
    if !st.tables {
        return ktest::skip("acpi::init has not run (desk change request CP-2/1): no I/O APIC data");
    }
    let madt = crate::tables().and_then(|a| a.madt.as_ref()).ok_or("tables without MADT")?;
    kassert!(st.ioapics.len() == madt.ioapics.len(), "{} of {} I/O APICs opened", st.ioapics.len(), madt.ioapics.len());
    let mut unmasked = 0;
    for a in &st.ioapics {
        for pin in 0..a.apic.pins {
            let e = a.apic.entry(pin).ok_or("entry unreadable")?;
            let gsi = a.apic.gsi_base.saturating_add(pin);
            match st.routes.iter().find(|r| r.line.gsi == gsi) {
                Some(r) => {
                    let want_masked = r.stormed() || !r.has_handler();
                    kassert!(e.masked() == want_masked, "{} gsi {gsi}: masked={} stormed={} handler={}", r.purpose.name(), e.masked(), r.stormed(), r.has_handler());
                    kassert!(e.vector() == r.vector.vector && e.delivery() == 0, "{}: entry {:#x}", r.purpose.name(), e.low);
                    kassert!(e.level() == r.line.level && e.active_low() == r.line.active_low, "{}: trigger/polarity {:#x}", r.purpose.name(), e.low);
                    kassert!(e.dest() as u32 == st.lapic.id && e.low & crate::ioapic::DEST_LOGICAL == 0, "{}: dest {:#x}", r.purpose.name(), e.high);
                    unmasked += !e.masked() as u32;
                }
                None if e.delivery() == DM_SMI => {}
                None => kassert!(e.masked(), "gsi {gsi} (I/O APIC {} pin {pin}) unmasked: {:#x}", a.apic.id, e.low),
            }
        }
    }
    let fadt = crate::tables().and_then(|a| a.fadt.as_ref());
    if let Some(f) = fadt.filter(|f| !f.hw_reduced()) {
        let sci = irq::route(Purpose::Sci).ok_or("SCI not routed")?;
        kassert!(sci.source == f.sci_int as u32 && sci.line.level, "SCI {} level={}", sci.source, sci.line.level);
        // Mini-PC: GSI 9, level, active low (MADT override flags 0xF); q35 says active high.
        if let Some(o) = madt.isa_override(f.sci_int as u8) {
            kassert!(sci.line.gsi == o.gsi);
        }
    }
    kassert!(unmasked as usize == st.routes.iter().filter(|r| !r.stormed() && r.has_handler()).count());
    Ok(())
}

static ROUTE_CALLS: AtomicU32 = AtomicU32::new(0);
static ROUTE_PURPOSE_OK: AtomicBool = AtomicBool::new(false);

fn route_seen(p: Purpose) {
    ROUTE_PURPOSE_OK.store(p == Purpose::Sci, Ordering::Relaxed);
    ROUTE_CALLS.fetch_add(1, Ordering::Release);
}

/// The SCI route's vector raised by a self-IPI: the shared route handler finds the route, counts and calls
/// the owner's handler; restores the owner afterwards.
fn route_dispatch() -> R {
    ensure()?;
    let Some(r) = irq::route(Purpose::Sci) else {
        return ktest::skip("no SCI route (no tables or no FADT)");
    };
    let _irq = IrqGuard::new();
    let old = irq::set_handler(Purpose::Sci, Some(route_seen)).map_err(|e| format!("{e:?}"))?;
    ROUTE_CALLS.store(0, Ordering::Release);
    ROUTE_PURPOSE_OK.store(false, Ordering::Relaxed);
    let before = r.count();
    let sent = apic::send_ipi(IpiDest::SelfOnly, IpiKind::Fixed(r.vector.vector));
    let ok = sent.is_ok() && sti_until(|| ROUTE_CALLS.load(Ordering::Acquire) > 0);
    let _ = irq::set_handler(Purpose::Sci, old);
    kassert!(sent.is_ok(), "{sent:?}");
    kassert!(ok, "route handler not called");
    kassert!(ROUTE_PURPOSE_OK.load(Ordering::Relaxed));
    kassert!(r.count() > before);
    kassert!(!apic::isr_bit(r.vector.vector));
    Ok(())
}

fn sci_masked(st: &State, r: &irq::Route) -> Option<bool> {
    st.ioapics[r.ioapic].apic.entry(r.pin).map(|e| e.masked())
}

/// What a storm does to an owned line (pin masked, flag, deferred Warn event), and `rearm` undoing it; without
/// an owner `rearm` leaves the pin masked.
fn storm_mask() -> R {
    let st = ensure()?;
    let Some(r) = irq::route(Purpose::Sci) else {
        return ktest::skip("no SCI route (no tables or no FADT)");
    };
    if r.stormed() {
        return ktest::skip("the SCI line is in a real storm already");
    }
    let _irq = IrqGuard::new();
    let old = irq::set_handler(Purpose::Sci, Some(route_seen)).map_err(|e| format!("{e:?}"))?;
    let result = (|| -> R {
        irq::force_storm(Purpose::Sci).map_err(|e| format!("{e:?}"))?;
        let masked = sci_masked(st, r) == Some(true);
        let flagged = r.stormed();
        irq::poll_events();
        let rearmed = irq::rearm(Purpose::Sci);
        let unmasked = sci_masked(st, r) == Some(false);
        kassert!(masked, "storm did not mask the pin");
        kassert!(flagged);
        kassert!(rearmed.is_ok() && unmasked && !r.stormed(), "rearm: {rearmed:?}, unmasked {unmasked}");
        // A storm on an owned line, then the owner reinstalls its handler: that rearms too.
        irq::force_storm(Purpose::Sci).map_err(|e| format!("{e:?}"))?;
        let _ = irq::set_handler(Purpose::Sci, Some(route_seen));
        kassert!(!r.stormed() && sci_masked(st, r) == Some(false), "installing a handler did not rearm the stormed line");
        // Without an owner, rearm keeps the pin masked.
        let _ = irq::set_handler(Purpose::Sci, None);
        irq::force_storm(Purpose::Sci).map_err(|e| format!("{e:?}"))?;
        let rearmed = irq::rearm(Purpose::Sci);
        kassert!(rearmed.is_ok() && !r.stormed() && sci_masked(st, r) == Some(true), "rearm unmasked a line nobody owns");
        Ok(())
    })();
    let _ = irq::set_handler(Purpose::Sci, old);
    result
}

/// Routed lines stay masked until an owner installs a handler; removing it masks the pin again.
fn owner_unmask() -> R {
    let st = ensure()?;
    let Some(r) = irq::route(Purpose::Sci) else {
        return ktest::skip("no SCI route (no tables or no FADT)");
    };
    if r.has_handler() {
        return ktest::skip("the SCI has an owner already");
    }
    let _irq = IrqGuard::new();
    kassert!(sci_masked(st, r) == Some(true), "SCI pin unmasked without an owner");
    let old = irq::set_handler(Purpose::Sci, Some(route_seen)).map_err(|e| format!("{e:?}"))?;
    let on = sci_masked(st, r) == Some(false);
    let _ = irq::set_handler(Purpose::Sci, old);
    let off = sci_masked(st, r) == Some(true);
    kassert!(old.is_none());
    kassert!(on, "installing a handler did not unmask the pin");
    kassert!(off, "removing the handler did not mask the pin");
    Ok(())
}

/// The public IPI path refuses what the dispatcher cannot serve and what would start another CPU; nothing is
/// sent for any of these.
fn ipi_refused() -> R {
    let st = ensure()?;
    let me = st.lapic.id;
    for v in [0x02u8, 0x0E, 0x11, 0x15, 0x1D, 0x1E, 0x1F, 0x20, 0x27, 0x2F, vectors::SPURIOUS_VECTOR] {
        let r = apic::send_ipi(IpiDest::SelfOnly, IpiKind::Fixed(v));
        kassert!(r == Err(apic::IpiError::Invalid), "fixed {v:#x}: {r:?}");
    }
    for d in [IpiDest::Physical(me), IpiDest::AllButSelf, IpiDest::All] {
        kassert!(apic::send_ipi(d, IpiKind::Init) == Err(apic::IpiError::Invalid), "INIT {d:?} accepted");
        kassert!(apic::send_ipi(d, IpiKind::Startup(0x08)) == Err(apic::IpiError::Invalid), "SIPI {d:?} accepted");
    }
    kassert!(!apic::isr_bit(vectors::SPURIOUS_VECTOR), "0xFF in service");
    Ok(())
}

/// Every firmware LVT that is still unmasked with fixed delivery carries a reserved device vector; the ones CP
/// masked are masked (classic LVTs and AMD's extended ones).
fn firmware_lvts() -> R {
    let st = ensure()?;
    let now = apic::lvt_snapshot().ok_or("LAPIC not set up")?;
    let mut lvts: Vec<(&str, Option<u32>)> = alloc::vec![("thermal", now.thermal), ("perf", now.perf), ("cmci", now.cmci), ("error", Some(now.error))];
    for (i, v) in now.eilvt.iter().enumerate() {
        lvts.push((["eilvt0", "eilvt1", "eilvt2", "eilvt3"][i], *v));
    }
    kassert!(now.eilvt.iter().filter(|v| v.is_some()).count() as u32 == st.lapic.eilvt_count);
    for (name, v) in lvts {
        let Some(v) = v else { continue };
        if v & apic::LVT_MASKED != 0 || v & apic::DM_MASK != apic::DM_FIXED {
            continue;
        }
        let vec = v as u8;
        kassert!((vectors::FIRST_DYNAMIC..=vectors::LAST_DYNAMIC).contains(&vec), "{name} unmasked on vector {vec:#x}");
        kassert!(st.reserved_lvt.iter().any(|&(n, r)| n == name && r == vec), "{name} vector {vec:#x} not reserved");
        kassert!(vectors::reserve(0, vec) == Err(vectors::VectorError::Taken), "{name} vector {vec:#x} free");
    }
    for (name, vec) in &st.masked_lvt {
        let reg = match *name {
            "thermal" => apic::REG_LVT_THERMAL,
            "perf" => apic::REG_LVT_PERF,
            "cmci" => apic::REG_LVT_CMCI,
            "error" => apic::REG_LVT_ERROR,
            n => apic::REG_EILVT0 + 0x10 * n.trim_start_matches("eilvt").parse::<u32>().map_err(|_| format!("lvt name {n}"))?,
        };
        let val = apic::read(reg).ok_or("masked LVT unreadable")?;
        kassert!(val & apic::LVT_MASKED != 0, "{name} (vector {vec:#x}) still unmasked: {val:#x}");
    }
    Ok(())
}

// --- 8042 loopback: a real interrupt through an I/O APIC pin (QEMU's i8042; the Mini-PC has none). ---

static KBD_BYTE: AtomicU32 = AtomicU32::new(u32::MAX);

fn kbd_drain(p: Purpose) {
    // Reading the output buffer drops the line (edge pin, but leave the 8042 empty for the next byte).
    if let Ok(b) = hal::portio::inb(0x60) {
        KBD_BYTE.store(b as u32 | if p == Purpose::Ps2Mouse { 0x100 } else { 0 }, Ordering::Release);
    }
}

fn i8042_wait(mask: u8, want: u8) -> bool {
    (0..1_000_000).any(|_| hal::portio::inb(0x64).is_ok_and(|s| s & mask == want))
}

fn i8042_cmd(cmd: u8) -> Result<(), String> {
    if !i8042_wait(0x02, 0) {
        return Err("8042 input buffer stays full".into());
    }
    hal::portio::outb(0x64, cmd).map_err(|e| format!("{e:?}"))
}

fn i8042_data(v: u8) -> Result<(), String> {
    if !i8042_wait(0x02, 0) {
        return Err("8042 input buffer stays full".into());
    }
    hal::portio::outb(0x60, v).map_err(|e| format!("{e:?}"))
}

fn ps2_loopback() -> R {
    ensure()?;
    let Some(kbd) = irq::route(Purpose::Ps2Keyboard) else {
        return ktest::skip("no PS/2 route (no 8042 per FADT, or no tables)");
    };
    let mouse = irq::route(Purpose::Ps2Mouse);
    let _irq = IrqGuard::new();
    for _ in 0..64 {
        if !hal::portio::inb(0x64).is_ok_and(|s| s & 1 != 0) {
            break;
        }
        let _ = hal::portio::inb(0x60);
    }
    i8042_cmd(0x20)?;
    kassert!(i8042_wait(0x01, 0x01), "no command byte");
    let cfg = hal::portio::inb(0x60).map_err(|e| format!("{e:?}"))?;
    // IRQ1 and IRQ12 on (bits 0, 1), translation off (bit 6) so the byte comes back as written.
    i8042_cmd(0x60)?;
    i8042_data((cfg | 0x03) & !0x40)?;
    let old_k = irq::set_handler(Purpose::Ps2Keyboard, Some(kbd_drain)).map_err(|e| format!("{e:?}"))?;
    let old_m = mouse.and_then(|_| irq::set_handler(Purpose::Ps2Mouse, Some(kbd_drain)).ok()).flatten();
    let result = (|| -> R {
        for (route, cmd, byte, tag) in [(Some(kbd), 0xD2u8, 0xA5u8, 0u32), (mouse, 0xD3, 0x5A, 0x100)] {
            let Some(route) = route else { continue };
            KBD_BYTE.store(u32::MAX, Ordering::Release);
            let before = route.count();
            i8042_cmd(cmd)?;
            i8042_data(byte)?;
            kassert!(sti_until(|| KBD_BYTE.load(Ordering::Acquire) != u32::MAX), "{}: no interrupt on gsi {}", route.purpose.name(), route.line.gsi);
            kassert!(KBD_BYTE.load(Ordering::Acquire) == byte as u32 | tag, "{}: got {:#x}", route.purpose.name(), KBD_BYTE.load(Ordering::Acquire));
            kassert!(route.count() > before);
            kassert!(!apic::isr_bit(route.vector.vector));
        }
        Ok(())
    })();
    let _ = irq::set_handler(Purpose::Ps2Keyboard, old_k);
    if mouse.is_some() {
        let _ = irq::set_handler(Purpose::Ps2Mouse, old_m);
    }
    let _ = i8042_cmd(0x60).and_then(|_| i8042_data(cfg));
    result
}

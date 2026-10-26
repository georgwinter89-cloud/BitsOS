//! CP-4 ktests (all `Safe`: they only read the APs' own registers through `hal::smp::call_on` and send fixed
//! IPIs to CPUs CP started). With one CPU (no AP planned) they skip. Limits are generous and functional; timing
//! is judged only on the Mini-PC (from the `cpu ap.up` / `cpu ap.tsc` events).

use crate::report;
use crate::smp::outcome;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use hal::apic::{self, IpiDest, IpiKind};
use hal::cpu;
use hal::percpu::{self, PerCpu};
use hal::smp;
use hal::vectors;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("cp.smp.all_up", Safe, all_up);
ktest!("cp.smp.topology", Safe, topology);
ktest!("cp.smp.ap_registers", Safe, ap_registers);
ktest!("cp.smp.mtrr", Safe, mtrr);
ktest!("cp.smp.mtrr_copy", QemuOnly, mtrr_copy);
ktest!("cp.smp.ipi_every_cpu", Safe, ipi_every_cpu);
ktest!("cp.smp.call_every_cpu", Safe, call_every_cpu);
ktest!("cp.smp.percpu", Safe, percpu_values);
ktest!("cp.smp.ist_stacks", Safe, ist_stacks);
ktest!("cp.smp.boot_stacks", Safe, boot_stacks);
ktest!("cp.smp.vectors", Safe, per_cpu_vectors);
ktest!("cp.smp.tsc_offsets", Safe, tsc_offsets);
ktest!("cp.smp.apic_map", Safe, apic_map);
ktest!("cp.smp.call_owners", Safe, call_owners);
ktest!("cp.smp.start_guards", Safe, start_guards);
ktest!("cp.smp.tsc_offsets_hardware", Safe, tsc_offsets_hardware);

const CALL_NS: u64 = 1_000_000_000;

/// The online APs; a skip when there are none.
fn aps() -> Result<Vec<usize>, String> {
    let o = outcome().ok_or("the bring-up initcall did not run")?;
    let v: Vec<usize> = (1..percpu::cpu_count()).collect();
    if v.is_empty() {
        let why = o.not_started.unwrap_or("the MADT lists one CPU");
        return Err(ktest::skip(&format!("no AP: {why}")).unwrap_err());
    }
    Ok(v)
}

fn call(cpu: usize, f: fn(u64) -> u64, arg: u64) -> Result<u64, String> {
    smp::call_on(cpu, f, arg, time::now_ns, CALL_NS).map_err(|e| format!("call_on cpu {cpu}: {e:?}"))
}

fn all_up() -> R {
    let o = outcome().ok_or("the bring-up initcall did not run")?;
    if let Some(why) = o.not_started {
        if o.limit.bsp_only() {
            return ktest::skip(why);
        }
        // Not started at all is only fine where there is nothing to start (or CONFIG asked for one CPU).
        let madt_cpus = acpi::tables().and_then(|a| a.madt.as_ref()).map_or(1, |m| m.enabled_cpus());
        kassert!(madt_cpus <= 1, "bring-up did not start ({why}) but the MADT lists {madt_cpus} CPUs");
        return ktest::skip(why);
    }
    kassert!(o.failed.is_empty(), "{} AP(s) failed: {:?}", o.failed.len(), o.failed.iter().map(|f| (f.apic_id, report::stage_name(f.stage), f.reason)).collect::<Vec<_>>());
    kassert!(o.up.len() + o.failed.len() + o.held_back == o.planned, "{} of {} APs up, {} failed, {} held back by CONFIG cpus=", o.up.len(), o.planned, o.failed.len(), o.held_back);
    kassert!(percpu::cpu_count() == 1 + o.up.len(), "cpu_count {} with {} APs up", percpu::cpu_count(), o.up.len());
    for (i, r) in o.up.iter().enumerate() {
        kassert!(r.cpu as usize == i + 1, "CPU numbers not dense: record {i} is cpu {}", r.cpu);
        kassert!(r.sipis == 1 || r.sipis == 2);
        kassert!(smp::is_online(r.cpu as usize));
    }
    if o.planned == 0 {
        return ktest::skip("one CPU");
    }
    Ok(())
}

fn topology() -> R {
    let n = percpu::cpu_count();
    let mut cpus = Vec::new();
    for c in 0..n {
        let t = smp::topology(c).ok_or_else(|| format!("cpu {c} has no topology"))?;
        let apic = smp::apic_id(c).ok_or_else(|| format!("cpu {c} has no APIC ID"))?;
        cpus.push((c, apic, t));
    }
    let p = report::topology_problems(&cpus);
    kassert!(p.is_empty(), "{}", p.join("; "));
    let s = report::shape(&cpus.iter().map(|c| c.2).collect::<Vec<_>>());
    kassert!(s.cpus == n && s.cores >= 1 && s.packages >= 1 && s.packages <= s.cores);
    kassert!(s.max_threads_per_core >= 1);
    // The hybrid bit decides whether core types are reported at all.
    let hybrid = cpu::live().has("hybrid_cpu");
    kassert!(hybrid || (s.p_cores == 0 && s.e_cores == 0), "core types on a CPU without the hybrid bit");
    // CP-6 capacity hint: in 1..=1024 everywhere, 1024 on some CPU, and uniform (all 1024) without the hybrid bit.
    let caps: Vec<u16> = (0..n).filter_map(smp::capacity).collect();
    kassert!(caps.len() == n, "capacity missing for an online CPU");
    kassert!(caps.iter().all(|&c| (1..=smp::CAPACITY_SCALE).contains(&c)), "capacity out of range: {caps:?}");
    kassert!(caps.contains(&smp::CAPACITY_SCALE), "no CPU at full capacity: {caps:?}");
    kassert!(hybrid || caps.iter().all(|&c| c == smp::CAPACITY_SCALE), "non-uniform capacity without the hybrid bit: {caps:?}");
    kassert!(hybrid || smp::max_perf() == 0, "HWP number read on a non-hybrid CPU");
    if n == 1 {
        return ktest::skip("one CPU: only the BSP's topology checked");
    }
    Ok(())
}

// Register reads on the target CPU (hard-IRQ context there: no heap, no locks).
const R_CR0: u64 = 0;
const R_CR4: u64 = 1;
const R_EFER: u64 = 2;
const R_XCR0: u64 = 3;
const R_PAT: u64 = 4;
const R_TSC_AUX: u64 = 5;
const R_GS_SELF: u64 = 6;
const R_THIS_CPU: u64 = 7;
const R_APIC_ID: u64 = 8;
const R_LSTAR: u64 = 9;
const R_STAR: u64 = 10;
const R_XSS: u64 = 11;
const R_IST1: u64 = 21;
/// `R_MSR | n` reads MSR n (only MSRs the caller knows exist on that CPU).
const R_MSR: u64 = 1 << 32;

fn read_reg(sel: u64) -> u64 {
    if sel & R_MSR != 0 {
        return hal::msr::read(sel as u32);
    }
    match sel {
        R_CR0 => cpu::read_cr0(),
        R_CR4 => cpu::read_cr4(),
        R_EFER => hal::msr::read(hal::msr::IA32_EFER),
        R_XCR0 => cpu::read_xcr0(),
        R_PAT => hal::msr::read(hal::msr::IA32_PAT),
        R_TSC_AUX => hal::msr::read(hal::msr::IA32_TSC_AUX),
        R_GS_SELF => percpu::gs_self(),
        R_THIS_CPU => percpu::this_cpu() as u64,
        R_APIC_ID => apic::id().map_or(u64::MAX, |i| i as u64),
        R_LSTAR => hal::msr::read(hal::msr::IA32_LSTAR),
        R_STAR => hal::msr::read(hal::msr::IA32_STAR),
        R_XSS => hal::msr::read(hal::msr::IA32_XSS),
        s if (R_IST1..R_IST1 + 3).contains(&s) => hal::gdt::tss_ist((s - R_IST1 + 1) as u8),
        _ => u64::MAX,
    }
}

fn ap_registers() -> R {
    let aps = aps()?;
    let f = cpu::detect();
    let bsp = |sel| read_reg(sel);
    let cr4_mask = !cpu::CR4_PCIDE;
    for c in aps {
        let r = |sel| call(c, read_reg, sel);
        let cr0 = r(R_CR0)?;
        kassert!(cr0 == cpu::CR0_WANTED, "cpu {c}: CR0 {cr0:#x}, wanted {:#x} (CD/NW clear, written whole)", cpu::CR0_WANTED);
        let cr4 = r(R_CR4)?;
        kassert!(cr4 & cr4_mask == bsp(R_CR4) & cr4_mask, "cpu {c}: CR4 {cr4:#x}, BSP {:#x}", bsp(R_CR4));
        let efer = r(R_EFER)?;
        kassert!(efer == bsp(R_EFER), "cpu {c}: EFER {efer:#x}, BSP {:#x}", bsp(R_EFER));
        let xcr0 = r(R_XCR0)?;
        kassert!(xcr0 == bsp(R_XCR0), "cpu {c}: XCR0 {xcr0:#x}, BSP {:#x}", bsp(R_XCR0));
        let pat = r(R_PAT)?;
        kassert!(pat == hal::pat::LINUX_PAT, "cpu {c}: PAT {pat:#x}");
        if f.rdtscp || f.rdpid {
            let aux = r(R_TSC_AUX)?;
            kassert!(cpu::tsc_aux_split(aux).0 as usize == c, "cpu {c}: TSC_AUX {aux:#x}");
        }
        let gs = r(R_GS_SELF)?;
        kassert!(gs != 0 && gs == percpu::local_addr_of(c), "cpu {c}: gs:[0] {gs:#x}, its block {:#x}", percpu::local_addr_of(c));
        kassert!(gs != percpu::local_addr_of(0), "cpu {c} shares the BSP's GS block");
        kassert!(r(R_THIS_CPU)? as usize == c, "cpu {c}: this_cpu says {}", r(R_THIS_CPU)?);
        let id = r(R_APIC_ID)?;
        kassert!(Some(id as u32) == smp::apic_id(c), "cpu {c}: LAPIC ID {id}, map {:?}", smp::apic_id(c));
        kassert!(r(R_LSTAR)? == bsp(R_LSTAR) && r(R_STAR)? == bsp(R_STAR), "cpu {c}: SYSCALL MSRs differ from the BSP's");
        if f.xsaves {
            kassert!(r(R_XSS)? == 0, "cpu {c}: IA32_XSS {:#x}", r(R_XSS)?);
        }
        let ist = hal::gdt::ist_stacks_of(c).ok_or_else(|| format!("cpu {c}: no tables"))?;
        for (k, s) in ist.iter().enumerate() {
            let top = r(R_IST1 + k as u64)?;
            kassert!(top == s.top, "cpu {c}: TSS IST{} {top:#x}, its stack top {:#x}", k + 1, s.top);
        }
    }
    Ok(())
}

fn mtrr() -> R {
    let aps = aps()?;
    let Some(bsp) = cpu::mtrr_state() else { return ktest::skip("no CPUID MTRR") };
    let o = outcome().ok_or("no outcome")?;
    for c in aps {
        let def = call(c, read_reg, R_MSR | hal::msr::IA32_MTRR_DEF_TYPE as u64)?;
        kassert!(def == bsp.def_type, "cpu {c}: MTRRdefType {def:#x}, BSP {:#x}", bsp.def_type);
        let cap = call(c, read_reg, R_MSR | hal::msr::IA32_MTRRCAP as u64)?;
        kassert!(cap == bsp.cap, "cpu {c}: MTRRcap {cap:#x}, BSP {:#x}", bsp.cap);
        for &(m, v) in &bsp.ranges {
            let mine = call(c, read_reg, R_MSR | m as u64)?;
            kassert!(mine == v, "cpu {c}: MTRR {m:#x} = {mine:#x}, BSP {v:#x}");
        }
        if let Some(r) = o.up.iter().find(|r| r.cpu as usize == c) {
            kassert!(!matches!(r.mtrr, smp::MtrrOutcome::Copied { verified: false, .. }), "cpu {c}: copy not verified");
        }
    }
    Ok(())
}

static POKE_MSR: AtomicU64 = AtomicU64::new(0);
static POKE_VAL: AtomicU64 = AtomicU64::new(0);

fn poke(_: u64) -> u64 {
    hal::msr::write(POKE_MSR.load(Ordering::Acquire) as u32, POKE_VAL.load(Ordering::Acquire));
    0
}

fn resync(_: u64) -> u64 {
    match smp::resync_mtrr() {
        Some(Ok(true)) => 1,
        Some(Ok(false)) => 0,
        None => 2,
        Some(Err(_)) => 3,
    }
}

/// The MTRR copy path on demand (KVM keeps an AP's MTRRs over INIT, so start-up may not have needed it): one
/// unused variable range of the last AP (mask valid bit clear, so the change has no effect) gets another base,
/// the AP resynchronises from the BSP's snapshot by the same procedure as at start-up and must end equal.
/// QemuOnly: it writes MTRRs on purpose.
fn mtrr_copy() -> R {
    let aps = aps()?;
    let c = *aps.last().ok_or("no AP")?;
    let bsp = smp::bsp_mtrr().ok_or("no BSP MTRR snapshot")?;
    let vcnt = (bsp.cap & 0xFF) as u32;
    let unused = (0..vcnt).map(|i| 0x200 + 2 * i).find(|&base| bsp.ranges.iter().any(|&(m, v)| m == base + 1 && v & (1 << 11) == 0));
    let Some(base_msr) = unused else { return ktest::skip("every variable MTRR is in use") };
    let old = bsp.ranges.iter().find(|r| r.0 == base_msr).map(|r| r.1).ok_or("base not in the snapshot")?;
    POKE_MSR.store(base_msr as u64, Ordering::Release);
    POKE_VAL.store(old ^ (1 << 20), Ordering::Release);
    call(c, poke, 0)?;
    let changed = call(c, read_reg, R_MSR | base_msr as u64)?;
    kassert!(changed == old ^ (1 << 20), "cpu {c}: poke of MTRR {base_msr:#x} did not stick ({changed:#x})");
    let r = call(c, resync, 0)?;
    kassert!(r == 1, "cpu {c}: resync returned {r} (1 = copied)");
    let back = call(c, read_reg, R_MSR | base_msr as u64)?;
    kassert!(back == old, "cpu {c}: MTRR {base_msr:#x} = {back:#x} after the copy, BSP {old:#x}");
    let def = call(c, read_reg, R_MSR | hal::msr::IA32_MTRR_DEF_TYPE as u64)?;
    kassert!(def == bsp.def_type, "cpu {c}: MTRRdefType {def:#x} after the copy, BSP {:#x}", bsp.def_type);
    kassert!(call(c, resync, 0)? == 0, "cpu {c}: a second resync found differences");
    let cr0 = call(c, read_reg, R_CR0)?;
    kassert!(cr0 == cpu::CR0_WANTED, "cpu {c}: CR0 {cr0:#x} after the copy (caches must be on again)");
    Ok(())
}

static HITS: PerCpu<AtomicU64> = PerCpu::new(|_| AtomicU64::new(0));

fn hit(_v: u8) {
    HITS.get().fetch_add(1, Ordering::Relaxed);
}

fn wait_for(cond: impl Fn() -> bool) -> bool {
    let deadline = time::now_ns().saturating_add(CALL_NS);
    while !cond() {
        if time::now_ns() > deadline {
            return cond();
        }
        core::hint::spin_loop();
    }
    true
}

fn ipi_every_cpu() -> R {
    let aps = aps()?;
    for c in aps {
        let v = vectors::alloc(hit, Some(c as u32)).map_err(|e| format!("cpu {c}: vector: {e:?}"))?;
        let before = HITS.get_for(c).map_or(0, |h| h.load(Ordering::Relaxed));
        let apic_id = smp::apic_id(c).ok_or("no APIC ID")?;
        let sent = apic::send_ipi(IpiDest::Physical(apic_id), IpiKind::Fixed(v.vector));
        let ok = sent.is_ok() && wait_for(|| HITS.get_for(c).is_some_and(|h| h.load(Ordering::Relaxed) > before));
        let delivered = vectors::count(v);
        vectors::free(v);
        kassert!(sent.is_ok(), "cpu {c}: send {sent:?}");
        kassert!(ok, "cpu {c} (APIC {apic_id}): fixed IPI on {:#x} not handled within 1 s", v.vector);
        kassert!(delivered >= 1, "cpu {c}: vector table counted {delivered}");
        // A wake-up IPI is counted too.
        let (_, wakes) = smp::ipi_counts(c).ok_or("no slot")?;
        smp::wake(c).map_err(|e| format!("cpu {c}: wake {e:?}"))?;
        kassert!(wait_for(|| smp::ipi_counts(c).is_some_and(|(_, w)| w > wakes)), "cpu {c}: wake-up IPI not seen");
    }
    Ok(())
}

fn call_every_cpu() -> R {
    let aps = aps()?;
    fn square_plus_cpu(x: u64) -> u64 {
        x * x + percpu::this_cpu() as u64
    }
    for c in aps {
        let (calls, _) = smp::ipi_counts(c).ok_or("no slot")?;
        let v = call(c, square_plus_cpu, 12)?;
        kassert!(v == 144 + c as u64, "cpu {c}: result {v}");
        kassert!(smp::ipi_counts(c).is_some_and(|(n, _)| n > calls));
    }
    // On this CPU the call runs directly.
    kassert!(call(percpu::this_cpu(), square_plus_cpu, 3)? == 9 + percpu::this_cpu() as u64);
    kassert!(smp::call_on(percpu::cpu_count() + 3, square_plus_cpu, 0, time::now_ns, CALL_NS) == Err(smp::CallError::NotOnline));
    Ok(())
}

static COUNTS: PerCpu<AtomicU64> = PerCpu::new(|_| AtomicU64::new(0));

fn percpu_values() -> R {
    let aps = aps()?;
    fn bump(_: u64) -> u64 {
        COUNTS.get().fetch_add(1, Ordering::Relaxed);
        percpu::this_cpu() as u64
    }
    let before: Vec<u64> = COUNTS.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    for &c in &aps {
        kassert!(call(c, bump, 0)? == c as u64);
    }
    for (i, v) in COUNTS.iter().enumerate() {
        let now = v.load(Ordering::Relaxed);
        let want = before[i] + (i != 0) as u64;
        kassert!(now == want, "cpu {i}: count {now}, wanted {want} (each CPU bumps only its own value)");
    }
    kassert!(COUNTS.iter().count() == percpu::cpu_count());
    kassert!(percpu::possible_cpus() >= percpu::cpu_count());
    Ok(())
}

fn ist_stacks() -> R {
    let aps = aps()?;
    let root = hal::paging::current_root();
    let mut seen: Vec<u64> = hal::gdt::ist_stacks_of(0).ok_or("no BSP tables")?.iter().map(|s| s.top).collect();
    for c in aps {
        for s in hal::gdt::ist_stacks_of(c).ok_or_else(|| format!("cpu {c}: no tables"))? {
            let g = s.guard.ok_or_else(|| format!("cpu {c}: IST{} without a guard", s.ist))?;
            kassert!(hal::paging::translate(root, g).is_none(), "cpu {c}: IST{} guard mapped", s.ist);
            kassert!(s.top - s.bottom == hal::gdt::IST_STACK_PAGES * 4096);
            let mut a = s.bottom;
            while a < s.top {
                let (_, fl) = hal::paging::translate(root, a).ok_or_else(|| format!("cpu {c}: IST{} page {a:#x} unmapped", s.ist))?;
                kassert!(fl.contains(hal::paging::PageFlags::WRITABLE | hal::paging::PageFlags::NX));
                a += 4096;
            }
            kassert!(!seen.contains(&s.top), "cpu {c}: IST{} shared with another CPU", s.ist);
            seen.push(s.top);
        }
    }
    Ok(())
}

fn boot_stacks() -> R {
    let aps = aps()?;
    let root = hal::paging::current_root();
    for c in aps {
        let (guard, bottom, top) = smp::boot_stack(c).ok_or("no slot")?;
        kassert!(guard != 0 && bottom == guard + 4096 && top - bottom == smp::AP_STACK_PAGES * 4096, "cpu {c}: stack {guard:#x} {bottom:#x} {top:#x}");
        kassert!(hal::paging::translate(root, guard).is_none(), "cpu {c}: boot stack guard mapped");
        kassert!(hal::paging::translate(root, top - 8).is_some() && hal::paging::translate(root, bottom).is_some());
    }
    Ok(())
}

fn per_cpu_vectors() -> R {
    let aps = aps()?;
    fn h(_: u8) {}
    for c in aps {
        let st = vectors::stats(c as u32).ok_or_else(|| format!("cpu {c}: no vector table"))?;
        let v = vectors::alloc(h, Some(c as u32)).map_err(|e| format!("cpu {c}: {e:?}"))?;
        kassert!(v.cpu as usize == c && vectors::is_registered(v));
        kassert!(vectors::stats(c as u32).map(|s| s.dynamic_taken) == Some(st.dynamic_taken + 1));
        vectors::free(v);
    }
    Ok(())
}

fn tsc_offsets() -> R {
    let aps = aps()?;
    let o = outcome().ok_or("no outcome")?;
    let hyper = cpu::detect().hypervisor;
    for c in aps {
        let r = o.up.iter().find(|r| r.cpu as usize == c).ok_or_else(|| format!("cpu {c}: no record"))?;
        let t = r.tsc.ok_or_else(|| format!("cpu {c}: no TSC round came back"))?;
        kassert!(t.rounds >= 1 && t.rounds <= smp::TSC_ROUNDS as usize, "cpu {c}: {} rounds", t.rounds);
        // Generous on hardware (the Mini-PC check list judges the logged value against 38 ticks); none in QEMU.
        kassert!(hyper || t.magnitude() < 100_000, "cpu {c}: TSC offset {} ticks", t.median);
    }
    Ok(())
}

fn apic_map() -> R {
    let n = percpu::cpu_count();
    let mut ids = Vec::new();
    for c in 0..n {
        let id = smp::apic_id(c).ok_or_else(|| format!("cpu {c} has no APIC ID"))?;
        kassert!(smp::cpu_of_apic(id) == Some(c), "APIC {id} maps back to {:?}, not {c}", smp::cpu_of_apic(id));
        kassert!(!ids.contains(&id), "APIC ID {id} twice");
        ids.push(id);
    }
    kassert!(smp::apic_id(n).is_none(), "a CPU beyond cpu_count has an APIC ID");
    kassert!(Some(ids[0]) == apic::id(), "BSP's APIC ID");
    if n == 1 {
        return ktest::skip("one CPU");
    }
    Ok(())
}

/// The value `mix` returns for `arg` on `cpu` (each caller knows what its own request must give back).
fn mix_expected(arg: u64, cpu: usize) -> u64 {
    arg.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ cpu as u64
}

fn mix(arg: u64) -> u64 {
    mix_expected(arg, percpu::this_cpu())
}

const HAMMER_ROUNDS: u64 = 3000;
/// Retries of one call while the target's mailbox belongs to the other caller.
const BUSY_RETRIES: u32 = 10_000_000;

/// `rounds` calls of `mix` on `target`, each with an argument tagged by `tag`: (right results, wrong or lost).
fn hammer_loop(target: usize, tag: u64, rounds: u64) -> (u64, u64) {
    let (mut ok, mut bad) = (0, 0);
    for i in 0..rounds {
        let arg = (tag << 32) | i;
        let mut tries = 0;
        loop {
            match smp::call_on(target, mix, arg, time::now_ns, CALL_NS) {
                Ok(r) if r == mix_expected(arg, target) => ok += 1,
                Err(smp::CallError::Busy) if tries < BUSY_RETRIES => {
                    tries += 1;
                    core::hint::spin_loop();
                    continue;
                }
                _ => bad += 1,
            }
            break;
        }
    }
    (ok, bad)
}

static HAMMER_OK: AtomicU64 = AtomicU64::new(0);
static HAMMER_BAD: AtomicU64 = AtomicU64::new(0);
static HAMMER_DONE: AtomicU64 = AtomicU64::new(0);

/// Runs on the first AP (from its SMP_CALL interrupt): hammers the second AP while the BSP does the same.
fn hammer(x: u64) -> u64 {
    let (ok, bad) = hammer_loop((x & 0xFFFF) as usize, x >> 16, HAMMER_ROUNDS);
    HAMMER_OK.store(ok, Ordering::Relaxed);
    HAMMER_BAD.store(bad, Ordering::Relaxed);
    HAMMER_DONE.store(1, Ordering::Release);
    0
}

/// Two callers (the BSP and an AP) call the same target at once: each gets its own results, none is lost or
/// stolen, and a caller that gave up (`Late`) leaves the target `Busy` until its call ends, then free again.
fn call_owners() -> R {
    let aps = aps()?;
    if aps.len() < 2 {
        return ktest::skip("needs two APs (-smp 3 or more)");
    }
    let (helper, target) = (aps[0], aps[1]);
    HAMMER_DONE.store(0, Ordering::Release);
    // A short timeout on purpose: the helper keeps hammering after this caller returned `Late`.
    let mut started = false;
    for _ in 0..100 {
        match smp::call_on(helper, hammer, (target as u64) | (0xA << 16), time::now_ns, 2_000_000) {
            Ok(_) | Err(smp::CallError::Late) => {
                started = true;
                break;
            }
            Err(smp::CallError::Timeout) => continue,
            Err(e) => return Err(format!("start the helper: {e:?}")),
        }
    }
    kassert!(started, "the helper never took the call");
    if HAMMER_DONE.load(Ordering::Acquire) == 0 {
        let r = smp::call_on(helper, mix, 1, time::now_ns, CALL_NS);
        kassert!(r == Err(smp::CallError::Busy) || HAMMER_DONE.load(Ordering::Acquire) == 1, "helper's mailbox while its abandoned call runs: {r:?}");
    }
    let (ok, bad) = hammer_loop(target, 0xB, HAMMER_ROUNDS);
    kassert!(bad == 0 && ok == HAMMER_ROUNDS, "BSP: {ok} right, {bad} wrong or lost");
    // Generous: under TCG thousands of IPI round trips take a while.
    let deadline = time::now_ns().saturating_add(60 * CALL_NS);
    while HAMMER_DONE.load(Ordering::Acquire) == 0 && time::now_ns() < deadline {
        core::hint::spin_loop();
    }
    kassert!(HAMMER_DONE.load(Ordering::Acquire) == 1, "the helper did not finish");
    let (hok, hbad) = (HAMMER_OK.load(Ordering::Relaxed), HAMMER_BAD.load(Ordering::Relaxed));
    kassert!(hbad == 0 && hok == HAMMER_ROUNDS, "helper: {hok} right, {hbad} wrong or lost");
    // The helper freed its own mailbox when the abandoned call ended.
    kassert!(wait_for(|| smp::call_on(helper, mix, 7, time::now_ns, CALL_NS) == Ok(mix_expected(7, helper))), "helper's mailbox stays taken");
    Ok(())
}

/// `start_ap` refuses a running CPU's APIC ID (INIT would reset it) and a wrong CPU number, and `prepare` runs
/// once. The frame sources give nothing, so even a broken check could not reach an INIT (it would stop at the
/// first allocation).
fn start_guards() -> R {
    let mut none = || None;
    let mut keep = |_p: u64| {};
    let mut table = || None;
    fn no_free(_: u64) {}
    let mut env = smp::Env { alloc_stack_page: &mut none, free_stack_page: &mut keep, alloc_table: &mut table, free_frame: no_free, clock: time::now_ns };
    let nmi = apic::NmiPin { lint: 1, active_low: false };
    let next = percpu::cpu_count() as u32;
    for c in 0..percpu::cpu_count() {
        let id = smp::apic_id(c).ok_or_else(|| format!("cpu {c}: no APIC ID"))?;
        let req = smp::ApRequest { cpu: next, apic_id: id, node: 0, nmi, ready_ns: 1 };
        let r = smp::start_ap(&req, &mut env).map(|_| ());
        kassert!(r == Err(smp::SmpError::AlreadyOnline) || (outcome().is_some_and(|o| o.not_started.is_some()) && r == Err(smp::SmpError::NotPrepared)), "cpu {c} (APIC {id}): {r:?}");
    }
    let req = smp::ApRequest { cpu: 0, apic_id: u32::MAX - 1, node: 0, nmi, ready_ns: 1 };
    let r = smp::start_ap(&req, &mut env).map(|_| ());
    kassert!(matches!(r, Err(smp::SmpError::WrongIndex) | Err(smp::SmpError::NotPrepared)), "CPU number 0: {r:?}");
    if let Some(t) = acpi::trampoline()
        && outcome().is_some_and(|o| o.not_started.is_none())
    {
        let r = smp::prepare(t, 0, &mut || None).map(|_| ());
        kassert!(r == Err(smp::SmpError::AlreadyPrepared), "second prepare: {r:?}");
    }
    Ok(())
}

/// The Mini-PC check (spec §6.1): every AP's TSC offset within 38 ticks of the BSP's. Hardware only.
fn tsc_offsets_hardware() -> R {
    if cpu::detect().hypervisor {
        return ktest::skip("timing is never judged under a hypervisor");
    }
    let aps = aps()?;
    let o = outcome().ok_or("no outcome")?;
    for c in aps {
        let r = o.up.iter().find(|r| r.cpu as usize == c).ok_or_else(|| format!("cpu {c}: no record"))?;
        let t = r.tsc.ok_or_else(|| format!("cpu {c}: no TSC round came back"))?;
        kassert!(t.magnitude() <= crate::tsc::CHECK_TICKS, "cpu {c}: TSC offset {} ticks (best {}, rtt {}, TSC_ADJUST {:?})", t.median, t.best, t.min_rtt, r.tsc_adjust);
    }
    Ok(())
}

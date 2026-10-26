//! CP-1 ktests: every set-up bit as CPUID says, faults caught by hooks and resumed, IST stacks with guards.
//! All `Safe` (they run on the Mini-PC too) except the real double fault (`QemuOnly`; hal refuses it on
//! hardware anyway). They test the CPU they run on (the BSP); `smp_ktest` checks the APs.

use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hal::cpu::{self, Features};
use hal::gdt;
use hal::idt::{self, ExceptionHook, ProbeTarget, TrapFrame};
use ksync::SpinLock;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("cp.cpu.cr0", Safe, cr0);
ktest!("cp.cpu.cr4", Safe, cr4);
ktest!("cp.cpu.efer", Safe, efer);
ktest!("cp.cpu.xcr0", Safe, xcr0);
ktest!("cp.cpu.pat", Safe, pat);
ktest!("cp.cpu.tsc_aux", Safe, tsc_aux);
ktest!("cp.cpu.v3", Safe, v3);
ktest!("cp.cpu.facts", Safe, facts);
ktest!("cp.cpu.mca", Safe, mca);
ktest!("cp.cpu.fpu_reset", Safe, fpu_reset);
ktest!("cp.msr.read_checked", Safe, msr_read_checked);
ktest!("cp.cpu.vendor_paths", Safe, vendor_paths);
ktest!("cp.percpu.gs", Safe, percpu_gs);
ktest!("cp.trap.gp_resume", Safe, gp_resume);
ktest!("cp.trap.pf_resume", Safe, pf_resume);
ktest!("cp.ist.guards", Safe, ist_guards);
ktest!("cp.ist.nmi", Safe, ist_nmi);
ktest!("cp.ist.machine_check", Safe, ist_mc);
ktest!("cp.ist.double_fault", QemuOnly, ist_double_fault);

fn cr0() -> R {
    let v = cpu::read_cr0();
    kassert!(v == cpu::CR0_WANTED, "CR0 {v:#x}, wanted {:#x}", cpu::CR0_WANTED);
    Ok(())
}

fn cr4() -> R {
    let f = cpu::detect();
    let v = cpu::read_cr4();
    let want: [(&str, u64, bool); 13] = [
        ("OSFXSR", cpu::CR4_OSFXSR, true),
        ("OSXMMEXCPT", cpu::CR4_OSXMMEXCPT, true),
        ("OSXSAVE", cpu::CR4_OSXSAVE, f.xsave),
        ("SMEP", cpu::CR4_SMEP, f.smep),
        ("SMAP", cpu::CR4_SMAP, f.smap),
        ("UMIP", cpu::CR4_UMIP, f.umip),
        ("FSGSBASE", cpu::CR4_FSGSBASE, f.fsgsbase),
        ("MCE", cpu::CR4_MCE, f.mce),
        ("PGE", cpu::CR4_PGE, f.pge),
        ("TSD", cpu::CR4_TSD, false),
        ("PCE", cpu::CR4_PCE, false),
        ("PKE", cpu::CR4_PKE, false),
        ("CET", cpu::CR4_CET, false),
    ];
    for (name, bit, on) in want {
        kassert!((v & bit != 0) == on, "CR4.{name} is {}, CPUID wants {on} (CR4 {v:#x})", v & bit != 0);
    }
    kassert!(cpu::smap_enabled() == f.smap);
    kassert!(v & cpu::CR4_LA57 == 0, "CR4.LA57 on");
    // CET off: the enable MSRs are zero too, not only CR4.CET.
    if f.shstk || f.ibt {
        let (u, s) = (hal::msr::read(hal::msr::IA32_U_CET), hal::msr::read(hal::msr::IA32_S_CET));
        kassert!(u == 0 && s == 0, "U_CET {u:#x} S_CET {s:#x}");
    }
    // CPUID.1:ECX.OSXSAVE mirrors CR4.OSXSAVE: what programs check before using AVX.
    kassert!(cpu::live().has("osxsave") == f.xsave, "CPUID OSXSAVE does not mirror CR4");
    Ok(())
}

fn efer() -> R {
    let f = cpu::detect();
    let v = hal::msr::read(hal::msr::IA32_EFER);
    kassert!(v & hal::msr::EFER_LMA != 0, "EFER.LMA off: {v:#x}");
    kassert!(v & hal::msr::EFER_SCE != 0, "EFER.SCE off: {v:#x}");
    kassert!((v & hal::msr::EFER_NXE != 0) == f.nx, "EFER.NXE {v:#x}, nx {}", f.nx);
    Ok(())
}

/// Bytes of the standard XSAVE area for `xcr0`, from CPUID 0xD's per-state offsets and sizes.
fn xsave_bytes(xcr0: u64) -> u32 {
    let v = cpu::live();
    let mut end = 512 + 64;
    for i in 2..63u32 {
        if xcr0 & (1 << i) != 0 {
            let d = v.get(0xD, i);
            end = end.max(d.ebx + d.eax);
        }
    }
    end
}

fn xcr0() -> R {
    let f = cpu::detect();
    kassert!(f.xsave, "no XSAVE (R10 requires it)");
    let supported = cpu::xcr0_supported(&cpu::live());
    let want = cpu::xcr0_wanted(supported);
    let have = cpu::read_xcr0();
    kassert!(have == want, "XCR0 {have:#x}, wanted {want:#x} (CPUID offers {supported:#x})");
    kassert!(have & 0x7 == 0x7, "x87/SSE/AVX not all on: {have:#x}");
    let avx512 = f.avx512f && supported & cpu::XSTATE_AVX512 == cpu::XSTATE_AVX512;
    kassert!((have & cpu::XSTATE_AVX512 == cpu::XSTATE_AVX512) == avx512, "AVX-512 states {have:#x}, CPU offers {avx512}");
    // CPUID 0xD.0 EBX follows the XCR0 now set: SC sizes the per-thread area from it.
    let facts = cpu::facts();
    let expect = xsave_bytes(have);
    kassert!(facts.xsave_size == expect, "XSAVE size {} for XCR0 {have:#x}, offsets say {expect}", facts.xsave_size);
    Ok(())
}

fn pat() -> R {
    let v = hal::msr::read(hal::msr::IA32_PAT);
    kassert!(v == hal::pat::LINUX_PAT, "PAT {v:#x}, Linux layout {:#x}", hal::pat::LINUX_PAT);
    Ok(())
}

fn tsc_aux() -> R {
    let f = cpu::detect();
    if !(f.rdtscp || f.rdpid) {
        return Ok(());
    }
    let aux = hal::msr::read(hal::msr::IA32_TSC_AUX);
    let cpu_index = hal::percpu::this_cpu() as u32;
    kassert!(aux == cpu::tsc_aux_value(cpu_index, 0), "TSC_AUX {aux:#x}, CPU {cpu_index} node 0");
    kassert!(cpu::tsc_aux_split(aux) == (cpu_index, 0));
    if f.rdpid {
        kassert!(cpu::rdpid() == Some(aux), "rdpid {:?} vs TSC_AUX {aux}", cpu::rdpid());
    }
    Ok(())
}

fn v3() -> R {
    let m = cpu::missing_v3_features();
    kassert!(m.is_empty(), "missing x86-64-v3 features: {m:?}");
    Ok(())
}

fn facts() -> R {
    let f = cpu::detect();
    let x = cpu::facts();
    kassert!(!x.vendor.is_empty() && x.family != 0, "vendor {:?} family {}", x.vendor, x.family);
    kassert!(x.flags.len() == cpu::FLAGS.len());
    for (name, have) in [("avx512f", f.avx512f), ("pcid", f.pcid), ("invpcid", f.invpcid), ("smap", f.smap), ("nx", f.nx)] {
        kassert!(x.has(name) == have, "facts {name} {} vs features {have}", x.has(name));
    }
    kassert!(x.has("avx2") && x.has("bmi2") && x.has("xsave"));
    kassert!(x.cr0 == cpu::read_cr0() && x.cr4 == cpu::read_cr4() && x.xcr0 == cpu::read_xcr0());
    kassert!(x.phys_addr_bits >= 36 && x.virt_addr_bits >= 48, "address bits {} / {}", x.phys_addr_bits, x.virt_addr_bits);
    kassert!(x.missing_v3.is_empty());
    Ok(())
}

fn mca() -> R {
    let f = cpu::detect();
    let r = cpu::mca_report(&f);
    kassert!(r.supported == (f.mca && f.mce));
    if r.supported {
        kassert!(cpu::read_cr4() & cpu::CR4_MCE != 0, "CR4.MCE off");
        kassert!(r.banks == cpu::facts().mca_banks);
        kassert!(r.records.iter().all(|x| x.bank < r.banks && x.status & cpu::MCI_STATUS_VAL != 0));
    }
    Ok(())
}

/// The x87 control word and MXCSR are the reset values `init_this_cpu` loaded (the kernel is soft-float, so
/// nothing changes them in kernel code).
fn fpu_reset() -> R {
    let (fcw, mxcsr) = cpu::fpu_control_words();
    kassert!(fcw == 0x037F, "x87 control word {fcw:#x}, fninit gives 0x37f");
    kassert!(mxcsr & 0xFFFF == 0x1F80, "MXCSR {mxcsr:#x}, reset value 0x1f80");
    Ok(())
}

/// CP-6: the vendor's own paths. Intel never reaches an AMD-only MSR and AMD no Intel-only one; Intel RAPL only by
/// model on bare metal; every CPU's timer is TSC-deadline exactly where CPUID offers it (unless CONFIG says
/// oneshot); hybrid CPUs with HWP recorded a performance number. Read-only (rerunnable on the Mini-PC).
fn vendor_paths() -> R {
    use hal::msr;
    let f = cpu::detect();
    let v = cpu::live();
    kassert!(!(f.intel && f.amd), "Intel and AMD at once");
    if f.intel {
        kassert!(!cpu::amd_mtrr_msrs(&v), "SYSCFG/TOM/TOM2 path taken on Intel");
        kassert!(msr::read_checked(msr::AMD_SYSCFG).is_none(), "msr_read allows SYSCFG on Intel");
        kassert!(msr::read_checked(msr::AMD_PKG_ENERGY_STATUS).is_none(), "AMD RAPL allowed on Intel");
    }
    if f.amd {
        for m in [msr::INTEL_RAPL_POWER_UNIT, msr::INTEL_PKG_ENERGY_STATUS, msr::IA32_PM_ENABLE, msr::IA32_HWP_CAPABILITIES] {
            kassert!(msr::read_checked(m).is_none(), "Intel-only MSR {m:#x} allowed on AMD");
        }
    }
    let rapl = msr::intel_rapl_model(&v);
    kassert!(!(rapl && f.hypervisor), "Intel RAPL allowed in a guest");
    kassert!(msr::read_checked(msr::INTEL_RAPL_POWER_UNIT).is_some() == rapl);
    // HWP_CAPABILITIES is readable exactly when CPUID hwp and PM_ENABLE bit 0 (else it would #GP).
    let hwp_on = msr::read_checked(msr::IA32_PM_ENABLE).is_some_and(|x| x & 1 != 0);
    kassert!(msr::read_checked(msr::IA32_HWP_CAPABILITIES).is_some() == hwp_on, "HWP_CAPABILITIES gate");
    if v.has("hybrid_cpu") && hwp_on && f.intel {
        kassert!(hal::smp::max_perf() != 0, "hybrid CPU with HWP on but no performance number");
    }
    let deadline = v.has("tsc_deadline_timer");
    let cfg = time::lapic::config();
    if cfg == time::lapic::Config::Poll {
        return ktest::skip("hrtimer=poll: no LAPIC timers to check");
    }
    for c in 0..hal::percpu::cpu_count() {
        let Some(kind) = time::lapic::cpu_stats(c).and_then(|s| s.kind) else { continue };
        let want = if deadline && cfg != time::lapic::Config::OneShot { time::lapic::Kind::TscDeadline } else { time::lapic::Kind::OneShot };
        kassert!(kind == want, "CPU {c}: timer {} with tsc_deadline_timer={deadline}, config {}", kind.name(), cfg.name());
    }
    Ok(())
}

/// MCP's msr_read path: listed MSRs read, unknown ones and absent banks refused (never a #GP).
fn msr_read_checked() -> R {
    use hal::msr;
    let f = cpu::detect();
    kassert!(msr::read_checked(msr::IA32_EFER) == Some(msr::read(msr::IA32_EFER)));
    kassert!(msr::read_checked(0x10).is_none(), "TSC is not on the list");
    kassert!(msr::read_checked(0x480).is_none(), "VMX_BASIC is never a bank");
    kassert!(msr::read_checked(msr::IA32_TSC_AUX).is_some() == (f.rdtscp || f.rdpid));
    let caps = msr::ReadCaps::live();
    if caps.mca {
        let last = caps.mca_banks.min(if caps.smca { 256 } else { msr::LEGACY_MCA_BANKS });
        for b in 0..last {
            let status = msr::mc_reg(caps.smca, b, msr::MC_STATUS).ok_or("no STATUS address")?;
            kassert!(msr::read_checked(status).is_some(), "bank {b} STATUS refused");
            // ADDR/MISC: read only with ADDRV/MISCV; with the bit clear the read must be refused.
            let st = msr::read(status);
            for (reg, bit) in [(msr::MC_ADDR, cpu::MCI_STATUS_ADDRV), (msr::MC_MISC, cpu::MCI_STATUS_MISCV)] {
                let m = msr::mc_reg(caps.smca, b, reg).ok_or("no register address")?;
                kassert!(msr::read_checked(m).is_some() == (st & bit != 0), "bank {b} reg {reg} status {st:#x}");
            }
        }
        let past = msr::mc_reg(caps.smca, caps.mca_banks, msr::MC_STATUS);
        kassert!(past.is_none_or(|m| msr::read_checked(m).is_none()), "bank past MCG_CAP.Count allowed");
    }
    Ok(())
}

fn percpu_gs() -> R {
    let gs = hal::percpu::gs_self();
    let p = hal::percpu::local_ptr() as u64;
    kassert!(gs == p, "gs:[0] {gs:#x}, CpuLocal at {p:#x}");
    kassert!(hal::msr::read(hal::msr::IA32_GS_BASE) == p);
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Hooks: one test at a time installs a hook, records what the trap saw, and puts the previous hook back.

struct Hit {
    count: AtomicU64,
    vector: AtomicU64,
    rip: AtomicU64,
    error: AtomicU64,
    cr2: AtomicU64,
    frame: AtomicU64,
    rsp: AtomicU64,
}

static HIT: Hit = Hit {
    count: AtomicU64::new(0),
    vector: AtomicU64::new(0),
    rip: AtomicU64::new(0),
    error: AtomicU64::new(0),
    cr2: AtomicU64::new(0),
    frame: AtomicU64::new(0),
    rsp: AtomicU64::new(0),
};

static PREV: SpinLock<Option<ExceptionHook>> = SpinLock::named("cp.ktest.prev_hook", None);
static ARMED: AtomicBool = AtomicBool::new(false);
const MARK: u64 = 0x5AFE_0C0D_E000_0001;

fn record(f: &TrapFrame) {
    HIT.vector.store(f.vector, Ordering::Relaxed);
    HIT.rip.store(f.rip, Ordering::Relaxed);
    HIT.error.store(f.error, Ordering::Relaxed);
    HIT.cr2.store(cpu::read_cr2(), Ordering::Relaxed);
    HIT.frame.store(f as *const TrapFrame as u64, Ordering::Relaxed);
    HIT.rsp.store(f.rsp, Ordering::Relaxed);
    HIT.count.fetch_add(1, Ordering::AcqRel);
}

fn chain(f: &mut TrapFrame) -> bool {
    let prev = *PREV.lock();
    prev.is_some_and(|h| h(f))
}

/// Resumes the probe load (any vector) and marks rax.
fn resume_hook(f: &mut TrapFrame) -> bool {
    let site = idt::probe_site();
    if f.rip != site.fault_rip {
        return chain(f);
    }
    record(f);
    f.rip = site.resume_rip;
    f.rax = MARK;
    true
}

/// Takes one armed `int n` and returns to the next instruction. Only the frame of hal's own `int` is taken:
/// a real NMI (or #MC) arriving meanwhile goes on to the previous hook.
fn soft_hook(f: &mut TrapFrame) -> bool {
    if !idt::is_soft_int_frame(f) || !ARMED.swap(false, Ordering::AcqRel) {
        return chain(f);
    }
    record(f);
    true
}

fn with_hook<T>(vector: u8, hook: ExceptionHook, body: impl FnOnce() -> T) -> T {
    let _irq = cpu::IrqGuard::new();
    let prev = idt::exception_hook(vector);
    *PREV.lock() = prev;
    HIT.count.store(0, Ordering::Release);
    idt::set_exception_hook(vector, hook);
    let r = body();
    match prev {
        Some(p) => idt::set_exception_hook(vector, p),
        None => idt::clear_exception_hook(vector),
    }
    *PREV.lock() = None;
    r
}

fn hits() -> u64 {
    HIT.count.load(Ordering::Acquire)
}

fn gp_resume() -> R {
    let v = with_hook(idt::GENERAL_PROTECTION, resume_hook, || idt::probe_load(ProbeTarget::NonCanonical));
    kassert!(hits() == 1, "hook saw {} traps", hits());
    kassert!(v == Some(MARK), "probe returned {v:x?}: not resumed through the hook");
    kassert!(HIT.vector.load(Ordering::Relaxed) == idt::GENERAL_PROTECTION as u64);
    kassert!(HIT.error.load(Ordering::Relaxed) == 0, "#GP error {:#x}", HIT.error.load(Ordering::Relaxed));
    kassert!(HIT.rip.load(Ordering::Relaxed) == idt::probe_site().fault_rip);
    kassert!(idt::exception_hook(idt::GENERAL_PROTECTION).is_none_or(|h| h as usize != resume_hook as ExceptionHook as usize), "hook not restored");
    Ok(())
}

fn pf_resume() -> R {
    let guard = gdt::ist_stack(gdt::IST_DOUBLE_FAULT).and_then(|s| s.guard).ok_or_else(|| String::from("IST1 has no guard page (fallback active)"))?;
    let v = with_hook(idt::PAGE_FAULT, resume_hook, || idt::probe_load(ProbeTarget::IstGuard(gdt::IST_DOUBLE_FAULT)));
    kassert!(hits() == 1 && v == Some(MARK), "hits {} value {v:x?}", hits());
    kassert!(HIT.vector.load(Ordering::Relaxed) == idt::PAGE_FAULT as u64);
    let cr2 = HIT.cr2.load(Ordering::Relaxed);
    kassert!(cr2 == guard, "cr2 {cr2:#x}, guard page {guard:#x}");
    let err = HIT.error.load(Ordering::Relaxed);
    // Not present (P=0), read (W=0), supervisor (U=0), not an instruction fetch.
    kassert!(err & 0b1_0111 == 0, "#PF error {err:#x}");
    Ok(())
}

fn ist_guards() -> R {
    let root = hal::paging::current_root();
    for s in gdt::ist_stacks() {
        let guard = s.guard.ok_or_else(|| alloc::format!("IST{} has no guard (fallback active)", s.ist))?;
        kassert!(gdt::tss_ist(s.ist) == s.top, "TSS IST{} {:#x}, stack top {:#x}", s.ist, gdt::tss_ist(s.ist), s.top);
        kassert!(s.top % 16 == 0 && s.bottom == guard + gdt::IST_GUARD_PAGES * 4096);
        kassert!(s.top - s.bottom == gdt::IST_STACK_PAGES * 4096);
        kassert!(guard >= gdt::CPU_AREA_BASE && s.top <= bootinfo::layout::VMAP_END, "IST{} outside the CPU area", s.ist);
        for g in 0..gdt::IST_GUARD_PAGES {
            kassert!(hal::paging::translate(root, guard + g * 4096).is_none(), "IST{} guard page mapped", s.ist);
        }
        let mut a = s.bottom;
        while a < s.top {
            let (_, fl) = hal::paging::translate(root, a).ok_or_else(|| alloc::format!("IST{} page {a:#x} unmapped", s.ist))?;
            kassert!(fl.contains(hal::paging::PageFlags::WRITABLE | hal::paging::PageFlags::NX), "IST{} page {a:#x} flags {:#x}", s.ist, fl.0);
            kassert!(!fl.contains(hal::paging::PageFlags::USER));
            a += 4096;
        }
    }
    Ok(())
}

fn soft_ist(vector: u8, ist: u8, raise: fn()) -> R {
    let stack = gdt::ist_stack(ist).ok_or("no IST stack")?;
    // Static check first: the gate names the slot and the TSS slot holds this stack's top.
    kassert!(idt::gate_ist(vector) == ist, "IDT gate {vector} uses IST{}, wanted IST{ist}", idt::gate_ist(vector));
    kassert!(gdt::tss_ist(ist) == stack.top, "TSS IST{ist} {:#x}, stack top {:#x}", gdt::tss_ist(ist), stack.top);
    ARMED.store(true, Ordering::Release);
    with_hook(vector, soft_hook, raise);
    ARMED.store(false, Ordering::Release);
    kassert!(hits() == 1, "vector {vector}: hook saw {} traps", hits());
    kassert!(HIT.vector.load(Ordering::Relaxed) == vector as u64);
    let frame = HIT.frame.load(Ordering::Relaxed);
    kassert!(stack.contains(frame), "vector {vector} frame at {frame:#x}, IST{ist} is {:#x}..{:#x}", stack.bottom, stack.top);
    let rsp = HIT.rsp.load(Ordering::Relaxed);
    kassert!(!stack.contains(rsp), "interrupted rsp {rsp:#x} should be the test's own stack");
    Ok(())
}

fn ist_nmi() -> R {
    // A real NMI on this CPU during the soft `int 2` would overwrite its IST2 frame: no CPU may send an NMI IPI
    // meanwhile, and one sent just before gets a millisecond to arrive first.
    let _quiet = hal::apic::pause_nmi_ipis();
    time::delay_ns(1_000_000);
    soft_ist(idt::NMI, gdt::IST_NMI, idt::soft_int_nmi)
}

fn ist_mc() -> R {
    soft_ist(idt::MACHINE_CHECK, gdt::IST_MACHINE_CHECK, idt::soft_int_machine_check)
}

/// A real #DF (#GP whose gate is absent in a private IDT copy, so the other CPUs are not affected). Under a
/// hypervisor only (class QemuOnly): on hardware a mistake would reset the machine into Windows (spec trap 24),
/// and the IST1 layout is already covered by `cp.ist.guards`.
fn ist_double_fault() -> R {
    let f: Features = cpu::detect();
    kassert!(idt::gate_ist(idt::DOUBLE_FAULT) == gdt::IST_DOUBLE_FAULT, "#DF gate not on IST1");
    if !f.hypervisor {
        // hal refuses the live #DF on hardware itself; make sure it does.
        kassert!(idt::double_fault_selftest().is_err(), "double_fault_selftest ran on hardware");
        return Ok(());
    }
    let r = idt::double_fault_selftest().map_err(String::from)?;
    kassert!(r.stack.contains(r.frame_addr), "#DF frame at {:#x}, IST1 {:#x}..{:#x}", r.frame_addr, r.stack.bottom, r.stack.top);
    kassert!(r.stack.guard.is_some(), "IST1 unguarded");
    kassert!(r.error == 0, "#DF error code {:#x}", r.error);
    // The #GP gate is back: a normal #GP is caught again.
    gp_resume()
}

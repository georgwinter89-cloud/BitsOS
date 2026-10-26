//! CPU basics and the per-CPU set-up (owner: CP; V2-PLAN §2.3, spec/core.md §3).
//!
//! What lives here: CPUID (through `CpuidView`, so decoding is host-tested with recorded values), one feature
//! table that both the kernel's decisions (`Features`) and `hw_info` (`facts`) read, the control-register
//! set-up of every CPU (`init_this_cpu`: CR0 written whole, CR4, EFER, XCR0, TSC_AUX), the R10 x86-64-v3
//! check and the read-only machine-check bank report.
//!
//! The kernel itself stays soft-float (R10): XCR0 only decides which user states XSAVE handles (SC's job).
//!
//! Intel and AMD paths (CP-6; the Mini-PC is an AMD Ryzen 7 8845HS, the Gaming-PC an Intel Core Ultra 7 265K,
//! Arrow Lake, family 6 model 0xC6: 8 P + 12 E cores, no SMT, no AVX-512). Every vendor-specific step asks CPUID
//! (or, where Intel has no bit, the model) first, because `msr::read` cannot recover from a #GP:
//! - **Timer**: TSC-deadline wherever CPUID 1 ECX[24] says so (the 265K, KVM `-cpu host` on it), else the
//!   calibrated one-shot LAPIC timer (the 8845HS has no TSC-deadline); `apic::timer::setup`, `time::lapic`.
//!   Linux additionally refuses TSC-deadline on Haswell..Kaby Lake with old microcode
//!   (linux-6.6/arch/x86/kernel/apic/apic.c:530-575); V2 does not, neither machine is affected (see cp-CP-6.md).
//! - **AMD-only MSRs**: SYSCFG, TOM, TOM2 only when `amd_mtrr_msrs` (AuthenticAMD, family 0xF or later), like
//!   linux-6.6/arch/x86/kernel/cpu/mtrr/generic.c:107; the AMD RAPL MSRs only with CPUID 0x8000_0007 EDX[14].
//!   On Intel `amd_syscfg_fix` does nothing, `mtrr_load` never touches SYSCFG, and `msr::access` refuses them.
//! - **Intel-only MSRs**: IA32_PM_ENABLE with CPUID 6 EAX[7] `hwp`; IA32_HWP_CAPABILITIES only when its bit 0 is
//!   set (read only on hybrid CPUs, by `smp`); the Intel RAPL counters by model list and never in a guest
//!   (`msr::intel_rapl_model`).
//! - **Hybrid cores**: core type from CPUID 0x1A and the capacity hint for SC from the HWP numbers
//!   (`smp::capacity`; the module text of `smp` has the details). QEMU hides the hybrid bit, so the hybrid path
//!   is proven by recorded CPUID values (tests here and in `smp`) and on the Gaming-PC itself.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

pub mod reset;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

pub fn cpuid(leaf: u32, sub: u32) -> CpuidResult {
    let r = core::arch::x86_64::__cpuid_count(leaf, sub);
    CpuidResult { eax: r.eax, ebx: r.ebx, ecx: r.ecx, edx: r.edx }
}

pub fn rdtsc() -> u64 {
    // SAFETY: rdtsc is unprivileged unless CR4.TSD is set, which `cr4_wanted` always clears.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The TSC read in program order (lfence before and after): what cross-CPU offset measurements need.
pub fn rdtsc_ordered() -> u64 {
    // SAFETY: lfence orders loads only; rdtsc as above.
    unsafe {
        core::arch::x86_64::_mm_lfence();
        let t = core::arch::x86_64::_rdtsc();
        core::arch::x86_64::_mm_lfence();
        t
    }
}

// ---------------------------------------------------------------------------------------------------------
// CPUID view and the feature table

/// CPUID through a function. Leaves above the CPU's maximum read as zero: Intel answers out-of-range leaves
/// with the highest basic leaf's data, which would otherwise look like feature bits.
pub struct CpuidView<'a> {
    f: &'a dyn Fn(u32, u32) -> CpuidResult,
    max_basic: u32,
    max_ext: u32,
    max_sub7: u32,
}

static LIVE_CPUID: fn(u32, u32) -> CpuidResult = cpuid;

impl<'a> CpuidView<'a> {
    pub fn new(f: &'a dyn Fn(u32, u32) -> CpuidResult) -> Self {
        let max_basic = f(0, 0).eax;
        let max_ext = f(0x8000_0000, 0).eax;
        let max_sub7 = if max_basic >= 7 { f(7, 0).eax } else { 0 };
        CpuidView { f, max_basic, max_ext, max_sub7 }
    }

    pub fn get(&self, leaf: u32, sub: u32) -> CpuidResult {
        let ok = if leaf >= 0x8000_0000 {
            leaf <= self.max_ext
        } else {
            leaf <= self.max_basic && (leaf != 7 || sub <= self.max_sub7)
        };
        if ok { (self.f)(leaf, sub) } else { CpuidResult::default() }
    }

    pub fn max_basic(&self) -> u32 {
        self.max_basic
    }
    pub fn max_ext(&self) -> u32 {
        self.max_ext
    }

    /// Whether the named flag of `FLAGS` is set (false for unknown names; a host test checks that every
    /// name the kernel uses exists).
    pub fn has(&self, name: &str) -> bool {
        FLAGS.iter().find(|d| d.name == name).is_some_and(|d| self.bit(d))
    }

    pub fn bit(&self, d: &FlagDef) -> bool {
        let r = self.get(d.leaf, d.sub);
        let v = match d.reg {
            Reg::Eax => r.eax,
            Reg::Ebx => r.ebx,
            Reg::Ecx => r.ecx,
            Reg::Edx => r.edx,
        };
        v & (1 << d.bit) != 0
    }
}

/// The CPU this code runs on.
pub fn live() -> CpuidView<'static> {
    CpuidView::new(&LIVE_CPUID)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reg {
    Eax,
    Ebx,
    Ecx,
    Edx,
}

/// One named CPUID bit (names as in Linux's /proc/cpuinfo where one exists).
#[derive(Clone, Copy, Debug)]
pub struct FlagDef {
    pub name: &'static str,
    pub leaf: u32,
    pub sub: u32,
    pub reg: Reg,
    pub bit: u8,
}

const fn fl(name: &'static str, leaf: u32, sub: u32, reg: Reg, bit: u8) -> FlagDef {
    FlagDef { name, leaf, sub, reg, bit }
}

use Reg::{Eax, Ebx, Ecx, Edx};

/// Every CPUID bit the kernel decides on or `hw_info` shows. Adding a bit here is all it takes.
pub static FLAGS: &[FlagDef] = &[
    fl("fpu", 1, 0, Edx, 0),
    fl("de", 1, 0, Edx, 2),
    fl("tsc", 1, 0, Edx, 4),
    fl("msr", 1, 0, Edx, 5),
    fl("pae", 1, 0, Edx, 6),
    fl("mce", 1, 0, Edx, 7),
    fl("cx8", 1, 0, Edx, 8),
    fl("apic", 1, 0, Edx, 9),
    fl("mtrr", 1, 0, Edx, 12),
    fl("pge", 1, 0, Edx, 13),
    fl("mca", 1, 0, Edx, 14),
    fl("cmov", 1, 0, Edx, 15),
    fl("pat", 1, 0, Edx, 16),
    fl("clflush", 1, 0, Edx, 19),
    fl("fxsr", 1, 0, Edx, 24),
    fl("sse", 1, 0, Edx, 25),
    fl("sse2", 1, 0, Edx, 26),
    fl("ht", 1, 0, Edx, 28),
    fl("sse3", 1, 0, Ecx, 0),
    fl("pclmulqdq", 1, 0, Ecx, 1),
    fl("monitor", 1, 0, Ecx, 3),
    fl("ssse3", 1, 0, Ecx, 9),
    fl("fma", 1, 0, Ecx, 12),
    fl("cx16", 1, 0, Ecx, 13),
    fl("pcid", 1, 0, Ecx, 17),
    fl("sse4_1", 1, 0, Ecx, 19),
    fl("sse4_2", 1, 0, Ecx, 20),
    fl("x2apic", 1, 0, Ecx, 21),
    fl("movbe", 1, 0, Ecx, 22),
    fl("popcnt", 1, 0, Ecx, 23),
    fl("tsc_deadline_timer", 1, 0, Ecx, 24),
    fl("aes", 1, 0, Ecx, 25),
    fl("xsave", 1, 0, Ecx, 26),
    fl("osxsave", 1, 0, Ecx, 27),
    fl("avx", 1, 0, Ecx, 28),
    fl("f16c", 1, 0, Ecx, 29),
    fl("rdrand", 1, 0, Ecx, 30),
    fl("hypervisor", 1, 0, Ecx, 31),
    fl("arat", 6, 0, Eax, 2),
    fl("hwp", 6, 0, Eax, 7),
    fl("fsgsbase", 7, 0, Ebx, 0),
    fl("tsc_adjust", 7, 0, Ebx, 1),
    fl("bmi1", 7, 0, Ebx, 3),
    fl("avx2", 7, 0, Ebx, 5),
    fl("smep", 7, 0, Ebx, 7),
    fl("bmi2", 7, 0, Ebx, 8),
    fl("erms", 7, 0, Ebx, 9),
    fl("invpcid", 7, 0, Ebx, 10),
    fl("avx512f", 7, 0, Ebx, 16),
    fl("avx512dq", 7, 0, Ebx, 17),
    fl("rdseed", 7, 0, Ebx, 18),
    fl("adx", 7, 0, Ebx, 19),
    fl("smap", 7, 0, Ebx, 20),
    fl("avx512ifma", 7, 0, Ebx, 21),
    fl("clflushopt", 7, 0, Ebx, 23),
    fl("clwb", 7, 0, Ebx, 24),
    fl("avx512cd", 7, 0, Ebx, 28),
    fl("sha_ni", 7, 0, Ebx, 29),
    fl("avx512bw", 7, 0, Ebx, 30),
    fl("avx512vl", 7, 0, Ebx, 31),
    fl("avx512vbmi", 7, 0, Ecx, 1),
    fl("umip", 7, 0, Ecx, 2),
    fl("pku", 7, 0, Ecx, 3),
    fl("ospke", 7, 0, Ecx, 4),
    fl("waitpkg", 7, 0, Ecx, 5),
    fl("avx512_vbmi2", 7, 0, Ecx, 6),
    fl("shstk", 7, 0, Ecx, 7),
    fl("gfni", 7, 0, Ecx, 8),
    fl("vaes", 7, 0, Ecx, 9),
    fl("vpclmulqdq", 7, 0, Ecx, 10),
    fl("avx512_vnni", 7, 0, Ecx, 11),
    fl("avx512_bitalg", 7, 0, Ecx, 12),
    fl("avx512_vpopcntdq", 7, 0, Ecx, 14),
    fl("la57", 7, 0, Ecx, 16),
    fl("rdpid", 7, 0, Ecx, 22),
    fl("movdiri", 7, 0, Ecx, 27),
    fl("movdir64b", 7, 0, Ecx, 28),
    fl("fsrm", 7, 0, Edx, 4),
    fl("avx512_vp2intersect", 7, 0, Edx, 8),
    fl("serialize", 7, 0, Edx, 14),
    fl("hybrid_cpu", 7, 0, Edx, 15),
    fl("ibt", 7, 0, Edx, 20),
    fl("amx_bf16", 7, 0, Edx, 22),
    fl("avx512_fp16", 7, 0, Edx, 23),
    fl("amx_tile", 7, 0, Edx, 24),
    fl("amx_int8", 7, 0, Edx, 25),
    fl("avx_vnni", 7, 1, Eax, 4),
    fl("avx512_bf16", 7, 1, Eax, 5),
    fl("fzrm", 7, 1, Eax, 10),
    fl("fsrs", 7, 1, Eax, 11),
    fl("fsrc", 7, 1, Eax, 12),
    fl("xsaveopt", 0xD, 1, Eax, 0),
    fl("xsavec", 0xD, 1, Eax, 1),
    fl("xgetbv1", 0xD, 1, Eax, 2),
    fl("xsaves", 0xD, 1, Eax, 3),
    fl("lahf_lm", 0x8000_0001, 0, Ecx, 0),
    fl("svm", 0x8000_0001, 0, Ecx, 2),
    fl("abm", 0x8000_0001, 0, Ecx, 5),
    fl("sse4a", 0x8000_0001, 0, Ecx, 6),
    fl("3dnowprefetch", 0x8000_0001, 0, Ecx, 8),
    fl("topoext", 0x8000_0001, 0, Ecx, 22),
    fl("syscall", 0x8000_0001, 0, Edx, 11),
    fl("nx", 0x8000_0001, 0, Edx, 20),
    fl("pdpe1gb", 0x8000_0001, 0, Edx, 26),
    fl("rdtscp", 0x8000_0001, 0, Edx, 27),
    fl("lm", 0x8000_0001, 0, Edx, 29),
    fl("succor", 0x8000_0007, 0, Ebx, 1),
    fl("smca", 0x8000_0007, 0, Ebx, 3),
    fl("constant_tsc", 0x8000_0007, 0, Edx, 8),
    // AMD running average power limit: the energy MSRs 0xC001_0299/029A/029B (Linux amd.c, X86_FEATURE_RAPL).
    fl("rapl", 0x8000_0007, 0, Edx, 14),
    fl("invlpgb", 0x8000_0008, 0, Ebx, 3),
    fl("rdpru", 0x8000_0008, 0, Ebx, 4),
    fl("wbnoinvd", 0x8000_0008, 0, Ebx, 9),
];

/// Feature bits the kernel decides on (read from `FLAGS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    pub nx: bool,
    pub smep: bool,
    pub smap: bool,
    pub umip: bool,
    pub fsgsbase: bool,
    pub pcid: bool,
    pub invpcid: bool,
    pub x2apic: bool,
    pub xsave: bool,
    pub xsaves: bool,
    pub avx: bool,
    pub avx512f: bool,
    pub rdrand: bool,
    pub rdseed: bool,
    pub rdtscp: bool,
    pub rdpid: bool,
    pub invariant_tsc: bool,
    pub tsc_deadline: bool,
    pub pdpe1gb: bool,
    pub mce: bool,
    pub mca: bool,
    pub smca: bool,
    pub pge: bool,
    pub pat: bool,
    pub la57: bool,
    pub pku: bool,
    pub shstk: bool,
    pub ibt: bool,
    pub hypervisor: bool,
    pub amd: bool,
    pub intel: bool,
}

/// Names `Features` reads from `FLAGS` (the host test checks each exists).
pub const FEATURE_NAMES: &[&str] = &[
    "nx", "smep", "smap", "umip", "fsgsbase", "pcid", "invpcid", "x2apic", "xsave", "xsaves", "avx", "avx512f",
    "rdrand", "rdseed", "rdtscp", "rdpid", "constant_tsc", "tsc_deadline_timer", "pdpe1gb", "mce", "mca", "smca",
    "pge", "pat", "la57", "pku", "shstk", "ibt", "hypervisor",
];

impl Features {
    pub fn from_view(v: &CpuidView) -> Self {
        let vendor = v.get(0, 0);
        Features {
            nx: v.has("nx"),
            smep: v.has("smep"),
            smap: v.has("smap"),
            umip: v.has("umip"),
            fsgsbase: v.has("fsgsbase"),
            pcid: v.has("pcid"),
            invpcid: v.has("invpcid"),
            x2apic: v.has("x2apic"),
            xsave: v.has("xsave"),
            xsaves: v.has("xsaves"),
            avx: v.has("avx"),
            avx512f: v.has("avx512f"),
            rdrand: v.has("rdrand"),
            rdseed: v.has("rdseed"),
            rdtscp: v.has("rdtscp"),
            rdpid: v.has("rdpid"),
            invariant_tsc: v.has("constant_tsc"),
            tsc_deadline: v.has("tsc_deadline_timer"),
            pdpe1gb: v.has("pdpe1gb"),
            mce: v.has("mce"),
            mca: v.has("mca"),
            smca: v.has("smca"),
            pge: v.has("pge"),
            pat: v.has("pat"),
            la57: v.has("la57"),
            pku: v.has("pku"),
            shstk: v.has("shstk"),
            ibt: v.has("ibt"),
            hypervisor: v.has("hypervisor"),
            // "AuthenticAMD" / "GenuineIntel": ebx holds the first four letters.
            amd: vendor.ebx == u32::from_le_bytes(*b"Auth"),
            intel: vendor.ebx == u32::from_le_bytes(*b"Genu"),
        }
    }
}

pub fn detect() -> Features {
    Features::from_view(&live())
}

// ---------------------------------------------------------------------------------------------------------
// Control registers and XCR0: the wanted values (pure, host-tested) and the set-up of one CPU

pub const CR0_PE: u64 = 1 << 0;
pub const CR0_MP: u64 = 1 << 1;
pub const CR0_EM: u64 = 1 << 2;
pub const CR0_TS: u64 = 1 << 3;
pub const CR0_ET: u64 = 1 << 4;
pub const CR0_NE: u64 = 1 << 5;
pub const CR0_WP: u64 = 1 << 16;
pub const CR0_AM: u64 = 1 << 18;
pub const CR0_NW: u64 = 1 << 29;
pub const CR0_CD: u64 = 1 << 30;
pub const CR0_PG: u64 = 1 << 31;
/// CR0 is written whole (Linux's CR0_STATE): caches on (CD/NW clear, spec trap 12), native x87 errors, WP so
/// the kernel honours read-only pages, AM so user alignment checks work, no EM/TS (eager FPU state).
pub const CR0_WANTED: u64 = CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_WP | CR0_AM | CR0_PG;

pub const CR4_VME: u64 = 1 << 0;
pub const CR4_PVI: u64 = 1 << 1;
pub const CR4_TSD: u64 = 1 << 2;
pub const CR4_DE: u64 = 1 << 3;
pub const CR4_PSE: u64 = 1 << 4;
pub const CR4_PAE: u64 = 1 << 5;
pub const CR4_MCE: u64 = 1 << 6;
pub const CR4_PGE: u64 = 1 << 7;
pub const CR4_PCE: u64 = 1 << 8;
pub const CR4_OSFXSR: u64 = 1 << 9;
pub const CR4_OSXMMEXCPT: u64 = 1 << 10;
pub const CR4_UMIP: u64 = 1 << 11;
pub const CR4_LA57: u64 = 1 << 12;
pub const CR4_VMXE: u64 = 1 << 13;
pub const CR4_SMXE: u64 = 1 << 14;
pub const CR4_FSGSBASE: u64 = 1 << 16;
pub const CR4_PCIDE: u64 = 1 << 17;
pub const CR4_OSXSAVE: u64 = 1 << 18;
pub const CR4_SMEP: u64 = 1 << 20;
pub const CR4_SMAP: u64 = 1 << 21;
pub const CR4_PKE: u64 = 1 << 22;
pub const CR4_CET: u64 = 1 << 23;

/// Bits CP-1 does not own and leaves as found: PAE and LA57 cannot change in long mode (LA57 set is refused
/// by `check_handover` before anything is written), PCIDE is MM's (MM-5a), VMXE/SMXE belong to whoever
/// entered VMX/SMX (nobody here), DE is harmless.
const CR4_KEEP: u64 = CR4_PAE | CR4_LA57 | CR4_PCIDE | CR4_VMXE | CR4_SMXE | CR4_DE;

/// CR4 for this CPU. Everything else is cleared: TSD (rdtsc stays unprivileged), PCE (no user perf
/// counters), VME/PVI (no virtual-8086), PKE (see `XSTATE_USER_ENABLED`), CET (off, spec §2.3).
pub fn cr4_wanted(current: u64, f: &Features) -> u64 {
    let mut cr4 = (current & CR4_KEEP) | CR4_PAE | CR4_OSFXSR | CR4_OSXMMEXCPT;
    for (on, bit) in [
        (f.pge, CR4_PGE),
        (f.mce, CR4_MCE),
        (f.xsave, CR4_OSXSAVE),
        (f.smep, CR4_SMEP),
        (f.smap, CR4_SMAP),
        (f.umip, CR4_UMIP),
        (f.fsgsbase, CR4_FSGSBASE),
    ] {
        if on {
            cr4 |= bit;
        }
    }
    cr4
}

pub const XSTATE_X87: u64 = 1 << 0;
pub const XSTATE_SSE: u64 = 1 << 1;
pub const XSTATE_AVX: u64 = 1 << 2;
pub const XSTATE_OPMASK: u64 = 1 << 5;
pub const XSTATE_ZMM_HI256: u64 = 1 << 6;
pub const XSTATE_HI16_ZMM: u64 = 1 << 7;
pub const XSTATE_AVX512: u64 = XSTATE_OPMASK | XSTATE_ZMM_HI256 | XSTATE_HI16_ZMM;
/// User states BitsOS enables when CPUID 0xD offers them: x87, SSE, AVX and the three AVX-512 states (the ones
/// that make programs faster; dav1d/libvpx pick their AVX2/AVX-512 paths from CPUID and XCR0). A deliberate
/// deviation from spec/core.md §3.3's "every user state", decided in CP-1 (notes/cp-CP-1.md):
/// - MPX (3, 4): gone from current CPUs, Linux dropped it.
/// - PKRU (9): Linux turns it on with CPUID PKU, but without `pkey_alloc`/`pkey_mprotect` (ENOSYS today) no
///   program can tag a page, so it brings nothing; with CR4.PKE on, a program's PKRU would also govern the
///   kernel's own user copies, and SC would have to switch PKRU per thread. Enable it together with the pkey
///   syscalls (CR4.PKE, XCR0 bit 9, PKRU in the XSAVE area and the signal frame).
/// - AMX tiles (17, 18): Linux enables them per process through arch_prctl(ARCH_REQ_XCOMP_PERM) and XFD;
///   neither machine has AMX.
/// - APX (19): needs SC's switch path first; neither machine has it.
pub const XSTATE_USER_ENABLED: u64 = XSTATE_X87 | XSTATE_SSE | XSTATE_AVX | XSTATE_AVX512;

/// XCR0 for a CPU whose CPUID 0xD.0 reports `supported` (EDX:EAX). Keeps XSETBV's rules: x87 always, AVX
/// needs SSE, the three AVX-512 states all or none and only with AVX.
pub fn xcr0_wanted(supported: u64) -> u64 {
    let mut x = (supported & XSTATE_USER_ENABLED) | XSTATE_X87 | XSTATE_SSE;
    if x & XSTATE_AVX == 0 || x & XSTATE_AVX512 != XSTATE_AVX512 {
        x &= !XSTATE_AVX512;
    }
    x
}

/// The XCR0 bits CPUID 0xD.0 says the CPU supports.
pub fn xcr0_supported(v: &CpuidView) -> u64 {
    let d = v.get(0xD, 0);
    (d.edx as u64) << 32 | d.eax as u64
}

/// What the kernel cannot run with (spec/core.md §2.3 "refuse LA57"): 5-level paging would make the 4-level
/// tables, the layout and every canonical-address assumption wrong. bitsboot refuses it already; this is the
/// kernel's own check, for every CPU before its set-up writes anything.
pub fn check_handover(cr4: u64) -> Result<(), &'static str> {
    if cr4 & CR4_LA57 != 0 {
        return Err("CR4.LA57 is on: BitsOS uses 4-level paging (the loader should have refused 5-level paging)");
    }
    Ok(())
}

/// IA32_TSC_AUX in Linux's encoding, which the vDSO's getcpu reads back with rdpid/rdtscp:
/// `(node << 12) | cpu` (arch/x86/include/asm/segment.h, VDSO_CPUNODE_BITS = 12). This is a Linux ABI
/// encoding, not a kernel limit: CPU indexes of 4096 and more do not fit, so they are stored masked and
/// `tsc_aux_cpu` says the value cannot be trusted (the vDSO then has to fall back to the getcpu syscall).
pub const TSC_AUX_CPU_BITS: u32 = 12;
pub const TSC_AUX_CPU_MASK: u64 = (1 << TSC_AUX_CPU_BITS) - 1;

pub fn tsc_aux_value(cpu: u32, node: u32) -> u64 {
    (node as u64) << TSC_AUX_CPU_BITS | (cpu as u64 & TSC_AUX_CPU_MASK)
}

/// (cpu, node) from an IA32_TSC_AUX value written by `tsc_aux_value`.
pub fn tsc_aux_split(aux: u64) -> (u32, u32) {
    ((aux & TSC_AUX_CPU_MASK) as u32, (aux >> TSC_AUX_CPU_BITS) as u32)
}

/// Whether `cpu` is representable in TSC_AUX (fewer than 4096 CPUs).
pub fn tsc_aux_cpu_fits(cpu: u32) -> bool {
    (cpu as u64) <= TSC_AUX_CPU_MASK
}

static SMAP_ON: AtomicBool = AtomicBool::new(false);

/// True when SMAP is on, so user copies must bracket accesses with stac/clac.
pub fn smap_enabled() -> bool {
    SMAP_ON.load(Ordering::Relaxed)
}

#[cfg(target_os = "none")]
mod imp {
    use core::arch::asm;
    pub fn read_cr0() -> u64 {
        let v;
        // SAFETY: reading a control register has no side effects (ring 0).
        unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }
    pub fn write_cr0(v: u64) {
        // SAFETY: only `init_this_cpu` writes CR0, with CR0_WANTED: PE/PG stay set (we run paged in long
        // mode), the other bits only change the cache mode and fault checks.
        unsafe { asm!("mov cr0, {}", in(reg) v, options(nostack, preserves_flags)) };
    }
    pub fn read_cr2() -> u64 {
        let v;
        // SAFETY: as read_cr0.
        unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }
    pub fn read_cr3() -> u64 {
        let v;
        // SAFETY: as read_cr0.
        unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }
    pub fn read_cr4() -> u64 {
        let v;
        // SAFETY: as read_cr0.
        unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }
    pub fn write_cr4(v: u64) {
        // SAFETY: only `init_this_cpu` writes CR4, with `cr4_wanted`: feature bits CPUID reported, the bits
        // that cannot change in long mode kept as found.
        unsafe { asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags)) };
    }
    pub fn xgetbv(index: u32) -> u64 {
        let (lo, hi): (u32, u32);
        // SAFETY: callers check CR4.OSXSAVE first (xgetbv #UDs otherwise); no memory effects.
        unsafe { asm!("xgetbv", in("ecx") index, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
        (hi as u64) << 32 | lo as u64
    }
    pub fn xsetbv(index: u32, v: u64) {
        // SAFETY: `init_this_cpu` passes `xcr0_wanted`, which keeps XSETBV's rules (x87 set, AVX with SSE,
        // AVX-512 all or none) and only CPUID-supported bits, after setting CR4.OSXSAVE. The kernel holds
        // no vector state that a change of XCR0 could lose (soft-float, R10).
        unsafe { asm!("xsetbv", in("ecx") index, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nomem, nostack, preserves_flags)) };
    }
    pub fn interrupts_enabled() -> bool {
        let f: u64;
        // SAFETY: pushfq/pop only touch the stack slot they use.
        unsafe { asm!("pushfq; pop {}", out(reg) f, options(nomem, preserves_flags)) };
        f & (1 << 9) != 0
    }
    pub fn disable_interrupts() {
        // SAFETY: cli only masks maskable interrupts on this CPU.
        unsafe { asm!("cli", options(nomem, nostack)) };
    }
    pub fn enable_interrupts() {
        // SAFETY: the IDT is loaded before anyone calls this (kernel init order).
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
    pub fn halt() {
        // SAFETY: hlt waits for the next interrupt; harmless.
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
    pub fn enable_and_halt() {
        // SAFETY: sti;hlt is the race-free idle sequence (the interrupt shadow covers hlt).
        unsafe { asm!("sti; hlt", options(nomem, nostack)) };
    }
    pub fn rdpid() -> u64 {
        let v: u64;
        // SAFETY: callers check CPUID RDPID; it reads IA32_TSC_AUX and has no other effect.
        unsafe { asm!("rdpid {}", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }
    pub fn fpu_reset() {
        let mxcsr: u32 = 0x1F80;
        // SAFETY: called by `init_this_cpu` after CR0 (EM/TS clear, MP/NE set) and CR4 (OSFXSR) are written, so
        // neither instruction faults. fninit discards the firmware's x87 state (all exceptions masked, empty
        // stack); ldmxcsr loads the reset MXCSR (all SIMD exceptions masked, reserved bits zero). With NE set,
        // an unmasked exception left pending by the firmware would otherwise raise #MF/#XM on the first use.
        // The kernel is soft-float (R10) and holds no FP state that this could lose.
        unsafe { asm!("fninit", "ldmxcsr [{}]", in(reg) &mxcsr, options(readonly, nostack, preserves_flags)) };
    }
    /// CR0.CD on, NW off, wbinvd, then a full TLB flush (CR4.PGE toggled, or CR3 reloaded without PGE).
    pub fn cache_disable_flush() {
        let cr0 = read_cr0();
        // SAFETY: the MTRR change sequence of the SDM (12.11.7.2): no-fill cache mode and a write-back of every
        // cache line change the cache mode only; the caller has interrupts off and restores CR0 in
        // `cache_enable`. PE/PG stay set.
        unsafe { asm!("mov cr0, {}", "wbinvd", in(reg) (cr0 | super::CR0_CD) & !super::CR0_NW, options(nostack)) };
        flush_tlb();
    }
    pub fn cache_flush_tlb() {
        // SAFETY: wbinvd writes caches back; no other effect.
        unsafe { asm!("wbinvd", options(nostack, preserves_flags)) };
        flush_tlb();
    }
    pub fn wbinvd() {
        // SAFETY: wbinvd writes every modified cache line back to memory and invalidates the caches; nothing
        // else changes (power-off flushes before SLP_EN, as ACPICA's `ACPI_FLUSH_CPU_CACHE` in hwsleep.c).
        unsafe { asm!("wbinvd", options(nostack, preserves_flags)) };
    }
    pub fn cache_enable() {
        let cr0 = read_cr0();
        // SAFETY: caches back on (CD/NW clear), the state `init_this_cpu` set (CR0_WANTED has neither).
        unsafe { asm!("mov cr0, {}", in(reg) cr0 & !(super::CR0_CD | super::CR0_NW), options(nostack, preserves_flags)) };
    }
    fn flush_tlb() {
        let cr4 = read_cr4();
        if cr4 & super::CR4_PGE != 0 {
            // SAFETY: CR4 without PGE and back flushes every TLB entry, global ones included (SDM 4.10.4.1); no
            // other CR4 bit changes.
            unsafe { asm!("mov cr4, {}", "mov cr4, {}", in(reg) cr4 & !super::CR4_PGE, in(reg) cr4, options(nostack, preserves_flags)) };
        } else {
            // SAFETY: reloading CR3 with its own value flushes the non-global TLB entries.
            unsafe { asm!("mov {t}, cr3", "mov cr3, {t}", t = out(reg) _, options(nostack, preserves_flags)) };
        }
    }
    pub fn fpu_control_words() -> (u16, u32) {
        let mut fcw: u16 = 0;
        let mut mxcsr: u32 = 0;
        // SAFETY: fnstcw/stmxcsr store 2 and 4 bytes into the two locals; CR4.OSFXSR is on after
        // `init_this_cpu` (stmxcsr would #UD before it, and only tests call this, after start-up).
        unsafe { asm!("fnstcw [{}]", "stmxcsr [{}]", in(reg) &mut fcw, in(reg) &mut mxcsr, options(nostack, preserves_flags)) };
        (fcw, mxcsr)
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    pub fn read_cr0() -> u64 {
        0
    }
    pub fn write_cr0(_: u64) {}
    pub fn read_cr2() -> u64 {
        0
    }
    pub fn read_cr3() -> u64 {
        0
    }
    pub fn read_cr4() -> u64 {
        0
    }
    pub fn write_cr4(_: u64) {}
    pub fn xgetbv(_: u32) -> u64 {
        0
    }
    pub fn xsetbv(_: u32, _: u64) {}
    pub fn interrupts_enabled() -> bool {
        false
    }
    pub fn disable_interrupts() {}
    pub fn enable_interrupts() {}
    pub fn halt() {
        core::hint::spin_loop()
    }
    pub fn enable_and_halt() {
        core::hint::spin_loop()
    }
    pub fn rdpid() -> u64 {
        0
    }
    pub fn fpu_reset() {}
    pub fn fpu_control_words() -> (u16, u32) {
        (0x037F, 0x1F80)
    }
    pub fn cache_disable_flush() {}
    pub fn cache_flush_tlb() {}
    pub fn wbinvd() {}
    pub fn cache_enable() {}
}

pub use imp::{enable_and_halt, halt, interrupts_enabled, read_cr0, read_cr2, read_cr3, read_cr4};

/// Writes every modified cache line of this CPU back to memory (before a power-off: some chipsets lose dirty
/// lines on the S5 transition). Slow (milliseconds); no other effect.
pub use imp::wbinvd;

/// XCR0 of this CPU (0 while CR4.OSXSAVE is off).
pub fn read_xcr0() -> u64 {
    if read_cr4() & CR4_OSXSAVE != 0 { imp::xgetbv(0) } else { 0 }
}

/// IA32_TSC_AUX through RDPID (`tsc_aux_value(cpu, node)`, set by `init_this_cpu`; `tsc_aux_split` decodes
/// it); `None` without RDPID.
pub fn rdpid() -> Option<u64> {
    live().has("rdpid").then(imp::rdpid)
}

/// The x87 control word and MXCSR of this CPU (what `init_this_cpu`'s reset left: 0x37F and 0x1F80).
pub fn fpu_control_words() -> (u16, u32) {
    imp::fpu_control_words()
}

/// Masks interrupts on this CPU until dropped, then restores the previous state.
pub struct IrqGuard {
    was_enabled: bool,
}

impl IrqGuard {
    pub fn new() -> Self {
        let was_enabled = imp::interrupts_enabled();
        imp::disable_interrupts();
        IrqGuard { was_enabled }
    }
}

impl Default for IrqGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        if self.was_enabled {
            imp::enable_interrupts();
        }
    }
}

/// Turns interrupts on for this CPU (kernel init, after the IDT and the handlers exist).
pub fn enable_interrupts() {
    imp::enable_interrupts()
}

pub fn disable_interrupts() {
    imp::disable_interrupts()
}

pub fn spin_hint() {
    core::hint::spin_loop()
}

/// What `init_this_cpu` found and set (for the boot event).
#[derive(Clone, Copy, Debug, Default)]
pub struct SetupReport {
    pub cr0_before: u64,
    pub cr0: u64,
    pub cr4_before: u64,
    pub cr4: u64,
    pub efer: u64,
    pub xcr0: u64,
    /// IA32_U_CET / IA32_S_CET as found (0 without CPUID SHSTK/IBT); both are zeroed when set.
    pub u_cet_before: u64,
    pub s_cet_before: u64,
}

const SETUP_WORDS: usize = 8;
static BSP_SETUP: [core::sync::atomic::AtomicU64; SETUP_WORDS] = [const { core::sync::atomic::AtomicU64::new(0) }; SETUP_WORDS];

/// The BSP's set-up as `init_bsp` did it (the boot event is emitted later, once klog exists).
pub fn bsp_setup() -> SetupReport {
    let g = |i: usize| BSP_SETUP[i].load(Ordering::Relaxed);
    SetupReport { cr0_before: g(0), cr0: g(1), cr4_before: g(2), cr4: g(3), efer: g(4), xcr0: g(5), u_cet_before: g(6), s_cet_before: g(7) }
}

/// The per-CPU set-up of spec/core.md §3 (the BSP now, every AP with CP-4): CR0 written whole, CR4 with a
/// global-page flush (the firmware's global entries go), x87/MXCSR reset, CET MSRs zeroed, EFER.NXE/SCE,
/// XCR0 = every enabled user state CPUID offers, IA32_TSC_AUX = `tsc_aux_value(cpu_index, node)` for
/// rdtscp/rdpid and the vDSO's getcpu. Interrupts are off (kernel init).
///
/// Panics (before writing anything) when `check_handover` refuses the CPU's state: the kernel cannot run on
/// it, and this runs before klog exists, so the panic message on the serial line is the report.
pub fn init_this_cpu(f: &Features, cpu_index: u32, node: u32) -> SetupReport {
    match init_this_cpu_like(f, cpu_index, node, 0) {
        Ok(r) => r,
        Err((e, cr4)) => panic!("CPU {cpu_index}: {e} (CR4 {cr4:#x})"),
    }
}

/// `init_this_cpu` for an AP: the CR4 bits CP-1 leaves as the firmware set them (`CR4_FROM_BSP`) are taken from
/// the BSP's CR4 `bsp_cr4`, so every CPU ends with the BSP's CR4. `Err` (nothing written) when `check_handover`
/// refuses the AP's state: `smp` then refuses the AP with that reason instead of panicking on it.
pub fn init_ap_cpu(f: &Features, cpu_index: u32, node: u32, bsp_cr4: u64) -> Result<SetupReport, &'static str> {
    init_this_cpu_like(f, cpu_index, node, bsp_cr4 & CR4_FROM_BSP).map_err(|(e, _)| e)
}

/// CR4 bits an AP copies from the BSP: DE, which `cr4_wanted` keeps as the firmware left it.
pub const CR4_FROM_BSP: u64 = CR4_DE;

fn init_this_cpu_like(f: &Features, cpu_index: u32, node: u32, extra_cr4: u64) -> Result<SetupReport, (&'static str, u64)> {
    let mut r = SetupReport { cr0_before: imp::read_cr0(), cr4_before: imp::read_cr4(), ..Default::default() };
    check_handover(r.cr4_before).map_err(|e| (e, r.cr4_before))?;
    imp::write_cr0(CR0_WANTED);
    r.cr0 = CR0_WANTED;

    let cr4 = cr4_wanted(r.cr4_before | extra_cr4, f);
    // CR4 without PGE and then with it flushes every TLB entry, global ones included (SDM 4.10.4.1).
    imp::write_cr4(cr4 & !CR4_PGE);
    imp::write_cr4(cr4);
    r.cr4 = cr4;
    SMAP_ON.store(cr4 & CR4_SMAP != 0, Ordering::Relaxed);
    imp::fpu_reset();

    // CET off (spec §2.3): CR4.CET is clear, so the enables have no effect today, but stale ones from the
    // firmware would surprise whoever turns CET on later. Zeroed as Linux does when it does not use CET. Gated on
    // this CPU's own CPUID, not `f` (the BSP's on an AP): the relaxed AP check does not require CET, and the two
    // MSRs #GP on a CPU without it.
    if cet_msrs_exist(&live()) {
        use crate::msr::{IA32_S_CET, IA32_U_CET};
        r.u_cet_before = crate::msr::read(IA32_U_CET);
        r.s_cet_before = crate::msr::read(IA32_S_CET);
        for (msr, was) in [(IA32_U_CET, r.u_cet_before), (IA32_S_CET, r.s_cet_before)] {
            if was != 0 {
                crate::msr::write(msr, 0);
            }
        }
    }

    let mut efer = crate::msr::read(crate::msr::IA32_EFER) | crate::msr::EFER_SCE;
    if f.nx {
        efer |= crate::msr::EFER_NXE;
    }
    crate::msr::write(crate::msr::IA32_EFER, efer);
    r.efer = efer;

    if f.xsave {
        let x = xcr0_wanted(xcr0_supported(&live()));
        imp::xsetbv(0, x);
        r.xcr0 = x;
    }
    if f.rdtscp || f.rdpid {
        crate::msr::write(crate::msr::IA32_TSC_AUX, tsc_aux_value(cpu_index, node));
    }
    Ok(r)
}

/// Whether IA32_U_CET / IA32_S_CET exist on the CPU `v` describes (CPUID SHSTK or IBT; pure).
pub fn cet_msrs_exist(v: &CpuidView) -> bool {
    v.has("shstk") || v.has("ibt")
}

/// BSP set-up (kernel start order in the desk's main.rs).
pub fn init_bsp(f: &Features) -> SetupReport {
    // Node 0 until CP-4 reads the topology (SRAT/CPUID); both machines have one node.
    let r = init_this_cpu(f, 0, 0);
    let words: [u64; SETUP_WORDS] = [r.cr0_before, r.cr0, r.cr4_before, r.cr4, r.efer, r.xcr0, r.u_cet_before, r.s_cet_before];
    for (i, v) in words.into_iter().enumerate() {
        BSP_SETUP[i].store(v, Ordering::Relaxed);
    }
    r
}

// ---------------------------------------------------------------------------------------------------------
// The AP check (V2-PLAN §2.3, relaxed): an AP is refused only when it lacks something the BSP's set-up uses or
// its XSAVE layout differs (one area size for every thread); every other difference is logged by CP-4.

/// Features `init_this_cpu`, the paging code and the scheduler rely on once the BSP decided to use them: an AP
/// without one of them cannot run the kernel as the BSP set it up (writing CR4.SMAP on a CPU without SMAP
/// faults, a missing invariant TSC breaks the clock, ...). Having more than the BSP is fine.
pub const AP_REQUIRED: &[&str] = &[
    "nx", "smep", "smap", "umip", "fsgsbase", "x2apic", "xsave", "xsaves", "avx", "avx512f", "constant_tsc", "pge",
    "mce", "pat", "rdtscp", "rdpid", "pcid", "invpcid", "pdpe1gb", "syscall", "lm",
];

/// The first required feature the BSP has and the AP lacks (pure).
pub fn ap_missing(bsp: &CpuidView, ap: &CpuidView) -> Option<&'static str> {
    AP_REQUIRED.iter().copied().find(|n| bsp.has(n) && !ap.has(n))
}

/// What decides the XSAVE area: the supported user states and, for each state the kernel enables, its size and
/// standard offset (CPUID 0xD), plus the XSAVE instruction variants (0xD.1 EAX).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XsaveSig {
    pub supported: u64,
    pub variants: u32,
    /// (size, offset) per state bit (0 for the states that are not enabled).
    pub states: [(u32, u32); 64],
}

impl XsaveSig {
    pub fn from_view(v: &CpuidView) -> XsaveSig {
        let supported = xcr0_supported(v);
        let wanted = xcr0_wanted(supported);
        let mut states = [(0, 0); 64];
        for (i, st) in states.iter_mut().enumerate().skip(2) {
            if wanted & (1 << i) != 0 {
                let d = v.get(0xD, i as u32);
                *st = (d.eax, d.ebx);
            }
        }
        XsaveSig { supported, variants: v.get(0xD, 1).eax & 0xF, states }
    }
}

/// Why an AP's XSAVE layout differs from the BSP's (pure; `None` = same).
pub fn xsave_mismatch(bsp: &XsaveSig, ap: &XsaveSig) -> Option<&'static str> {
    if xcr0_wanted(bsp.supported) != xcr0_wanted(ap.supported) {
        return Some("XSAVE: the enabled user states differ from the BSP's (CPUID 0xD.0)");
    }
    if bsp.variants != ap.variants {
        return Some("XSAVE: the instruction variants differ from the BSP's (CPUID 0xD.1)");
    }
    if bsp.states != ap.states {
        return Some("XSAVE: a state's size or offset differs from the BSP's (CPUID 0xD.n)");
    }
    None
}

/// Every `FLAGS` bit of one CPU as a bit set (built without allocating, so an AP can record it before it
/// may take the heap's lock). `FLAGS` is a static table; a host test keeps it within `FlagBits`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlagBits(pub [u64; 4]);

impl FlagBits {
    pub const CAPACITY: usize = 256;

    pub fn from_view(v: &CpuidView) -> FlagBits {
        let mut b = [0u64; 4];
        for (i, d) in FLAGS.iter().enumerate().take(Self::CAPACITY) {
            if v.bit(d) {
                b[i / 64] |= 1 << (i % 64);
            }
        }
        FlagBits(b)
    }

    pub fn has_index(&self, i: usize) -> bool {
        i < Self::CAPACITY && self.0[i / 64] & (1 << (i % 64)) != 0
    }

    /// Names of `FLAGS` bits that differ from `other` (logged, not refused: the relaxed AP compare).
    pub fn differences(&self, other: &FlagBits) -> Vec<&'static str> {
        FLAGS.iter().enumerate().filter(|(i, _)| self.has_index(*i) != other.has_index(*i)).map(|(_, d)| d.name).collect()
    }
}

// ---------------------------------------------------------------------------------------------------------
// MTRRs and AMD SYSCFG (spec/core.md §3.6, §11.12, V1 cpu/apmtrr.asm): every AP's memory types are compared with
// the BSP's and copied by the SDM procedure when they differ (QEMU clears an AP's MTRRs on INIT; real cores keep
// them). MTRRcap, TOM and TOM2 must match (different hardware, nothing to copy); SYSCFG's memory-type enables are
// copied with the MTRRs, MtrrFixDramModEn is cleared on every core as Linux does.

pub const MTRR_DEF_E: u64 = 1 << 11;
pub const MTRR_DEF_FE: u64 = 1 << 10;
pub const MTRRCAP_FIX: u64 = 1 << 8;
pub const SYSCFG_MTRR_FIX_DRAM_EN: u64 = 1 << 18;
pub const SYSCFG_MTRR_FIX_DRAM_MOD_EN: u64 = 1 << 19;
pub const SYSCFG_MTRR_VAR_DRAM_EN: u64 = 1 << 20;
pub const SYSCFG_MTRR_TOM2_EN: u64 = 1 << 21;
pub const SYSCFG_TOM2_FORCE_WB: u64 = 1 << 22;
/// The SYSCFG bits that change memory types (copied from the BSP).
pub const SYSCFG_MEMTYPE: u64 = SYSCFG_MTRR_FIX_DRAM_EN | SYSCFG_MTRR_VAR_DRAM_EN | SYSCFG_MTRR_TOM2_EN | SYSCFG_TOM2_FORCE_WB;

/// The fixed-range MTRRs (64K, 2x16K, 8x4K).
pub const MTRR_FIXED: [u32; 11] = [0x250, 0x258, 0x259, 0x268, 0x269, 0x26A, 0x26B, 0x26C, 0x26D, 0x26E, 0x26F];

/// The MTRR MSRs `cap` (IA32_MTRRCAP) says exist: variable base/mask pairs (VCNT, bits 0-7), then the fixed
/// ones (FIX, bit 8). Pure.
pub fn mtrr_msrs(cap: u64) -> Vec<u32> {
    let vcnt = (cap & 0xFF) as u32;
    let mut v: Vec<u32> = (0..vcnt * 2).map(|i| 0x200 + i).collect();
    if cap & MTRRCAP_FIX != 0 {
        v.extend_from_slice(&MTRR_FIXED);
    }
    v
}

/// One CPU's memory-type registers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MtrrState {
    pub cap: u64,
    pub def_type: u64,
    /// (MSR, value) for every `mtrr_msrs(cap)` entry.
    pub ranges: Vec<(u32, u64)>,
    /// AMD: (SYSCFG, TOM, TOM2).
    pub amd: Option<(u64, u64, u64)>,
}

impl MtrrState {
    /// Reads through `read` (pure over the reader; `amd` = AMD family 0xF or later).
    pub fn read_with(amd: bool, read: &mut dyn FnMut(u32) -> u64) -> MtrrState {
        let cap = read(crate::msr::IA32_MTRRCAP);
        MtrrState {
            cap,
            def_type: read(crate::msr::IA32_MTRR_DEF_TYPE),
            ranges: mtrr_msrs(cap).into_iter().map(|m| (m, read(m))).collect(),
            amd: amd.then(|| (read(crate::msr::AMD_SYSCFG), read(crate::msr::AMD_TOM), read(crate::msr::AMD_TOM2))),
        }
    }
}

/// What an AP does about its MTRRs (pure).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MtrrAction {
    Same,
    /// Copy the BSP's MTRRs (and SYSCFG's memory-type bits when `syscfg`).
    Copy { syscfg: bool },
    Refuse(&'static str),
}

pub fn mtrr_compare(bsp: &MtrrState, ap: &MtrrState) -> MtrrAction {
    if bsp.cap != ap.cap {
        return MtrrAction::Refuse("MTRRcap differs from the BSP's");
    }
    let syscfg = match (bsp.amd, ap.amd) {
        (Some((bs, bt, bt2)), Some((s, t, t2))) => {
            if bt != t || bt2 != t2 {
                return MtrrAction::Refuse("AMD TOM/TOM2 differ from the BSP's");
            }
            (bs & SYSCFG_MEMTYPE) != (s & SYSCFG_MEMTYPE)
        }
        (None, None) => false,
        _ => return MtrrAction::Refuse("AMD memory-type registers on one CPU only"),
    };
    if syscfg || bsp.def_type != ap.def_type || bsp.ranges != ap.ranges {
        MtrrAction::Copy { syscfg }
    } else {
        MtrrAction::Same
    }
}

/// `mtrr_compare(bsp, this CPU)` without building this CPU's state (no allocation: usable from an interrupt).
pub fn mtrr_action_live(bsp: &MtrrState) -> MtrrAction {
    let rd = crate::msr::read;
    let cap = rd(crate::msr::IA32_MTRRCAP);
    let amd = bsp.amd.map(|_| (rd(crate::msr::AMD_SYSCFG), rd(crate::msr::AMD_TOM), rd(crate::msr::AMD_TOM2)));
    let mine = MtrrState { cap, def_type: rd(crate::msr::IA32_MTRR_DEF_TYPE), ranges: Vec::new(), amd };
    match mtrr_compare(&bsp.clone_head(), &mine) {
        MtrrAction::Same if bsp.ranges.iter().any(|&(m, v)| rd(m) != v) => MtrrAction::Copy { syscfg: false },
        a => a,
    }
}

impl MtrrState {
    /// The state without its range list (cheap to build; `Vec::new` does not allocate).
    fn clone_head(&self) -> MtrrState {
        MtrrState { cap: self.cap, def_type: self.def_type, ranges: Vec::new(), amd: self.amd }
    }
}

/// SYSCFG for an AP: the BSP's memory-type enables, MtrrFixDramModEn clear, every other bit the AP's own.
pub fn syscfg_for_ap(bsp: u64, ap: u64) -> u64 {
    (ap & !(SYSCFG_MEMTYPE | SYSCFG_MTRR_FIX_DRAM_MOD_EN)) | (bsp & SYSCFG_MEMTYPE)
}

/// The SYSCFG bits outside the memory-type enables and MtrrFixDramModEn in which an AP differs from the BSP
/// (pure). Not copied and not refused (spec §3.6 says refuse; CP-4 keeps the AP and logs it, see the notes):
/// `smp` reports them so a hardware run shows any such core.
pub fn syscfg_other_differences(bsp: u64, ap: u64) -> u64 {
    (bsp ^ ap) & !(SYSCFG_MEMTYPE | SYSCFG_MTRR_FIX_DRAM_MOD_EN)
}

/// AMD family 0xF or later (where SYSCFG, TOM and TOM2 exist), from this CPU's CPUID.
pub fn amd_mtrr_msrs(v: &CpuidView) -> bool {
    let vendor = v.get(0, 0);
    let amd = vendor.ebx == u32::from_le_bytes(*b"Auth") && vendor.ecx == u32::from_le_bytes(*b"cAMD");
    amd && (v.get(1, 0).eax >> 8) & 0xF == 0xF
}

/// This CPU's MTRR state; `None` without CPUID MTRR.
pub fn mtrr_state() -> Option<MtrrState> {
    let v = live();
    v.has("mtrr").then(|| MtrrState::read_with(amd_mtrr_msrs(&v), &mut crate::msr::read))
}

/// Clears SYSCFG.MtrrFixDramModEn on this CPU (AMD only; Linux `mtrr/generic.c`): it only makes the fixed
/// MTRRs' RdMem/WrMem bits accessible, and left set on some cores their MTRRs would read differently. Returns
/// (before, after) when it changed something.
pub fn amd_syscfg_fix() -> Option<(u64, u64)> {
    if !amd_mtrr_msrs(&live()) {
        return None;
    }
    let before = crate::msr::read(crate::msr::AMD_SYSCFG);
    if before & SYSCFG_MTRR_FIX_DRAM_MOD_EN == 0 {
        return None;
    }
    let after = before & !SYSCFG_MTRR_FIX_DRAM_MOD_EN;
    crate::msr::write(crate::msr::AMD_SYSCFG, after);
    Some((before, after))
}

/// Loads `bsp`'s MTRRs (and SYSCFG's memory-type bits when `syscfg`) on this CPU by the SDM procedure (Vol. 3A
/// 12.11.7.2, V1 `mtrr_copy`): interrupts off, caches to no-fill and written back, TLB flushed, MTRRs off, the
/// ranges written, caches and TLB flushed again, MTRRs on with the BSP's default type, caches on.
pub fn mtrr_load(bsp: &MtrrState, syscfg: bool) {
    let _irq = IrqGuard::new();
    imp::cache_disable_flush();
    crate::msr::write(crate::msr::IA32_MTRR_DEF_TYPE, bsp.def_type & !(MTRR_DEF_E | MTRR_DEF_FE));
    if syscfg && let Some((s, _, _)) = bsp.amd {
        let mine = crate::msr::read(crate::msr::AMD_SYSCFG);
        crate::msr::write(crate::msr::AMD_SYSCFG, syscfg_for_ap(s, mine));
    }
    for &(m, v) in &bsp.ranges {
        crate::msr::write(m, v);
    }
    imp::cache_flush_tlb();
    crate::msr::write(crate::msr::IA32_MTRR_DEF_TYPE, bsp.def_type);
    imp::cache_enable();
}

// ---------------------------------------------------------------------------------------------------------
// R10 and the facts for hw_info

/// x86-64-v3 = v2 (CMPXCHG16B, POPCNT, SSE3, SSSE3, SSE4.1, SSE4.2, LAHF) + AVX, AVX2, BMI1, BMI2, F16C, FMA,
/// LZCNT (abm), MOVBE, XSAVE; plus NX, which V2 needs for W^X.
pub const V3_FLAGS: &[&str] = &[
    "cx16", "popcnt", "sse3", "ssse3", "sse4_1", "sse4_2", "lahf_lm", "avx", "avx2", "bmi1", "bmi2", "f16c", "fma",
    "abm", "movbe", "xsave", "nx",
];

/// Names of missing v3 features (empty = fine).
pub fn missing_v3(v: &CpuidView) -> Vec<&'static str> {
    V3_FLAGS.iter().copied().filter(|n| !v.has(n)).collect()
}

/// R10: BitsOS requires x86-64-v3 (V2-PLAN §1). Returns the names of missing features (empty = fine).
pub fn missing_v3_features() -> Vec<&'static str> {
    missing_v3(&live())
}

/// Display family, model and stepping from CPUID 1.EAX (extended fields combined as the SDM and APM say).
pub fn decode_fms(eax: u32) -> (u32, u32, u32) {
    let stepping = eax & 0xF;
    let base_model = (eax >> 4) & 0xF;
    let base_family = (eax >> 8) & 0xF;
    let ext_model = (eax >> 16) & 0xF;
    let ext_family = (eax >> 20) & 0xFF;
    let family = if base_family == 0xF { base_family + ext_family } else { base_family };
    let model = if base_family == 0xF || base_family == 0x6 { (ext_model << 4) | base_model } else { base_model };
    (family, model, stepping)
}

fn regs_text(rs: &[u32]) -> String {
    let mut b: Vec<u8> = rs.iter().flat_map(|r| r.to_le_bytes()).collect();
    if let Some(end) = b.iter().position(|&c| c == 0) {
        b.truncate(end);
    }
    String::from_utf8_lossy(&b).trim().into()
}

/// Facts about the CPU for `hw_info` and the boot event (read-only).
#[derive(Clone, Debug, Default)]
pub struct CpuFacts {
    pub vendor: String,
    pub brand: String,
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    pub max_basic_leaf: u32,
    pub max_ext_leaf: u32,
    pub phys_addr_bits: u32,
    pub virt_addr_bits: u32,
    pub initial_apic_id: u32,
    /// Every entry of `FLAGS` with its value, in table order.
    pub flags: Vec<(&'static str, bool)>,
    pub xcr0_supported: u64,
    /// Bytes of the standard-format XSAVE area for the XCR0 now set (CPUID 0xD.0 EBX).
    pub xsave_size: u32,
    /// Bytes for every state the CPU supports (CPUID 0xD.0 ECX).
    pub xsave_size_max: u32,
    /// Bytes of the compacted XSAVES area for XCR0 | IA32_XSS (CPUID 0xD.1 EBX).
    pub xsaves_size: u32,
    pub missing_v3: Vec<&'static str>,
    pub xcr0: u64,
    pub cr0: u64,
    pub cr4: u64,
    pub efer: u64,
    pub pat: u64,
    pub mca_banks: u32,
}

impl CpuFacts {
    /// The CPUID part (pure; `facts` adds the live registers).
    pub fn from_view(v: &CpuidView) -> Self {
        let l0 = v.get(0, 0);
        let l1 = v.get(1, 0);
        let (family, model, stepping) = decode_fms(l1.eax);
        let brand_regs: Vec<u32> = (0x8000_0002..=0x8000_0004)
            .flat_map(|l| {
                let r = v.get(l, 0);
                [r.eax, r.ebx, r.ecx, r.edx]
            })
            .collect();
        let a = v.get(0x8000_0008, 0);
        let d0 = v.get(0xD, 0);
        let d1 = v.get(0xD, 1);
        CpuFacts {
            vendor: regs_text(&[l0.ebx, l0.edx, l0.ecx]),
            brand: regs_text(&brand_regs),
            family,
            model,
            stepping,
            max_basic_leaf: v.max_basic(),
            max_ext_leaf: v.max_ext(),
            phys_addr_bits: a.eax & 0xFF,
            virt_addr_bits: (a.eax >> 8) & 0xFF,
            initial_apic_id: l1.ebx >> 24,
            flags: FLAGS.iter().map(|d| (d.name, v.bit(d))).collect(),
            xcr0_supported: (d0.edx as u64) << 32 | d0.eax as u64,
            xsave_size: d0.ebx,
            xsave_size_max: d0.ecx,
            xsaves_size: d1.ebx,
            missing_v3: missing_v3(v),
            ..Default::default()
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|&(n, on)| on && n == name)
    }
}

/// Facts about the CPU this runs on, with the live register values.
pub fn facts() -> CpuFacts {
    let v = live();
    let mut f = CpuFacts::from_view(&v);
    f.xcr0 = read_xcr0();
    f.cr0 = read_cr0();
    f.cr4 = read_cr4();
    f.efer = crate::msr::read(crate::msr::IA32_EFER);
    f.pat = if v.has("pat") { crate::msr::read(crate::msr::IA32_PAT) } else { 0 };
    f.mca_banks = if v.has("mca") { (crate::msr::read(crate::msr::IA32_MCG_CAP) & 0xFF) as u32 } else { 0 };
    f
}

// ---------------------------------------------------------------------------------------------------------
// Machine check: `init_this_cpu` sets CR4.MCE; at boot the banks are read (never written) and logged.

pub const MCI_STATUS_VAL: u64 = 1 << 63;
pub const MCI_STATUS_OVER: u64 = 1 << 62;
pub const MCI_STATUS_UC: u64 = 1 << 61;
pub const MCI_STATUS_EN: u64 = 1 << 60;
pub const MCI_STATUS_MISCV: u64 = 1 << 59;
pub const MCI_STATUS_ADDRV: u64 = 1 << 58;
pub const MCI_STATUS_PCC: u64 = 1 << 57;

/// A bank holding a valid record (left by the firmware, a previous boot or the error that caused a reset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct McaRecord {
    pub bank: u32,
    pub status: u64,
    pub addr: Option<u64>,
    pub misc: Option<u64>,
    /// AMD SMCA: which block the bank belongs to (MCA_IPID).
    pub ipid: Option<u64>,
}

impl McaRecord {
    pub fn uncorrected(&self) -> bool {
        self.status & MCI_STATUS_UC != 0
    }
    /// The architectural MCA error code (bits 15:0).
    pub fn mca_code(&self) -> u16 {
        self.status as u16
    }
}

#[derive(Clone, Debug, Default)]
pub struct McaReport {
    pub supported: bool,
    pub mcg_cap: u64,
    pub mcg_status: u64,
    pub banks: u32,
    pub records: Vec<McaRecord>,
}

/// What `mca_collect` asks its reader for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McaReg {
    Status,
    Addr,
    Misc,
    Ipid,
}

/// Turns bank reads into records (pure). ADDR and MISC are read only when STATUS says they are valid.
pub fn mca_collect(banks: u32, smca: bool, read: &mut dyn FnMut(u32, McaReg) -> u64) -> Vec<McaRecord> {
    let mut out = Vec::new();
    for b in 0..banks {
        let status = read(b, McaReg::Status);
        if status & MCI_STATUS_VAL == 0 {
            continue;
        }
        out.push(McaRecord {
            bank: b,
            status,
            addr: (status & MCI_STATUS_ADDRV != 0).then(|| read(b, McaReg::Addr)),
            misc: (status & MCI_STATUS_MISCV != 0).then(|| read(b, McaReg::Misc)),
            ipid: smca.then(|| read(b, McaReg::Ipid)),
        });
    }
    out
}

/// How many of `banks` (MCG_CAP.Count) have addresses in the layout: all of them with SMCA, at most 32 in the
/// legacy layout (0x480 on is VMX, not bank 32).
pub fn mca_readable_banks(banks: u32, smca: bool) -> u32 {
    if smca { banks } else { banks.min(crate::msr::LEGACY_MCA_BANKS) }
}

/// Reads MCG_CAP/MCG_STATUS and every bank's status (read-only; Linux logs the same at boot). AMD SMCA CPUs
/// are read at their SMCA addresses, as Linux does.
pub fn mca_report(f: &Features) -> McaReport {
    use crate::msr;
    if !(f.mca && f.mce) {
        return McaReport::default();
    }
    let mcg_cap = msr::read(msr::IA32_MCG_CAP);
    let banks = (mcg_cap & 0xFF) as u32;
    let smca = f.smca;
    let reg = |b: u32, r: u32| msr::mc_reg(smca, b, r).map_or(0, msr::read);
    let records = mca_collect(mca_readable_banks(banks, smca), smca, &mut |b, r| match r {
        McaReg::Status => reg(b, msr::MC_STATUS),
        McaReg::Addr => reg(b, msr::MC_ADDR),
        McaReg::Misc => reg(b, msr::MC_MISC),
        McaReg::Ipid => reg(b, msr::SMCA_IPID),
    });
    McaReport { supported: true, mcg_cap, mcg_status: msr::read(msr::IA32_MCG_STATUS), banks, records }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    /// A recorded CPUID table: (leaf, sub) -> registers.
    struct Table(BTreeMap<(u32, u32), CpuidResult>);

    fn bits(list: &[u8]) -> u32 {
        list.iter().fold(0, |a, &b| a | 1 << b)
    }
    fn r(eax: u32, ebx: u32, ecx: u32, edx: u32) -> CpuidResult {
        CpuidResult { eax, ebx, ecx, edx }
    }
    fn text(s: &[u8]) -> Vec<u32> {
        s.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }
    fn vendor(t: &mut BTreeMap<(u32, u32), CpuidResult>, max: u32, name: &[u8; 12]) {
        let w = text(name);
        t.insert((0, 0), r(max, w[0], w[2], w[1]));
    }
    fn brand(t: &mut BTreeMap<(u32, u32), CpuidResult>, name: &str) {
        let mut b = name.as_bytes().to_vec();
        b.resize(48, 0);
        let w = text(&b);
        for i in 0..3 {
            t.insert((0x8000_0002 + i as u32, 0), r(w[i * 4], w[i * 4 + 1], w[i * 4 + 2], w[i * 4 + 3]));
        }
    }
    const EDX1: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 12, 13, 14, 15, 16, 17, 19, 23, 24, 25, 26, 28];

    /// Zen 4 like the Mini-PC's Ryzen 7 PRO 8845HS (family 0x19 model 0x75 stepping 2).
    fn zen4() -> Table {
        let mut t = BTreeMap::new();
        vendor(&mut t, 0x10, b"AuthenticAMD");
        let ecx1 = bits(&[0, 1, 3, 9, 12, 13, 17, 19, 20, 21, 22, 23, 25, 26, 27, 28, 29, 30]);
        t.insert((1, 0), r(0x00A7_0F52, 0x0010_0800, ecx1, bits(EDX1)));
        t.insert((6, 0), r(0x4, 0, 0, 0));
        let ebx7 = bits(&[0, 3, 5, 7, 8, 9, 10, 16, 17, 18, 19, 20, 21, 23, 24, 28, 29, 30, 31]);
        let ecx7 = bits(&[1, 2, 3, 4, 6, 7, 8, 9, 10, 11, 12, 14, 22]);
        t.insert((7, 0), r(1, ebx7, ecx7, bits(&[4])));
        t.insert((7, 1), r(bits(&[5]), 0, 0, 0));
        t.insert((0xD, 0), r(0x2E7, 0x980, 0x988, 0));
        t.insert((0xD, 1), r(0xF, 0x998, 0, 0));
        t.insert((0x8000_0000, 0), r(0x8000_0028, 0, 0, 0));
        t.insert((0x8000_0001, 0), r(0, 0, bits(&[0, 2, 5, 6, 8, 22]), bits(&[11, 20, 26, 27, 29])));
        brand(&mut t, "AMD Ryzen 7 PRO 8845HS w/ Radeon 780M Graphics");
        t.insert((0x8000_0007, 0), r(0, bits(&[1, 3]), 0, bits(&[8])));
        t.insert((0x8000_0008, 0), r(0x3030, bits(&[4, 9]), 0, 0));
        Table(t)
    }

    /// Arrow Lake like the Gaming-PC's Core Ultra 7 265K: v3, hybrid, no AVX-512, TSC-deadline.
    fn arrow_lake() -> Table {
        let mut t = BTreeMap::new();
        vendor(&mut t, 0x23, b"GenuineIntel");
        let ecx1 = bits(&[0, 1, 3, 9, 12, 13, 17, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30]);
        t.insert((1, 0), r(0x000C_0662, 0x0080_0800, ecx1, bits(EDX1)));
        // The host's flags: turbo, arat, pln, pts, hwp, hwp_notify, hwp_act_window, hwp_epp, hwp_pkg_req.
        t.insert((6, 0), r(bits(&[1, 2, 4, 6, 7, 8, 9, 10, 11]), 0, 0, 0));
        let ebx7 = bits(&[0, 1, 3, 5, 7, 8, 9, 10, 18, 19, 20, 23, 24, 29]);
        let ecx7 = bits(&[2, 5, 8, 9, 10, 22, 27, 28]);
        t.insert((7, 0), r(1, ebx7, ecx7, bits(&[4, 14, 15])));
        t.insert((7, 1), r(bits(&[4]), 0, 0, 0));
        t.insert((0xD, 0), r(0x207, 0x340, 0xA88, 0));
        t.insert((0xD, 1), r(0xF, 0x670, 0, 0));
        t.insert((0x8000_0000, 0), r(0x8000_0008, 0, 0, 0));
        t.insert((0x8000_0001, 0), r(0, 0, bits(&[0, 5, 8]), bits(&[11, 20, 26, 27, 29])));
        brand(&mut t, "Intel(R) Core(TM) Ultra 7 265K");
        t.insert((0x8000_0007, 0), r(0, 0, 0, bits(&[8])));
        t.insert((0x8000_0008, 0), r(0x3027, 0, 0, 0));
        Table(t)
    }

    /// An Ivy-Bridge-like CPU: AVX but no AVX2/BMI/FMA/MOVBE/LZCNT.
    fn old() -> Table {
        let mut t = BTreeMap::new();
        vendor(&mut t, 0xD, b"GenuineIntel");
        let ecx1 = bits(&[0, 1, 9, 13, 19, 20, 23, 25, 26, 27, 28, 29, 30]);
        t.insert((1, 0), r(0x0003_06A9, 0, ecx1, bits(EDX1)));
        t.insert((7, 0), r(0, bits(&[0, 7, 9]), 0, 0));
        t.insert((0x8000_0000, 0), r(0x8000_0008, 0, 0, 0));
        t.insert((0x8000_0001, 0), r(0, 0, bits(&[0]), bits(&[11, 20, 27, 29])));
        Table(t)
    }

    fn with<T>(t: &Table, f: impl FnOnce(&CpuidView) -> T) -> T {
        let g = |l, s| t.0.get(&(l, s)).copied().unwrap_or_default();
        f(&CpuidView::new(&g))
    }

    #[test]
    fn every_used_name_is_in_the_table() {
        for n in FEATURE_NAMES.iter().chain(V3_FLAGS) {
            assert!(FLAGS.iter().any(|d| d.name == *n), "{n} missing from FLAGS");
        }
        let mut names: Vec<&str> = FLAGS.iter().map(|d| d.name).collect();
        names.sort();
        let len = names.len();
        names.dedup();
        assert_eq!(len, names.len(), "duplicate flag name");
        assert!(FLAGS.iter().all(|d| d.bit < 32));
    }

    #[test]
    fn zen4_features_and_facts() {
        let t = zen4();
        let (f, facts) = with(&t, |v| (Features::from_view(v), CpuFacts::from_view(v)));
        assert!(f.amd && !f.intel && f.nx && f.smep && f.smap && f.umip && f.fsgsbase && f.avx512f);
        assert!(f.xsave && f.xsaves && f.rdpid && f.rdtscp && f.pcid && f.invpcid && f.mce && f.mca && f.smca);
        assert!(f.invariant_tsc && !f.tsc_deadline && !f.hypervisor && !f.la57 && f.pge && f.pat);
        assert_eq!((facts.family, facts.model, facts.stepping), (0x19, 0x75, 2));
        assert_eq!(facts.vendor, "AuthenticAMD");
        assert_eq!(facts.brand, "AMD Ryzen 7 PRO 8845HS w/ Radeon 780M Graphics");
        assert_eq!((facts.phys_addr_bits, facts.virt_addr_bits), (48, 48));
        assert!(facts.missing_v3.is_empty(), "{:?}", facts.missing_v3);
        assert!(facts.has("avx512_bf16") && facts.has("avx512_vnni") && facts.has("vaes") && !facts.has("avx_vnni"));
        assert_eq!(facts.flags.len(), FLAGS.len());
        let x = xcr0_wanted(facts.xcr0_supported);
        assert_eq!(x, XSTATE_X87 | XSTATE_SSE | XSTATE_AVX | XSTATE_AVX512, "PKRU (bit 9) stays off");
    }

    #[test]
    fn arrow_lake_features() {
        let t = arrow_lake();
        let (f, facts) = with(&t, |v| (Features::from_view(v), CpuFacts::from_view(v)));
        assert!(f.intel && !f.amd && !f.avx512f && f.tsc_deadline && f.fsgsbase && f.pcid && f.invpcid && !f.smca);
        assert_eq!((facts.family, facts.model, facts.stepping), (6, 0xC6, 2));
        assert!(facts.missing_v3.is_empty(), "{:?}", facts.missing_v3);
        assert!(facts.has("hybrid_cpu") && facts.has("avx_vnni") && facts.has("serialize"));
        assert_eq!(xcr0_wanted(facts.xcr0_supported), XSTATE_X87 | XSTATE_SSE | XSTATE_AVX);
        assert_eq!(facts.brand, "Intel(R) Core(TM) Ultra 7 265K");
        assert_eq!(facts.phys_addr_bits, 39);
        assert!(facts.has("hwp") && facts.has("arat"));
    }

    /// CP-6: the Intel paths never take an AMD-only MSR, and Intel RAPL is decided by model, never in a guest.
    #[test]
    fn intel_msr_paths() {
        let arl = arrow_lake();
        assert!(!with(&arl, amd_mtrr_msrs), "no SYSCFG/TOM/TOM2 on Intel");
        assert!(with(&arl, crate::msr::intel_rapl_model), "265K (family 6 model 0xC6) has RAPL");
        assert!(!with(&zen4(), crate::msr::intel_rapl_model) && with(&zen4(), amd_mtrr_msrs));
        assert!(!with(&zen4(), |v| v.has("hwp")));
        assert!(!with(&old(), crate::msr::intel_rapl_model), "Ivy Bridge is not on the list");
        let mut guest = arrow_lake();
        let one = guest.0.get_mut(&(1, 0)).unwrap();
        one.ecx |= 1 << 31;
        assert!(!with(&guest, crate::msr::intel_rapl_model), "KVM passes the model through but has no RAPL MSRs");
    }

    #[test]
    fn old_cpu_is_refused() {
        let t = old();
        let m = with(&t, |v| missing_v3(v));
        for n in ["avx2", "bmi1", "bmi2", "fma", "movbe", "abm"] {
            assert!(m.contains(&n), "{n} not reported missing: {m:?}");
        }
        assert!(!m.contains(&"avx") && !m.contains(&"nx"));
    }

    #[test]
    fn leaves_beyond_max_read_zero() {
        // Max basic leaf 1 and no extended leaves: Intel would answer leaf 7 with leaf 1's data.
        let g = |l: u32, _s: u32| match l {
            0 => r(1, 0, 0, 0),
            0x8000_0000 => r(0x8000_0000, 0, 0, 0),
            _ => r(!0, !0, !0, !0),
        };
        let v = CpuidView::new(&g);
        assert_eq!(v.get(7, 0), CpuidResult::default());
        assert!(!v.has("avx2") && !v.has("nx"));
        assert!(v.has("avx"), "leaf 1 is in range");
        // Leaf 7 sub-leaf 1 only when 7.0 EAX says so.
        let t = old();
        assert!(!with(&t, |v| v.has("avx_vnni")));
    }

    #[test]
    fn xcr0_rules() {
        assert_eq!(xcr0_wanted(0), XSTATE_X87 | XSTATE_SSE);
        assert_eq!(xcr0_wanted(0x7), 0x7);
        // MPX, PKRU, AMX, APX are never enabled.
        assert_eq!(xcr0_wanted(0x7 | 0x18 | 0x200 | 0x6_0000 | 0x8_0000), 0x7);
        // Partial AVX-512, or AVX-512 without AVX: none of it.
        assert_eq!(xcr0_wanted(0x7 | XSTATE_OPMASK), 0x7);
        assert_eq!(xcr0_wanted(0x3 | XSTATE_AVX512), 0x3);
        assert_eq!(xcr0_wanted(0xE7), 0xE7);
    }

    #[test]
    fn cr4_keeps_and_clears() {
        let all = Features { pge: true, mce: true, xsave: true, smep: true, smap: true, umip: true, fsgsbase: true, ..Default::default() };
        let cur = CR4_PAE | CR4_TSD | CR4_PCE | CR4_CET | CR4_PKE | CR4_VME | CR4_PCIDE | CR4_DE;
        let w = cr4_wanted(cur, &all);
        assert_eq!(w & (CR4_TSD | CR4_PCE | CR4_CET | CR4_PKE | CR4_VME), 0);
        for b in [CR4_PAE, CR4_PCIDE, CR4_DE, CR4_PGE, CR4_MCE, CR4_OSXSAVE, CR4_SMEP, CR4_SMAP, CR4_UMIP, CR4_FSGSBASE, CR4_OSFXSR, CR4_OSXMMEXCPT] {
            assert!(w & b != 0, "bit {b:#x}");
        }
        assert_eq!(cr4_wanted(0, &Features::default()), CR4_PAE | CR4_OSFXSR | CR4_OSXMMEXCPT);
        assert_eq!(CR0_WANTED & (CR0_CD | CR0_NW | CR0_EM | CR0_TS), 0);
        assert_eq!(CR0_WANTED, 0x8005_0033, "Linux CR0_STATE");
    }

    #[test]
    fn la57_is_refused() {
        assert!(check_handover(CR4_PAE | CR4_OSFXSR | CR4_PGE).is_ok());
        assert!(check_handover(CR4_PAE | CR4_LA57).is_err());
    }

    #[test]
    fn tsc_aux_encoding() {
        assert_eq!(tsc_aux_value(0, 0), 0);
        assert_eq!(tsc_aux_value(15, 0), 15);
        assert_eq!(tsc_aux_value(5, 1), 0x1005, "Linux: (node << 12) | cpu");
        assert_eq!(tsc_aux_split(0x2_0FFF), (0xFFF, 0x20));
        assert_eq!(tsc_aux_split(tsc_aux_value(77, 3)), (77, 3));
        assert!(tsc_aux_cpu_fits(4095) && !tsc_aux_cpu_fits(4096));
        assert_eq!(tsc_aux_value(4096, 0) & TSC_AUX_CPU_MASK, 0, "masked, never spills into the node");
    }

    #[test]
    fn mca_banks_per_layout() {
        assert_eq!(mca_readable_banks(10, false), 10);
        assert_eq!(mca_readable_banks(64, false), 32, "legacy layout ends at bank 31");
        assert_eq!(mca_readable_banks(64, true), 64);
    }

    #[test]
    fn zen4_cet_and_pku_flags() {
        let t = zen4();
        let f = with(&t, |v| Features::from_view(v));
        assert!(f.pku && f.shstk && !f.ibt, "Zen 4: PKU and shadow stacks, no IBT");
    }

    #[test]
    fn flag_bits() {
        assert!(FLAGS.len() <= FlagBits::CAPACITY, "FLAGS outgrew FlagBits");
        let (z, a) = (zen4(), arrow_lake());
        let zb = with(&z, FlagBits::from_view);
        let ab = with(&a, FlagBits::from_view);
        assert_eq!(zb.differences(&zb), Vec::<&str>::new());
        let d = zb.differences(&ab);
        assert!(d.contains(&"avx512f") && d.contains(&"hybrid_cpu") && !d.contains(&"avx2"), "{d:?}");
        let i = FLAGS.iter().position(|f| f.name == "avx2").unwrap();
        assert!(zb.has_index(i) && !zb.has_index(FlagBits::CAPACITY));
    }

    #[test]
    fn ap_checks() {
        let (z, a) = (zen4(), arrow_lake());
        assert_eq!(with(&z, |bsp| with(&z, |ap| ap_missing(bsp, ap))), None);
        // A BSP with AVX-512 refuses an AP without it; the other way round is fine.
        assert_eq!(with(&z, |bsp| with(&a, |ap| ap_missing(bsp, ap))), Some("avx512f"));
        let missing = with(&a, |bsp| with(&z, |ap| ap_missing(bsp, ap)));
        assert!(missing.is_none() || missing == Some("rdpid") || missing == Some("pcid"), "{missing:?}");
        let zs = with(&z, XsaveSig::from_view);
        assert_eq!(xsave_mismatch(&zs, &zs), None);
        assert!(xsave_mismatch(&zs, &with(&a, XsaveSig::from_view)).is_some(), "AVX-512 states differ");
        let mut other = zs;
        other.states[2].0 += 64;
        assert_eq!(xsave_mismatch(&zs, &other), Some("XSAVE: a state's size or offset differs from the BSP's (CPUID 0xD.n)"));
        // A BSP with CET (Zen 4: shadow stacks) admits an AP without it; that AP must not touch the CET MSRs.
        let mut no_cet = zen4();
        let l7 = no_cet.0.get(&(7, 0)).copied().unwrap_or_default();
        no_cet.0.insert((7, 0), CpuidResult { ecx: l7.ecx & !(1 << 7), edx: l7.edx & !(1 << 20), ..l7 });
        assert!(with(&z, cet_msrs_exist), "the Zen 4 BSP has CET MSRs");
        assert_eq!(with(&z, |bsp| with(&no_cet, |ap| ap_missing(bsp, ap))), None, "CET is not required");
        assert!(!with(&no_cet, cet_msrs_exist), "the AP's own CPUID gates the CET MSRs");
    }

    fn mtrr(def: u64, amd: Option<(u64, u64, u64)>) -> MtrrState {
        let cap = 0x508; // FIX, WC, 8 variable ranges
        MtrrState { cap, def_type: def, ranges: mtrr_msrs(cap).into_iter().map(|m| (m, m as u64)).collect(), amd }
    }

    #[test]
    fn mtrr_rules() {
        assert_eq!(mtrr_msrs(0x508).len(), 16 + 11);
        assert_eq!(mtrr_msrs(0x2).len(), 4, "two variable pairs, no fixed ranges");
        let bsp = mtrr(0xC06, None);
        assert_eq!(mtrr_compare(&bsp, &bsp), MtrrAction::Same);
        // QEMU after INIT: everything zero.
        let cleared = MtrrState { def_type: 0, ranges: bsp.ranges.iter().map(|&(m, _)| (m, 0)).collect(), ..bsp.clone() };
        assert_eq!(mtrr_compare(&bsp, &cleared), MtrrAction::Copy { syscfg: false });
        let mut other_cap = bsp.clone();
        other_cap.cap = 0x50A;
        assert!(matches!(mtrr_compare(&bsp, &other_cap), MtrrAction::Refuse(_)));
        let amd = mtrr(0xC06, Some((SYSCFG_MTRR_VAR_DRAM_EN | SYSCFG_MTRR_TOM2_EN, 0xE000_0000, 0x8_6000_0000)));
        let mut ap = amd.clone();
        ap.amd = Some((SYSCFG_MTRR_TOM2_EN | SYSCFG_MTRR_FIX_DRAM_MOD_EN, 0xE000_0000, 0x8_6000_0000));
        assert_eq!(mtrr_compare(&amd, &ap), MtrrAction::Copy { syscfg: true });
        ap.amd = Some((SYSCFG_MTRR_VAR_DRAM_EN | SYSCFG_MTRR_TOM2_EN | SYSCFG_MTRR_FIX_DRAM_MOD_EN, 0xE000_0000, 0x8_6000_0000));
        assert_eq!(mtrr_compare(&amd, &ap), MtrrAction::Same, "ModEn alone is not a memory-type difference");
        ap.amd = Some((0, 0xD000_0000, 0x8_6000_0000));
        assert!(matches!(mtrr_compare(&amd, &ap), MtrrAction::Refuse(_)), "TOM differs");
        assert!(matches!(mtrr_compare(&amd, &bsp), MtrrAction::Refuse(_)));
        let s = syscfg_for_ap(SYSCFG_MTRR_VAR_DRAM_EN | SYSCFG_MTRR_TOM2_EN | 0x1, SYSCFG_MTRR_FIX_DRAM_MOD_EN | SYSCFG_MTRR_FIX_DRAM_EN | 0x40);
        assert_eq!(s, SYSCFG_MTRR_VAR_DRAM_EN | SYSCFG_MTRR_TOM2_EN | 0x40, "memory-type bits from the BSP, ModEn clear, the rest the AP's");
        assert_eq!(syscfg_other_differences(SYSCFG_MTRR_VAR_DRAM_EN | 0x1, SYSCFG_MTRR_FIX_DRAM_MOD_EN | 0x1), 0, "memory-type bits and ModEn do not count");
        assert_eq!(syscfg_other_differences(0x1, 0x40), 0x41);
        let mut reads = Vec::new();
        let st = MtrrState::read_with(true, &mut |m| {
            reads.push(m);
            if m == crate::msr::IA32_MTRRCAP { 0x102 } else { 7 }
        });
        assert_eq!(st.ranges.len(), 4 + 11);
        assert_eq!(st.amd, Some((7, 7, 7)));
        assert!(reads.contains(&crate::msr::AMD_TOM2));
    }

    #[test]
    fn fms_decoding() {
        assert_eq!(decode_fms(0x00A7_0F52), (0x19, 0x75, 2));
        assert_eq!(decode_fms(0x000C_0662), (6, 0xC6, 2));
        assert_eq!(decode_fms(0x0001_0543), (5, 4, 3), "extended model only for family 6 and 0xF");
    }

    #[test]
    fn mca_records() {
        let status3 = MCI_STATUS_VAL | MCI_STATUS_ADDRV | MCI_STATUS_EN | 0x0135;
        let mut reads = Vec::new();
        let recs = mca_collect(5, true, &mut |b, reg| {
            reads.push((b, reg));
            match (b, reg) {
                (3, McaReg::Status) => status3,
                (3, McaReg::Addr) => 0x1234_5000,
                (3, McaReg::Ipid) => 0xB0_0000_0000,
                (_, McaReg::Status) => 0,
                _ => panic!("read {reg:?} of empty bank {b}"),
            }
        });
        assert_eq!(recs, alloc::vec![McaRecord { bank: 3, status: status3, addr: Some(0x1234_5000), misc: None, ipid: Some(0xB0_0000_0000) }]);
        assert!(!recs[0].uncorrected());
        assert_eq!(recs[0].mca_code(), 0x135);
        assert!(!reads.contains(&(3, McaReg::Misc)), "MISC is read only with MISCV");
        assert!(mca_collect(0, false, &mut |_, _| panic!("no banks")).is_empty());
    }
}

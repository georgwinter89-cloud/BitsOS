//! Local APIC, IPIs and the legacy 8259s (owner: CP, item CP-3; spec/core.md §5.1, §5.2, §11.10, §11.13).
//!
//! The BSP's LAPIC is switched to **x2APIC** (MSR access, no MMIO window per core, no delivery-status polling)
//! when CPUID offers it; QEMU CPU models without x2APIC (and `apic=xapic` in CONFIG.TXT) keep the **xAPIC** at its
//! MMIO page, mapped uncached through `hal::mmio`. Firmware that already turned x2APIC on keeps it (leaving it
//! needs a full disable). `init_bsp` then sets TPR 0, the spurious vector 0xFF (EDK2 leaves 0x0F, an exception
//! vector), LINT0 masked, NMI on the pin the MADT names (LINT1 by default), the LVT timer masked with its vector
//! 0xF0 until CP-5, and acknowledges in-service bits the firmware left. The thermal, performance, CMCI and error
//! LVTs, and on AMD the extended LVTs (EILVT, APM Vol. 2 §16.4.5), stay as the firmware set them (spec §5.1):
//! `acpi::irq` reserves the vector of an unmasked one, or masks it (`mask_lvt`) when that vector is not a device
//! vector (an exception vector, a system vector or 0xFF would break the dispatcher).
//!
//! IPIs: in x2APIC mode `wrmsr` to the ICR does not wait for earlier stores, so `mfence; lfence` go before it
//! (V1 `apic/ipi.asm:21-27`, Linux `weak_wrmsr_fence`); xAPIC writes ICR high then low with interrupts off and
//! polls the delivery-status bit with a bound. The public `send_ipi` sends only fixed IPIs on vectors
//! 0x30-0xFE and NMIs; INIT and SIPI (which make another CPU run code from a physical page) stay inside hal
//! (`send_init_sipi`, for CP-4's AP start-up).
//!
//! The I/O APICs are plain MMIO behind `hal::mmio::IoMem` and live in safe code (`acpi::ioapic`).

use crate::mmio::IoMem;
use crate::msr;
use crate::portio::raw;
use alloc::boxed::Box;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering};

pub use crate::vectors::{LAPIC_TIMER, SPURIOUS_VECTOR};
use crate::vectors::{FIRST_DYNAMIC, LAST_SYSTEM};

pub mod timer;

// Register offsets in the xAPIC page; the x2APIC MSR is `0x800 + offset / 16` (`x2apic_msr`).
pub const REG_ID: u32 = 0x020;
pub const REG_VERSION: u32 = 0x030;
pub const REG_TPR: u32 = 0x080;
pub const REG_PPR: u32 = 0x0A0;
pub const REG_EOI: u32 = 0x0B0;
pub const REG_LDR: u32 = 0x0D0;
pub const REG_SVR: u32 = 0x0F0;
pub const REG_ISR: u32 = 0x100;
pub const REG_TMR: u32 = 0x180;
pub const REG_IRR: u32 = 0x200;
pub const REG_ESR: u32 = 0x280;
pub const REG_LVT_CMCI: u32 = 0x2F0;
pub const REG_ICR: u32 = 0x300;
/// xAPIC only (x2APIC has a 64-bit ICR).
pub const REG_ICR_HIGH: u32 = 0x310;
pub const REG_LVT_TIMER: u32 = 0x320;
pub const REG_LVT_THERMAL: u32 = 0x330;
pub const REG_LVT_PERF: u32 = 0x340;
pub const REG_LVT_LINT0: u32 = 0x350;
pub const REG_LVT_LINT1: u32 = 0x360;
pub const REG_LVT_ERROR: u32 = 0x370;
pub const REG_TIMER_INIT: u32 = 0x380;
pub const REG_TIMER_CURRENT: u32 = 0x390;
pub const REG_TIMER_DIV: u32 = 0x3E0;
/// AMD extended APIC space (APM Vol. 2 §16.4.5; x2APIC MSRs 0x840 and 0x850-0x853, as Linux reads them).
pub const REG_EXT_FEATURE: u32 = 0x400;
pub const REG_EILVT0: u32 = 0x500;
/// The architecture defines four extended LVT registers (0x500-0x530); a larger count field is clamped.
pub const EILVT_REGS: u32 = 4;

pub const APIC_BASE_BSP: u64 = 1 << 8;
pub const APIC_BASE_EXTD: u64 = 1 << 10;
pub const APIC_BASE_EN: u64 = 1 << 11;
pub const APIC_BASE_ADDR: u64 = 0x000F_FFFF_FFFF_F000;

pub const SVR_ENABLE: u32 = 1 << 8;
pub const LVT_MASKED: u32 = 1 << 16;
pub const LVT_LEVEL: u32 = 1 << 15;
pub const LVT_REMOTE_IRR: u32 = 1 << 14;
pub const LVT_ACTIVE_LOW: u32 = 1 << 13;
pub const LVT_PENDING: u32 = 1 << 12;
pub const DM_MASK: u32 = 0x700;
pub const DM_FIXED: u32 = 0x000;
pub const DM_SMI: u32 = 0x200;
pub const DM_NMI: u32 = 0x400;
pub const DM_INIT: u32 = 0x500;
pub const DM_STARTUP: u32 = 0x600;
pub const DM_EXTINT: u32 = 0x700;
pub const ICR_BUSY: u32 = 1 << 12;
pub const ICR_ASSERT: u32 = 1 << 14;
pub const ICR_SELF: u32 = 1 << 18;
pub const ICR_ALL: u32 = 2 << 18;
pub const ICR_ALL_BUT_SELF: u32 = 3 << 18;

/// MSR number of LAPIC register `reg` in x2APIC mode.
pub const fn x2apic_msr(reg: u32) -> u32 {
    0x800 + (reg >> 4)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Not set up by `init_bsp` (or globally disabled).
    Off,
    XApic,
    X2Apic,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::XApic => "xapic",
            Mode::X2Apic => "x2apic",
        }
    }
    fn from_u8(v: u8) -> Mode {
        match v {
            1 => Mode::XApic,
            2 => Mode::X2Apic,
            _ => Mode::Off,
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            Mode::Off => 0,
            Mode::XApic => 1,
            Mode::X2Apic => 2,
        }
    }
}

/// The mode IA32_APIC_BASE describes.
pub fn mode_of_base(base: u64) -> Mode {
    match (base & APIC_BASE_EN != 0, base & APIC_BASE_EXTD != 0) {
        (false, _) => Mode::Off,
        (true, false) => Mode::XApic,
        (true, true) => Mode::X2Apic,
    }
}

static MODE: AtomicU8 = AtomicU8::new(0);
/// The BSP's LAPIC base (physical); every AP must have its LAPIC there too (the xAPIC window is shared).
static BASE_PHYS: AtomicU64 = AtomicU64::new(0);
/// AMD extended LVT registers this LAPIC has (0 elsewhere), found by `init_bsp`.
static EILVT_COUNT: AtomicU8 = AtomicU8::new(0);
/// The xAPIC window (a leaked `IoMem`), set once by `init_bsp` in xAPIC mode.
static XAPIC: AtomicPtr<IoMem> = AtomicPtr::new(core::ptr::null_mut());

/// The mode `init_bsp` put this LAPIC in (`Off` before).
pub fn mode() -> Mode {
    Mode::from_u8(MODE.load(Ordering::Acquire))
}

fn window() -> Option<&'static IoMem> {
    let p = XAPIC.load(Ordering::Acquire);
    // SAFETY: XAPIC holds null or a pointer from `Box::leak` in `init_bsp`, never freed.
    (!p.is_null()).then(|| unsafe { &*p })
}

/// Whether reading `reg` is architecturally defined in `mode` with `max_lvt` (version bits 16-23): in x2APIC
/// mode `rdmsr` of a register that does not exist raises #GP, so reads go through this list (pure; host-tested).
pub fn readable(reg: u32, mode: Mode, max_lvt: u32) -> bool {
    let in_range = |base: u32| reg >= base && reg < base + 0x80 && (reg - base).is_multiple_of(0x10);
    match reg {
        REG_ID | REG_VERSION | REG_TPR | REG_PPR | REG_LDR | REG_SVR | REG_ESR | REG_ICR => true,
        REG_LVT_TIMER | REG_LVT_LINT0 | REG_LVT_LINT1 | REG_LVT_ERROR | REG_TIMER_INIT | REG_TIMER_CURRENT | REG_TIMER_DIV => true,
        REG_ICR_HIGH => mode == Mode::XApic,
        REG_LVT_PERF => max_lvt >= 4,
        REG_LVT_THERMAL => max_lvt >= 5,
        REG_LVT_CMCI => max_lvt >= 6,
        _ => in_range(REG_ISR) || in_range(REG_TMR) || in_range(REG_IRR),
    }
}

fn raw_read(m: Mode, reg: u32) -> u32 {
    match m {
        Mode::X2Apic => msr::read(x2apic_msr(reg)) as u32,
        Mode::XApic => window().and_then(|w| w.read32(reg as usize).ok()).unwrap_or(0),
        Mode::Off => 0,
    }
}

fn raw_write(m: Mode, reg: u32, v: u32) {
    match m {
        Mode::X2Apic => msr::write(x2apic_msr(reg), v as u64),
        Mode::XApic => {
            if let Some(w) = window() {
                let _ = w.write32(reg as usize, v);
            }
        }
        Mode::Off => {}
    }
}

/// Max LVT index of this LAPIC (version register bits 16-23; 0 before `init_bsp`).
pub fn max_lvt() -> u32 {
    let m = mode();
    if m == Mode::Off { 0 } else { (raw_read(m, REG_VERSION) >> 16) & 0xFF }
}

/// Index of extended LVT register `reg` (0x500, 0x510, ...), whether or not this LAPIC has it.
pub fn eilvt_index(reg: u32) -> Option<u32> {
    let n = reg.checked_sub(REG_EILVT0)? / 0x10;
    (reg.is_multiple_of(0x10) && n < EILVT_REGS).then_some(n)
}

/// Number of AMD extended LVTs (pure; host-tested): only an AMD CPU with CPUID 0x8000_0001 ECX bit 3
/// (ExtApicSpace) has the extended register space at all, and its feature register (0x400) bits 16-23 count them.
pub fn eilvt_count_from(amd_ext_apic_space: bool, ext_feature: impl FnOnce() -> u32) -> u32 {
    if !amd_ext_apic_space {
        return 0;
    }
    ((ext_feature() >> 16) & 0xFF).min(EILVT_REGS)
}

/// Extended LVTs of this LAPIC (0 before `init_bsp` and on every non-AMD CPU).
pub fn eilvt_count() -> u32 {
    EILVT_COUNT.load(Ordering::Acquire) as u32
}

fn amd_ext_apic_space() -> bool {
    let v = crate::cpu::live();
    let vendor = v.get(0, 0);
    // KVM hides ExtApicSpace from guests (its LAPIC has no extended space), so this is bare-metal AMD only.
    vendor.ebx == u32::from_le_bytes(*b"Auth") && v.get(0x8000_0001, 0).ecx & (1 << 3) != 0
}

/// Reads a LAPIC register; `None` before `init_bsp` or when `readable` says the register does not exist.
pub fn read(reg: u32) -> Option<u32> {
    let m = mode();
    let ok = readable(reg, m, max_lvt()) || eilvt_index(reg).is_some_and(|n| n < eilvt_count());
    (m != Mode::Off && ok).then(|| raw_read(m, reg))
}

/// Sets the mask bit of a firmware-owned LVT (thermal, perf, CMCI, error, an AMD extended LVT) that exists on
/// this LAPIC; false for any other register or before `init_bsp`. Used when such an LVT carries a vector the
/// dispatcher cannot serve.
pub fn mask_lvt(reg: u32) -> bool {
    let m = mode();
    let classic = matches!(reg, REG_LVT_THERMAL | REG_LVT_PERF | REG_LVT_CMCI | REG_LVT_ERROR) && readable(reg, m, max_lvt());
    let ext = eilvt_index(reg).is_some_and(|n| n < eilvt_count());
    if m == Mode::Off || !(classic || ext) {
        return false;
    }
    let _irq = crate::cpu::IrqGuard::new();
    let v = raw_read(m, reg);
    raw_write(m, reg, v | LVT_MASKED);
    true
}

/// This CPU's APIC ID (32 bits in x2APIC mode, 8 in xAPIC mode); `None` before `init_bsp`.
pub fn id() -> Option<u32> {
    match mode() {
        Mode::X2Apic => Some(raw_read(Mode::X2Apic, REG_ID)),
        Mode::XApic => Some(raw_read(Mode::XApic, REG_ID) >> 24),
        Mode::Off => None,
    }
}

/// End of interrupt for the highest in-service vector (no-op before `init_bsp`). IRQ-safe, lock-free.
pub fn eoi() {
    raw_write(mode(), REG_EOI, 0);
}

/// Whether `vector` is in service (ISR) / pending (IRR) on this CPU.
pub fn isr_bit(vector: u8) -> bool {
    reg_bit(REG_ISR, vector)
}
pub fn irr_bit(vector: u8) -> bool {
    reg_bit(REG_IRR, vector)
}
/// Whether the in-service vector was accepted level-triggered (TMR).
pub fn tmr_bit(vector: u8) -> bool {
    reg_bit(REG_TMR, vector)
}

fn reg_bit(base: u32, vector: u8) -> bool {
    read(base + 0x10 * (vector as u32 / 32)).is_some_and(|w| w & (1 << (vector % 32)) != 0)
}

fn any_in_service(m: Mode) -> bool {
    (0..8).any(|i| raw_read(m, REG_ISR + 0x10 * i) != 0)
}

/// The LVT and control registers as one moment saw them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LvtSnapshot {
    pub svr: u32,
    pub tpr: u32,
    pub timer: u32,
    pub lint0: u32,
    pub lint1: u32,
    pub error: u32,
    /// `None` where `max_lvt` says the entry does not exist.
    pub perf: Option<u32>,
    pub thermal: Option<u32>,
    pub cmci: Option<u32>,
    /// AMD extended LVTs 0-3 (`None` where this LAPIC has none).
    pub eilvt: [Option<u32>; EILVT_REGS as usize],
}

fn snapshot(m: Mode, max_lvt: u32) -> LvtSnapshot {
    let opt = |reg| readable(reg, m, max_lvt).then(|| raw_read(m, reg));
    let n = eilvt_count();
    LvtSnapshot {
        svr: raw_read(m, REG_SVR),
        tpr: raw_read(m, REG_TPR),
        timer: raw_read(m, REG_LVT_TIMER),
        lint0: raw_read(m, REG_LVT_LINT0),
        lint1: raw_read(m, REG_LVT_LINT1),
        error: raw_read(m, REG_LVT_ERROR),
        perf: opt(REG_LVT_PERF),
        thermal: opt(REG_LVT_THERMAL),
        cmci: opt(REG_LVT_CMCI),
        eilvt: [None; EILVT_REGS as usize],
    }
    .with_eilvt(m, n)
}

impl LvtSnapshot {
    fn with_eilvt(mut self, m: Mode, n: u32) -> Self {
        for i in 0..EILVT_REGS {
            self.eilvt[i as usize] = (i < n).then(|| raw_read(m, REG_EILVT0 + 0x10 * i));
        }
        self
    }
}

/// Which LINT pin carries NMI and its polarity (from the MADT's LAPIC NMI entries; spec §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NmiPin {
    /// 0 or 1.
    pub lint: u8,
    pub active_low: bool,
}

/// What `init_bsp` should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LapicConfig {
    /// Switch to x2APIC when CPUID offers it (default true; `apic=xapic` in CONFIG.TXT turns it off).
    pub want_x2apic: bool,
    /// `None`: LINT1, active high (the virtual-wire convention, Linux's default without a MADT entry).
    pub nmi: Option<NmiPin>,
}

impl Default for LapicConfig {
    fn default() -> Self {
        LapicConfig { want_x2apic: true, nmi: None }
    }
}

/// The LVT values for LINT0 and LINT1 with NMI on `nmi` (pure; host-tested). NMI is always edge (SDM 11.5.1:
/// the trigger mode of an NMI LVT is ignored); the other pin is masked (LINT0 keeps ExtINT as Linux does).
pub fn lint_values(nmi: NmiPin) -> (u32, u32) {
    let nmi_val = DM_NMI | if nmi.active_low { LVT_ACTIVE_LOW } else { 0 };
    if nmi.lint == 0 { (nmi_val, LVT_MASKED | DM_NMI) } else { (LVT_MASKED | DM_EXTINT, nmi_val) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApicError {
    /// The LAPIC cannot be used: xAPIC needed but its page could not be mapped, and no x2APIC.
    NoWindow(crate::mmio::MmioError),
    /// `init_bsp` ran before (the report of the first run stays valid).
    AlreadyUp,
}

/// What `init_bsp` found and did (the boot event and `hw_info`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LapicReport {
    pub mode: Mode,
    pub firmware_mode: Mode,
    pub cpuid_x2apic: bool,
    pub apic_base_before: u64,
    pub apic_base: u64,
    pub base_phys: u64,
    pub id: u32,
    pub version: u32,
    pub max_lvt: u32,
    /// AMD extended LVTs (0 on other CPUs).
    pub eilvt_count: u32,
    pub before: LvtSnapshot,
    pub after: LvtSnapshot,
    /// ESR after clearing it (0 = no error latched).
    pub esr: u32,
    /// In-service vectors the firmware left, acknowledged with EOIs.
    pub stale_isr_acked: u32,
    pub nmi: NmiPin,
    /// xAPIC was asked for, but the firmware had x2APIC on already, so it stays.
    pub kept_firmware_x2apic: bool,
    /// xAPIC was wanted but its page could not be mapped, so x2APIC is used.
    pub xapic_map_failed: bool,
}

/// Sets up the BSP's LAPIC and decides the mode every AP follows (`init_ap`). Interrupts stay as they are (off
/// at boot).
pub fn init_bsp(cfg: &LapicConfig) -> Result<LapicReport, ApicError> {
    if mode() != Mode::Off {
        return Err(ApicError::AlreadyUp);
    }
    let _irq = crate::cpu::IrqGuard::new();
    let before_base = msr::read(msr::IA32_APIC_BASE);
    let cpuid_x2 = crate::cpu::live().has("x2apic");
    let fw = mode_of_base(before_base);
    let mut target = if fw == Mode::X2Apic || (cfg.want_x2apic && cpuid_x2) { Mode::X2Apic } else { Mode::XApic };
    let mut map_failed = false;
    let base_phys = before_base & APIC_BASE_ADDR;
    if before_base & APIC_BASE_EN == 0 {
        msr::write(msr::IA32_APIC_BASE, before_base | APIC_BASE_EN);
    }
    if target == Mode::XApic && window().is_none() {
        match crate::mmio::map_device(base_phys, 0x1000, crate::paging::CacheType::Uncached) {
            Ok(w) => XAPIC.store(Box::leak(Box::new(w)), Ordering::Release),
            Err(_) if cpuid_x2 => {
                target = Mode::X2Apic;
                map_failed = true;
            }
            Err(e) => return Err(ApicError::NoWindow(e)),
        }
    }
    if target == Mode::X2Apic && fw != Mode::X2Apic {
        msr::write(msr::IA32_APIC_BASE, before_base | APIC_BASE_EN | APIC_BASE_EXTD);
    }
    let m = target;
    let version = raw_read(m, REG_VERSION);
    let max_lvt = (version >> 16) & 0xFF;
    let eilvt = eilvt_count_from(amd_ext_apic_space(), || raw_read(m, REG_EXT_FEATURE));
    EILVT_COUNT.store(eilvt as u8, Ordering::Release);
    let before = snapshot(m, max_lvt);
    let nmi = cfg.nmi.unwrap_or(NmiPin { lint: 1, active_low: false });
    let (esr, acked) = program_local(m, nmi);
    let id = if m == Mode::X2Apic { raw_read(m, REG_ID) } else { raw_read(m, REG_ID) >> 24 };
    BASE_PHYS.store(base_phys, Ordering::Release);
    MODE.store(m.as_u8(), Ordering::Release);
    Ok(LapicReport {
        mode: m,
        firmware_mode: fw,
        cpuid_x2apic: cpuid_x2,
        apic_base_before: before_base,
        apic_base: msr::read(msr::IA32_APIC_BASE),
        base_phys,
        id,
        version,
        max_lvt,
        eilvt_count: eilvt,
        before,
        after: snapshot(m, max_lvt),
        esr,
        stale_isr_acked: acked,
        nmi,
        kept_firmware_x2apic: fw == Mode::X2Apic && !cfg.want_x2apic,
        xapic_map_failed: map_failed,
    })
}

/// The register set-up every LAPIC gets (BSP and APs): TPR 0, timer stopped and masked on its vector, spurious
/// vector 0xFF, LINT0/LINT1 for `nmi`, ESR cleared, stale in-service bits acknowledged. Returns (ESR, EOIs sent).
fn program_local(m: Mode, nmi: NmiPin) -> (u32, u32) {
    raw_write(m, REG_TPR, 0);
    raw_write(m, REG_TIMER_INIT, 0);
    raw_write(m, REG_LVT_TIMER, LVT_MASKED | LAPIC_TIMER as u32);
    // Spurious vector first, so anything the LINT changes shake loose lands on 0xFF, not on 0x0F.
    raw_write(m, REG_SVR, SVR_ENABLE | SPURIOUS_VECTOR as u32);
    let (l0, l1) = lint_values(nmi);
    raw_write(m, REG_LVT_LINT0, l0);
    raw_write(m, REG_LVT_LINT1, l1);
    // ESR: xAPIC latches on a write (back to back, Linux `lapic_setup_esr`); x2APIC accepts only 0.
    raw_write(m, REG_ESR, 0);
    raw_write(m, REG_ESR, 0);
    let esr = raw_read(m, REG_ESR);
    // Firmware may leave vectors in service; each EOI retires the highest. 256 is every vector once.
    let mut acked = 0;
    while acked < 256 && any_in_service(m) {
        raw_write(m, REG_EOI, 0);
        acked += 1;
    }
    (esr, acked)
}

/// What `init_ap` found and did on one AP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApLapic {
    pub apic_base_before: u64,
    pub apic_base: u64,
    pub id: u32,
    pub version: u32,
    pub esr: u32,
    pub stale_isr_acked: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApLapicError {
    /// The BSP's LAPIC is not set up (`init_bsp` first).
    BspNotUp,
    /// This AP's LAPIC sits at another physical address than the BSP's (the xAPIC window is shared).
    OtherBase(u64),
    /// The BSP runs x2APIC but this CPU has no x2APIC.
    NoX2Apic,
}

/// Sets up the LAPIC of the AP this runs on, in the BSP's mode (x2APIC or xAPIC), with NMI on `nmi`. Interrupts
/// are off (AP start-up).
pub fn init_ap(nmi: NmiPin) -> Result<ApLapic, ApLapicError> {
    let m = mode();
    if m == Mode::Off {
        return Err(ApLapicError::BspNotUp);
    }
    let _irq = crate::cpu::IrqGuard::new();
    let before = msr::read(msr::IA32_APIC_BASE);
    if before & APIC_BASE_ADDR != BASE_PHYS.load(Ordering::Acquire) {
        return Err(ApLapicError::OtherBase(before & APIC_BASE_ADDR));
    }
    let fw = mode_of_base(before);
    match (m, fw) {
        (Mode::X2Apic, Mode::X2Apic) | (Mode::XApic, Mode::XApic) => {}
        (Mode::X2Apic, _) => {
            if !crate::cpu::live().has("x2apic") {
                return Err(ApLapicError::NoX2Apic);
            }
            // Disabled -> x2APIC is not a valid transition (SDM 11.12.5): enable xAPIC first.
            let en = (before & !APIC_BASE_EXTD) | APIC_BASE_EN;
            msr::write(msr::IA32_APIC_BASE, en);
            msr::write(msr::IA32_APIC_BASE, en | APIC_BASE_EXTD);
        }
        (Mode::XApic, Mode::X2Apic) => {
            // Leaving x2APIC needs a full disable first; this AP has no state worth keeping yet.
            msr::write(msr::IA32_APIC_BASE, before & !(APIC_BASE_EN | APIC_BASE_EXTD));
            msr::write(msr::IA32_APIC_BASE, (before & !APIC_BASE_EXTD) | APIC_BASE_EN);
        }
        (Mode::XApic, _) => msr::write(msr::IA32_APIC_BASE, (before & !APIC_BASE_EXTD) | APIC_BASE_EN),
        (Mode::Off, _) => return Err(ApLapicError::BspNotUp),
    }
    let version = raw_read(m, REG_VERSION);
    let (esr, acked) = program_local(m, nmi);
    let id = if m == Mode::X2Apic { raw_read(m, REG_ID) } else { raw_read(m, REG_ID) >> 24 };
    Ok(ApLapic { apic_base_before: before, apic_base: msr::read(msr::IA32_APIC_BASE), id, version, esr, stale_isr_acked: acked })
}

/// Reprograms LINT0/LINT1 for `nmi` (the MADT's pin, known once the APIC ID is). No-op before `init_bsp`.
pub fn set_nmi_pin(nmi: NmiPin) {
    let m = mode();
    if m == Mode::Off {
        return;
    }
    let _irq = crate::cpu::IrqGuard::new();
    let (l0, l1) = lint_values(nmi);
    raw_write(m, REG_LVT_LINT0, l0);
    raw_write(m, REG_LVT_LINT1, l1);
}

/// What `quiet_local` did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalQuiet {
    /// The timer was unmasked before (it ran or could fire).
    pub timer_was_live: bool,
    /// Timer, LINT0 and the error LVT are masked now.
    pub masked: bool,
}

/// Quiets this CPU's LAPIC for a Reload or a reset (RB-5a change request 10): the timer stopped and masked (in
/// either mode, the TSC deadline cleared), LINT0 and the error LVT masked; LINT1 (NMI) and SVR stay. Works in
/// x2APIC and xAPIC mode (the MMIO window is this module's). Interrupts off inside; no lock, no allocation.
pub fn quiet_local() -> LocalQuiet {
    let m = mode();
    if m == Mode::Off {
        return LocalQuiet::default();
    }
    let _irq = crate::cpu::IrqGuard::new();
    let timer_was_live = raw_read(m, REG_LVT_TIMER) & LVT_MASKED == 0;
    timer::stop();
    for reg in [REG_LVT_LINT0, REG_LVT_ERROR] {
        let v = raw_read(m, reg);
        raw_write(m, reg, v | LVT_MASKED);
    }
    let masked = [REG_LVT_TIMER, REG_LVT_LINT0, REG_LVT_ERROR].iter().all(|&r| raw_read(m, r) & LVT_MASKED != 0);
    LocalQuiet { timer_was_live, masked }
}

/// The LVT and control registers now; `None` before `init_bsp`.
pub fn lvt_snapshot() -> Option<LvtSnapshot> {
    let m = mode();
    (m != Mode::Off).then(|| snapshot(m, max_lvt()))
}

/// IA32_APIC_BASE as it is now.
pub fn apic_base_msr() -> u64 {
    msr::read(msr::IA32_APIC_BASE)
}

// ---------------------------------------------------------------------------------------------------------
// IPIs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiDest {
    /// One CPU by APIC ID (physical destination mode).
    Physical(u32),
    SelfOnly,
    AllButSelf,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiKind {
    Fixed(u8),
    Nmi,
    /// INIT, level assert, no de-assert (0x4500; spec §11.11).
    Init,
    /// Start-up with the trampoline page number (0x4600 | page).
    Startup(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpiError {
    NotEnabled,
    /// An xAPIC destination above 0xFF, a fixed vector outside 0x30-0xFE, a non-fixed IPI to "self", or INIT/SIPI
    /// through the public `send_ipi`.
    Invalid,
    /// xAPIC: the delivery-status bit stayed busy.
    Busy,
    /// An NMI while `pause_nmi_ipis` is held (a test runs on an IST stack a real NMI would overwrite), or a
    /// broadcast NMI while an AP is being started (`block_nmi_broadcast`).
    Paused,
}

/// The ICR value as (low dword, destination field) for `mode` (pure; host-tested). The destination is the full
/// 32-bit APIC ID in x2APIC mode (ICR bits 32-63) and `id << 24` of ICR_HIGH in xAPIC mode.
pub fn icr_value(mode: Mode, dest: IpiDest, kind: IpiKind) -> Result<(u32, u32), IpiError> {
    let low_kind = match kind {
        // Only device and system vectors: 0-15 are illegal for fixed delivery (ESR), 0x10-0x1F would enter an
        // exception gate without the error code some of them expect, 0x20-0x2F is the 8259 range, and 0xFF is the
        // spurious vector, which `vectors::dispatch` never acknowledges (a delivered 0xFF would stay in service
        // and block every interrupt below it).
        IpiKind::Fixed(v) if !(FIRST_DYNAMIC..=LAST_SYSTEM).contains(&v) => return Err(IpiError::Invalid),
        IpiKind::Fixed(v) => DM_FIXED | v as u32,
        IpiKind::Nmi => DM_NMI,
        IpiKind::Init => DM_INIT,
        IpiKind::Startup(page) => DM_STARTUP | page as u32,
    };
    let (shorthand, d) = match dest {
        IpiDest::Physical(id) => (0, id),
        IpiDest::SelfOnly if matches!(kind, IpiKind::Fixed(_)) => (ICR_SELF, 0),
        IpiDest::SelfOnly => return Err(IpiError::Invalid),
        IpiDest::AllButSelf => (ICR_ALL_BUT_SELF, 0),
        IpiDest::All => (ICR_ALL, 0),
    };
    let field = match mode {
        Mode::X2Apic => d,
        Mode::XApic if d <= 0xFF => d << 24,
        Mode::XApic => return Err(IpiError::Invalid),
        Mode::Off => return Err(IpiError::NotEnabled),
    };
    Ok((ICR_ASSERT | shorthand | low_kind, field))
}

/// Bound for the xAPIC delivery-status poll (generous: a real APIC clears it within a microsecond).
const ICR_SPINS: u32 = 1_000_000;

fn xapic_wait_idle() -> Result<(), IpiError> {
    for _ in 0..ICR_SPINS {
        if raw_read(Mode::XApic, REG_ICR) & ICR_BUSY == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(IpiError::Busy)
}

/// Holders of `pause_nmi_ipis` and NMI IPIs being sent right now.
static NMI_PAUSE: AtomicUsize = AtomicUsize::new(0);
static NMI_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// While held, no CPU sends an NMI IPI (`send_ipi` returns `Paused`); taking it waits for sends already past
/// the check. For `idt::soft_int_nmi`'s test, which a real NMI on the same CPU would break. Sources outside the
/// LAPIC ICR (LINT1, a watchdog through perf counters) are not covered.
pub fn pause_nmi_ipis() -> NmiPause {
    NMI_PAUSE.fetch_add(1, Ordering::SeqCst);
    // A send in flight finishes within one ICR write (x2APIC) or the bounded delivery poll (xAPIC).
    while NMI_IN_FLIGHT.load(Ordering::SeqCst) != 0 {
        core::hint::spin_loop();
    }
    NmiPause(())
}

/// See `pause_nmi_ipis`; dropping it allows NMI IPIs again.
pub struct NmiPause(());

impl Drop for NmiPause {
    fn drop(&mut self) {
        NMI_PAUSE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Holders of `block_nmi_broadcast` (an AP start in progress).
static NMI_BROADCAST_BLOCK: AtomicUsize = AtomicUsize::new(0);

/// While held, broadcast NMIs (`All`, `AllButSelf`) return `Paused`; NMIs to one APIC ID still go out. `smp`
/// holds it from an AP's first INIT until that AP has loaded the kernel's IDT (or was parked with INIT): a
/// broadcast reaches a starting AP too, and before its IDT is loaded an NMI triple-faults it, which on real
/// hardware resets the machine. Taking it waits for NMI sends already past the check.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
pub(crate) fn block_nmi_broadcast() -> NmiBroadcastBlock {
    NMI_BROADCAST_BLOCK.fetch_add(1, Ordering::SeqCst);
    while NMI_IN_FLIGHT.load(Ordering::SeqCst) != 0 {
        core::hint::spin_loop();
    }
    NmiBroadcastBlock(())
}

/// See `block_nmi_broadcast`; dropping it allows broadcast NMIs again.
pub(crate) struct NmiBroadcastBlock(());

impl Drop for NmiBroadcastBlock {
    fn drop(&mut self) {
        NMI_BROADCAST_BLOCK.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Sends a fixed IPI (vector 0x30-0xFE) or an NMI from this CPU. INIT and SIPI are refused here. An NMI returns
/// `Paused` while `pause_nmi_ipis` is held, and a broadcast NMI also while an AP is being started (its IDT is not
/// loaded yet; send to the online CPUs one by one through `smp::apic_id` instead).
pub fn send_ipi(dest: IpiDest, kind: IpiKind) -> Result<(), IpiError> {
    match kind {
        IpiKind::Fixed(_) => send_raw(dest, kind),
        IpiKind::Nmi => {
            NMI_IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
            let broadcast = matches!(dest, IpiDest::All | IpiDest::AllButSelf);
            let paused = NMI_PAUSE.load(Ordering::SeqCst) != 0 || (broadcast && NMI_BROADCAST_BLOCK.load(Ordering::SeqCst) != 0);
            let r = if paused { Err(IpiError::Paused) } else { send_raw(dest, kind) };
            NMI_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
            r
        }
        IpiKind::Init | IpiKind::Startup(_) => Err(IpiError::Invalid),
    }
}

/// INIT or SIPI to one CPU by APIC ID: hal's AP start-up only (CP-4), never reachable from safe code outside hal.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
pub(crate) fn send_init_sipi(apic_id: u32, kind: IpiKind) -> Result<(), IpiError> {
    match kind {
        IpiKind::Init | IpiKind::Startup(_) => send_raw(IpiDest::Physical(apic_id), kind),
        IpiKind::Fixed(_) | IpiKind::Nmi => Err(IpiError::Invalid),
    }
}

fn send_raw(dest: IpiDest, kind: IpiKind) -> Result<(), IpiError> {
    let m = mode();
    let (low, field) = icr_value(m, dest, kind)?;
    match m {
        Mode::X2Apic => {
            imp::wrmsr_fence();
            msr::write(x2apic_msr(REG_ICR), (field as u64) << 32 | low as u64);
            Ok(())
        }
        Mode::XApic => {
            let _irq = crate::cpu::IrqGuard::new();
            xapic_wait_idle()?;
            raw_write(m, REG_ICR_HIGH, field);
            raw_write(m, REG_ICR, low);
            xapic_wait_idle()
        }
        Mode::Off => Err(IpiError::NotEnabled),
    }
}

/// `int $0xff`: enters the spurious-vector gate as if the LAPIC had sent a spurious interrupt (tests: the
/// path must count it and send no EOI).
pub fn soft_int_spurious() {
    imp::soft_int_spurious()
}

// ---------------------------------------------------------------------------------------------------------
// The 8259 pair: masked for good (spec §5.2).

/// Masks both 8259 PICs (V2-PLAN §2.4). Kept for the G0 start order.
pub fn quiet_legacy_pic() {
    raw::outb(0x21, 0xFF);
    raw::outb(0xA1, 0xFF);
}

/// Re-initialises both 8259s with their vectors at `base..base+16` (0x20: the reserved range below
/// `vectors::FIRST_DYNAMIC`) and masks every line. LINT0 is masked too, so nothing they do reaches a CPU;
/// the re-init only makes sure no firmware vector base points into the exception range.
pub fn remap_and_mask_pic(base: u8) {
    let io_wait = || raw::outb(0x80, 0);
    raw::outb(0x21, 0xFF);
    raw::outb(0xA1, 0xFF);
    for (cmd, data, icw2, icw3) in [(0x20u16, 0x21u16, base, 0x04u8), (0xA0, 0xA1, base.wrapping_add(8), 0x02)] {
        raw::outb(cmd, 0x11); // ICW1: init, ICW4 follows
        io_wait();
        raw::outb(data, icw2);
        io_wait();
        raw::outb(data, icw3);
        io_wait();
        raw::outb(data, 0x01); // ICW4: 8086 mode
        io_wait();
    }
    raw::outb(0x21, 0xFF);
    raw::outb(0xA1, 0xFF);
}

/// The two interrupt mask registers (master, slave).
pub fn pic_masks() -> (u8, u8) {
    (raw::inb(0x21), raw::inb(0xA1))
}

#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .text
    .global hal_soft_int_spurious
hal_soft_int_spurious:
    int $0xff
    ret
"#,
    options(att_syntax)
);

#[cfg(target_os = "none")]
mod imp {
    unsafe extern "C" {
        fn hal_soft_int_spurious();
    }

    pub fn wrmsr_fence() {
        // SAFETY: two fences, no memory or register effects besides ordering (SDM: x2APIC ICR writes are not
        // serializing, so earlier stores must be globally visible before the IPI; spec §11.10).
        unsafe { core::arch::asm!("mfence", "lfence", options(nostack, preserves_flags)) };
    }

    pub fn soft_int_spurious() {
        // SAFETY: `int 0xff` enters gate 0xFF (DPL 0, no IST); the stub saves and restores every register and
        // `vectors::dispatch` returns without an EOI for this vector, so this behaves like a call.
        unsafe { hal_soft_int_spurious() }
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    pub fn wrmsr_fence() {}
    pub fn soft_int_spurious() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msr_numbers() {
        assert_eq!(x2apic_msr(REG_ID), 0x802);
        assert_eq!(x2apic_msr(REG_EOI), 0x80B);
        assert_eq!(x2apic_msr(REG_SVR), 0x80F);
        assert_eq!(x2apic_msr(REG_ICR), 0x830);
        assert_eq!(x2apic_msr(REG_LVT_LINT1), 0x836);
        assert_eq!(x2apic_msr(REG_TIMER_DIV), 0x83E);
    }

    #[test]
    fn readable_registers() {
        assert!(readable(REG_ISR + 0x70, Mode::X2Apic, 5));
        assert!(!readable(REG_IRR + 0x90, Mode::X2Apic, 5), "0x290: between ESR and CMCI, no register");
        assert!(readable(REG_TMR, Mode::X2Apic, 5));
        assert!(!readable(REG_ICR_HIGH, Mode::X2Apic, 5), "no ICR high in x2APIC mode (#GP)");
        assert!(readable(REG_ICR_HIGH, Mode::XApic, 5));
        assert!(!readable(0xE0, Mode::X2Apic, 5), "DFR does not exist in x2APIC mode");
        assert!(!readable(REG_LVT_CMCI, Mode::X2Apic, 5));
        assert!(readable(REG_LVT_CMCI, Mode::X2Apic, 6));
        assert!(!readable(REG_LVT_THERMAL, Mode::X2Apic, 4));
        assert!(readable(REG_LVT_PERF, Mode::X2Apic, 4));
        assert!(!readable(REG_ISR + 4, Mode::XApic, 5), "unaligned");
        assert!(!readable(REG_EOI, Mode::X2Apic, 5), "EOI is write-only");
    }

    #[test]
    fn base_modes() {
        assert_eq!(mode_of_base(0xFEE0_0900), Mode::XApic);
        assert_eq!(mode_of_base(0xFEE0_0D00), Mode::X2Apic);
        assert_eq!(mode_of_base(0xFEE0_0100), Mode::Off);
    }

    #[test]
    fn lints() {
        assert_eq!(lint_values(NmiPin { lint: 1, active_low: false }), (LVT_MASKED | DM_EXTINT, DM_NMI));
        assert_eq!(lint_values(NmiPin { lint: 0, active_low: true }), (DM_NMI | LVT_ACTIVE_LOW, LVT_MASKED | DM_NMI));
    }

    #[test]
    fn icr() {
        assert_eq!(icr_value(Mode::X2Apic, IpiDest::Physical(0x1234), IpiKind::Fixed(0x40)), Ok((0x4040, 0x1234)));
        assert_eq!(icr_value(Mode::XApic, IpiDest::Physical(3), IpiKind::Fixed(0x40)), Ok((0x4040, 0x0300_0000)));
        assert_eq!(icr_value(Mode::XApic, IpiDest::Physical(0x100), IpiKind::Fixed(0x40)), Err(IpiError::Invalid));
        assert_eq!(icr_value(Mode::X2Apic, IpiDest::SelfOnly, IpiKind::Fixed(0xEE)), Ok((0x4_40EE, 0)));
        assert_eq!(icr_value(Mode::X2Apic, IpiDest::SelfOnly, IpiKind::Nmi), Err(IpiError::Invalid));
        assert_eq!(icr_value(Mode::X2Apic, IpiDest::Physical(5), IpiKind::Init), Ok((0x4500, 5)), "spec: 0x4500, no de-assert");
        assert_eq!(icr_value(Mode::X2Apic, IpiDest::Physical(5), IpiKind::Startup(0x08)), Ok((0x4608, 5)));
        assert_eq!(icr_value(Mode::XApic, IpiDest::AllButSelf, IpiKind::Nmi), Ok((0xC_4400, 0)));
        assert_eq!(icr_value(Mode::X2Apic, IpiDest::All, IpiKind::Fixed(0x30)), Ok((0x8_4030, 0)));
        assert_eq!(icr_value(Mode::X2Apic, IpiDest::Physical(1), IpiKind::Fixed(0x0F)), Err(IpiError::Invalid));
        assert_eq!(icr_value(Mode::Off, IpiDest::Physical(1), IpiKind::Fixed(0x30)), Err(IpiError::NotEnabled));
    }

    #[test]
    fn icr_refuses_vectors_the_dispatcher_cannot_serve() {
        for v in [0x00u8, 0x0F, 0x10, 0x11, 0x15, 0x1D, 0x1E, 0x1F, 0x20, 0x27, 0x2F, 0xFF] {
            for m in [Mode::X2Apic, Mode::XApic] {
                assert_eq!(icr_value(m, IpiDest::SelfOnly, IpiKind::Fixed(v)), Err(IpiError::Invalid), "{v:#x} {m:?}");
            }
        }
        assert!(icr_value(Mode::X2Apic, IpiDest::SelfOnly, IpiKind::Fixed(FIRST_DYNAMIC)).is_ok());
        assert!(icr_value(Mode::X2Apic, IpiDest::SelfOnly, IpiKind::Fixed(LAST_SYSTEM)).is_ok());
    }

    #[test]
    fn public_send_refuses_init_and_sipi() {
        for d in [IpiDest::Physical(1), IpiDest::All, IpiDest::AllButSelf] {
            assert_eq!(send_ipi(d, IpiKind::Init), Err(IpiError::Invalid));
            assert_eq!(send_ipi(d, IpiKind::Startup(8)), Err(IpiError::Invalid));
        }
        assert_eq!(send_ipi(IpiDest::SelfOnly, IpiKind::Fixed(0xFF)), Err(IpiError::Invalid));
        // The crate-private path takes only INIT/SIPI (the host has no LAPIC, so it ends at NotEnabled).
        assert_eq!(send_init_sipi(1, IpiKind::Fixed(0x40)), Err(IpiError::Invalid));
        assert_eq!(send_init_sipi(1, IpiKind::Init), Err(IpiError::NotEnabled));
    }

    #[test]
    fn eilvt_registers() {
        assert_eq!(eilvt_index(REG_EILVT0), Some(0));
        assert_eq!(eilvt_index(0x530), Some(3));
        assert_eq!(eilvt_index(0x540), None, "four registers");
        assert_eq!(eilvt_index(0x508), None);
        assert_eq!(eilvt_index(0x4F0), None);
        assert_eq!(x2apic_msr(REG_EXT_FEATURE), 0x840);
        assert_eq!(x2apic_msr(0x530), 0x853);
        assert_eq!(eilvt_count_from(false, || panic!("not read without ExtApicSpace")), 0);
        assert_eq!(eilvt_count_from(true, || 0x0004_0007), 4);
        assert_eq!(eilvt_count_from(true, || 0x00FF_0000), 4, "clamped to the four registers that exist");
        assert_eq!(eilvt_count_from(true, || 0), 0);
    }
}

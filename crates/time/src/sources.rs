//! The counters the TSC is calibrated against (spec/core.md §7.1, §11.6/7; V1 `time/hpet.asm`, `time/pit.asm`):
//! the HPET (Mini-PC: 14.318 MHz, 32-bit counter, **stopped by the firmware at handover**, comparators only to
//! IRQ 22/23, so BitsOS starts it and never plans HPET interrupts), the ACPI PM timer (3.579545 MHz, 24 or 32
//! bits) and the PIT (channel 2 gate). The caller (CP's `acpi` side) checked the addresses with the register
//! guard; nothing here trusts a value it reads (an absent device reads all ones).

use crate::calib::{self, CalibError, Counter};
use hal::mmio::IoMem;

const HPET_CAP: usize = 0x00;
const HPET_PERIOD: usize = 0x04;
const HPET_CONF: usize = 0x10;
const HPET_COUNT: usize = 0xF0;
const HPET_TIMER0: usize = 0x100;
/// The register block (1 KiB holds the capabilities and up to 24 comparators).
pub const HPET_SIZE: usize = 0x400;
const HPET_CONF_ENABLE: u32 = 1 << 0;
const HPET_CONF_LEGACY: u32 = 1 << 1;
const HPET_TN_INT_ENB: u32 = 1 << 2;
/// Period bounds (V1 `HPET_FS_MIN/MAX`; the spec allows at most 100 ns per count).
pub const HPET_FS_MIN: u32 = 1_000_000;
pub const HPET_FS_MAX: u32 = 100_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HpetError {
    /// The block could not be mapped (`hal::mmio`).
    Map,
    /// Nothing answers (all ones) or the period is outside 1..100 ns.
    Absent(u32),
    BadPeriod(u32),
}

/// What `Hpet::start` found and did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HpetInfo {
    pub phys: u64,
    pub fs_per_count: u32,
    pub comparators: u32,
    pub counter_64bit: bool,
    /// The counter was running when we came (firmware or the kernel before a Reload).
    pub was_running: bool,
    /// Legacy replacement was on (and is off now).
    pub legacy_was_on: bool,
}

impl HpetInfo {
    pub fn hz(&self) -> u64 {
        1_000_000_000_000_000 / self.fs_per_count as u64
    }
}

/// A started HPET (comparator interrupts and legacy replacement off, main counter running).
pub struct Hpet {
    regs: IoMem,
    pub info: HpetInfo,
}

impl Hpet {
    /// Maps and starts the HPET at `phys` (V1 `hpet_init`; also after a Reload).
    pub fn start(phys: u64) -> Result<Hpet, HpetError> {
        let regs = hal::mmio::map_device(phys, HPET_SIZE, hal::paging::CacheType::Uncached).map_err(|_| HpetError::Map)?;
        let cap = regs.read32(HPET_CAP).map_err(|_| HpetError::Map)?;
        let period = regs.read32(HPET_PERIOD).map_err(|_| HpetError::Map)?;
        if cap == u32::MAX {
            return Err(HpetError::Absent(cap));
        }
        if !(HPET_FS_MIN..=HPET_FS_MAX).contains(&period) {
            return Err(HpetError::BadPeriod(period));
        }
        // The last index sits in bits 12:8; 24 comparators fill the 1 KiB block.
        let comparators = (((cap >> 8) & 0x1F) + 1).min(24);
        let conf = regs.read32(HPET_CONF).map_err(|_| HpetError::Map)?;
        for n in 0..comparators as usize {
            let off = HPET_TIMER0 + 0x20 * n;
            if let Ok(v) = regs.read32(off)
                && v & HPET_TN_INT_ENB != 0
            {
                let _ = regs.write32(off, v & !HPET_TN_INT_ENB);
            }
        }
        let _ = regs.write32(HPET_CONF, (conf & !HPET_CONF_LEGACY) | HPET_CONF_ENABLE);
        let info = HpetInfo {
            phys,
            fs_per_count: period,
            comparators,
            counter_64bit: cap & (1 << 13) != 0,
            was_running: conf & HPET_CONF_ENABLE != 0,
            legacy_was_on: conf & HPET_CONF_LEGACY != 0,
        };
        Ok(Hpet { regs, info })
    }

    /// The low 32 bits of the main counter (the same for a 32- and a 64-bit counter).
    pub fn read(&self) -> u64 {
        self.regs.read32(HPET_COUNT).unwrap_or(0) as u64
    }

    pub fn counter(&self) -> Counter {
        Counter { mask: 0xFFFF_FFFF, fs_per_count: self.info.fs_per_count as u64 }
    }

    /// Whether the counter moves (a stopped or absent HPET reads one value).
    pub fn runs(&self) -> bool {
        let a = self.read();
        let t = hal::cpu::rdtsc();
        while hal::cpu::rdtsc().wrapping_sub(t) < 1_000_000 {
            if self.read() != a {
                return true;
            }
            hal::cpu::spin_hint();
        }
        false
    }

    pub fn calibrate(&self) -> Result<u64, CalibError> {
        measure_counter(&mut || self.read(), self.counter())
    }
}

/// The ACPI PM timer at `port` with `bits` (24 or 32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmTimer {
    pub port: u16,
    pub bits: u8,
}

impl PmTimer {
    pub fn read(&self) -> u64 {
        hal::portio::inl(self.port).unwrap_or(0) as u64
    }
    pub fn counter(&self) -> Counter {
        Counter { mask: if self.bits >= 32 { 0xFFFF_FFFF } else { 0xFF_FFFF }, fs_per_count: calib::PM_FS }
    }
    pub fn calibrate(&self) -> Result<u64, CalibError> {
        measure_counter(&mut || self.read() & self.counter().mask, self.counter())
    }
}

fn measure_counter(read: &mut dyn FnMut() -> u64, ctr: Counter) -> Result<u64, CalibError> {
    // Each window end's reads with interrupts off (V1 `tsc_sample`): an interrupt between the TSC and the read
    // would make that sample look slow, and it is dropped anyway, but the window itself may be interrupted.
    let mut tsc = || {
        let _irq = hal::cpu::IrqGuard::new();
        hal::cpu::rdtsc_ordered()
    };
    let mut pause = || {
        for _ in 0..100 {
            hal::cpu::spin_hint();
        }
    };
    calib::measure(read, &mut tsc, &mut pause, ctr, calib::WINDOW_FS)
}

/// PIT channel 2 with the speaker gate: TSC ticks during `ms` ms (at most 54) of PIT time; `None` without a
/// PIT (the gate never ends) or on the host.
pub fn calibrate_with_pit(ms: u64) -> Option<u64> {
    if !hal::BARE_METAL || ms == 0 || ms > 54 {
        return None;
    }
    let ticks = (calib::PIT_HZ * ms / 1000) as u16;
    let _irq = hal::cpu::IrqGuard::new();
    let p61 = hal::portio::inb(0x61).ok()?;
    hal::portio::outb(0x61, (p61 & !0x02) | 0x01).ok()?; // gate on, speaker off
    hal::portio::outb(0x43, 0b1011_0000).ok()?; // channel 2, lobyte/hibyte, mode 0
    hal::portio::outb(0x42, ticks as u8).ok()?;
    hal::portio::outb(0x42, (ticks >> 8) as u8).ok()?;
    let start = hal::cpu::rdtsc_ordered();
    let mut spins = 0u64;
    while hal::portio::inb(0x61).ok()? & 0x20 == 0 {
        spins += 1;
        if spins > 50_000_000 {
            let _ = hal::portio::outb(0x61, p61);
            return None;
        }
    }
    let end = hal::cpu::rdtsc_ordered();
    hal::portio::outb(0x61, p61).ok()?;
    let exact_ns = ticks as u128 * 1_000_000_000 / calib::PIT_HZ as u128;
    Some(((end - start) as u128 * 1_000_000_000 / exact_ns.max(1)) as u64)
}

/// CPUID 0x15 (TSC/crystal ratio) and 0x16 (base MHz) where the CPU enumerates them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuidRates {
    /// Exact rate from leaf 0x15 when the crystal is enumerated.
    pub leaf15_hz: Option<u64>,
    /// Leaf 0x16 base frequency (rough, MHz).
    pub leaf16_mhz: Option<u32>,
}

pub fn cpuid_rates() -> CpuidRates {
    let max = hal::cpu::cpuid(0, 0).eax;
    let mut r = CpuidRates::default();
    if max >= 0x15 {
        let l = hal::cpu::cpuid(0x15, 0);
        r.leaf15_hz = calib::cpuid15_hz(l.eax, l.ebx, l.ecx);
    }
    if max >= 0x16 {
        let mhz = hal::cpu::cpuid(0x16, 0).eax & 0xFFFF;
        r.leaf16_mhz = (mhz != 0).then_some(mhz);
    }
    r
}

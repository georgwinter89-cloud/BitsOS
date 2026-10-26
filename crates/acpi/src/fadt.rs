//! The FADT ("FACP", ACPI 6.5 §5.2.9): fixed hardware registers, reset register, SMI command, boot flags.
//! Field offsets as in V1 `acpi/fadt.asm` and Linux `include/acpi/actbl.h`.

use crate::bytes::{u16_at, u32_at, u64_at, u8_at};
use crate::sdt::{Gas, Space};
use crate::{Note, Notes};

/// ACPI 1.0's FADT length; anything shorter is not a FADT to trust (V1 `fadt_read`).
pub const MIN_LEN: usize = 116;

/// First revision with IAPC_BOOT_ARCH (ACPI 2.0; Linux `FADT2_REVISION_ID` in arch/x86/kernel/acpi/boot.c).
/// Revision 2 FADTs (ACPI 1.0 plus the reset register) leave offset 109 reserved, possibly garbage.
pub const BOOT_ARCH_REVISION: u8 = 3;

/// First revision with the reset register (V1 `fadt.asm`, ACPI 1.0b's revision-2 FADT added it).
pub const RESET_REVISION: u8 = 2;

pub mod flags {
    pub const WBINVD: u32 = 1 << 0;
    /// TMR_VAL_EXT: the PM timer counts 32 bits, else 24.
    pub const TMR_VAL_EXT: u32 = 1 << 8;
    pub const RESET_REG_SUP: u32 = 1 << 10;
    pub const HW_REDUCED_ACPI: u32 = 1 << 20;
    /// Low Power S0 Idle (Modern Standby).
    pub const LOW_POWER_S0_IDLE: u32 = 1 << 21;
}

/// IAPC_BOOT_ARCH bits.
pub mod boot_arch {
    pub const LEGACY_DEVICES: u16 = 1 << 0;
    pub const I8042: u16 = 1 << 1;
    pub const NO_VGA: u16 = 1 << 2;
    pub const NO_MSI: u16 = 1 << 3;
    pub const NO_ASPM: u16 = 1 << 4;
    pub const NO_CMOS_RTC: u16 = 1 << 5;
}

/// The reset register and the value that resets the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetReg {
    pub reg: Gas,
    pub value: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fadt {
    pub revision: u8,
    pub minor_revision: u8,
    pub length: u32,
    pub flags: u32,
    pub boot_arch: u16,
    pub preferred_pm_profile: u8,
    pub sci_int: u16,
    /// System I/O port that ACPI_ENABLE/ACPI_DISABLE go to; 0 = none (hardware already in ACPI mode).
    pub smi_cmd: u32,
    pub acpi_enable: u8,
    pub acpi_disable: u8,
    pub pm1a_evt: Option<Gas>,
    pub pm1b_evt: Option<Gas>,
    pub pm1a_cnt: Option<Gas>,
    pub pm1b_cnt: Option<Gas>,
    pub pm_tmr: Option<Gas>,
    pub gpe0: Option<Gas>,
    pub gpe1: Option<Gas>,
    pub pm1_evt_len: u8,
    pub pm1_cnt_len: u8,
    pub pm_tmr_len: u8,
    pub gpe0_len: u8,
    pub gpe1_len: u8,
    pub gpe1_base: u8,
    pub century: u8,
    /// Present when the table is long and new enough and names an address (whether to use it: `RESET_REG_SUP`).
    pub reset: Option<ResetReg>,
    pub sleep_control: Option<Gas>,
    pub sleep_status: Option<Gas>,
    /// X_DSDT where set, else DSDT.
    pub dsdt: u64,
    pub facs: u64,
}

impl Fadt {
    pub fn has(&self, f: u32) -> bool {
        self.flags & f != 0
    }
    pub fn hw_reduced(&self) -> bool {
        self.has(flags::HW_REDUCED_ACPI)
    }
    pub fn low_power_s0(&self) -> bool {
        self.has(flags::LOW_POWER_S0_IDLE)
    }
    /// The reset register when the firmware says it is supported (Linux's rule). Callers write exactly one
    /// byte (`value`), which is also what lets `check_gas(.., 1, true)` pass 0xCF9.
    pub fn usable_reset(&self) -> Option<ResetReg> {
        self.reset.filter(|_| self.has(flags::RESET_REG_SUP))
    }
    /// `boot_arch` exists from revision 3 (ACPI 2.0) on; before, "no 8042" cannot be told and the answer is
    /// "maybe" (Linux `FADT2_REVISION_ID`).
    pub fn has_8042(&self) -> Option<bool> {
        (self.revision >= BOOT_ARCH_REVISION).then_some(self.boot_arch & boot_arch::I8042 != 0)
    }
    pub fn legacy_devices(&self) -> Option<bool> {
        (self.revision >= BOOT_ARCH_REVISION).then_some(self.boot_arch & boot_arch::LEGACY_DEVICES != 0)
    }
    pub fn no_cmos_rtc(&self) -> bool {
        self.boot_arch & boot_arch::NO_CMOS_RTC != 0
    }
    pub fn no_msi(&self) -> bool {
        self.boot_arch & boot_arch::NO_MSI != 0
    }
    /// PM timer as (port, bits): system I/O only, never on hardware-reduced ACPI.
    pub fn pm_timer_port(&self) -> Option<(u16, u8)> {
        let g = self.pm_tmr?;
        if self.hw_reduced() || g.space != Space::Io || g.address == 0 || g.address > 0xFFFF {
            return None;
        }
        Some((g.address as u16, if self.has(flags::TMR_VAL_EXT) { 32 } else { 24 }))
    }
}

/// Parses a FADT whose checksum the caller checked. `None` when it is shorter than ACPI 1.0's.
pub fn parse(t: &[u8], phys: u64, notes: &mut Notes) -> Option<Fadt> {
    if t.len() < MIN_LEN {
        notes.push(Note::table(*b"FACP", phys, "too_short", t.len() as u64));
        return None;
    }
    let len = t.len();
    let revision = u8_at(t, 8)?;
    let r8 = |o: usize| u8_at(t, o).unwrap_or(0);
    let r32 = |o: usize| u32_at(t, o).unwrap_or(0);
    let mut f = Fadt {
        revision,
        minor_revision: if len >= 132 { r8(131) & 0x0F } else { 0 },
        length: len as u32,
        flags: r32(112),
        boot_arch: if revision >= BOOT_ARCH_REVISION { u16_at(t, 109).unwrap_or(0) } else { 0 },
        preferred_pm_profile: r8(45),
        sci_int: u16_at(t, 46).unwrap_or(0),
        smi_cmd: r32(48),
        acpi_enable: r8(52),
        acpi_disable: r8(53),
        pm1_evt_len: r8(88),
        pm1_cnt_len: r8(89),
        pm_tmr_len: r8(91),
        gpe0_len: r8(92),
        gpe1_len: r8(93),
        gpe1_base: r8(94),
        century: r8(108),
        pm1a_evt: None,
        pm1b_evt: None,
        pm1a_cnt: None,
        pm1b_cnt: None,
        pm_tmr: None,
        gpe0: None,
        gpe1: None,
        reset: None,
        sleep_control: None,
        sleep_status: None,
        dsdt: 0,
        facs: 0,
    };
    let reg = |x_off: usize, legacy_off: usize, bytes: u8, name: &'static str, notes: &mut Notes| {
        pick(t, x_off, legacy_off, bytes, name, phys, notes)
    };
    f.pm1a_evt = reg(148, 56, f.pm1_evt_len, "pm1a_evt", notes);
    f.pm1b_evt = reg(160, 60, f.pm1_evt_len, "pm1b_evt", notes);
    f.pm1a_cnt = reg(172, 64, f.pm1_cnt_len, "pm1a_cnt", notes);
    f.pm1b_cnt = reg(184, 68, f.pm1_cnt_len, "pm1b_cnt", notes);
    f.pm_tmr = reg(208, 76, f.pm_tmr_len, "pm_tmr", notes);
    f.gpe0 = reg(220, 80, f.gpe0_len, "gpe0", notes);
    f.gpe1 = reg(232, 84, f.gpe1_len, "gpe1", notes);
    // Before revision 2 the reset bytes are reserved (V1 rule). ACPI defines the reset register as one byte
    // at bit 0 (V1 and Linux write exactly one byte); any other shape is a firmware bug and not used, so the
    // guard's 0xCF9 exception only ever sees one-byte writes.
    if len >= 129 && revision >= RESET_REVISION {
        let g = Gas::parse(t, 116)?;
        let one_byte = matches!(g.bit_width, 0 | 8) && g.bit_offset == 0;
        if g.address != 0 && !one_byte {
            notes.push(Note::field(*b"FACP", phys, "reset_width", "reset", ((g.bit_width as u64) << 8) | g.bit_offset as u64));
        } else if g.address != 0 {
            f.reset = Some(ResetReg { reg: g, value: r8(128) });
        }
    }
    if len >= 268 {
        f.sleep_control = Gas::parse(t, 244).filter(|g| g.address != 0);
        f.sleep_status = Gas::parse(t, 256).filter(|g| g.address != 0);
    }
    let dsdt32 = r32(40) as u64;
    let xdsdt = if len >= 148 { u64_at(t, 140).unwrap_or(0) } else { 0 };
    if xdsdt != 0 && dsdt32 != 0 && xdsdt != dsdt32 {
        notes.push(Note::table(*b"FACP", phys, "dsdt_mismatch", xdsdt));
    }
    f.dsdt = if xdsdt != 0 { xdsdt } else { dsdt32 };
    let facs32 = r32(36) as u64;
    let xfacs = if len >= 140 { u64_at(t, 132).unwrap_or(0) } else { 0 };
    f.facs = if xfacs != 0 { xfacs } else { facs32 };
    Some(f)
}

/// The X_ register if the table holds it and it names an address, else the legacy port as a system-I/O
/// register of `bytes` bytes, else none (V1 `fadt_gas`; ACPICA prefers X_ too). Differences are noted.
fn pick(t: &[u8], x_off: usize, legacy_off: usize, bytes: u8, name: &'static str, phys: u64, notes: &mut Notes) -> Option<Gas> {
    let legacy = u32_at(t, legacy_off).unwrap_or(0);
    let x = if t.len() >= x_off + Gas::LEN { Gas::parse(t, x_off).filter(|g| g.address != 0) } else { None };
    match x {
        Some(g) => {
            if legacy != 0 && (g.space != Space::Io || g.address != legacy as u64) {
                notes.push(Note::field(*b"FACP", phys, "x_mismatch", name, g.address));
            }
            Some(g)
        }
        None if legacy != 0 => Some(Gas::io(legacy, bytes)),
        None => None,
    }
}

//! The Intel chipset's TCO watchdog, facts only (plan/spec/gaming-pc-roadmap.md R3): does it exist on this board,
//! where, and in what state did the firmware leave it. Read once at boot on an Intel CPU, logged, never written: no
//! timer is started, stopped or reloaded and no status bit is cleared (clearing is a write of 1). A driver comes
//! later, once a real machine has printed these lines (`plan/spec/gaming-pc-roadmap.md` R3).
//!
//! Where: the chipset's SMBus function, device 31 function 4, holds the TCO I/O base at config 0x50 and its enable at
//! 0x54 (linux-7.3.1/drivers/i2c/busses/i2c-i801.c:153-166, :1428-1440). The registers:
//! linux-7.3.1/drivers/watchdog/iTCO_wdt.c:72-80; their bits: :234-239 (NO_REBOOT, version 6), :297-303 (halt),
//! :535-536 (time-out and second time-out status).
//!
//! On AMD nothing here runs (the Mini-PC's watchdog is the FCH's, `fch-wdt`).

use alloc::format;
use alloc::string::String;

/// The chipset's SMBus function sits on the chipset's root bus: bus 0 on most boards, bus 0x80 on boards with a
/// second root for the chipset (the Gaming-PC's Z890).
pub const BUSES: [u8; 2] = [0x00, 0x80];
pub const SMBUS_DEV: u8 = 31;
pub const SMBUS_FN: u8 = 4;
pub const VENDOR_INTEL: u16 = 0x8086;
/// Class 0x0C (serial bus), subclass 0x05 (SMBus).
pub const CLASS_SMBUS: u32 = 0x0C05;
/// i2c-i801.c:153-154, :166.
pub const CFG_TCOBASE: u8 = 0x50;
pub const CFG_TCOCTL: u8 = 0x54;
pub const TCOCTL_EN: u32 = 1 << 8;
/// iTCO_wdt.c:72, :76-78, :80.
pub const TCO_RLD: u16 = 0x00;
pub const TCO1_STS: u16 = 0x04;
pub const TCO2_STS: u16 = 0x06;
pub const TCO1_CNT: u16 = 0x08;
pub const TCO_TMR: u16 = 0x12;
/// TCO1_STS bit 3: the timer ran out once (iTCO_wdt.c:535).
pub const STS1_TIMEOUT: u16 = 1 << 3;
/// TCO2_STS bit 1: it ran out twice, which resets the machine unless NO_REBOOT is set (iTCO_wdt.c:536); bit 2: the
/// firmware did not finish its start (:546).
pub const STS2_SECOND_TO: u16 = 1 << 1;
pub const STS2_BOOT: u16 = 1 << 2;
/// TCO1_CNT bit 0: NO_REBOOT on this chipset generation (iTCO_wdt.c:234-239); bit 11: timer halted (:297-303); bit
/// 12: the control register is locked until reset (Intel's chipset datasheets; Linux has no name for it).
pub const CNT_NO_REBOOT: u16 = 1 << 0;
pub const CNT_TMR_HLT: u16 = 1 << 11;
pub const CNT_LOCK: u16 = 1 << 12;
/// The timer counts in steps of 0.6 s and holds 10 bits (iTCO_wdt.c:143, :149, :362).
pub const TMR_MASK: u16 = 0x3FF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcoRegs {
    pub rld: u16,
    pub sts1: u16,
    pub sts2: u16,
    pub cnt1: u16,
    pub tmr: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tco {
    pub bus: u8,
    pub device_id: u16,
    pub tcobase: u32,
    pub tcoctl: u32,
    /// `None`: TCO is switched off in TCOCTL, the base is no I/O port, or the register guard refused the ports.
    pub regs: Option<TcoRegs>,
}

impl Tco {
    pub fn enabled(&self) -> bool {
        self.tcoctl & TCOCTL_EN != 0
    }
    /// The I/O base (bit 0 of the config word marks I/O space; i2c-i801.c:1439).
    pub fn base(&self) -> Option<u16> {
        let b = self.tcobase & !1;
        (self.enabled() && b != 0 && b <= 0xFFE0).then_some(b as u16)
    }
    /// Seconds the timer is set to (0.6 s steps).
    pub fn timeout_s(&self) -> Option<u32> {
        self.regs.map(|r| (r.tmr & TMR_MASK) as u32 * 6 / 10)
    }
    pub fn lines(&self) -> alloc::vec::Vec<String> {
        let mut v = alloc::vec![format!(
            "acpi: TCO watchdog at {:02x}:1f.4 (8086:{:04x}) tcobase {:#x} tcoctl {:#x} enabled {}",
            self.bus, self.device_id, self.tcobase, self.tcoctl, self.enabled()
        )];
        if let Some(r) = self.regs {
            v.push(format!(
                "acpi: TCO tco1_sts {:#06x} tco2_sts {:#06x} tco1_cnt {:#06x} tmr {} ({} s) rld {}: halted {}, no_reboot {}, locked {}, timed_out {}, second_timeout {}",
                r.sts1,
                r.sts2,
                r.cnt1,
                r.tmr & TMR_MASK,
                self.timeout_s().unwrap_or(0),
                r.rld & TMR_MASK,
                r.cnt1 & CNT_TMR_HLT != 0,
                r.cnt1 & CNT_NO_REBOOT != 0,
                r.cnt1 & CNT_LOCK != 0,
                r.sts1 & STS1_TIMEOUT != 0,
                r.sts2 & STS2_SECOND_TO != 0
            ));
        }
        v
    }
}

/// Looks for the chipset's SMBus function on `BUSES` and reads the TCO facts through `cfg` (bus, offset of 00:1f.4's
/// configuration space) and `io16` (a port; `None`: refused). Pure over the two readers; reads only.
pub fn probe(cfg: &mut dyn FnMut(u8, u8) -> u32, io16: &mut dyn FnMut(u16) -> Option<u16>) -> Option<Tco> {
    for bus in BUSES {
        let id = cfg(bus, 0x00);
        if id as u16 != VENDOR_INTEL || cfg(bus, 0x08) >> 16 != CLASS_SMBUS {
            continue;
        }
        let mut t = Tco { bus, device_id: (id >> 16) as u16, tcobase: cfg(bus, CFG_TCOBASE), tcoctl: cfg(bus, CFG_TCOCTL), regs: None };
        if let Some(base) = t.base() {
            let mut r = |off: u16| io16(base + off);
            t.regs = (|| Some(TcoRegs { rld: r(TCO_RLD)?, sts1: r(TCO1_STS)?, sts2: r(TCO2_STS)?, cnt1: r(TCO1_CNT)?, tmr: r(TCO_TMR)? }))();
        }
        return Some(t);
    }
    None
}

static FOUND: ksync::Once<Option<Tco>> = ksync::Once::new();

/// Boot (from `live::init`, one CPU, interrupts off, before the initcalls: nobody else uses 0xCF8 yet, as for the
/// guard's own scan). Only on a GenuineIntel CPU; elsewhere nothing is read and nothing is logged.
pub(crate) fn boot() {
    if !hal::cpu::detect().intel {
        return;
    }
    let found = FOUND.get_or_init(|| {
        probe(&mut |bus, off| hal::portio::pci_config_read32(bus, SMBUS_DEV, SMBUS_FN, off), &mut |port| {
            crate::live::check_io(port as u64, 2, false).ok()?;
            hal::portio::inw(port).ok()
        })
    });
    match found {
        Some(t) => {
            use klog::Hex;
            klog::event!(Info, "acpi", "tco", bus = Hex(t.bus as u64), device = Hex(t.device_id as u64), tcobase = Hex(t.tcobase as u64), tcoctl = Hex(t.tcoctl as u64), enabled = t.enabled(), readable = t.regs.is_some();
                "the chipset's watchdog as the firmware left it; BitsOS only reads it");
            if let Some(r) = t.regs {
                klog::event!(Info, "acpi", "tco.regs", tco1_sts = Hex(r.sts1 as u64), tco2_sts = Hex(r.sts2 as u64), tco1_cnt = Hex(r.cnt1 as u64), tmr = r.tmr & TMR_MASK, rld = r.rld & TMR_MASK,
                    timeout_s = t.timeout_s().unwrap_or(0), halted = r.cnt1 & CNT_TMR_HLT != 0, no_reboot = r.cnt1 & CNT_NO_REBOOT != 0, locked = r.cnt1 & CNT_LOCK != 0,
                    timed_out = r.sts1 & STS1_TIMEOUT != 0, second_timeout = r.sts2 & STS2_SECOND_TO != 0);
            }
        }
        None => klog::event!(Info, "acpi", "tco.absent"; "no Intel SMBus function at 00:1f.4 or 80:1f.4: no TCO watchdog to describe"),
    }
}

/// What the boot found (`None`: not an Intel CPU, no such function, or before `init`).
pub fn found() -> Option<Tco> {
    FOUND.get().copied().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// Configuration space of one 00:1f.4 per bus; any offset but the four the probe may read panics.
    fn cfg(present: &'static [(u8, u32, u32, u32, u32)]) -> impl FnMut(u8, u8) -> u32 {
        move |bus, off| {
            let f = present.iter().find(|p| p.0 == bus);
            match (f, off) {
                (None, 0x00 | 0x08) => u32::MAX,
                (Some(f), 0x00) => f.1,
                (Some(f), 0x08) => f.2,
                (Some(f), CFG_TCOBASE) => f.3,
                (Some(f), CFG_TCOCTL) => f.4,
                (_, o) => panic!("config read of {bus:02x}:1f.4 offset {o:#x}"),
            }
        }
    }

    /// The Gaming-PC's shape: nothing at 00:1f.4, the SMBus 8086:7f23 at 80:1f.4. Base and control are what such
    /// boards usually show; the real values are what the first boot prints.
    const GP: &[(u8, u32, u32, u32, u32)] = &[(0x80, 0x7F23_8086, 0x0C05_0010, 0x0000_0401, 0x0000_0100)];

    #[test]
    fn the_gaming_pcs_shape_is_read_and_only_read() {
        let seen = RefCell::new(Vec::new());
        let t = probe(&mut cfg(GP), &mut |p| {
            seen.borrow_mut().push(p);
            Some(match p - 0x400 {
                TCO_RLD => 0x0004,
                TCO1_STS => 0x0000,
                TCO2_STS => 0x0000,
                TCO1_CNT => 0x0800,
                TCO_TMR => 0x0004,
                o => panic!("port read at base + {o:#x}"),
            })
        })
        .expect("found on bus 0x80");
        assert_eq!((t.bus, t.device_id, t.base()), (0x80, 0x7F23, Some(0x400)));
        assert_eq!(*seen.borrow(), [0x400, 0x404, 0x406, 0x408, 0x412], "five reads inside the 32-port block");
        assert_eq!(t.timeout_s(), Some(2));
        let l = t.lines();
        assert_eq!(l[0], "acpi: TCO watchdog at 80:1f.4 (8086:7f23) tcobase 0x401 tcoctl 0x100 enabled true");
        assert_eq!(l[1], "acpi: TCO tco1_sts 0x0000 tco2_sts 0x0000 tco1_cnt 0x0800 tmr 4 (2 s) rld 4: halted true, no_reboot false, locked false, timed_out false, second_timeout false");
    }

    #[test]
    fn a_reset_by_the_watchdog_shows() {
        let t = probe(&mut cfg(GP), &mut |p| Some(match p - 0x400 {
            TCO1_STS => STS1_TIMEOUT,
            TCO2_STS => STS2_SECOND_TO,
            TCO1_CNT => CNT_LOCK | CNT_NO_REBOOT,
            _ => 0x3FF,
        }))
        .unwrap();
        assert!(t.lines()[1].ends_with("halted false, no_reboot true, locked true, timed_out true, second_timeout true"));
        assert_eq!(t.timeout_s(), Some(613));
    }

    #[test]
    fn switched_off_or_refused_reads_no_port() {
        const OFF: &[(u8, u32, u32, u32, u32)] = &[(0x00, 0x7F23_8086, 0x0C05_0010, 0x0000_0401, 0)];
        let t = probe(&mut cfg(OFF), &mut |_| panic!("port read with TCO switched off")).unwrap();
        assert!(!t.enabled() && t.regs.is_none() && t.base().is_none());
        assert_eq!(t.lines().len(), 1);
        // No base, or one whose block would run past the port space.
        for base in [0x0000_0001u32, 0x0000_FFF1, 0x0001_0401] {
            let t = Tco { bus: 0, device_id: 0, tcobase: base, tcoctl: TCOCTL_EN, regs: None };
            assert_eq!(t.base(), None, "{base:#x}");
        }
        // The register guard refuses the ports: found, no registers, no further read after the first refusal.
        let n = RefCell::new(0);
        let t = probe(&mut cfg(GP), &mut |_| {
            *n.borrow_mut() += 1;
            None
        })
        .unwrap();
        assert!(t.regs.is_none());
        assert_eq!(*n.borrow(), 1);
    }

    #[test]
    fn other_machines_have_none() {
        // The Mini-PC's shape (no function 31.4 at all) and QEMU's q35 (its SMBus is function 3): nothing found,
        // and nothing but the ID and class words is ever read.
        assert_eq!(probe(&mut cfg(&[]), &mut |_| panic!("port read")), None);
        // An Intel function there that is no SMBus, and an SMBus of another vendor.
        const NOT: &[(u8, u32, u32, u32, u32)] = &[(0x00, 0x7F24_8086, 0x0C80_0010, 0, 0), (0x80, 0x790B_1022, 0x0C05_0071, 0, 0)];
        assert_eq!(probe(&mut cfg(NOT), &mut |_| panic!("port read")), None);
    }
}

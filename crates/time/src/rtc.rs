//! The CMOS real-time clock, read only (spec/core.md §7.3: Windows on the same machine owns it and keeps local
//! time there; BitsOS never writes it). V1 `clock.asm` / MEHRKERN P6: a bounded wait for the update cycle, 0xFF
//! from port 0x71 means no RTC, the FADT may say there is none. Linux `drivers/rtc/rtc-mc146818-lib.c`
//! (`mc146818_get_time`): read while no update is in progress, twice until two reads agree.

use crate::tz::DateTime;

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;
const REG_A: u8 = 0x0A;
const REG_B: u8 = 0x0B;
const REG_A_UIP: u8 = 0x80;
const REG_B_24H: u8 = 0x02;
const REG_B_BINARY: u8 = 0x04;
/// The update-in-progress wait (V1: 3 ms; an update takes under 2 ms).
pub const UIP_WAIT_NS: u64 = 3_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcError {
    /// The FADT says the machine has no CMOS RTC.
    NoneDeclared,
    /// Port 0x71 reads 0xFF: nothing answers.
    Absent,
    /// The update-in-progress flag never cleared, or two reads never agreed.
    Busy,
    /// The registers hold no valid date (BCD garbage, month 13, ...).
    Invalid,
}

impl RtcError {
    pub fn name(self) -> &'static str {
        match self {
            RtcError::NoneDeclared => "none_declared",
            RtcError::Absent => "absent",
            RtcError::Busy => "busy",
            RtcError::Invalid => "invalid",
        }
    }
}

/// The raw registers of one read: seconds, minutes, hours, day, month, year, century (0: none), register B.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Raw {
    pub sec: u8,
    pub min: u8,
    pub hour: u8,
    pub day: u8,
    pub month: u8,
    pub year: u8,
    pub century: u8,
    pub reg_b: u8,
}

fn bcd(v: u8) -> Option<u8> {
    let (hi, lo) = (v >> 4, v & 0x0F);
    (hi <= 9 && lo <= 9).then_some(hi * 10 + lo)
}

/// The registers to a local date and time (pure; host-tested): BCD or binary (register B bit 2), 12- or
/// 24-hour (bit 1; in 12-hour mode bit 7 of the hour is PM), the century register if the FADT names one, else
/// 20xx.
pub fn decode(r: &Raw) -> Option<DateTime> {
    let binary = r.reg_b & REG_B_BINARY != 0;
    let conv = |v: u8| if binary { Some(v) } else { bcd(v) };
    let pm = r.reg_b & REG_B_24H == 0 && r.hour & 0x80 != 0;
    let mut hour = conv(r.hour & 0x7F)?;
    if r.reg_b & REG_B_24H == 0 {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour = (hour % 12) + if pm { 12 } else { 0 };
    }
    let century = if r.century != 0 { conv(r.century)? as u32 } else { 20 };
    let dt = DateTime {
        year: century * 100 + conv(r.year)? as u32,
        month: conv(r.month)?,
        day: conv(r.day)?,
        hour,
        min: conv(r.min)?,
        sec: conv(r.sec)?,
    };
    dt.valid().then_some(dt)
}

fn cmos(reg: u8) -> u8 {
    // Index with bit 7 clear (NMIs stay enabled, as Linux `CMOS_READ`).
    let _ = hal::portio::outb(INDEX, reg & 0x7F);
    hal::portio::inb(DATA).unwrap_or(0xFF)
}

fn read_raw(century_reg: u8) -> Raw {
    Raw {
        sec: cmos(0x00),
        min: cmos(0x02),
        hour: cmos(0x04),
        day: cmos(0x07),
        month: cmos(0x08),
        year: cmos(0x09),
        century: if century_reg != 0 { cmos(century_reg) } else { 0 },
        reg_b: cmos(REG_B),
    }
}

/// Reads the RTC (local time as the firmware keeps it). `century_reg`: the FADT's CENTURY index (0: none);
/// `declared_absent`: the FADT's IAPC_BOOT_ARCH says "no CMOS RTC". Interrupts off while reading.
pub fn read(century_reg: u8, declared_absent: bool) -> Result<DateTime, RtcError> {
    if declared_absent {
        return Err(RtcError::NoneDeclared);
    }
    if !hal::BARE_METAL {
        return Err(RtcError::Absent);
    }
    let _irq = hal::cpu::IrqGuard::new();
    if cmos(REG_A) == 0xFF && cmos(REG_B) == 0xFF {
        return Err(RtcError::Absent);
    }
    let start = crate::now_ns();
    let mut last: Option<Raw> = None;
    loop {
        if crate::now_ns().saturating_sub(start) > 2 * UIP_WAIT_NS {
            return Err(RtcError::Busy);
        }
        if cmos(REG_A) & REG_A_UIP != 0 {
            hal::cpu::spin_hint();
            continue;
        }
        let r = read_raw(century_reg);
        if last == Some(r) {
            return decode(&r).ok_or(RtcError::Invalid);
        }
        last = Some(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_24h() {
        let r = Raw { sec: 0x59, min: 0x30, hour: 0x23, day: 0x15, month: 0x10, year: 0x26, century: 0x20, reg_b: REG_B_24H };
        assert_eq!(decode(&r), Some(DateTime { year: 2026, month: 10, day: 15, hour: 23, min: 30, sec: 59 }));
    }

    #[test]
    fn binary_12h_pm_and_midnight() {
        let r = Raw { sec: 5, min: 7, hour: 0x80 | 3, day: 1, month: 2, year: 30, century: 0, reg_b: REG_B_BINARY };
        assert_eq!(decode(&r).map(|d| (d.year, d.hour)), Some((2030, 15)));
        let r = Raw { hour: 12, ..r };
        assert_eq!(decode(&r).map(|d| d.hour), Some(0), "12 AM is midnight");
        let r = Raw { hour: 0x80 | 12, ..r };
        assert_eq!(decode(&r).map(|d| d.hour), Some(12), "12 PM is noon");
    }

    #[test]
    fn garbage_is_refused() {
        let ok = Raw { sec: 0, min: 0, hour: 0x10, day: 0x01, month: 0x01, year: 0x26, century: 0, reg_b: REG_B_24H };
        assert!(decode(&ok).is_some());
        assert_eq!(decode(&Raw { month: 0x13, ..ok }), None, "month 13");
        assert_eq!(decode(&Raw { day: 0x3A, ..ok }), None, "not BCD");
        assert_eq!(decode(&Raw { hour: 0x24, ..ok }), None, "hour 24");
        assert_eq!(decode(&Raw { day: 0x31, month: 0x02, ..ok }), None, "31 February");
        assert_eq!(decode(&Raw { hour: 0x13, reg_b: 0, ..ok }), None, "13 in 12-hour mode");
        assert_eq!(decode(&Raw { sec: 0xFF, min: 0xFF, hour: 0xFF, day: 0xFF, month: 0xFF, year: 0xFF, century: 0, reg_b: 0xFF }), None);
    }

    #[test]
    fn host_reads_nothing() {
        assert_eq!(read(0, true), Err(RtcError::NoneDeclared));
        assert_eq!(read(0x32, false), Err(RtcError::Absent));
    }
}

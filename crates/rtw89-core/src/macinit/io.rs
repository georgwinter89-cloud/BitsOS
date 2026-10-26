//! Small register helpers shared by the macinit files (JS js/macinit-io.mjs):
//! the C's read-modify-writes as one op, the warn-only polls, and which errors
//! the sch TX path keeps instead of returning.

use crate::bus::{Bus, Width};
use crate::error::{Rtw89Error, Rtw89Result};

pub use crate::pci::dmac::{MacSel, check_mac_en, mac_en_check};

// Linux: a read32, clear-and-set, write32 of one register as rtw89_write32_mask does
// (core.h:8224): one RMW op, because the new value depends on nothing but the old one.
// The op's clear mask is `clr | set`, as kit's mask32 queues it.
pub fn rmw32<B: Bus + ?Sized>(bus: &mut B, a: u32, clr: u32, set: u32) -> Rtw89Result<()> {
    bus.rmw(Width::W32, a, clr | set, set)?;
    Ok(())
}

// Linux: the same for read16/write16 as in rtw89_write16_mask (core.h:8238)
pub fn rmw16<B: Bus + ?Sized>(bus: &mut B, a: u32, clr: u32, set: u32) -> Rtw89Result<()> {
    bus.rmw(Width::W16, a, (clr | set) & 0xffff, set & 0xffff)?;
    Ok(())
}

// Linux: read_poll_timeout_atomic(rtw89_read32_mask, val == want, 1 us, timeout_us) whose
// failure only warns, as in rst_bacam (mac.c:2904): reads with a 1 us delay between them,
// bounded by the clock and by the waited steps (a clock that does not move cannot make
// it endless), one more read after the deadline decides as Linux's macro does. Returns
// `Ok(None)` when the condition held, `Ok(Some(last))` when the time ran out.
pub fn poll_warn<B: Bus + ?Sized>(
    bus: &mut B,
    a: u32,
    mask: u32,
    want: u32,
    timeout_us: u32,
) -> Rtw89Result<Option<u32>> {
    let limit_ms = u64::from(timeout_us.div_ceil(1000));
    let start = bus.now_ms();
    let mut waited_us: u64 = 0;
    loop {
        if bus.r32(a)? & mask == want {
            return Ok(None);
        }
        if bus.now_ms().saturating_sub(start) >= limit_ms || waited_us >= u64::from(timeout_us) {
            let last = bus.r32(a)?;
            return Ok(if last & mask == want {
                None
            } else {
                Some(last)
            });
        }
        bus.delay_us(1)?;
        waited_us += 1;
    }
}

/// Linux: none; read_poll_timeout's timeout in microseconds -> a poll op's
/// milliseconds, rounded up, at least 1 (JS `usToMs`)
pub const fn us_to_ms(us: u32) -> u16 {
    let ms = us.div_ceil(1000);
    if ms == 0 {
        1
    } else if ms > u16::MAX as u32 {
        u16::MAX
    } else {
        ms as u16
    }
}

/// Linux: none; an error return of the C that its callers on this chip ignore (the JS
/// `chipFailure(e, FwError)`): everything the chip or the firmware reports. A lost bus
/// and a request the driver itself got wrong are never swallowed.
pub fn chip_reported(e: &Rtw89Error) -> bool {
    use crate::bus::BusError;
    !matches!(
        e,
        Rtw89Error::Bus(BusError::Gone | BusError::Transport | BusError::Refused { .. })
            | Rtw89Error::Invalid { .. }
            | Rtw89Error::BadDmaAddress { .. }
    )
}

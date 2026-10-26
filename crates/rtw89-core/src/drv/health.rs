//! Is the chip still well (spec/wlan.md §2.3, V1 K6 health.asm / health-fw.asm, the fault catalogue
//! of V1's wlan-robust-test.mjs)? Two parts:
//! - [`fw_check`]: the firmware's own state, read every [`HEALTH_MS`] by the driver thread: a halt
//!   it posted in R_AX_HALT_C2H_CTRL (codes 1..3 it repaired itself, any other code stopped it) and
//!   whether it still runs (R_AX_WCPU_FW_CTRL status 7).
//! - [`Health`]: when chip errors of the running driver mean "switch the chip off and bring it up
//!   again": a chip that does not answer ([`crate::deadread::chip_answers`] after a dead-looking
//!   read), the link lost to a chip error, [`FAIL_ROW`] failed rounds in a row, failed rounds over
//!   [`FAIL_SPAN_MS`] without a good one between, or a transmit ring full for [`TX_STALL_MS`].
//!
//! Linux reaches its SER (rtw89_ser) through the HALT_C2H interrupt and recovers in place; BitsOS
//! has no SER port, so every verdict here ends in a fresh bring-up (V1's K6 did the same).

use alloc::format;
use alloc::string::String;

use crate::bus::{Bus, field};
use crate::deadread;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

/// How often the thread looks at the firmware (rtw89's track work period)
pub const HEALTH_MS: u64 = 2_000;
/// Failed rounds in a row that switch the chip off (V1 K6: 5)
pub const FAIL_ROW: u32 = 5;
/// Failed rounds this far apart, none good between, switch the chip off (V1 K6: 30 s)
pub const FAIL_SPAN_MS: u64 = 30_000;
/// A transmit ring that takes nothing for this long switches the chip off (V1 K6: 10 s)
pub const TX_STALL_MS: u64 = 10_000;

/// reg.h:243 R_AX_UDM0: the firmware's status word (rtw89_fw_st_dbg_dump)
pub const R_AX_UDM0: u32 = 0x1F0;
/// mac.h:769-771: MAC_AX_ERR_L0_ERR_CMAC0 .. MAC_AX_ERR_L0_RESET_DONE: errors the firmware repairs
/// itself (ser.c rtw89_ser_notify -> SER_EV_L0_RESET)
pub const ERR_L0_LAST: u32 = 3;

/// What [`fw_check`] found
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwHealth {
    Running,
    /// an L0 error the firmware repaired; the link stays
    Repaired {
        code: u32,
        status: u32,
        epc: u32,
    },
    /// the firmware posted a halt (CPU exception, assertion, an L1 error): chip off
    Halted {
        code: u32,
        status: u32,
        epc: u32,
    },
    /// R_AX_WCPU_FW_CTRL no longer at status 7: chip off
    NotReady {
        value: u32,
    },
}

// Linux: rtw89_mac_get_err_status (mac.c:814) without its 100 ms poll (the HALT_C2H interrupt
// started it there; here a non-zero CTRL is the report) and rtw89_fw_get_rdy_ax (mac.c:7413).
// HALT_C2H_CTRL back to 0 as 0.29.29 has no SER_POST_RECOVER_DMAC. Not Linux: no SER, the
// verdict is the caller's (V1 health-fw.asm wl_fw_check).
pub fn fw_check<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<FwHealth> {
    let ctrl = bus.r32(R_AX_HALT_C2H_CTRL)?;
    if ctrl != 0 && !deadread::looks_dead(ctrl) {
        let code = bus.r32(R_AX_HALT_C2H)?;
        bus.w32(R_AX_HALT_C2H_CTRL, 0)?;
        // rtw89_fw_st_dbg_dump's FW status and EPC
        let status = bus.r32(R_AX_UDM0)?;
        let epc = bus.r32(R_AX_UDM2)?;
        if (1..=ERR_L0_LAST).contains(&code) {
            return Ok(FwHealth::Repaired { code, status, epc });
        }
        return Ok(FwHealth::Halted { code, status, epc });
    }
    let v = bus.r32(R_AX_WCPU_FW_CTRL)?;
    if deadread::looks_dead(ctrl) || deadread::looks_dead(v) {
        // a dead-looking read: the caller confirms with chip_answers
        return Err(Rtw89Error::FwStatus {
            reg: if deadread::looks_dead(ctrl) {
                R_AX_HALT_C2H_CTRL
            } else {
                R_AX_WCPU_FW_CTRL
            },
            value: if deadread::looks_dead(ctrl) { ctrl } else { v },
            status: field(v, B_AX_WCPU_FWDL_STS_MASK),
        });
    }
    if field(v, B_AX_WCPU_FWDL_STS_MASK) != RTW89_FWDL_WCPU_FW_INIT_RDY {
        return Ok(FwHealth::NotReady { value: v });
    }
    Ok(FwHealth::Running)
}

/// Why the chip goes off and up again
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// the chip does not answer (register and the value it read)
    Dead {
        reg: u32,
        value: u32,
    },
    FwHalted {
        code: u32,
    },
    FwNotReady {
        value: u32,
    },
    /// a chip access of the running link failed
    LinkChipError,
    /// `n` failed rounds in a row
    FailedRow {
        n: u32,
    },
    /// failed rounds over `ms` with no good round between
    FailedSpan {
        ms: u64,
    },
    /// the transmit ring took nothing for `ms`
    TxStall {
        ms: u64,
    },
}

impl core::fmt::Display for Verdict {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Verdict::Dead { reg, value } => {
                write!(
                    f,
                    "the chip does not answer (register {reg:#07x} read {value:#010x})"
                )
            }
            Verdict::FwHalted { code } => write!(f, "the firmware halted with error {code:#010x}"),
            Verdict::FwNotReady { value } => write!(
                f,
                "the firmware no longer runs (register {R_AX_WCPU_FW_CTRL:#07x} read {value:#010x})"
            ),
            Verdict::LinkChipError => f.write_str("a chip error ended the link"),
            Verdict::FailedRow { n } => write!(f, "{n} failed rounds in a row"),
            Verdict::FailedSpan { ms } => write!(f, "failed rounds for {} s", ms / 1000),
            Verdict::TxStall { ms } => {
                write!(f, "the transmit ring took nothing for {} s", ms / 1000)
            }
        }
    }
}

/// The failed rounds and the transmit stall since the last good round
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Health {
    /// failed rounds in a row and when the first of them was
    pub failed: u32,
    pub first_fail: Option<u64>,
    /// since when the transmit ring is full
    pub tx_full_since: Option<u64>,
    /// the next firmware look
    pub fw_due: u64,
}

impl Health {
    pub fn new(now: u64) -> Health {
        Health {
            fw_due: now + HEALTH_MS,
            ..Health::default()
        }
    }

    /// A round whose chip accesses went through
    pub fn good(&mut self) {
        self.failed = 0;
        self.first_fail = None;
    }

    /// A round with a chip error at `now`: a verdict when the rounds say the chip is broken
    pub fn failed(&mut self, now: u64) -> Option<Verdict> {
        self.failed += 1;
        let first = *self.first_fail.get_or_insert(now);
        if self.failed >= FAIL_ROW {
            return Some(Verdict::FailedRow { n: self.failed });
        }
        let span = now.saturating_sub(first);
        if self.failed >= 2 && span >= FAIL_SPAN_MS {
            return Some(Verdict::FailedSpan { ms: span });
        }
        None
    }

    /// A send at `now` found the ring full (`true`) or took the frame (`false`)
    pub fn tx(&mut self, now: u64, full: bool) -> Option<Verdict> {
        if !full {
            self.tx_full_since = None;
            return None;
        }
        let since = *self.tx_full_since.get_or_insert(now);
        let ms = now.saturating_sub(since);
        (ms >= TX_STALL_MS).then_some(Verdict::TxStall { ms })
    }
}

/// An error whose register read looks dead, confirmed: the chip does not answer (V1 wh_read). A
/// bus that is gone says so itself.
pub fn dead_verdict<B: Bus + ?Sized>(bus: &mut B, err: &Rtw89Error) -> Option<Verdict> {
    if let Rtw89Error::Bus(crate::bus::BusError::Gone) = err {
        return Some(Verdict::Dead {
            reg: 0,
            value: u32::MAX,
        });
    }
    let (reg, value) = err.dead_read()?;
    match deadread::chip_answers(bus) {
        Ok(true) => None,
        Ok(false) | Err(_) => Some(Verdict::Dead { reg, value }),
    }
}

/// The text of a firmware look for the log
pub fn fw_text(h: &FwHealth) -> String {
    match h {
        FwHealth::Running => String::from("running"),
        FwHealth::Repaired { code, status, epc } => format!(
            "the firmware repaired error {code:#010x}, status {status:#010x}, address {epc:#010x}"
        ),
        FwHealth::Halted { code, status, epc } => format!(
            "the firmware halted with error {code:#010x}, status {status:#010x}, address {epc:#010x}"
        ),
        FwHealth::NotReady { value } => format!(
            "the firmware no longer runs (register {R_AX_WCPU_FW_CTRL:#07x} read {value:#010x})"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Width;
    use crate::membus::MemBus;

    #[test]
    fn failed_rounds_in_a_row_and_over_30_s() {
        let mut h = Health::new(0);
        for t in 0..4 {
            assert_eq!(h.failed(t * 100), None);
        }
        assert_eq!(h.failed(500), Some(Verdict::FailedRow { n: 5 }));
        // V1 K6: two failed rounds 31 s apart, fewer than 5 in a row, nothing good between
        let mut h = Health::new(0);
        assert_eq!(h.failed(1_000), None);
        assert_eq!(h.failed(32_000), Some(Verdict::FailedSpan { ms: 31_000 }));
        // a good round between starts over
        let mut h = Health::new(0);
        assert_eq!(h.failed(1_000), None);
        h.good();
        assert_eq!(h.failed(32_000), None);
    }

    #[test]
    fn a_full_ring_for_10_s_is_a_stall() {
        let mut h = Health::new(0);
        assert_eq!(h.tx(100, true), None);
        assert_eq!(h.tx(9_000, true), None);
        assert_eq!(
            h.tx(9_500, false),
            None,
            "the ring took a frame: starts over"
        );
        assert_eq!(h.tx(10_000, true), None);
        assert_eq!(h.tx(20_000, true), Some(Verdict::TxStall { ms: 10_000 }));
    }

    #[test]
    fn firmware_looks() {
        let mut b = MemBus::new();
        let ready = RTW89_FWDL_WCPU_FW_INIT_RDY << B_AX_WCPU_FWDL_STS_MASK.trailing_zeros();
        b.poke(R_AX_WCPU_FW_CTRL, Width::W32, ready);
        assert_eq!(fw_check(&mut b).unwrap(), FwHealth::Running);
        b.poke(R_AX_HALT_C2H_CTRL, Width::W32, 1);
        b.poke(R_AX_HALT_C2H, Width::W32, 2);
        assert!(matches!(
            fw_check(&mut b).unwrap(),
            FwHealth::Repaired { code: 2, .. }
        ));
        assert_eq!(
            b.peek(R_AX_HALT_C2H_CTRL, Width::W32),
            0,
            "the report taken"
        );
        b.poke(R_AX_HALT_C2H_CTRL, Width::W32, 1);
        b.poke(R_AX_HALT_C2H, Width::W32, 0x2000_1234);
        assert!(matches!(
            fw_check(&mut b).unwrap(),
            FwHealth::Halted {
                code: 0x2000_1234,
                ..
            }
        ));
        b.poke(R_AX_WCPU_FW_CTRL, Width::W32, 0x02);
        assert_eq!(fw_check(&mut b).unwrap(), FwHealth::NotReady { value: 2 });
        b.poke(R_AX_WCPU_FW_CTRL, Width::W32, u32::MAX);
        assert!(fw_check(&mut b).is_err(), "a dead look goes to the caller");
    }
}

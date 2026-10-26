//! Card reset and the steps around it (spec net.md §2.2 "Probe and reset, in this order", `V1:igc.asm:60-194`,
//! Linux `igc_reset_hw_base`). No klog here: Quiesce uses these on the panic path; callers log the report.

use crate::hw::{until, Hw};
use crate::regs::{self, ctrl, status, Model};

/// What the reset saw; the caller logs it (B6) where logging is allowed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResetReport {
    /// STATUS.GIO_MASTER_ENABLE still set after 100 ms (Linux only logs it too and goes on).
    pub gio_timeout: bool,
    /// EECD auto-read not reported within 20 ms (QEMU never reports it).
    pub auto_read_timeout: bool,
}

/// Which reset bit (`V1:igc.asm:163` vs Linux igc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetKind {
    /// CTRL.RST (bit 26): the spec's and V1's, proven on the Mini-PC; Quiesce always uses it.
    Global,
    /// CTRL.DEV_RST (bit 29), what current Linux igc writes on the I225/I226; tried by the restart path after
    /// `Global` failed to bring the card back several times.
    Device,
}

const GIO_WAIT_US: u64 = 100_000;
const PRE_RESET_US: u64 = 10_000;
const AUTO_READ_US: u64 = 20_000;
const STEP_US: u64 = 100;

/// Every interrupt cause masked and the pending ones read away (`V1:igc.asm:173-184`). The two kinds keep
/// IMC/ICR at different offsets.
pub fn quiet<H: Hw + ?Sized>(hw: &H, model: Model) {
    hw.wr(model.imc(), !0);
    hw.wr(regs::EIMC, !0);
    let _ = hw.rd(model.icr());
}

/// The reset of Linux `igc_reset_hw_base` in V1's order: PTM off (I226), GIO master disable and its wait, all
/// causes masked, RCTL = 0, TCTL = PSP, flush, 10 ms, CTRL.RST (or DEV_RST), wait for the NVM auto-read,
/// masked again.
pub fn reset<H: Hw + ?Sized>(hw: &H, model: Model, kind: ResetKind) -> ResetReport {
    let mut r = ResetReport::default();
    if model == Model::I226 {
        // A PTM cycle across the reset hangs the I226; the 82576 has no PTM.
        hw.wr(regs::PTM_CTRL, hw.rd(regs::PTM_CTRL) & regs::PTM_CTRL_KEEP);
    }
    hw.set_bits(regs::CTRL, ctrl::GIO_MASTER_DISABLE);
    r.gio_timeout = !until(hw, GIO_WAIT_US, STEP_US, || hw.rd(regs::STATUS) & status::GIO_MASTER_ENABLE == 0);
    quiet(hw, model);
    hw.wr(regs::RCTL, 0);
    hw.wr(regs::TCTL, regs::TCTL_PSP);
    hw.flush();
    hw.delay_us(PRE_RESET_US);
    hw.set_bits(regs::CTRL, if kind == ResetKind::Device { ctrl::DEV_RST } else { ctrl::RST });
    r.auto_read_timeout = !until(hw, AUTO_READ_US, STEP_US, || hw.rd(regs::EECD) & regs::EECD_AUTO_RD != 0);
    quiet(hw, model);
    r
}

/// The I226's own values after each reset (`V1:igc.asm:188-194`): packet buffer sizes (a firmware that used
/// TSN leaves others), the IPv6 extension header erratum bit, Energy Efficient Ethernet off.
pub fn i226_defaults<H: Hw + ?Sized>(hw: &H) {
    hw.wr(regs::RXPBS, regs::RXPBS_I226);
    hw.wr(regs::TXPBS, regs::TXPBS_I226);
    hw.set_bits(regs::RFCTL, regs::RFCTL_IPV6_EXT_DIS);
    hw.clear_bits(regs::IPCNFG, regs::IPCNFG_EEE);
    hw.clear_bits(regs::EEER, regs::EEER_EEE);
}

/// Link up set, speed and duplex not forced: autonegotiation decides (`V1:igc.asm:81-84`).
pub fn set_link_up<H: Hw + ?Sized>(hw: &H) {
    let c = hw.rd(regs::CTRL);
    hw.wr(regs::CTRL, (c | ctrl::SLU) & !ctrl::FORCE_SPEED_DUPLEX);
}

/// The MAC address the NVM auto-read put into RAL/RAH 0 (`V1:igcring.asm:192-197`); `None` without the
/// address-valid bit (refuse the card) or from a card reading all ones.
pub fn read_mac<H: Hw + ?Sized>(hw: &H) -> Option<[u8; 6]> {
    let (lo, hi) = (hw.rd(regs::RAL), hw.rd(regs::RAH));
    if hi & (1 << 31) == 0 || (lo == !0 && hi == !0) {
        return None;
    }
    let m = [lo as u8, (lo >> 8) as u8, (lo >> 16) as u8, (lo >> 24) as u8, hi as u8, (hi >> 8) as u8];
    // All zeros or a multicast address is no station address either.
    (m != [0; 6] && m[0] & 1 == 0).then_some(m)
}

/// Link state from STATUS (Linux `igc_get_speed_and_duplex_copper`); a card reading all ones has no link
/// (`V1 igb-test`: "a card reading all ones has no link").
pub fn link_from_status(model: Model, s: u32) -> (bool, u32, bool) {
    if s == !0 || s & status::LU == 0 {
        return (false, 0, false);
    }
    let speed = if s & status::SPEED_1000 != 0 {
        if model == Model::I226 && s & status::SPEED_2500 != 0 { 2500 } else { 1000 }
    } else if s & status::SPEED_100 != 0 {
        100
    } else {
        10
    };
    (true, speed, s & status::FD != 0)
}

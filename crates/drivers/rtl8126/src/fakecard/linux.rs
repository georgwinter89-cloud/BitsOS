//! What Linux writes before a reset and around the start, typed from `r8169_main.c` a second time (not taken from
//! `seq`), and the matcher that looks for these lists in the fake's log.

use super::Op;
use crate::ocp::OCP_FLAG;
use crate::regs::OCPDR;

/// One write Linux makes, as the fake recognises it in the log.
#[derive(Clone, Copy, Debug)]
pub enum Need {
    W8(u16, u8),
    W16(u16, u16),
    W32(u16, u32),
    /// A write of this width whose bits under `mask` are `val`.
    Bits8(u16, u8, u8),
    Bits16(u16, u16, u16),
    Bits32(u16, u32, u32),
    Any16(u16),
    Any32(u16),
    /// A MAC OCP write to `reg` whose bits under `mask` are `val`.
    Ocp(u16, u16, u16),
    Cfg(u16, u8, u8),
    ReadRq,
}

use Need::*;

/// `rtl8169_irq_mask_and_ack`, `rtl_enable_rxdvgate` with its stop request (M:1669-1674, :2755-2760, :2743).
const QUIET: [Need; 4] = [W32(0x38, 0), W32(0x3c, 0xffff_ffff), Bits32(0xf0, 1 << 19, 1 << 19), Bits8(0x37, 0x80, 0x80)];

/// `rtl_init_one` up to `rtl_hw_reset` (M:5704-5710 with `rtl_hw_init_8125`, M:5514-5529).
pub const BEFORE_PROBE_RESET: &[Need] = &[
    W32(0x44, (8 << 27) | (7 << 8) | (1 << 11)),
    QUIET[0],
    QUIET[1],
    QUIET[2],
    QUIET[3],
    Bits8(0x37, 0x0c, 0),
    Bits8(0xd3, 0x80, 0),
    Ocp(0xe8de, 0x4000, 0),
    Ocp(0xc0aa, 0xffff, 0x07d0),
    Ocp(0xc0a6, 0xffff, 0x0150),
    Ocp(0xc01e, 0xffff, 0x5555),
];

/// `rtl8169_cleanup` up to `rtl_hw_reset` (M:4277-4300).
pub const BEFORE_RESET: &[Need] = &[QUIET[0], QUIET[1], Bits32(0x44, 0x3f, 0), QUIET[2], QUIET[3]];

/// `rtl_hw_start` up to the enable (M:4104-4131), every callee in its order: `rtl_hw_aspm_clkreq_enable(false)`
/// (M:3164-3185), CPlusCmd and the EEE idle timer (M:4109-4111, :2230), `rtl_hw_start_8125` (M:4043-4068),
/// `rtl_hw_start_8126a` (M:3975-3980), `rtl_hw_start_8125_common` (M:3861-3928), `rtl_enable_exit_l1` (M:3017), the
/// ASPM call again, the buffer size and ring addresses (M:2802, :2812-2815), the two lock pairs and the read-request
/// size (M:4124, :2636, :2656, :2659).
pub const BEFORE_ENABLE: &[Need] = &[
    W8(0x50, 0xc0),
    Ocp(0xe092, 0x00ff, 0),
    Bits8(0x34, 0x08, 0),
    Bits8(0x56, 0x01, 0),
    Any16(0xe0),
    W16(0x6048, 1500 + 14 + 0x20),
    W8(0x34, 0),
    W32(0xa00, 0),
    W32(0xa7c, 0),
    W16(0x7a, 0),
    Ocp(0xea84, 0x0003, 0x0003),
    Cfg(0x890, 0x01, 0),
    Cfg(0x70f, 0xff, 0x27),
    Bits8(0x54, 0x02, 0),
    W16(0x382, 0x221b),
    W32(0x4500, 0),
    W16(0x4800, 0),
    Ocp(0xd40a, 0x0010, 0),
    Bits8(0x52, 0x10, 0),
    Ocp(0xc140, 0xffff, 0xffff),
    Ocp(0xc142, 0xffff, 0xffff),
    Ocp(0xd3e2, 0x0fff, 0x03a9),
    Ocp(0xd3e4, 0x00ff, 0),
    Ocp(0xe860, 0x0080, 0x0080),
    Ocp(0xeb58, 0x0001, 0),
    Bits8(0xd8, 0x02, 0),
    Ocp(0xe614, 0x0700, 0x0400),
    Ocp(0xe63e, 0x0c30, 0x0020),
    Ocp(0xc0b4, 0x000c, 0x000c),
    Ocp(0xeb6a, 0x00ff, 0x0033),
    Ocp(0xeb50, 0x03e0, 0x0040),
    Ocp(0xe056, 0x00f0, 0),
    Ocp(0xe040, 0x1000, 0),
    Ocp(0xea1c, 0x0003, 0x0001),
    Ocp(0xea1c, 0x0300, 0),
    Ocp(0xe0c0, 0x4f0f, 0x4403),
    Ocp(0xe052, 0x00e8, 0x0068),
    Ocp(0xd430, 0x0fff, 0x047f),
    Ocp(0xea1c, 0x0004, 0),
    Ocp(0xeb54, 0x0001, 0x0001),
    Ocp(0xeb54, 0x0001, 0),
    Bits16(0x1880, 0x0030, 0),
    Ocp(0xe098, 0xffff, 0xc302),
    Bits32(0xf0, 1 << 19, 0),
    Ocp(0xc0ac, 0x1f80, 0x1f80),
    Ocp(0xe092, 0x00ff, 0),
    Bits8(0x34, 0x08, 0),
    Bits8(0x56, 0x01, 0),
    W16(0xda, 0x4000),
    Any32(0x24),
    Any32(0x20),
    Any32(0xe8),
    Any32(0xe4),
    W8(0x50, 0x00),
    W8(0x50, 0xc0),
    W8(0x50, 0x00),
    ReadRq,
];

/// What `rtl_hw_start` writes after the enable (M:4132-4135); without them the chip passes no frame.
pub const AFTER_ENABLE: &[Need] = &[
    W32(0x44, (8 << 27) | (7 << 8) | (1 << 11)),
    W32(0x40, (7 << 8) | (3 << 24)),
    Bits32(0x44, (3 << 22) | 0x30, 0),
    W32(0x0c, 0xffff_ffff),
    W32(0x08, 0xffff_ffff),
    Bits32(0x44, 0x0f, 0x0e),
];

pub(super) fn ocp_reg(cmd: u32) -> u32 {
    ((cmd >> 16) & 0x7fff) << 1
}

fn fits(need: &Need, op: &Op) -> bool {
    match (*need, *op) {
        (W8(o, v), Op::W8(a, b)) => o == a && v == b,
        (W16(o, v), Op::W16(a, b)) => o == a && v == b,
        (W32(o, v), Op::W32(a, b)) => o == a && v == b,
        (Bits8(o, m, v), Op::W8(a, b)) => o == a && b & m == v,
        (Bits16(o, m, v), Op::W16(a, b)) => o == a && b & m == v,
        (Bits32(o, m, v), Op::W32(a, b)) => o == a && b & m == v,
        (Any16(o), Op::W16(a, _)) => o == a,
        (Any32(o), Op::W32(a, _)) => o == a,
        (Ocp(r, m, v), Op::W32(OCPDR, cmd)) => cmd & OCP_FLAG != 0 && ocp_reg(cmd) == r as u32 && cmd as u16 & m == v,
        (Cfg(o, m, v), Op::Cfg(a, b)) => o == a && b & m == v,
        (ReadRq, Op::ReadRq) => true,
        _ => false,
    }
}

/// The first of `needs` that `ops` does not hold in order, if any.
pub fn first_missing(needs: &[Need], ops: &[Op], skip_cfg: bool) -> Option<Need> {
    let mut at = 0;
    for need in needs {
        if skip_cfg && matches!(need, Cfg(..) | ReadRq) {
            continue;
        }
        match ops[at..].iter().position(|op| fits(need, op)) {
            Some(i) => at += i + 1,
            None => return Some(*need),
        }
    }
    None
}

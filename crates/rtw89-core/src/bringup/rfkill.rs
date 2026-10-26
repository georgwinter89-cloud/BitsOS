//! The hardware radio switch of the RTL8852BE (JS bringup-steps.mjs `rfkillInit`).

use crate::bus::{Bus, Width, field};
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

// Linux: rtw89_rfkill_polling_init (core.c:7341) in rtw89_core_register_hw, with
// rtw89_core_rfkill_init (core.c:7324) and the first poll (rtw89_core_rfkill_get,
// core.c:7334); rtw8852b_rfkill_regs (rtw8852b.c:208): GPIO 9 as an input. `Ok(())` when
// the radio may run; `RadioBlocked` with the register and its value when the pin blocks it.
pub fn rfkill_init<B: Bus + ?Sized>(bus: &mut B) -> Rtw89Result<()> {
    bus.mask(
        Width::W16,
        R_AX_GPIO8_15_FUNC_SEL,
        B_AX_PINMUX_GPIO9_FUNC_SEL_MASK,
        0xf,
    )?;
    bus.mask(
        Width::W16,
        R_AX_GPIO_EXT_CTRL + 2,
        (B_AX_GPIO_MOD_9 | B_AX_GPIO_IO_SEL_9) >> 16,
        0,
    )?;
    let value = bus.r8(R_AX_GPIO_EXT_CTRL)?;
    if field(value.into(), B_AX_GPIO_IN_9) == 0 {
        return Err(Rtw89Error::RadioBlocked {
            reg: R_AX_GPIO_EXT_CTRL,
            value: value.into(),
        });
    }
    Ok(())
}

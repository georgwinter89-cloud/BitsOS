//! The thermal alert as Linux switches it on at the end of the power controller's start:
//! `smu_v14_0_enable_thermal_alert` (`S:` pm/swsmu/smu14/smu_v14_0.c:1815-1827) takes the SMU's interrupt source,
//! whose `set` is `smu_v14_0_set_irq_state` with AMDGPU_IRQ_STATE_ENABLE (S:754-797), and then sends
//! AllowIHHostInterrupt. Four registers, each Linux's change of what it reads (the trace: init.txt lines
//! 1870-1876). Fields: include/asic_reg/thm/thm_14_0_2_sh_mask.h:275-306, mp/mp_14_0_2_sh_mask.h:417-426.

use crate::ipfacts::hw;
use crate::regmap::Reg;

/// include/asic_reg/thm/thm_14_0_2_offset.h:51-54.
pub const THERMAL_INT_ENA: Reg = Reg { name: "THM_THERMAL_INT_ENA", hw_id: hw::THM, base_index: 0, offset: 0xA };
pub const THERMAL_INT_CTRL: Reg = Reg { name: "THM_THERMAL_INT_CTRL", hw_id: hw::THM, base_index: 0, offset: 0xB };
/// include/asic_reg/mp/mp_14_0_2_offset.h:287-290.
pub const MP1_IH_SW_INT: Reg = Reg { name: "MP1_SMN_IH_SW_INT", hw_id: hw::MP1, base_index: 1, offset: 0x141 };
pub const MP1_IH_SW_INT_CTRL: Reg = Reg { name: "MP1_SMN_IH_SW_INT_CTRL", hw_id: hw::MP1, base_index: 1, offset: 0x142 };

pub const DIG_THERM_INTH: u32 = 0x0000_00FF;
pub const DIG_THERM_INTL: u32 = 0x0000_FF00;
pub const THERM_INTH_MASK: u32 = 0x0100_0000;
pub const THERM_INTL_MASK: u32 = 0x0200_0000;
pub const THERM_TRIGGER_MASK: u32 = 0x0400_0000;
pub const THERM_IH_HW_ENA: u32 = 0x1000_0000;
pub const MAX_IH_CREDIT: u32 = 0xE000_0000;

/// `SMU_THERMAL_MINIMUM_ALERT_TEMP`, `SMU_THERMAL_MAXIMUM_ALERT_TEMP` (pm/swsmu/inc/amdgpu_smu.h:35-36), degrees.
pub const ALERT_MIN: i32 = 0;
pub const ALERT_MAX: i32 = 255;

/// THM_THERMAL_INT_CTRL as Linux leaves it (S:756-768): five interrupt credits, the hardware's own interrupt on,
/// both thresholds unmasked, the upper one at the table's software shutdown temperature (at most 255) and the
/// lower at the range's minimum (at least 0), the trigger's mask cleared. `min_milli` is the thermal range's
/// minimum in millidegrees.
pub const fn int_ctrl(read: u32, software_shutdown_temp: u16, min_milli: i32) -> u32 {
    let low = if min_milli / 1000 > ALERT_MIN { min_milli / 1000 } else { ALERT_MIN };
    let high = if (software_shutdown_temp as i32) < ALERT_MAX { software_shutdown_temp as i32 } else { ALERT_MAX };
    let mut v = (read & !MAX_IH_CREDIT) | 5 << 29;
    v |= THERM_IH_HW_ENA;
    v &= !(THERM_INTH_MASK | THERM_INTL_MASK);
    v = (v & !DIG_THERM_INTH) | (high as u32 & 0xFF);
    v = (v & !DIG_THERM_INTL) | (low as u32 & 0xFF) << 8;
    v & !THERM_TRIGGER_MASK
}

/// THM_THERMAL_INT_ENA (S:770-773): the three clear bits (THERM_INTH_CLR, THERM_INTL_CLR, THERM_TRIGGER_CLR).
pub const INT_ENA: u32 = 1 << 3 | 1 << 4 | 1 << 5;

/// MP1_SMN_IH_SW_INT (S:786-789): ID 0xFE, VALID 0.
pub const fn sw_int(read: u32) -> u32 {
    (read & !0x1FF) | 0xFE
}

/// MP1_SMN_IH_SW_INT_CTRL (S:791-793): INT_MASK 0.
pub const fn sw_int_ctrl(read: u32) -> u32 {
    read & !1
}

/// Wave 44: the handler of the three sources `smu_v14_0_register_irq_handler` registers (S:891-922), the only
/// ones Linux's dispatch hands it (amdgpu_irq.c:468-533): THM's two thresholds and MP1's interrupt to the
/// driver. Clients: include/soc15_ih_clientid.h:48, :64; sources S:804-805 and pm/swsmu/smu_cmn.h:39.
pub const CLIENT_THM: u8 = 0x0F;
pub const CLIENT_MP1: u8 = 0x1F;
/// `THM_11_0__SRCID__THM_DIG_THERM_L2H`: the temperature rose above DIG_THERM_INTH.
pub const SRC_L2H: u8 = 0;
/// `THM_11_0__SRCID__THM_DIG_THERM_H2L`: it fell below DIG_THERM_INTL.
pub const SRC_H2L: u8 = 1;
/// `SMU_IH_INTERRUPT_ID_TO_DRIVER`.
pub const SRC_TO_DRIVER: u8 = 0xFE;

/// The context ids of MP1's interrupt in `src_data[0]` (smu_cmn.h:40-47); only the two fan cases do something.
pub const CTX_FAN_ABNORMAL: u32 = 0x8;
pub const CTX_FAN_RECOVERY: u32 = 0x9;
pub const CONTEXTS: [(u32, &str); 8] = [
    (0x2, "BACO"),
    (0x3, "AC"),
    (0x4, "DC"),
    (0x5, "AUDIO_D0"),
    (0x6, "AUDIO_D3"),
    (0x7, "THERMAL_THROTTLING"),
    (CTX_FAN_ABNORMAL, "FAN_ABNORMAL"),
    (CTX_FAN_RECOVERY, "FAN_RECOVERY"),
];

pub fn context_name(ctx: u32) -> &'static str {
    CONTEXTS.iter().find(|(c, _)| *c == ctx).map_or("unknown", |(_, n)| n)
}

/// `MP1_SMN_IH_SW_INT_CTRL__INT_ACK_MASK` (mp/mp_14_0_2_sh_mask.h:426).
pub const INT_ACK: u32 = 0x100;
/// `AMDGPU_SWCTF_EXTRA_DELAY` (amdgpu/amdgpu.h:318): the software CTF check runs this long after the alert.
pub const SWCTF_DELAY_MS: u64 = 50;

/// Which of the handler's three sources an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    High,
    Low,
    Smu,
}

impl Source {
    pub const ALL: [Source; 3] = [Source::High, Source::Low, Source::Smu];

    pub const fn of(client: u8, src: u8) -> Option<Source> {
        match (client, src) {
            (CLIENT_THM, SRC_L2H) => Some(Source::High),
            (CLIENT_THM, SRC_H2L) => Some(Source::Low),
            (CLIENT_MP1, SRC_TO_DRIVER) => Some(Source::Smu),
            _ => None,
        }
    }

    pub const fn ids(self) -> (u8, u8) {
        match self {
            Source::High => (CLIENT_THM, SRC_L2H),
            Source::Low => (CLIENT_THM, SRC_H2L),
            Source::Smu => (CLIENT_MP1, SRC_TO_DRIVER),
        }
    }

    pub const fn plain(self) -> &'static str {
        match self {
            Source::High => "the card's temperature rose above its upper threshold",
            Source::Low => "the card's temperature fell below its lower threshold",
            Source::Smu => "the power controller sent the driver an interrupt",
        }
    }
}

/// The acknowledge of MP1's interrupt (S:844-847): INT_ACK set in what was read, every other bit kept.
pub const fn ack(read: u32) -> u32 {
    read | INT_ACK
}

/// The upper threshold moved (S:858-863, :871-876): DIG_THERM_INTH replaced by `high & 0xff`, the trigger's mask
/// cleared, every other bit kept.
pub const fn rearm(read: u32, high: u32) -> u32 {
    ((read & !DIG_THERM_INTH) | (high & 0xFF)) & !THERM_TRIGGER_MASK
}

/// FAN_ABNORMAL's new limit (S:850-854): `software_shutdown_temp + software_shutdown_temp_offset` in Linux's
/// u32 `high`, at most 255 (a negative sum wraps and so ends at 255, as in Linux).
pub const fn high_fan_abnormal(shutdown: u16, offset: i16) -> u32 {
    let h = (shutdown as i32 + offset as i32) as u32;
    if h < ALERT_MAX as u32 { h } else { ALERT_MAX as u32 }
}

/// FAN_RECOVERY's limit (S:867-869): the table's own, at most 255.
pub const fn high_fan_recovery(shutdown: u16) -> u32 {
    if (shutdown as u32) < ALERT_MAX as u32 { shutdown as u32 } else { ALERT_MAX as u32 }
}

/// What the software CTF check decides after its delay (`smu_swctf_delayed_work_handler`,
/// pm/swsmu/amdgpu_smu.c:1303-1330): nothing only when the table names a limit, the hotspot temperature was read
/// (millidegrees, `hotspot_milli`) and lies below that limit; in every other case Linux powers the machine off.
pub const fn swctf_powers_off(shutdown: u16, hotspot_milli: Option<u32>) -> bool {
    match hotspot_milli {
        Some(h) if shutdown != 0 => h / 1000 >= shutdown as u32,
        _ => true,
    }
}

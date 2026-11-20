//! Wave 44: the model's thermal block and the PMFW's metrics table, as Linux's thermal handler meets them
//! (`smu_v14_0_irq_process`, ../pm/swsmu/smu14/smu_v14_0.c:807-882, and `smu_v14_0_2_read_sensor`,
//! ../pm/swsmu/smu14/smu_v14_0_2_ppt.c:801-880). After the start's four registers went the model
//!
//! - raises each of the three alerts as the card would (`raise_thermal`), but only once the start armed it: THM's
//!   thresholds with the hardware's interrupt on and the threshold unmasked, MP1's interrupt after
//!   AllowIHHostInterrupt with its mask cleared; and on its own when a test's hotspot temperature crosses the
//!   upper threshold (`set_temps`), so a test shows the whole way from a temperature to the taker;
//! - takes MP1_SMN_IH_SW_INT_CTRL only as the acknowledge of an interrupt it raised and nobody acknowledged
//!   yet, with INT_ACK set in what it holds and nothing else changed (it keeps the bit as written: whether the
//!   card clears it is a row of section 7, `thm-ack`); THM_THERMAL_INT_CTRL only right after the acknowledge of
//!   FAN_ABNORMAL or FAN_RECOVERY, with Linux's limit from the model's own table; THM_THERMAL_INT_ENA and
//!   MP1_SMN_IH_SW_INT never again;
//! - answers TransferTableSmu2Dram with argument 5 only after Linux's start ended (AllowIHHostInterrupt
//!   accepted), writing a metrics table with its temperatures into the driver table; every other message of
//!   the start is refused from then on (`fakepmfw`).

use super::fakemodel::{MC_BASE, Model};
use super::fakepmfw::FakePmfw;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use rdna4_core::ih7::{Iv, Ring};
use rdna4_core::metrics14::{self, Temps};
use rdna4_core::thm14;

/// How many reads of the answer register showed 0 to Linux after its metrics transfer (init.txt line 9954).
pub const METRICS_READS: u32 = 237;

/// What a test makes the card report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alert {
    High,
    Low,
    /// MP1's interrupt with this context id in `src_data[0]`.
    Smu(u32),
}

#[derive(Clone, Debug, Default)]
pub struct FakeThermal {
    /// What the next metrics table holds (degrees; a cold card's idle temperatures by default, invented).
    pub temps: Temps,
    /// MP1 interrupts raised and not acknowledged, their context ids in order.
    pub raised: VecDeque<u32>,
    /// The upper threshold the acknowledge of a fan case asks for: the next write of THM_THERMAL_INT_CTRL.
    pub(super) rearm: Option<u32>,
    pub acks: u32,
    pub rearms: u32,
    pub transfers: u32,
    /// The hotspot stands above the upper threshold (the THM block's view, `thm-sensors` in section 7).
    above: bool,
}

impl FakeThermal {
    pub fn new() -> FakeThermal {
        FakeThermal { temps: Temps { edge: 34, hotspot: 38, mem: 40, ..Temps::default() }, ..FakeThermal::default() }
    }

    /// A new PMFW: what the old one raised is gone; the card's temperatures stay.
    pub(super) fn after_reload(&self) -> FakeThermal {
        FakeThermal { temps: self.temps, ..FakeThermal::default() }
    }
}

impl Model {
    fn thm_armed(&self, a: Alert) -> Result<(), String> {
        let p = &self.pmfw;
        let c = p.thm_ctrl;
        let ok = match a {
            Alert::High => p.thermal >= 1 && c & thm14::THERM_IH_HW_ENA != 0 && c & thm14::THERM_INTH_MASK == 0,
            Alert::Low => p.thermal >= 1 && c & thm14::THERM_IH_HW_ENA != 0 && c & thm14::THERM_INTL_MASK == 0,
            Alert::Smu(_) => p.ih_allowed && p.sw_int & 0xFF == u32::from(thm14::SRC_TO_DRIVER) && p.sw_int_ctrl & 1 == 0,
        };
        if ok { Ok(()) } else { Err(format!("{a:?}: the alert is not armed (Linux's start arms it at its end, smu_v14_0.c:754-797 and AllowIHHostInterrupt)")) }
    }

    /// The card reports `a` through the IH, as Linux's handler expects the entry.
    pub fn raise_thermal(&mut self, a: Alert) -> Result<(), String> {
        self.thm_armed(a)?;
        let (client_id, src_id, d0) = match a {
            Alert::High => (thm14::CLIENT_THM, thm14::SRC_L2H, 0),
            Alert::Low => (thm14::CLIENT_THM, thm14::SRC_H2L, 0),
            Alert::Smu(ctx) => (thm14::CLIENT_MP1, thm14::SRC_TO_DRIVER, ctx),
        };
        self.raise(Ring::Ring0, &Iv { client_id, src_id, src_data: [d0, 0, 0, 0], ..Iv::default() })?;
        if let Alert::Smu(ctx) = a {
            self.pmfw.thm.raised.push_back(ctx);
        }
        Ok(())
    }

    /// The temperatures of the next metrics table; a hotspot that rises above the upper threshold raises the
    /// alert as the card does (once per crossing).
    pub fn set_temps(&mut self, edge: u16, hotspot: u16, mem: u16) -> Result<(), String> {
        let t = &mut self.pmfw.thm.temps;
        (t.edge, t.hotspot, t.mem) = (edge, hotspot, mem);
        self.thm_cross()
    }

    fn thm_cross(&mut self) -> Result<(), String> {
        let high = self.pmfw.thm_ctrl & thm14::DIG_THERM_INTH;
        let hot = u32::from(self.pmfw.thm.temps.hotspot) > high;
        let rose = hot && !self.pmfw.thm.above;
        self.pmfw.thm.above = hot;
        if rose && self.thm_armed(Alert::High).is_ok() { self.raise_thermal(Alert::High) } else { Ok(()) }
    }

    /// TransferTableSmu2Dram of the metrics table (`fakepmfw`'s message 0x12 with argument 5).
    pub(super) fn thm_metrics(&mut self) -> Result<(), String> {
        if !(self.pmfw.enabled && self.pmfw.ih_allowed) {
            return Err(String::from("TransferTableSmu2Dram of the metrics table before Linux's start of the power controller ended (Linux reads a sensor only once its start passed)"));
        }
        let Some(at) = FakePmfw::table_mc(self.pmfw.driver) else { return Err(String::from("a metrics transfer with no driver table")) };
        let thm = &mut self.pmfw.thm;
        thm.transfers += 1;
        thm.temps.counter = thm.temps.counter.wrapping_add(1);
        let mut bytes = alloc::vec![0u8; metrics14::METRICS_BYTES];
        metrics14::synthetic(&thm.temps, &mut bytes);
        self.poke(at - MC_BASE, &bytes);
        Ok(())
    }

    /// A write of one of the thermal registers after the start's four went: Linux's handler, or a refusal.
    /// `None` for any other byte.
    pub(super) fn thm_write(&mut self, byte: u32, v: u32) -> Option<bool> {
        let r = self.regs.clone();
        if byte == r.mp1_ih_sw_int_ctrl {
            let Some(ctx) = self.pmfw.thm.raised.front().copied() else {
                return Some(self.refuse(String::from("MP1_SMN_IH_SW_INT_CTRL: an acknowledge with no power controller interrupt pending")));
            };
            let want = thm14::ack(self.pmfw.sw_int_ctrl);
            if v != want {
                return Some(self.refuse(format!("MP1_SMN_IH_SW_INT_CTRL written {v:#x}: Linux's acknowledge of what it holds is {want:#x}")));
            }
            self.pmfw.thm.raised.pop_front();
            (self.pmfw.sw_int_ctrl, self.pmfw.thm.acks) = (v, self.pmfw.thm.acks + 1);
            let (sd, off) = (self.pmfw.table.software_shutdown_temp, self.pmfw.table.fan_abnormal_temp_limit_offset);
            self.pmfw.thm.rearm = match ctx {
                thm14::CTX_FAN_ABNORMAL => Some(thm14::high_fan_abnormal(sd, off)),
                thm14::CTX_FAN_RECOVERY => Some(thm14::high_fan_recovery(sd)),
                _ => None,
            };
            return Some(true);
        }
        if byte == r.thm_int_ctrl {
            let Some(h) = self.pmfw.thm.rearm.take() else {
                return Some(self.refuse(String::from("THM_THERMAL_INT_CTRL written after the start but not right after the acknowledge of a fan fault or its recovery")));
            };
            let want = thm14::rearm(self.pmfw.thm_ctrl, h);
            if v != want {
                return Some(self.refuse(format!("THM_THERMAL_INT_CTRL written {v:#x}: Linux's limit {h} in what it holds is {want:#x}")));
            }
            (self.pmfw.thm_ctrl, self.pmfw.thm.rearms) = (v, self.pmfw.thm.rearms + 1);
            return Some(self.thm_cross().map_or_else(|e| self.refuse(e), |()| true));
        }
        if byte == r.thm_int_ena || byte == r.mp1_ih_sw_int {
            let what = if byte == r.thm_int_ena { "THM_THERMAL_INT_ENA" } else { "MP1_SMN_IH_SW_INT" };
            return Some(self.refuse(format!("{what} written after the start: Linux's handler never writes it")));
        }
        None
    }
}

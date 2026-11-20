//! Wave 44: the thermal handler's bus accesses, each through a `rdna4_core::thmgate::Gate` opened for that one
//! action: the card's own `Hw` (the IH thread's mapping, the tests' model) behind `Gated`, which lets a register
//! read only when it is one of the action's, a write only when the gate says it is the action's next access,
//! and the driver table only after the transfer was answered OK. Whatever a caller asks, nothing else reaches
//! the card from here.

use super::regs14::Regs14;
use super::smu14::{self, Line, SmuStop};
use crate::gpucore::psp::{Bound, Cond, Hw};
use alloc::format;
use alloc::string::String;
use rdna4_core::metrics14::{self, Temps};
use rdna4_core::smu14::msg;
use rdna4_core::thm14;
use rdna4_core::thmgate::{Action, Gate};

/// The card for one action of the handler.
pub struct Gated<'a> {
    hw: &'a mut dyn Hw,
    regs: &'a Regs14,
    gate: Gate,
    /// The driver table (VRAM offset), the one memory the sensors' read may read.
    table: Option<u64>,
    /// What the gate refused, the first.
    pub refused: Option<&'static str>,
}

impl<'a> Gated<'a> {
    pub fn new(hw: &'a mut dyn Hw, regs: &'a Regs14, action: Action, table: Option<u64>) -> Gated<'a> {
        Gated { hw, regs, gate: Gate::new(action), table, refused: None }
    }

    fn refuse(&mut self, why: &'static str, byte: u32, value: u32) -> bool {
        self.refused.get_or_insert(why);
        klog::event!(Error, "gx", "rdna4.thermal.refused", byte = klog::Hex(u64::from(byte)), value = klog::Hex(u64::from(value)), why = why; "the thermal handler's gate refused an access");
        false
    }
}

impl Hw for Gated<'_> {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        let reg = self.regs.reg_at(byte).filter(|r| self.gate.may_read(*r))?;
        let v = self.hw.reg_read(byte)?;
        self.gate.saw_read(reg, v);
        Some(v)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        let Some(reg) = self.regs.reg_at(byte) else { return self.refuse("a register the handler does not know", byte, value) };
        if let Err(why) = self.gate.judge(reg, value) {
            return self.refuse(why, byte, value);
        }
        if !self.hw.reg_write(byte, value) {
            return false;
        }
        self.gate.wrote(reg, value);
        true
    }
    fn mem_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.gate.may_read_table() && self.table == Some(at) && buf.len() == metrics14::METRICS_BYTES && self.hw.mem_read(at, buf)
    }
    fn mem_write(&mut self, _at: u64, _data: &[u8]) -> bool {
        false
    }
    fn now_us(&mut self) -> u64 {
        self.hw.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.hw.delay_us(us)
    }
    fn wait_begin(&mut self, byte: u32, cond: Cond, bound: Bound) {
        self.hw.wait_begin(byte, cond, bound)
    }
    fn wait_end(&mut self) {
        self.hw.wait_end()
    }
}

/// MP1's interrupt acknowledged as Linux does it (S:844-847), and for the two fan cases the upper threshold moved
/// to `high` (S:849-877). Stricter than Linux: a register that reads all ones (a card that does not answer) is
/// not written back.
pub fn acknowledge(hw: &mut dyn Hw, regs: &Regs14, ctx: u32, high: Option<u32>) -> Result<(), String> {
    let mut g = Gated::new(hw, regs, Action::Ack { ctx, high }, None);
    let rmw = |g: &mut Gated, byte: u32, change: &dyn Fn(u32) -> u32, what: &str| -> Result<(), String> {
        let v = g.reg_read(byte).filter(|v| *v != u32::MAX).ok_or_else(|| format!("{what} does not read"))?;
        if g.reg_write(byte, change(v)) { Ok(()) } else { Err(format!("{what} was not written ({})", g.refused.unwrap_or("the card did not take it"))) }
    };
    rmw(&mut g, regs.mp1_ih_sw_int_ctrl, &thm14::ack, "MP1_SMN_IH_SW_INT_CTRL")?;
    if let Some(h) = high {
        rmw(&mut g, regs.thm_int_ctrl, &|v| thm14::rearm(v, h), "THM_THERMAL_INT_CTRL")?;
    }
    Ok(())
}

/// The temperatures as Linux's `read_sensor` reads them: TransferTableSmu2Dram with argument 5 in Linux's
/// mailbox form (`smu14::send`: the answer register polled first, as for every message but the start's first),
/// then `sizeof(SmuMetricsExternal_t)` bytes out of the driver table (on the heap), parsed by the documented
/// header. HDP 7 has no invalidate (Linux's `amdgpu_hdp_invalidate` does nothing here), so none is made.
pub fn read_temps(hw: &mut dyn Hw, regs: &Regs14, driver_at: u64) -> Result<Temps, String> {
    let mut g = Gated::new(hw, regs, Action::Metrics, Some(driver_at));
    let mut line = Line { first: false, ..Line::default() };
    smu14::send(&mut g, regs, &mut line, msg::TRANSFER_TABLE_SMU2DRAM, metrics14::TRANSFER_ARG, false).map_err(|e| match e {
        SmuStop::Answer { code, .. } => format!("TransferTableSmu2Dram (0x12, argument 5) answered {code:#x}: Linux's {}", rdna4_core::smu14::decode(code).errno()),
        SmuStop::Timeout { .. } => format!("TransferTableSmu2Dram (0x12, argument 5): no answer within Linux's {} reads", smu14::ANSWER_READS),
        SmuStop::Busy { .. } => String::from("the message before has no answer: nothing was sent"),
        SmuStop::Hung { value } => format!("the answer register holds {value:#x}, which Linux takes for a hang: nothing was sent"),
        SmuStop::Refused { .. } => format!("refused by the handler's gate ({})", g.refused.unwrap_or("?")),
        other => format!("{other:?}"),
    })?;
    let mut bytes = alloc::vec![0u8; metrics14::METRICS_BYTES];
    if !g.mem_read(driver_at, &mut bytes) {
        return Err(String::from("the driver table does not read"));
    }
    metrics14::parse(&bytes).map_err(String::from)
}

//! Wave 44: the one way the thermal interrupt's handler reaches the card. It is the third user of the SMU's
//! mailbox, after the look's questions (`refuse::SMU_QUESTIONS`) and the start (`smugate::Gate`, which ends with
//! the start and then takes no message more). Each of the handler's actions opens one gate of its own, and the
//! gate lets through exactly Linux's bus accesses of that action, in Linux's order, each only as Linux's change
//! of what the gate saw read:
//!
//! - the acknowledge of MP1's interrupt (`smu_v14_0_irq_process`, `S:` pm/swsmu/smu14/smu_v14_0.c:844-847):
//!   MP1_SMN_IH_SW_INT_CTRL read, then written with INT_ACK set and nothing else changed; then, for FAN_ABNORMAL
//!   and FAN_RECOVERY only (S:849-877), THM_THERMAL_INT_CTRL read and written with DIG_THERM_INTH replaced by the
//!   limit Linux computes from the table and the trigger's mask cleared;
//! - the sensors' read (`smu_cmn_update_table`, pm/swsmu/smu_cmn.c:1109-1160): the answer register read as
//!   answered, 0 into it, the argument 5 (`metrics14::TRANSFER_ARG`), the message TransferTableSmu2Dram 0x12,
//!   the answer; then the driver table may be read.
//!
//! Every other write is refused, a message on the never list first of all (SetPptLimit 0x32 among them),
//! whatever the action. The gate opens nothing for limits, fans, voltages or clocks.

use crate::metrics14;
use crate::refuse::{self, Verdict};
use crate::regmap::{self, Reg};
use crate::smu14::{self, Resp, msg};
use crate::thm14;

/// What one gate is opened for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// MP1's interrupt with this context id; `high`: the new upper threshold for the two fan cases.
    Ack { ctx: u32, high: Option<u32> },
    /// One read of the PMFW's metrics table.
    Metrics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    ReadAck,
    WriteAck(u32),
    ReadCtrl,
    WriteCtrl(u32),
    PollAnswer,
    Clear,
    Argue,
    Message,
    Answer,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    action: Action,
    step: Step,
    answer: Option<u32>,
}

fn same(a: Reg, b: Reg) -> bool {
    a.hw_id == b.hw_id && a.base_index == b.base_index && a.offset == b.offset
}

/// The registers any of the handler's actions reads or writes.
pub const REGS: [Reg; 5] = [thm14::MP1_IH_SW_INT_CTRL, thm14::THERMAL_INT_CTRL, regmap::SMU_ANSWER, regmap::SMU_ARGUMENT, regmap::SMU_MESSAGE];

impl Gate {
    pub fn new(action: Action) -> Gate {
        let step = match action {
            Action::Ack { .. } => Step::ReadAck,
            Action::Metrics => Step::PollAnswer,
        };
        Gate { action, step, answer: None }
    }

    /// Whether this write is the action's next bus access.
    pub fn judge(&self, reg: Reg, value: u32) -> Result<(), &'static str> {
        if same(reg, regmap::SMU_MESSAGE) {
            // By construction, whatever the action.
            if let Ok(m) = u16::try_from(value)
                && let Verdict::Never(_) = refuse::smu_verdict(m)
            {
                return Err("an SMU message on the never list");
            }
            return match (self.action, self.step) {
                (Action::Metrics, Step::Message) if value == u32::from(msg::TRANSFER_TABLE_SMU2DRAM) => Ok(()),
                (Action::Metrics, Step::Message) => Err("the thermal handler sends TransferTableSmu2Dram and no other message"),
                _ => Err("an SMU message outside the sensors' read, or before its answer register and argument"),
            };
        }
        if same(reg, regmap::SMU_ANSWER) {
            return match (self.step, value) {
                (Step::Clear, 0) => Ok(()),
                (Step::Clear, _) => Err("the SMU's answer register is written with 0 only"),
                _ => Err("the answer register before it was read as answered, or outside the sensors' read"),
            };
        }
        if same(reg, regmap::SMU_ARGUMENT) {
            return match self.step {
                Step::Argue if value == metrics14::TRANSFER_ARG => Ok(()),
                Step::Argue => Err("not the metrics table's argument (5) Linux's source computes"),
                _ => Err("the SMU's argument outside the sensors' read, or before 0 went into the answer register"),
            };
        }
        if same(reg, thm14::MP1_IH_SW_INT_CTRL) {
            return match self.step {
                Step::WriteAck(read) if value == thm14::ack(read) => Ok(()),
                Step::WriteAck(_) => Err("not Linux's acknowledge of what MP1_SMN_IH_SW_INT_CTRL read (INT_ACK set, nothing else changed)"),
                _ => Err("MP1_SMN_IH_SW_INT_CTRL outside the acknowledge of an MP1 interrupt, or before it was read"),
            };
        }
        if same(reg, thm14::THERMAL_INT_CTRL) {
            return match (self.action, self.step) {
                (Action::Ack { high: Some(h), .. }, Step::WriteCtrl(read)) if value == thm14::rearm(read, h) => Ok(()),
                (Action::Ack { high: Some(_), .. }, Step::WriteCtrl(_)) => Err("not Linux's new upper threshold in what THM_THERMAL_INT_CTRL read"),
                _ => Err("THM_THERMAL_INT_CTRL only after the acknowledge of FAN_ABNORMAL or FAN_RECOVERY, once read"),
            };
        }
        Err("the thermal handler writes nothing else")
    }

    /// A write `judge` passed reached the card.
    pub fn wrote(&mut self, reg: Reg, _value: u32) {
        self.step = match self.step {
            Step::WriteAck(_) if same(reg, thm14::MP1_IH_SW_INT_CTRL) => match self.action {
                Action::Ack { high: Some(_), .. } => Step::ReadCtrl,
                _ => Step::Done,
            },
            Step::WriteCtrl(_) if same(reg, thm14::THERMAL_INT_CTRL) => Step::Done,
            Step::Clear if same(reg, regmap::SMU_ANSWER) => Step::Argue,
            Step::Argue if same(reg, regmap::SMU_ARGUMENT) => Step::Message,
            Step::Message if same(reg, regmap::SMU_MESSAGE) => Step::Answer,
            other => other,
        };
    }

    /// A read came back with `value`.
    pub fn saw_read(&mut self, reg: Reg, value: u32) {
        self.step = match self.step {
            Step::ReadAck if same(reg, thm14::MP1_IH_SW_INT_CTRL) => Step::WriteAck(value),
            Step::ReadCtrl if same(reg, thm14::THERMAL_INT_CTRL) => Step::WriteCtrl(value),
            Step::PollAnswer if same(reg, regmap::SMU_ANSWER) && smu14::decode(value).lets_send() => Step::Clear,
            Step::Answer if same(reg, regmap::SMU_ANSWER) && value != 0 => {
                self.answer = Some(value);
                Step::Done
            }
            other => other,
        };
    }

    /// Whether a register may be read at all: one of the action's own.
    pub fn may_read(&self, reg: Reg) -> bool {
        match self.action {
            Action::Ack { high, .. } => same(reg, thm14::MP1_IH_SW_INT_CTRL) || (high.is_some() && same(reg, thm14::THERMAL_INT_CTRL)),
            Action::Metrics => same(reg, regmap::SMU_ANSWER),
        }
    }

    /// The transfer was answered OK: the driver table holds the metrics now.
    pub fn may_read_table(&self) -> bool {
        self.action == Action::Metrics && self.step == Step::Done && self.answer.is_some_and(|a| smu14::decode(a) == Resp::Ok)
    }

    /// Every access of the action went.
    pub fn done(&self) -> bool {
        self.step == Step::Done
    }
}

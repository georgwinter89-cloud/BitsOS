//! The one way to the power controller's start messages: `refuse::write_refused` lets only questions through to
//! the SMU's mailbox, and a `Gate` opens it for the stage that starts the power controller, one bus access at a
//! time, as Linux's `smu_msg_v1_send_msg` takes them (`C:` pm/swsmu/smu_cmn.c:459-578): the answer register read
//! as answered (but before the first message, C:508-511), 0 into it, the argument, the message; then the answer,
//! and the argument register where Linux reads it. The gate holds its own `smuseq::Script`, moved only by what
//! it saw cross the bus, so a message passes only when it is that sequence's next with exactly the argument
//! Linux's source computes for it here: the tables' addresses, 1 for the transfer, the link limits from the table
//! the gate itself saw read out of VRAM, the clock and level the answers before named. A message on the never
//! list stays refused whatever the sequence says. The thermal alert's four registers pass only at their step,
//! each only as Linux's change of what the gate saw read there.

use crate::refuse::{self, Verdict};
use crate::regmap::{self, Reg};
use crate::smu14::{self, Resp};
use crate::smuseq::{Script, Step};
use crate::thm14;

/// Where one message stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mailbox {
    /// Nothing may be written: the answer register was not read as answered since the last message.
    Idle,
    /// It was: 0 may go into the answer register.
    Ready,
    /// The argument may go.
    Cleared,
    Argued(u32),
    /// The message went: its answer is awaited.
    Sent,
    /// Answered OK: Linux reads the argument register next.
    Value,
}

/// Where the thermal alert's registers stand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Thermal {
    ReadCtrl,
    WriteCtrl(u32),
    WriteEna,
    ReadSwInt,
    WriteSwInt(u32),
    ReadSwIntCtrl,
    WriteSwIntCtrl(u32),
}

/// What the gate says of a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Judged {
    /// Not a register of the power controller's start: others decide.
    NotMine,
    Pass,
    Refused(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    script: Script,
    mailbox: Mailbox,
    /// No message went through this gate yet: Linux's first one does not look at the answer register
    /// (`SMU_FW_INIT`, C:508-511).
    first: bool,
    thermal: Thermal,
}

fn same(a: Reg, b: Reg) -> bool {
    a.hw_id == b.hw_id && a.base_index == b.base_index && a.offset == b.offset
}

impl Gate {
    pub fn new(script: Script) -> Gate {
        Gate { script, mailbox: Mailbox::Idle, first: true, thermal: Thermal::ReadCtrl }
    }

    /// The sequence as the bus showed it.
    pub fn script(&self) -> &Script {
        &self.script
    }

    fn message(&self, value: u32) -> Judged {
        let Ok(m) = u16::try_from(value) else { return Judged::Refused("upper bits name no SMU message") };
        // By construction, whatever the sequence holds.
        if let Verdict::Never(_) = refuse::smu_verdict(m) {
            return Judged::Refused("an SMU message on the never list");
        }
        let Mailbox::Argued(arg) = self.mailbox else { return Judged::Refused("an SMU message before answer 0 and its argument went") };
        match self.script.next() {
            Step::Send { msg, arg: a, .. } if msg == m && a == arg => Judged::Pass,
            Step::Send { msg, .. } if msg == m => Judged::Refused("an SMU message with another argument than Linux's source computes for it here"),
            Step::Send { .. } => Judged::Refused(match refuse::smu_verdict(m) {
                Verdict::Start(_) | Verdict::Allowed(_) => "an SMU message out of Linux's order: another one is the start's next",
                _ => "not a message of Linux's start of the power controller",
            }),
            Step::Table => Judged::Refused("an SMU message before the PMFW's table was read out of the driver table"),
            Step::Thermal => Judged::Refused("an SMU message before the thermal alert's registers were written"),
            Step::End => Judged::Refused("the start of the power controller is over: no message more"),
        }
    }

    fn thermal(&self, reg: Reg, value: u32) -> Judged {
        if self.script.next() != Step::Thermal {
            return Judged::Refused("the thermal alert's registers are written only at their step of the start");
        }
        let Some(t) = self.script.facts.table else { return Judged::Refused("the thermal alert before the PMFW's table was read") };
        let want = match self.thermal {
            Thermal::WriteCtrl(read) if same(reg, thm14::THERMAL_INT_CTRL) => thm14::int_ctrl(read, t.software_shutdown_temp, smu14::THERMAL_MIN_MILLI),
            Thermal::WriteEna if same(reg, thm14::THERMAL_INT_ENA) => thm14::INT_ENA,
            Thermal::WriteSwInt(read) if same(reg, thm14::MP1_IH_SW_INT) => thm14::sw_int(read),
            Thermal::WriteSwIntCtrl(read) if same(reg, thm14::MP1_IH_SW_INT_CTRL) => thm14::sw_int_ctrl(read),
            _ => return Judged::Refused("a register of the thermal alert out of Linux's order, or before it was read"),
        };
        if value == want { Judged::Pass } else { Judged::Refused("not Linux's change of the thermal alert's register as it read") }
    }

    /// Whether this write is exactly Linux's next bus access.
    pub fn judge(&self, reg: Reg, value: u32) -> Judged {
        if same(reg, regmap::SMU_ANSWER) {
            if value != 0 {
                return Judged::Refused("the SMU's answer register is written with 0 only");
            }
            if !matches!(self.script.next(), Step::Send { .. }) {
                return Judged::Refused("no SMU message is the start's next step");
            }
            return match self.mailbox {
                Mailbox::Ready => Judged::Pass,
                Mailbox::Idle if self.first => Judged::Pass,
                Mailbox::Sent | Mailbox::Value => Judged::Refused("a new SMU message while the one before has no answer"),
                _ => Judged::Refused("a new SMU message before the answer register was read as answered"),
            };
        }
        if same(reg, regmap::SMU_ARGUMENT) {
            if self.mailbox != Mailbox::Cleared {
                return Judged::Refused("the SMU's argument before 0 went into the answer register");
            }
            return match self.script.next() {
                Step::Send { arg, .. } if arg == value => Judged::Pass,
                _ => Judged::Refused("not the argument Linux's source computes for the start's next message"),
            };
        }
        if same(reg, regmap::SMU_MESSAGE) {
            return self.message(value);
        }
        if [thm14::THERMAL_INT_CTRL, thm14::THERMAL_INT_ENA, thm14::MP1_IH_SW_INT, thm14::MP1_IH_SW_INT_CTRL].iter().any(|r| same(*r, reg)) {
            return self.thermal(reg, value);
        }
        Judged::NotMine
    }

    /// A write `judge` let through reached the card.
    pub fn wrote(&mut self, reg: Reg, value: u32) {
        if same(reg, regmap::SMU_ANSWER) {
            self.mailbox = Mailbox::Cleared;
        } else if same(reg, regmap::SMU_ARGUMENT) {
            self.mailbox = Mailbox::Argued(value);
        } else if same(reg, regmap::SMU_MESSAGE) {
            (self.mailbox, self.first) = (Mailbox::Sent, false);
            self.script.sent();
        } else if same(reg, thm14::THERMAL_INT_CTRL) {
            self.thermal = Thermal::WriteEna;
        } else if same(reg, thm14::THERMAL_INT_ENA) {
            self.thermal = Thermal::ReadSwInt;
        } else if same(reg, thm14::MP1_IH_SW_INT) {
            self.thermal = Thermal::ReadSwIntCtrl;
        } else if same(reg, thm14::MP1_IH_SW_INT_CTRL) {
            self.thermal = Thermal::ReadCtrl;
            self.script.thermal_done();
        }
    }

    /// A read came back with `value`.
    pub fn saw_read(&mut self, reg: Reg, value: u32) {
        if same(reg, regmap::SMU_ANSWER) {
            let r = smu14::decode(value);
            match self.mailbox {
                Mailbox::Sent if r == Resp::None => {}
                Mailbox::Sent if r == Resp::Ok => {
                    if matches!(self.script.next(), Step::Send { value: true, .. }) {
                        self.mailbox = Mailbox::Value;
                    } else {
                        let _ = self.script.answered(None);
                        self.mailbox = Mailbox::Idle;
                    }
                }
                Mailbox::Sent => {
                    self.script.failed(value);
                    self.mailbox = Mailbox::Idle;
                }
                Mailbox::Idle if r.lets_send() => self.mailbox = Mailbox::Ready,
                _ => {}
            }
        } else if same(reg, regmap::SMU_ARGUMENT) {
            if self.mailbox == Mailbox::Value {
                let _ = self.script.answered(Some(value));
                self.mailbox = Mailbox::Idle;
            }
        } else if self.script.next() == Step::Thermal {
            self.thermal = match self.thermal {
                Thermal::ReadCtrl if same(reg, thm14::THERMAL_INT_CTRL) => Thermal::WriteCtrl(value),
                Thermal::ReadSwInt if same(reg, thm14::MP1_IH_SW_INT) => Thermal::WriteSwInt(value),
                Thermal::ReadSwIntCtrl if same(reg, thm14::MP1_IH_SW_INT_CTRL) => Thermal::WriteSwIntCtrl(value),
                other => other,
            };
        }
    }

    /// Memory at the MC address `mc` was read: the driver table, when the PMFW's table is the next step.
    pub fn saw_memory(&mut self, mc: u64, bytes: &[u8]) {
        if self.script.next() == Step::Table && mc == self.script.tables().driver_mc && bytes.len() as u64 == smu14::DRIVER_TABLE_BYTES {
            let _ = self.script.table(bytes);
        }
    }
}

//! The model's SMU face: the index pair, the mailbox's order and the HDP flush register itself. The index pair
//! only at the firmware flags' SMN address (and never put back: Linux leaves it there); the mailbox as answer 0,
//! argument, message, a new message only after the answer register was read as answered since the one before
//! (but the first since the firmware started); what each message does and answers is the model's power controller (`fakepmfw`).

use super::fakemodel::{CLOCKS, Model};
use alloc::format;
use alloc::string::String;
use rdna4_core::refuse;
use rdna4_core::regmap::{Reg, SMN_MP1_FIRMWARE_FLAGS};

impl Model {
    /// A clock question's answer: `clk_id << 16` names the clock (smu_v14_0.c `smu_v14_0_get_dpm_ultimate_freq`).
    pub(super) fn clock(&self, msg: u32) -> u32 {
        let id = self.c82 >> 16;
        CLOCKS.iter().find(|c| c.0 == id).map_or(0, |c| if msg == 0x1D { c.1 } else { c.2 })
    }

    pub(super) fn smu_write(&mut self, reg: Reg, byte: u32, v: u32) -> bool {
        let r = self.regs.clone();
        if byte == r.hdp_direct {
            if v != 0 {
                return self.refuse(format!("HDP flush written {v:#x}"));
            }
            // The register itself flushes until the remap is written; after it `fakenb` refuses this place, and
            // Linux's remapped place is `fakenb`'s too (it flushes only once `nbif` wrote the remap).
            self.hdp_flushed();
            return true;
        }
        if byte == r.rsmu_index {
            if v != SMN_MP1_FIRMWARE_FLAGS {
                return self.refuse(format!("index pair aimed at SMN {v:#x}"));
            }
            self.index = v;
            return true;
        }
        if let Some(reached) = self.pmfw_thermal(byte, v) {
            return reached;
        }
        if byte == r.smu_answer {
            if v != 0 {
                return self.refuse(format!("answer register written {v:#x}"));
            }
            if !(self.pmfw.seen_answer || self.pmfw.fresh) {
                return self.refuse(String::from("a new message while the answer register was not read as answered since the one before (smu_cmn.c:514-522)"));
            }
            self.pmfw.seen_answer = false;
            self.pmfw.held = self.c90;
            (self.c90, self.pmfw.wait, self.smu_cleared, self.smu_arg_set) = (0, 0, true, false);
            return true;
        }
        if byte == r.smu_argument {
            if !self.smu_cleared {
                return self.refuse(String::from("argument before the answer was cleared"));
            }
            (self.c82, self.smu_arg_set) = (v, true);
            return true;
        }
        if byte == r.smu_message {
            if !(self.smu_cleared && self.smu_arg_set) {
                return self.refuse(String::from("message before answer 0 and argument"));
            }
            (self.smu_cleared, self.smu_arg_set) = (false, false);
            // What may never go and what nobody reviewed, whatever a driver's gate let through. A message the
            // model refuses never ran: the answer register reads as before it.
            let m = match u16::try_from(v) {
                Ok(m) if matches!(refuse::smu_verdict(m), refuse::Verdict::Allowed(_) | refuse::Verdict::Start(_)) => m,
                _ => {
                    self.c90 = self.pmfw.held;
                    return self.refuse(format!("SMU message {v:#x}: {}", refuse::write_refused(reg, v).unwrap_or("not a message of this model")));
                }
            };
            return self.pmfw_message(m);
        }
        self.refuse(format!("{} is no register a driver writes here", reg.name))
    }
}

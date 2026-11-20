//! SMU 14.0.2's mailbox as Linux drives it, bus access for bus access, and the look `smu`'s read-only questions.
//!
//! The firmware flags are read at Linux's FIXED SMN address through the PCIE index pair
//! (`smu_v14_0_check_fw_status`, ../pm/swsmu/smu14/smu_v14_0.c:213-227): the address into the index register, the
//! index read back, the data read (`amdgpu_device_indirect_rreg`, amdgpu_reg_access.c:590-610). Nothing is saved
//! and nothing put back: Linux leaves the index on its last address.
//!
//! A message is `smu_msg_v1_send_msg` (../pm/swsmu/smu_cmn.c:459-578) with the driver mailbox of 14.0.2
//! (C2PMSG_66 message, 82 argument, 90 answer, ../pm/swsmu/smu14/smu_v14_0_2_ppt.c:2142-2162): the answer register
//! polled until it is not 0 (but before the first message after the firmware started, smu_cmn.c:508-511), 0 into
//! it, the argument, the message, the answer polled again, and the argument read where the caller wants a value.
//! Each poll is Linux's own loop (`__smu_msg_v1_poll_stat`, smu_cmn.c:294-309): at most
//! `smu14::ANSWER_READS` reads 1 us apart, no clock asked, one wait in the stage's trail.
//!
//! Stricter than Linux in the look: any answer but OK ends it; only the reviewed questions
//! (`rdna4_core::refuse::SMU_QUESTIONS`) are sent, and of them only those whose answer comes in the argument
//! register (GetAllRunningSmuFeatures is not mapped for 14.0.2, smu_v14_0_2_ppt.c:94-150, so Linux never sends
//! it: neither does this). No power, fan, clock or voltage message. The start of the power controller, with the
//! messages that are no questions, is the stage `pmfw` (`stage_pmfw`).

use super::regs14::Regs14;
use crate::gpucore::psp::{self, Cond, Hw, PspError};
use alloc::vec::Vec;
use rdna4_core::regmap::{MP1_FW_INTERRUPTS_ENABLED, SMN_MP1_FIRMWARE_FLAGS};
use rdna4_core::smu14::{self as core14, Resp};

pub use rdna4_core::smu14::{ANSWER_READS, DRIVER_IF};

/// `SMU_RESP_OK` (smu_cmn.c:77).
pub const RESP_OK: u32 = 0x1;
/// `(reg & MP1_C2PMSG_90__CONTENT_MASK) != 0` with a mask of all ones (smu_cmn.c:303): the register is not 0.
pub const ANSWERED: Cond = Cond::Changed { from: 0 };

/// The questions the look asks, in order, with Linux's argument `clk_id << 16` (smu_v14_0.c:685-695, :985-1000,
/// :1305-1320): GFXCLK is PPCLK 0, UCLK is PPCLK 2 (SOCCLK is 1; smu14_driver_if_v14_0.h:455-458, mapped by
/// smu_v14_0_2_ppt.c:152, :156); level 0xFF asks for the number of levels.
pub const ASKED: [(u16, u32, &str); 12] = [
    (0x02, 0, "GetSmuVersion"),
    (0x03, 0, "GetDriverIfVersion"),
    (0x0C, 0, "GetRunningSmuFeaturesLow"),
    (0x0D, 0, "GetRunningSmuFeaturesHigh"),
    (0x01, 0x0BAD_F00D, "TestMessage"),
    (0x33, 0, "GetPptLimit (AC)"),
    (0x1D, 0 << 16, "GetMinDpmFreq GFXCLK"),
    (0x1E, 0 << 16, "GetMaxDpmFreq GFXCLK"),
    (0x27, 0 << 16, "GetDcModeMaxDpmFreq GFXCLK"),
    (0x1F, 0 << 16 | 0xFF, "GetDpmFreqByIndex GFXCLK levels"),
    (0x1D, 2 << 16, "GetMinDpmFreq UCLK"),
    (0x1E, 2 << 16, "GetMaxDpmFreq UCLK"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmuStop {
    /// A register did not answer (all ones).
    Gone { byte: u32 },
    /// A write was refused (by the guard: never past it).
    Refused { byte: u32 },
    /// The flags say the SMU's firmware does not run.
    NotRunning { flags: u32 },
    /// The message before this one has no answer after Linux's `ANSWER_READS` reads: nothing is sent
    /// (smu_cmn.c:514-522).
    Busy { last: u32 },
    /// The answer register holds a code Linux decodes as -EREMOTEIO (a halted or debug-held SMU, e.g. 0xFB): Linux
    /// sends nothing more (smu_cmn.c:514-522, decode :249-292), neither does this.
    Hung { value: u32 },
    /// No answer to `msg` within Linux's `ANSWER_READS` reads.
    Timeout { msg: u16 },
    /// An answer other than OK.
    Answer { msg: u16, code: u32 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SmuFacts {
    pub flags: u32,
    /// (message, argument, name, answer code, argument register after).
    pub answers: Vec<(u16, u32, &'static str, u32, u32)>,
    /// A mailbox write reached the SMU in this read-out: the firmware's first message is spent, whatever it
    /// answered (Linux leaves SMU_FW_INIT before the first send, smu_cmn.c:508-511).
    pub sent: bool,
    /// The last message that went out was answered (whatever the answer): the next one's pre-poll will pass.
    pub answered: bool,
}

impl SmuFacts {
    pub fn answer(&self, msg: u16, arg: u32) -> Option<u32> {
        self.answers.iter().find(|a| a.0 == msg && a.1 == arg).map(|a| a.4)
    }
}

fn gone(e: PspError, byte: u32) -> SmuStop {
    match e {
        PspError::WriteRefused { byte } => SmuStop::Refused { byte },
        _ => SmuStop::Gone { byte },
    }
}

fn rd(hw: &mut dyn Hw, byte: u32) -> Result<u32, SmuStop> {
    psp::read(hw, byte).map_err(|e| gone(e, byte))
}

fn wr(hw: &mut dyn Hw, byte: u32, v: u32) -> Result<(), SmuStop> {
    if hw.reg_write(byte, v) { Ok(()) } else { Err(SmuStop::Refused { byte }) }
}

/// `__smu_msg_v1_poll_stat` (smu_cmn.c:294-309): the answer, or `None` when it stayed 0 for all of Linux's reads.
fn poll(hw: &mut dyn Hw, byte: u32) -> Result<Option<u32>, SmuStop> {
    match psp::wait_reads(hw, byte, ANSWERED, ANSWER_READS, "smu answer") {
        Ok(v) => Ok(Some(v)),
        Err(PspError::Timeout { .. }) => Ok(None),
        Err(e) => Err(gone(e, byte)),
    }
}

/// The firmware flags through the index pair, in Linux's form.
pub fn fw_flags(hw: &mut dyn Hw, r: &Regs14) -> Result<u32, SmuStop> {
    wr(hw, r.rsmu_index, SMN_MP1_FIRMWARE_FLAGS)?;
    // Linux's posting read of the index (amdgpu_reg_access.c:605).
    let _ = hw.reg_read(r.rsmu_index);
    hw.reg_read(r.rsmu_data).ok_or(SmuStop::Gone { byte: r.rsmu_data })
}

/// `smu_v14_0_check_fw_status`: the flags, and that they say the firmware runs.
pub fn check_fw_status(hw: &mut dyn Hw, r: &Regs14) -> Result<u32, SmuStop> {
    let flags = fw_flags(hw, r)?;
    if flags & MP1_FW_INTERRUPTS_ENABLED == 0 { Err(SmuStop::NotRunning { flags }) } else { Ok(flags) }
}

/// Where the mailbox stands between two messages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Line {
    /// The next message is the first since the firmware started: Linux sends it without looking at the answer
    /// register (`SMU_FW_INIT`, smu_cmn.c:508-511).
    pub first: bool,
    /// A write reached the mailbox.
    pub sent: bool,
    /// What went out last has its answer (whatever it was).
    pub answered: bool,
}

/// One message, Linux's way. `want`: Linux reads the argument register after an OK.
pub fn send(hw: &mut dyn Hw, r: &Regs14, line: &mut Line, msg: u16, arg: u32, want: bool) -> Result<Option<u32>, SmuStop> {
    if !line.first {
        match poll(hw, r.smu_answer)? {
            Some(v) if core14::decode(v).lets_send() => {}
            Some(v) => return Err(SmuStop::Hung { value: v }),
            None => return Err(SmuStop::Busy { last: 0 }),
        }
    }
    wr(hw, r.smu_answer, 0)?;
    (line.first, line.sent, line.answered) = (false, true, false);
    wr(hw, r.smu_argument, arg)?;
    wr(hw, r.smu_message, u32::from(msg))?;
    let code = poll(hw, r.smu_answer)?.ok_or(SmuStop::Timeout { msg })?;
    line.answered = true;
    if core14::decode(code) != Resp::Ok {
        return Err(SmuStop::Answer { msg, code });
    }
    if want { rd(hw, r.smu_argument).map(Some) } else { Ok(None) }
}

/// One question of the look: its answer code and the argument register after it. `sent` turns true once a write
/// reached the mailbox.
pub fn ask(hw: &mut dyn Hw, r: &Regs14, msg: u16, arg: u32, first: bool, sent: &mut bool) -> Result<(u32, u32), SmuStop> {
    let mut line = Line { first, ..Line::default() };
    let r = send(hw, r, &mut line, msg, arg, true);
    *sent |= line.sent;
    r.map(|v| (RESP_OK, v.unwrap_or(0)))
}

/// The whole read-out: flags first (a firmware that does not run gets no message), then `ASKED`.
pub fn read_out(hw: &mut dyn Hw, r: &Regs14, first: bool) -> (SmuFacts, Result<(), SmuStop>) {
    let mut f = SmuFacts::default();
    let mut line = Line { first, ..Line::default() };
    let res = (|| {
        f.flags = fw_flags(hw, r)?;
        if f.flags & MP1_FW_INTERRUPTS_ENABLED == 0 {
            return Err(SmuStop::NotRunning { flags: f.flags });
        }
        for (msg, arg, name) in ASKED.iter() {
            let out = send(hw, r, &mut line, *msg, *arg, true);
            (f.sent, f.answered) = (line.sent, line.answered);
            f.answers.push((*msg, *arg, name, RESP_OK, out?.unwrap_or(0)));
        }
        Ok(())
    })();
    (f, res)
}

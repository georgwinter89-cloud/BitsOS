//! SN-13 (wave 25): the display's SMU mailbox (VBIOSSMC: MP1_SMN_C2PMSG_67 message, _83 argument, _91 response)
//! for the one message Linux's DP audio path sends: `VBIOSSMC_MSG_UpdatePmeRestore` (0xD, argument 0). Linux 6.6
//! sends it when the first audio stream is enabled (`dce110_hw_sequencer.c:1100-1104`, after `az_enable`, before
//! the audio packets: "apply the PME w/a in order to wake AZ from D3") and again when it is disabled (`:1129-1136`,
//! after the packets are off), through `dcn314_smu_enable_pme_wa` (`clk_mgr/dcn314/dcn314_smu.c:298`). The
//! 27.10. capture of amdgpu's load on the Mini-PC shows it once (`C2PMSG_67 <- 0xD`, answered OK after ~450 us).
//! V2 never sent it (SN-5 ported V1's kit, which had no SMU step): one of the differences to Linux's audio on/off.
//!
//! The protocol is Linux's `dcn314_smu_send_msg_with_param` (`dcn314_smu.c:118-160`): wait while the response
//! reads BUSY (still BUSY: nothing sent); response <- BUSY, argument, message; wait for the answer; a FAILED answer
//! is acknowledged with OK. Every wait is the caller's (`more`): bounded, a clock of its choice (thread context,
//! Quiesce, the panic clock). Nothing here touches hardware.

use crate::dpaudio::RegError;

/// MP1 segment 0 on DCN 3.1.4's clock manager (`dcn314_smu.c` MP1_BASE__INST0_SEG0, dword 0x16000) plus the
/// MP 13.0.5 offsets (`mp_13_0_5_offset.h`): BAR5 byte offsets.
pub const MSG: u32 = (0x16000 + 0x283) * 4;
pub const ARG: u32 = (0x16000 + 0x293) * 4;
pub const RESP: u32 = (0x16000 + 0x29B) * 4;

/// `VBIOSSMC_MSG_UpdatePmeRestore` (`dcn314_smu.c:76`).
pub const UPDATE_PME_RESTORE: u32 = 0xD;

/// `VBIOSSMC_Status_BUSY`, `VBIOSSMC_Result_OK`, `VBIOSSMC_Result_Failed` (`dcn314_smu.c:89-91`).
pub const BUSY: u32 = 0;
pub const OK: u32 = 1;
pub const FAILED: u32 = 0xFF;

/// The mailbox registers as the display owner gives them (writes are not display writes: they change no picture
/// and are counted apart).
pub trait Mailbox {
    fn read(&mut self, byte: u32) -> Result<u32, RegError>;
    fn write(&mut self, byte: u32, v: u32) -> Result<(), RegError>;
}

/// What one message did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// Answered OK.
    Ok,
    /// The mailbox still answered BUSY before the message (a message of someone else unanswered): nothing sent.
    NotReady,
    /// Sent; no answer within the caller's bound.
    NoAnswer,
    /// Sent; answered with this code (FAILED acknowledged with OK, as Linux).
    Answer(u32),
    /// A register access failed (no engine, out of range).
    Access(RegError),
    /// Another sender held the mailbox and the caller must not wait (Quiesce): nothing sent.
    Busy,
}

impl Sent {
    pub fn name(self) -> &'static str {
        match self {
            Sent::Ok => "ok",
            Sent::NotReady => "not_ready",
            Sent::NoAnswer => "no_answer",
            Sent::Answer(_) => "refused",
            Sent::Access(_) => "access",
            Sent::Busy => "busy",
        }
    }
    /// A 4-bit code for lock-free notes (0 = nothing done).
    pub fn code(self) -> u32 {
        match self {
            Sent::Ok => 1,
            Sent::NotReady => 2,
            Sent::NoAnswer => 3,
            Sent::Answer(_) => 4,
            Sent::Access(_) => 5,
            Sent::Busy => 6,
        }
    }
    /// The name of a note's code (`none` for 0).
    pub fn code_name(c: u32) -> &'static str {
        match c {
            1 => "ok",
            2 => "not_ready",
            3 => "no_answer",
            4 => "refused",
            5 => "access",
            6 => "busy",
            _ => "none",
        }
    }
}

/// Reads the response until it is not BUSY or `more` says the time is up (`more` also waits between reads).
fn wait<M: Mailbox + ?Sized>(m: &mut M, more: &mut dyn FnMut() -> bool) -> Result<u32, RegError> {
    loop {
        let v = m.read(RESP)?;
        if v != BUSY {
            return Ok(v);
        }
        if !more() {
            return Ok(BUSY);
        }
    }
}

/// One message with its argument, Linux's protocol (see the module text).
pub fn send<M: Mailbox + ?Sized>(m: &mut M, msg: u32, arg: u32, more: &mut dyn FnMut() -> bool) -> Sent {
    match wait(m, more) {
        Ok(BUSY) => return Sent::NotReady,
        Ok(_) => {}
        Err(e) => return Sent::Access(e),
    }
    for (reg, v) in [(RESP, BUSY), (ARG, arg), (MSG, msg)] {
        if let Err(e) = m.write(reg, v) {
            return Sent::Access(e);
        }
    }
    match wait(m, more) {
        Ok(OK) => Sent::Ok,
        Ok(BUSY) => Sent::NoAnswer,
        Ok(v) => {
            if v == FAILED {
                let _ = m.write(RESP, OK);
            }
            Sent::Answer(v)
        }
        Err(e) => Sent::Access(e),
    }
}

/// `UpdatePmeRestore(0)`, the DP audio path's message.
pub fn update_pme_restore<M: Mailbox + ?Sized>(m: &mut M, more: &mut dyn FnMut() -> bool) -> Sent {
    send(m, UPDATE_PME_RESTORE, 0, more)
}

//! SN-13 (w25): the display SMU mailbox protocol (Linux's `dcn314_smu_send_msg_with_param`) on a simulated PMFW.

use crate::dcnsmu::{self, ARG, BUSY, FAILED, MSG, Mailbox, OK, RESP, Sent};
use crate::dpaudio::RegError;
use std::vec::Vec;

/// The PMFW as the 27.10. capture shows it: the response reads OK when idle; a message makes it BUSY until it
/// answers `answer` after `delay` reads (None: never answers).
struct Pmfw {
    resp: u32,
    arg: u32,
    log: Vec<(u32, u32)>,
    answer: Option<u32>,
    delay: u32,
    pending: Option<u32>,
    gone: bool,
}

impl Pmfw {
    fn new(answer: Option<u32>, delay: u32) -> Pmfw {
        Pmfw { resp: OK, arg: !0, log: Vec::new(), answer, delay, pending: None, gone: false }
    }
}

impl Mailbox for Pmfw {
    fn read(&mut self, byte: u32) -> Result<u32, RegError> {
        if self.gone {
            return Err(RegError::NotAttached);
        }
        if byte == RESP
            && let Some(n) = self.pending
        {
            if n == 0 {
                self.pending = None;
                if let Some(a) = self.answer {
                    self.resp = a;
                }
            } else {
                self.pending = Some(n - 1);
            }
        }
        Ok(match byte {
            RESP => self.resp,
            ARG => self.arg,
            _ => 0,
        })
    }
    fn write(&mut self, byte: u32, v: u32) -> Result<(), RegError> {
        if self.gone {
            return Err(RegError::NotAttached);
        }
        self.log.push((byte, v));
        match byte {
            RESP => self.resp = v,
            ARG => self.arg = v,
            MSG => self.pending = Some(self.delay),
            _ => {}
        }
        Ok(())
    }
}

/// A bound of `n` waits.
fn bound(n: u32) -> impl FnMut() -> bool {
    let mut left = n;
    move || {
        if left == 0 {
            return false;
        }
        left -= 1;
        true
    }
}

#[test]
fn offsets_are_the_captured_dwords() {
    // trace-load.txt.zst (27.10.): wreg 0x16293 (argument), 0x16283 (message), rreg 0x1629b (response).
    assert_eq!((MSG / 4, ARG / 4, RESP / 4), (0x16283, 0x16293, 0x1629B));
    assert_eq!(dcnsmu::UPDATE_PME_RESTORE, 0xD);
}

#[test]
fn linux_order_and_ok() {
    let mut p = Pmfw::new(Some(OK), 3);
    assert_eq!(dcnsmu::update_pme_restore(&mut p, &mut bound(10)), Sent::Ok);
    assert_eq!(p.log, [(RESP, BUSY), (ARG, 0), (MSG, 0xD)], "response cleared, argument, then the message");
}

#[test]
fn busy_mailbox_sends_nothing() {
    let mut p = Pmfw::new(Some(OK), 0);
    p.resp = BUSY;
    assert_eq!(dcnsmu::update_pme_restore(&mut p, &mut bound(5)), Sent::NotReady);
    assert!(p.log.is_empty());
}

#[test]
fn silent_pmfw_is_bounded() {
    let mut p = Pmfw::new(None, 0);
    let mut calls = 0;
    let r = dcnsmu::update_pme_restore(&mut p, &mut || {
        calls += 1;
        calls <= 7
    });
    assert_eq!(r, Sent::NoAnswer);
    assert_eq!(calls, 8, "the bound ends the wait");
    assert_eq!(p.log.len(), 3);
}

#[test]
fn failed_answer_is_acknowledged() {
    let mut p = Pmfw::new(Some(FAILED), 1);
    assert_eq!(dcnsmu::update_pme_restore(&mut p, &mut bound(10)), Sent::Answer(FAILED));
    assert_eq!(p.log.last(), Some(&(RESP, OK)), "Linux writes OK back after FAILED");
    // Another refusal code is only reported.
    let mut p = Pmfw::new(Some(0xFE), 0);
    assert_eq!(dcnsmu::update_pme_restore(&mut p, &mut bound(10)), Sent::Answer(0xFE));
    assert_eq!(p.log.len(), 3);
}

#[test]
fn a_previous_failure_does_not_block() {
    // Linux only warns when the first wait ends on something other than OK; not BUSY means it sends.
    let mut p = Pmfw::new(Some(OK), 0);
    p.resp = FAILED;
    assert_eq!(dcnsmu::update_pme_restore(&mut p, &mut bound(3)), Sent::Ok);
}

#[test]
fn gone_engine_is_an_access_error() {
    let mut p = Pmfw::new(Some(OK), 0);
    p.gone = true;
    assert_eq!(dcnsmu::update_pme_restore(&mut p, &mut bound(3)), Sent::Access(RegError::NotAttached));
    for c in 0..8 {
        let _ = Sent::code_name(c);
    }
    assert_eq!(Sent::code_name(Sent::Ok.code()), "ok");
    assert_eq!(Sent::code_name(Sent::Busy.code()), "busy");
}

//! GX-7e: the driver's PMFW mailbox of SMU 13.0.4 (MP1 C2PMSG_66 message, 82 argument, 90 answer), generic over
//! the device (`Hw`): the fakes in tests, `psplive::PspHw` on the Mini-PC.
//!
//! Linux's protocol (smu_cmn.c:227-236, :263-279, :350-391): before every message the answer register is polled
//! until it is non-zero (the previous message is done; 2 s, `__smu_cmn_poll_stat` :109); then the answer 0, the
//! argument, the message; a waited message polls the answer again and reads the argument register back. A
//! message sent without waiting (EnableGfxImu, smu_v13_0.c:2303-2309) is noticed only by the next pre-poll.
//!
//! Stricter than Linux: every answer but OK is an error (Linux sends on after 0xFF/0xFE/0xFD/0xFC and only stops
//! on "none" or an undefined code); and only the messages of `ALLOWED` ever reach PMFW. PMFW also drives the
//! display's clocks and power (DCN has its own mailbox 67/83/91 on the same firmware, dcn314_smu.c): a message
//! that changes clocks, Z-states, DF states or power limits is refused before anything is written (GX-D9).
//!
//! GX-7j3: the one writer of this mailbox is this module (`Smu` for the stages, `send_quiet` for the stop and the
//! Quiesce); the display's mailbox (67/83/91) belongs to DMCUB and the sound driver's VBIOSSMC messages.

use super::psp::Hw;
use crate::ipmod::smu13::{self, Addrs, msg, resp};
use alloc::vec::Vec;

/// The only messages BitsOS sends (checked against the 27.10. capture and smu_v13_0_4_ppsmc.h): two queries, the
/// IMU start, the GFXOFF veto, the mode-2 reset (recovery only, argument `RESET_MODE_2` only) and (GX-7j3) the
/// driver's unload, PrepareMp1ForUnload, which Linux sends from smu_v13_0_4_system_features_control on every
/// unload, shutdown and kexec that is not s0ix (smu_v13_0_4_ppt.c:224-233, amdgpu_smu.c:1621 smu_disable_dpms).
/// It concerns the GFX driver's side of PMFW only; the display's clocks keep their own mailbox. Never
/// AllowGfxOff, never a clock, Z-state (0x25), VCN/JPEG (never powered up, so nothing to power down) or table
/// message.
pub const ALLOWED: &[u32] = &[msg::GET_DRIVER_IF_VERSION, msg::GET_PMFW_VERSION, msg::ENABLE_GFX_IMU, msg::DISALLOW_GFXOFF, msg::GFX_DEVICE_DRIVER_RESET, msg::PREPARE_MP1_FOR_UNLOAD];

/// Whether `(id, arg)` may be sent.
pub fn allowed(id: u32, arg: u32) -> bool {
    ALLOWED.contains(&id) && (id != msg::GFX_DEVICE_DRIVER_RESET || arg == smu13::RESET_MODE_2)
}

/// Linux's waits: 2 s for the pre-poll and for an answer (`usec_timeout * 20` rounds of 1 us, smu_cmn.c:112).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmuTimeouts {
    pub pre_poll_us: u64,
    pub answer_us: u64,
}

impl SmuTimeouts {
    pub const LINUX: SmuTimeouts = SmuTimeouts { pre_poll_us: 2_000_000, answer_us: 2_000_000 };
    /// The Quiesce hook's bound (interrupts off, other CPUs parked): the PSP's Quiesce waits (100 ms) for each.
    pub const QUIESCE: SmuTimeouts = SmuTimeouts { pre_poll_us: 100_000, answer_us: 100_000 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmuError {
    /// Not on `ALLOWED`: nothing written.
    Forbidden { id: u32, arg: u32 },
    /// The pre-poll found the previous message (`last`) still unanswered: PMFW is stuck (or a stale start).
    PrePoll { last: u32 },
    /// A waited message got no answer in time.
    Timeout { id: u32 },
    /// PMFW answered `code` (not OK).
    Answer { id: u32, code: u32 },
    /// A register did not read (refused or outside the BAR).
    Read { byte: u32 },
    /// The owner refused a write.
    Write { byte: u32 },
    /// FIRMWARE_FLAGS bit 0 is clear: PMFW does not run ("SMC is not ready", amdgpu_smu.c:1441).
    NotRunning { flags: u32 },
}

impl SmuError {
    /// Linux's errno for the same failure, positive like `PspError::errno` (__smu_cmn_reg2errno,
    /// smu_cmn.c:182-222; check_fw_status -EIO).
    pub fn errno(&self) -> i32 {
        match self {
            SmuError::Forbidden { .. } => 1, // EPERM
            SmuError::PrePoll { .. } | SmuError::Timeout { .. } => 62, // ETIME
            SmuError::Answer { code, .. } => match *code {
                resp::FAILED => 5,    // EIO
                resp::UNKNOWN => 95,  // EOPNOTSUPP
                resp::PREREQ => 22,   // EINVAL
                resp::BUSY => 16,     // EBUSY
                _ => 121,             // EREMOTEIO
            },
            SmuError::Read { .. } | SmuError::Write { .. } | SmuError::NotRunning { .. } => 5,
        }
    }

    /// PMFW may still work on a message of ours or never answers again: a reboot, not a retry.
    pub fn needs_reboot(&self) -> bool {
        matches!(self, SmuError::PrePoll { .. } | SmuError::Timeout { .. })
    }
}

/// One message as sent: id, argument, answer code (`None`: not waited for), argument register after the answer,
/// the time it was written and the time the answer was seen (us).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sent {
    pub id: u32,
    pub arg: u32,
    pub code: Option<u32>,
    pub out: Option<u32>,
    pub at_us: u64,
    pub answered_us: Option<u64>,
}

/// The poll step: 10 us for the first millisecond (Linux polls every 1 us; the capture's answers took 0.1 to
/// 30 ms), then 100 us, which sleeps on the live device (R5).
pub fn step_us(elapsed_us: u64) -> u64 {
    if elapsed_us < 1_000 { 10 } else { 100 }
}

/// Polls `byte` until `done(value)` or `limit_us`; bounded by time and by a try count derived from it (B4).
/// `Ok(Some(v))`: done; `Ok(None)`: timed out (the last value read in `last`).
pub fn poll(hw: &mut dyn Hw, byte: u32, limit_us: u64, last: &mut u32, done: impl Fn(u32) -> bool) -> Result<Option<u32>, SmuError> {
    let start = hw.now_us();
    let tries = limit_us / 10 + 2;
    for _ in 0..tries {
        let v = hw.reg_read(byte).ok_or(SmuError::Read { byte })?;
        *last = v;
        if done(v) {
            return Ok(Some(v));
        }
        let el = hw.now_us().saturating_sub(start);
        if el >= limit_us {
            break;
        }
        hw.delay_us(step_us(el).min(limit_us - el).max(1));
    }
    Ok(None)
}

/// The driver's mailbox: addresses, waits, and every message sent through it (oldest first; one run sends a
/// handful, so the list stays short).
pub struct Smu {
    pub a: Addrs,
    pub t: SmuTimeouts,
    pub sent: Vec<Sent>,
    /// The message sent without waiting whose answer no pre-poll has seen yet.
    pending: Option<usize>,
    /// No message sent by this client yet (the first pre-poll may meet PMFW fresh out of reset).
    first: bool,
}

impl Smu {
    pub fn new(a: Addrs, t: SmuTimeouts) -> Smu {
        Smu { a, t, sent: Vec::new(), pending: None, first: true }
    }

    /// FIRMWARE_FLAGS through the SMN window (PCIE_INDEX2 = its address, then PCIE_DATA2), as
    /// smu_v13_0_check_fw_status reads it; bit 0 must be set.
    pub fn fw_flags(&mut self, hw: &mut dyn Hw) -> Result<u32, SmuError> {
        if !hw.reg_write(self.a.smn_index, smu13::SMN_FIRMWARE_FLAGS) {
            return Err(SmuError::Write { byte: self.a.smn_index });
        }
        let v = hw.reg_read(self.a.smn_data).ok_or(SmuError::Read { byte: self.a.smn_data })?;
        if v == u32::MAX || v & smu13::FIRMWARE_FLAGS_UP == 0 {
            klog::event!(Error, "gx", "smu.not_running", flags = klog::Hex(u64::from(v)));
            return Err(SmuError::NotRunning { flags: v });
        }
        Ok(v)
    }

    /// Waits until the previous message is answered (Linux's pre-poll). The answer of a message sent without
    /// waiting is recorded on it here; an answer other than OK to it is an error (Linux goes on).
    ///
    /// The first pre-poll: answer 0 with message 0 is PMFW out of reset with no message ever sent (newer Linux
    /// ignores the first answer register value for this reason; the capture's kernel did not pre-poll its first
    /// message; GX-7b's classifier calls 0/0 cold): it is not waited for. Answer 0 with a message id is a message
    /// in flight (a crash left it) and is waited for like any other.
    pub fn pre_poll(&mut self, hw: &mut dyn Hw) -> Result<u32, SmuError> {
        if core::mem::take(&mut self.first) {
            let r = hw.reg_read(self.a.resp).ok_or(SmuError::Read { byte: self.a.resp })?;
            let m = hw.reg_read(self.a.msg).ok_or(SmuError::Read { byte: self.a.msg })?;
            if r == resp::NONE && m == 0 {
                klog::event!(Info, "gx", "smu.fresh"; "answer and message 0: PMFW out of reset, first pre-poll not waited for");
                return Ok(resp::NONE);
            }
        }
        let mut last = 0;
        let got = poll(hw, self.a.resp, self.t.pre_poll_us, &mut last, |v| v != resp::NONE)?;
        let Some(code) = got else {
            let m = hw.reg_read(self.a.msg).unwrap_or(u32::MAX);
            klog::event!(Error, "gx", "smu.pre_poll_timeout", last = klog::Hex(u64::from(m)), waited_us = self.t.pre_poll_us;
                "the previous message was never answered: PMFW stuck");
            return Err(SmuError::PrePoll { last: m });
        };
        if let Some(i) = self.pending.take() {
            let now = hw.now_us();
            let out = hw.reg_read(self.a.arg);
            if let Some(s) = self.sent.get_mut(i) {
                s.code = Some(code);
                s.out = out;
                s.answered_us = Some(now);
                if code != resp::OK {
                    klog::event!(Error, "gx", "smu.answer", id = klog::Hex(u64::from(s.id)), code = klog::Hex(u64::from(code)));
                    return Err(SmuError::Answer { id: s.id, code });
                }
            }
        }
        Ok(code)
    }

    /// One message: pre-poll, answer 0, argument, message (Linux's order); with `wait` the answer is polled and
    /// must be OK, `Ok(argument register)`; without, `Ok(arg)` at once.
    pub fn send(&mut self, hw: &mut dyn Hw, id: u32, arg: u32, wait: bool) -> Result<u32, SmuError> {
        if !allowed(id, arg) {
            klog::event!(Error, "gx", "smu.forbidden", id = klog::Hex(u64::from(id)), arg = klog::Hex(u64::from(arg)), name = smu13::msg_name(id);
                "not on the list of messages BitsOS sends (clocks, Z-states and display power stay PMFW's)");
            return Err(SmuError::Forbidden { id, arg });
        }
        self.pre_poll(hw)?;
        for (at, v) in [(self.a.resp, resp::NONE), (self.a.arg, arg), (self.a.msg, id)] {
            if !hw.reg_write(at, v) {
                return Err(SmuError::Write { byte: at });
            }
        }
        let at_us = hw.now_us();
        self.sent.push(Sent { id, arg, code: None, out: None, at_us, answered_us: None });
        let i = self.sent.len() - 1;
        klog::event!(Info, "gx", "smu.sent", id = klog::Hex(u64::from(id)), arg = klog::Hex(u64::from(arg)), name = smu13::msg_name(id), wait = wait);
        if !wait {
            self.pending = Some(i);
            return Ok(arg);
        }
        let mut last = 0;
        let Some(code) = poll(hw, self.a.resp, self.t.answer_us, &mut last, |v| v != resp::NONE)? else {
            klog::event!(Error, "gx", "smu.timeout", id = klog::Hex(u64::from(id)), waited_us = self.t.answer_us);
            return Err(SmuError::Timeout { id });
        };
        let now = hw.now_us();
        let out = hw.reg_read(self.a.arg).ok_or(SmuError::Read { byte: self.a.arg })?;
        let s = &mut self.sent[i];
        s.code = Some(code);
        s.out = Some(out);
        s.answered_us = Some(now);
        if code != resp::OK {
            klog::event!(Error, "gx", "smu.answer", id = klog::Hex(u64::from(id)), code = klog::Hex(u64::from(code)), meaning = smu13::resp_name(code));
            return Err(SmuError::Answer { id, code });
        }
        Ok(out)
    }

    /// Whether a message sent without waiting is still unanswered as far as this client knows.
    pub fn pending(&self) -> Option<Sent> {
        self.pending.and_then(|i| self.sent.get(i).copied())
    }
}

/// GX-7j3: one waited message without a client: no allocation, no log, no lock (the stop's unload, and the Quiesce
/// hook with interrupts off). Linux's handshake as `Smu::send`: the pre-poll (answer 0 with message 0 is PMFW out
/// of reset and goes on), answer 0, argument, message, the answer polled; `Ok(argument register)` on OK.
pub fn send_quiet(hw: &mut dyn Hw, a: &Addrs, t: SmuTimeouts, id: u32, arg: u32) -> Result<u32, SmuError> {
    if !allowed(id, arg) {
        return Err(SmuError::Forbidden { id, arg });
    }
    let r = hw.reg_read(a.resp).ok_or(SmuError::Read { byte: a.resp })?;
    let m = hw.reg_read(a.msg).ok_or(SmuError::Read { byte: a.msg })?;
    if !(r == resp::NONE && m == 0) {
        let mut last = 0;
        if poll(hw, a.resp, t.pre_poll_us, &mut last, |v| v != resp::NONE)?.is_none() {
            return Err(SmuError::PrePoll { last: m });
        }
    }
    for (at, v) in [(a.resp, resp::NONE), (a.arg, arg), (a.msg, id)] {
        if !hw.reg_write(at, v) {
            return Err(SmuError::Write { byte: at });
        }
    }
    let mut last = 0;
    let code = poll(hw, a.resp, t.answer_us, &mut last, |v| v != resp::NONE)?.ok_or(SmuError::Timeout { id })?;
    let out = hw.reg_read(a.arg).ok_or(SmuError::Read { byte: a.arg })?;
    if code != resp::OK {
        return Err(SmuError::Answer { id, code });
    }
    Ok(out)
}

/// GX-7j3: the driver's mailbox as a run finds it, before anything is written (`settle`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailboxState {
    /// The last message is answered (or none was ever sent).
    Free { code: u32, msg: u32 },
    /// A message was found unanswered (answer 0, its id in C2PMSG_66) and PMFW answered it within the wait.
    LateAnswer { msg: u32, code: u32, after_us: u64 },
    /// It stayed unanswered for the whole wait: PMFW is stuck.
    Stuck { msg: u32, waited_us: u64 },
}

/// GX-7j3: reads the mailbox (no write); a message left in flight (answer 0, message id set: a crash, or a stop
/// that left PMFW busy) is waited for with Linux's pre-poll bound. Linux's own reaction to a busy pre-poll is
/// to send nothing and fail (smu_cmn.c:368-373, -ETIME); a stuck PMFW is cleared only by a reset, and mode 2 is
/// refused on 13.0.4, so `Stuck` ends the run before the PSP is touched.
pub fn settle(hw: &mut dyn Hw, a: &Addrs, t: SmuTimeouts) -> Result<MailboxState, SmuError> {
    let code = hw.reg_read(a.resp).ok_or(SmuError::Read { byte: a.resp })?;
    let m = hw.reg_read(a.msg).ok_or(SmuError::Read { byte: a.msg })?;
    if code != resp::NONE || m == 0 {
        return Ok(MailboxState::Free { code, msg: m });
    }
    let t0 = hw.now_us();
    let mut last = 0;
    Ok(match poll(hw, a.resp, t.pre_poll_us, &mut last, |v| v != resp::NONE)? {
        Some(code) => MailboxState::LateAnswer { msg: m, code, after_us: hw.now_us().saturating_sub(t0) },
        None => MailboxState::Stuck { msg: m, waited_us: t.pre_poll_us },
    })
}

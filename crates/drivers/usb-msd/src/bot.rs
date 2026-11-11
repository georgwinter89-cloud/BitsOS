//! USB Mass Storage Bulk-Only Transport 1.0: command block wrapper out, data, command status wrapper in, and
//! reset recovery when that fails. The rules follow Linux `drivers/usb/storage/transport.c`
//! (`usb_stor_Bulk_transport` linux-6.6/drivers/usb/storage/transport.c:1115-1331, `interpret_urb_result` :256,
//! `usb_stor_reset_common` :1347) and V1 `usb/msdbot.asm` / `usb/msdreset.asm`:
//!
//! - data that came before a STALL counts; the stalled endpoint is cleared and the CSW read (BOT 6.7.2);
//! - an empty packet where the CSW should be is read once more (devices that add a zero-length packet after
//!   data of whole packets), a stalled CSW is cleared and read once more (BOT 5.3.3);
//! - a 13-byte "USBS" answer with this command's tag in the data phase of an IN command means the device skipped
//!   the data phase (Linux "Device skipped data phase");
//! - a CSW with another tag, a status above 2, a learnt signature that changes, a phase error (status 2), a
//!   short CSW, a failed CBW or data phase other than a STALL, a timeout: transport error -> reset recovery
//!   (Bulk-Only Mass Storage Reset, then CLEAR_FEATURE(HALT) on bulk IN and bulk OUT, each with the host side
//!   back to toggle 0) before the next command (BOT 5.3.4);
//! - dCSWDataResidue: clamped to the length asked; a device whose INQUIRY (36 bytes) or READ CAPACITY (8 bytes)
//!   passes whole with a residue gets its residues ignored from then on (Linux `US_FL_IGNORE_RESIDUE` heuristic);
//! - an unplugged device ends a command with `Gone` and never gets a reset.

use crate::escalate::{Escalation, Step};
use crate::link::{data_timeout_ns, Cmd, Dir, Link, Ran, CBW_TIMEOUT_NS, CSW_TIMEOUT_NS};
use crate::scsi::{self, op, Cdb};
use alloc::string::String;
use alloc::vec::Vec;
use usb_core::{Buffer, Request, UsbError, XferResult};

pub const CBW_SIGNATURE: [u8; 4] = *b"USBC";
pub const CSW_SIGNATURE: [u8; 4] = *b"USBS";
/// How long the device gets after a Bulk-Only reset before its endpoints are cleared. Linux waits up to 6 s
/// (transport.c:1371, a conservative margin for slow bridges); V1 did not wait at all and never needed to.
pub const RESET_SETTLE_NS: u64 = 100_000_000;
/// TEST UNIT READY after a reset: its status phase's time (with the CBW's 5 s: Linux's 10 s for that command).
const ANSWER_CSW_NS: u64 = 5_000_000_000;

klog::counter!(COMMANDS, "usb.msd.commands");
klog::counter!(TRANSPORT_ERRORS, "usb.msd.transport_errors");
klog::counter!(PHASE_ERRORS, "usb.msd.phase_errors");
klog::counter!(RESETS, "usb.msd.resets");
klog::counter!(RESET_FAILURES, "usb.msd.reset_failures");
klog::counter!(PORT_RESETS, "usb.msd.port_resets");
klog::counter!(PORT_RESET_FAILURES, "usb.msd.port_reset_failures");
klog::counter!(GAVE_UP, "usb.msd.gave_up");
klog::counter!(STALLS_CLEARED, "usb.msd.stalls_cleared");
klog::counter!(CSW_RETRIES, "usb.msd.csw_retries");
klog::counter!(SKIPPED_DATA, "usb.msd.skipped_data_phases");

/// A command block wrapper (BOT 5.1).
pub fn cbw(tag: u32, len: u32, dir: Dir, lun: u8, cdb: &Cdb) -> [u8; 31] {
    let mut b = [0u8; 31];
    b[0..4].copy_from_slice(&CBW_SIGNATURE);
    b[4..8].copy_from_slice(&tag.to_le_bytes());
    b[8..12].copy_from_slice(&len.to_le_bytes());
    b[12] = if dir == Dir::In { 0x80 } else { 0 };
    b[13] = lun & 0x0F;
    b[14] = cdb.len.min(16);
    b[15..31].copy_from_slice(&cdb.bytes);
    b
}

pub fn cbw_tag(cbw: &[u8; 31]) -> u32 {
    u32::from_le_bytes([cbw[4], cbw[5], cbw[6], cbw[7]])
}

/// A command status wrapper (BOT 5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csw {
    pub signature: [u8; 4],
    pub tag: u32,
    pub residue: u32,
    pub status: u8,
}

impl Csw {
    pub fn parse(b: &[u8; 13]) -> Csw {
        Csw {
            signature: [b[0], b[1], b[2], b[3]],
            tag: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            residue: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            status: b[12],
        }
    }
}

/// A command whose every stage ran whole without error and whose CSW passed with residue 0 (or the device's
/// residues are ignored): the fast path needs nothing more. `Link::run` stops after the first that is not.
pub fn clean(c: &Cmd, ran: &Ran) -> bool {
    if ran.cbw.error.is_some() || ran.cbw.actual != 31 {
        return false;
    }
    if let Some(req) = &c.data {
        match ran.data {
            Some(d) if d.error.is_none() && d.actual == req.len => {}
            _ => return false,
        }
    }
    let Some((r, b)) = ran.csw else { return false };
    if r.error.is_some() || r.actual != 13 {
        return false;
    }
    let s = Csw::parse(&b);
    s.signature == CSW_SIGNATURE && s.tag == cbw_tag(&c.cbw) && s.status == 0 && (s.residue == 0 || c.residue_ignored)
}

/// One SCSI command for the transport.
#[derive(Clone)]
pub struct Op {
    pub lun: u8,
    pub cdb: Cdb,
    pub dir: Dir,
    /// The data phase (`None` or a zero length for commands without data).
    pub data: Option<Request>,
}

impl Op {
    pub fn new(lun: u8, cdb: Cdb) -> Op {
        Op { lun, cdb, dir: Dir::None, data: None }
    }
    pub fn read(lun: u8, cdb: Cdb, req: Request) -> Op {
        Op { lun, cdb, dir: Dir::In, data: Some(req) }
    }
    pub fn write(lun: u8, cdb: Cdb, req: Request) -> Op {
        Op { lun, cdb, dir: Dir::Out, data: Some(req) }
    }
    fn len(&self) -> usize {
        self.data.as_ref().map_or(0, |r| r.len)
    }
}

/// How a command ended at the transport level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// bCSWStatus 0 (passed) or 1 (failed: the caller asks REQUEST SENSE).
    pub passed: bool,
    /// Bytes the data phase moved.
    pub moved: usize,
    /// Bytes of the data phase that did not happen (the larger of what did not move and the device's residue).
    pub residue: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BotError {
    /// The device was unplugged.
    Gone,
    /// The transport failed; reset recovery was done (or tried) before this returned.
    Transport(&'static str),
    /// Reset recovery that was due failed; no command was sent.
    ResetFailed,
}

impl BotError {
    pub fn as_str(&self) -> &'static str {
        match self {
            BotError::Gone => "device gone",
            BotError::Transport(why) => why,
            BotError::ResetFailed => "reset recovery failed",
        }
    }
}

/// Counters of one device (the global ones are klog counters).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BotStats {
    pub commands: u64,
    pub transport_errors: u64,
    pub phase_errors: u64,
    pub resets: u64,
    pub reset_failures: u64,
    pub port_resets: u64,
    pub port_reset_failures: u64,
    pub stalls_cleared: u64,
    pub csw_retries: u64,
    pub skipped_data: u64,
}

pub struct Bot<L: Link> {
    pub link: L,
    tag: u32,
    /// The first CSW's signature (Linux keeps it and only refuses a change; some devices do not say "USBS").
    signature: Option<[u8; 4]>,
    pub ignore_residue: bool,
    needs_reset: bool,
    small: Option<Buffer>,
    /// Bumped when the link says its buffers may be held by the controller (`Link::take_poisoned`).
    epoch: u64,
    /// How far up the ladder of recoveries this device is (`escalate`).
    esc: Escalation,
    /// A port reset succeeded and the request under way has not heard of it yet (`take_port_reset`).
    port_reset_seen: bool,
    /// A port reset succeeded and no command passed since: a UNIT ATTENTION now is the reset's (`just_reset`).
    reset_ua: bool,
    pub stats: BotStats,
}

impl<L: Link> Bot<L> {
    pub fn new(link: L) -> Bot<L> {
        Bot { link, tag: 0, signature: None, ignore_residue: false, needs_reset: false, small: None, epoch: 0, esc: Escalation::default(), port_reset_seen: false, reset_ua: false, stats: BotStats::default() }
    }

    /// Changes when buffers handed to the link before may not be used again.
    pub fn epoch(&mut self) -> u64 {
        if self.link.take_poisoned() {
            self.small = None;
            self.epoch += 1;
        }
        self.epoch
    }

    /// A reset recovery is due before the next command (an earlier one failed).
    pub fn needs_reset(&self) -> bool {
        self.needs_reset
    }

    /// Recoveries are under way, not at their end, and the device has not answered yet: a transport error now is
    /// worth another attempt. A device that answers after its reset and still fails this one command has a problem
    /// with this command (a sector its flash hangs on): the command's own attempts are the bound.
    pub fn recovering(&self) -> bool {
        self.esc.climbing()
    }

    /// True once after a port reset: the request's attempts start over (the device was set up anew and will
    /// answer with a UNIT ATTENTION first; the ladder bounds how often this comes).
    pub fn take_port_reset(&mut self) -> bool {
        core::mem::take(&mut self.port_reset_seen)
    }

    /// A port reset was done and no command has passed since.
    pub fn just_reset(&self) -> bool {
        self.reset_ua
    }

    /// The device is not known to be well: reset recovery is due or something was tried since the last command
    /// that passed.
    pub fn unhealthy(&self) -> bool {
        self.needs_reset || self.esc.dirty()
    }

    /// A command passed. Only one that reached the medium proves the device well; only one that is not itself about
    /// the attention ends the reset's aftermath.
    fn passed(&mut self, o: &Op) {
        if op::reaches_medium(o.cdb.opcode()) {
            self.esc.clean();
        }
        self.reset_ua &= o.cdb.opcode() == op::REQUEST_SENSE;
    }

    fn name(&self) -> String {
        self.link.name()
    }

    /// One command.
    pub fn command(&mut self, op: Op) -> Result<Outcome, BotError> {
        self.run(core::slice::from_ref(&op)).pop().unwrap_or(Err(BotError::Transport("command not run")))
    }

    /// Runs `ops` one after the other; every result but the last is a clean pass, the last may be anything (a
    /// failed command, a short one, a transport error: the run stops there). One entry per command that ran.
    pub fn run(&mut self, ops: &[Op]) -> Vec<Result<Outcome, BotError>> {
        let mut out = Vec::new();
        if ops.is_empty() {
            return out;
        }
        if self.link.gone() {
            out.push(Err(BotError::Gone));
            return out;
        }
        if self.needs_reset
            && let Err(e) = self.reset_recovery()
        {
            out.push(Err(e));
            return out;
        }
        let cmds: Vec<Cmd> = ops
            .iter()
            .map(|o| {
                self.tag = self.tag.wrapping_add(1);
                let len = o.len();
                let dir = if len == 0 { Dir::None } else { o.dir };
                Cmd {
                    cbw: cbw(self.tag, len as u32, dir, o.lun, &o.cdb),
                    dir,
                    data: o.data.clone().filter(|r| r.len > 0),
                    data_timeout_ns: data_timeout_ns(len),
                    residue_ignored: self.ignore_residue,
                }
            })
            .collect();
        let ran = self.link.run(&cmds);
        self.stats.commands += ran.len() as u64;
        COMMANDS.add(ran.len() as u64);
        if ran.is_empty() {
            out.push(self.transport("no stage ran"));
            return out;
        }
        for (i, r) in ran.iter().enumerate().take(cmds.len()) {
            let c = &cmds[i];
            if clean(c, r) {
                self.passed(&ops[i]);
                self.signature.get_or_insert(CSW_SIGNATURE);
                out.push(Ok(Outcome { passed: true, moved: c.data_len(), residue: 0 }));
            } else {
                // The link stops after the first command that is not clean; anything it reported after one is
                // ignored (the caller sends those commands again).
                out.push(self.finish(c, &ops[i], r));
                break;
            }
        }
        out
    }

    fn finish(&mut self, c: &Cmd, op: &Op, ran: &Ran) -> Result<Outcome, BotError> {
        if let Some(e) = ran.cbw.error {
            return self.fail(e, "CBW failed");
        }
        if ran.cbw.actual != 31 {
            return self.transport("CBW went out short");
        }
        let tag = cbw_tag(&c.cbw);
        let len = c.data_len();
        let mut moved = 0usize;
        let mut status: Option<[u8; 13]> = None;
        let mut data_clean = true;
        if let Some(req) = &c.data {
            let Some(d) = ran.data else { return self.transport("data phase not started") };
            moved = d.actual.min(len);
            data_clean = d.error.is_none() && moved == len;
            match d.error {
                None => {
                    if c.dir == Dir::In && moved == 13 && len != 13 {
                        let mut b = [0u8; 13];
                        if req.buf.read(req.offset, &mut b) {
                            let s = Csw::parse(&b);
                            let sig_ok = s.signature == self.signature.unwrap_or(CSW_SIGNATURE);
                            if sig_ok && s.tag == tag {
                                self.stats.skipped_data += 1;
                                SKIPPED_DATA.inc();
                                klog::event!(Debug, "usb", "msd.skipped_data_phase", device = self.name(), tag = tag;
                                    "the device sent its CSW in place of the data");
                                moved = 0;
                                status = Some(b);
                            }
                        }
                    }
                }
                Some(UsbError::Stall) => {
                    if let Err(e) = self.link.clear_halt(c.dir == Dir::In) {
                        return self.fail(e, "clearing a stalled data phase failed");
                    }
                    self.stats.stalls_cleared += 1;
                    STALLS_CLEARED.inc();
                }
                Some(e) => return self.fail(e, "data phase failed"),
            }
        }
        let b = match status {
            Some(b) => b,
            None => {
                let first = if data_clean { ran.csw } else { None };
                self.read_csw(first)?
            }
        };
        self.check(op, tag, &b, len, moved)
    }

    fn small(&mut self) -> Result<Buffer, UsbError> {
        self.epoch();
        if let Some(b) = &self.small {
            return Ok(b.clone());
        }
        let b = self.link.alloc(64)?;
        self.small = Some(b.clone());
        Ok(b)
    }

    fn csw_once(&mut self) -> (XferResult, [u8; 13]) {
        let buf = match self.small() {
            Ok(b) => b,
            Err(e) => return (XferResult::failed(e, 0), [0; 13]),
        };
        let r = self.link.bulk(true, Request::new(buf.clone(), 32, 13), CSW_TIMEOUT_NS);
        let mut b = [0u8; 13];
        let _ = buf.read(32, &mut b[..r.actual.min(13)]);
        (r, b)
    }

    /// The status phase with Linux's two second chances: an empty packet is read again, a STALL is cleared and
    /// read again (each once).
    fn read_csw(&mut self, first: Option<(XferResult, [u8; 13])>) -> Result<[u8; 13], BotError> {
        let mut attempt = match first {
            Some(a) => a,
            None => self.csw_once(),
        };
        let (mut zero_again, mut stall_again) = (false, false);
        loop {
            let (r, b) = attempt;
            match r.error {
                None if r.actual == 13 => return Ok(b),
                None if r.actual == 0 && !zero_again => {
                    zero_again = true;
                    self.stats.csw_retries += 1;
                    CSW_RETRIES.inc();
                    klog::event!(Debug, "usb", "msd.csw_retry", device = self.name(), why = "zero-length CSW");
                }
                None => return self.transport("CSW short"),
                Some(UsbError::Stall) if !stall_again => {
                    stall_again = true;
                    if let Err(e) = self.link.clear_halt(true) {
                        return self.fail(e, "clearing a stalled CSW failed");
                    }
                    self.stats.stalls_cleared += 1;
                    self.stats.csw_retries += 1;
                    STALLS_CLEARED.inc();
                    CSW_RETRIES.inc();
                    klog::event!(Debug, "usb", "msd.csw_retry", device = self.name(), why = "stalled CSW");
                }
                Some(e) => return self.fail(e, "CSW failed"),
            }
            attempt = self.csw_once();
        }
    }

    fn check(&mut self, op: &Op, tag: u32, b: &[u8; 13], len: usize, moved: usize) -> Result<Outcome, BotError> {
        let s = Csw::parse(b);
        if s.tag != tag {
            return self.transport("CSW of another command");
        }
        if s.status > 2 {
            return self.transport("CSW status above 2");
        }
        match self.signature {
            None => {
                if s.signature != CSW_SIGNATURE {
                    klog::event!(Info, "usb", "msd.csw_signature", device = self.name(),
                        signature = klog::Hex(u32::from_le_bytes(s.signature) as u64); "learnt a non-standard CSW signature");
                }
                self.signature = Some(s.signature);
            }
            Some(sig) if sig != s.signature => return self.transport("CSW signature changed"),
            Some(_) => {}
        }
        if s.status == 2 {
            self.stats.phase_errors += 1;
            PHASE_ERRORS.inc();
            return self.transport("phase error");
        }
        let mut residue = s.residue;
        if residue != 0 && !self.ignore_residue {
            let whole = s.status == 0 && moved == len;
            let probe = (op.cdb.opcode() == op::INQUIRY && len == 36) || (op.cdb.opcode() == op::READ_CAPACITY_10 && len == 8);
            if whole && probe {
                self.ignore_residue = true;
                klog::event!(Info, "usb", "msd.ignore_residue", device = self.name(), residue = residue,
                    opcode = klog::Hex(op.cdb.opcode() as u64); "bogus residue on a whole answer: residues ignored from now on");
            } else {
                residue = residue.min(len as u32);
            }
        }
        if self.ignore_residue {
            residue = 0;
        }
        let residue = residue.max((len - moved) as u32);
        if s.status == 0 {
            self.passed(op);
        }
        Ok(Outcome { passed: s.status == 0, moved, residue })
    }

    fn fail<T>(&mut self, e: UsbError, why: &'static str) -> Result<T, BotError> {
        if e == UsbError::Gone || self.link.gone() {
            return Err(BotError::Gone);
        }
        klog::event!(Debug, "usb", "msd.transport_detail", device = self.name(), why = why, error = e.as_str());
        self.transport(why)
    }

    fn transport<T>(&mut self, why: &'static str) -> Result<T, BotError> {
        if self.link.gone() {
            return Err(BotError::Gone);
        }
        self.stats.transport_errors += 1;
        TRANSPORT_ERRORS.inc();
        klog::event!(Warn, "usb", "msd.transport_error", device = self.name(), why = why; "reset recovery follows");
        match self.reset_recovery() {
            Err(BotError::Gone) => Err(BotError::Gone),
            _ => Err(BotError::Transport(why)),
        }
    }

    /// What a device gets that stopped answering (`escalate`): the next rung up. A rung counts only if the device
    /// answers after it (`answers`), else it is on up at once. Stays due when none worked (`Bot::run` tries again
    /// before the next command). Whatever is queued is cancelled first; the data of a request survives: it goes again
    /// after this (`disk.rs`).
    pub fn reset_recovery(&mut self) -> Result<(), BotError> {
        self.needs_reset = true;
        self.stats.resets += 1;
        RESETS.inc();
        let answered_before = self.esc.answering();
        loop {
            match self.esc.next(self.link.now_ns()) {
                Step::BotReset => match self.bot_reset() {
                    Ok(()) if self.answers() => return self.recovered(),
                    Ok(()) if self.link.gone() => return Err(BotError::Gone),
                    Ok(()) => klog::event!(Warn, "usb", "msd.reset_no_answer", device = self.name(); "Bulk-Only reset done, the device does not answer; a port reset follows"),
                    Err(e) if e == UsbError::Gone || self.link.gone() => return Err(BotError::Gone),
                    Err(e) => {
                        self.stats.reset_failures += 1;
                        RESET_FAILURES.inc();
                        klog::event!(Warn, "usb", "msd.reset_failed", device = self.name(), error = e.as_str(); "Bulk-Only reset failed; a port reset follows");
                    }
                },
                Step::PortReset { pause_ns, power, wait_ns } => {
                    self.link.abort_all();
                    if pause_ns > 0 {
                        self.link.sleep_ns(pause_ns);
                    }
                    match self.port_reset_now(power, wait_ns) {
                        Ok(()) => {
                            // A device that had answered before this reset gets no new attempts for its command.
                            self.port_reset_seen = !answered_before;
                            if self.answers() {
                                return self.recovered();
                            }
                            if self.link.gone() {
                                return Err(BotError::Gone);
                            }
                            klog::event!(Warn, "usb", "msd.reset_no_answer", device = self.name(), power = power; "port reset done, the device does not answer");
                        }
                        Err(e) if e == UsbError::Gone || self.link.gone() => return Err(BotError::Gone),
                        Err(UsbError::Invalid(why)) => {
                            klog::event!(Warn, "usb", "msd.port_reset_impossible", device = self.name(), why = why);
                            self.esc.no_port_reset();
                        }
                        Err(e) => {
                            self.stats.port_reset_failures += 1;
                            PORT_RESET_FAILURES.inc();
                            klog::event!(Warn, "usb", "msd.port_reset_failed", device = self.name(), error = e.as_str());
                        }
                    }
                }
                Step::GiveUp => return Err(self.give_up()),
            }
        }
    }

    /// One TEST UNIT READY after a reset: does a command get through (a sense is an answer too)? Linux's error handler
    /// asks the same (`scsi_eh_tur`, 10 s). It tells a device still wedged from one that hangs on one sector; it changes
    /// nothing else here (a sense it leaves behind goes with the next command).
    fn answers(&mut self) -> bool {
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;
        let Ok(buf) = self.small() else { return false };
        if !buf.write(0, &cbw(tag, 0, Dir::None, 0, &scsi::test_unit_ready())) {
            return false;
        }
        let r = self.link.bulk(false, Request::new(buf.clone(), 0, 31), CBW_TIMEOUT_NS);
        if r.error.is_some() || r.actual != 31 {
            return false;
        }
        let r = self.link.bulk(true, Request::new(buf.clone(), 32, 13), ANSWER_CSW_NS);
        let mut b = [0u8; 13];
        if r.error.is_some() || r.actual != 13 || !buf.read(32, &mut b) {
            return false;
        }
        let s = Csw::parse(&b);
        s.tag == tag && s.status <= 1
    }

    /// One port reset in place, and what the commands after it have to know (`take_port_reset`, `just_reset`).
    pub fn port_reset_now(&mut self, power: bool, wait_ns: u64) -> Result<(), UsbError> {
        klog::event!(Warn, "usb", "msd.port_reset", device = self.name(), power = power; "resetting the device's port; the disk stays");
        self.link.port_reset(power, wait_ns)?;
        self.stats.port_resets += 1;
        PORT_RESETS.inc();
        self.port_reset_seen = true;
        self.reset_ua = true;
        Ok(())
    }

    /// Reset recovery is due before the next command, as after a failure (ktests).
    pub fn mark_reset_due(&mut self) {
        self.needs_reset = true;
    }

    fn recovered(&mut self) -> Result<(), BotError> {
        self.esc.answered();
        self.needs_reset = false;
        klog::event!(Info, "usb", "msd.reset", device = self.name(), resets = self.stats.resets, port_resets = self.stats.port_resets; "reset recovery done");
        Ok(())
    }

    /// Bulk-Only Mass Storage Reset (BOT 5.3.4), a short settle, both bulk endpoints' halts cleared (toggle 0 on
    /// both sides).
    fn bot_reset(&mut self) -> Result<(), UsbError> {
        self.link.abort_all();
        self.link.bot_reset().and_then(|_| {
            self.link.sleep_ns(RESET_SETTLE_NS);
            let a = self.link.clear_halt(true);
            let b = self.link.clear_halt(false);
            a.and(b)
        })
    }

    fn give_up(&mut self) -> BotError {
        if self.esc.has_given_up() {
            return BotError::ResetFailed;
        }
        let will_go = self.link.give_up();
        GAVE_UP.inc();
        klog::event!(Error, "usb", "msd.gave_up", device = self.name(), will_detach = will_go, resets = self.stats.resets,
            port_resets = self.stats.port_resets; "no recovery worked");
        if will_go {
            self.esc.gave_up();
        } else {
            self.esc.clean();
        }
        BotError::ResetFailed
    }
}

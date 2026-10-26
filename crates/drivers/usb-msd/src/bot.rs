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

use crate::link::{data_timeout_ns, Cmd, Dir, Link, Ran, CSW_TIMEOUT_NS};
use crate::scsi::{op, Cdb};
use alloc::string::String;
use alloc::vec::Vec;
use usb_core::{Buffer, Request, UsbError, XferResult};

pub const CBW_SIGNATURE: [u8; 4] = *b"USBC";
pub const CSW_SIGNATURE: [u8; 4] = *b"USBS";
/// How long the device gets after a Bulk-Only reset before its endpoints are cleared. Linux waits up to 6 s
/// (transport.c:1371, a conservative margin for slow bridges); V1 did not wait at all and never needed to.
pub const RESET_SETTLE_NS: u64 = 100_000_000;

klog::counter!(COMMANDS, "usb.msd.commands");
klog::counter!(TRANSPORT_ERRORS, "usb.msd.transport_errors");
klog::counter!(PHASE_ERRORS, "usb.msd.phase_errors");
klog::counter!(RESETS, "usb.msd.resets");
klog::counter!(RESET_FAILURES, "usb.msd.reset_failures");
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
    pub stats: BotStats,
}

impl<L: Link> Bot<L> {
    pub fn new(link: L) -> Bot<L> {
        Bot { link, tag: 0, signature: None, ignore_residue: false, needs_reset: false, small: None, epoch: 0, stats: BotStats::default() }
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

    /// Reset recovery (BOT 5.3.4): whatever is queued cancelled, Bulk-Only Mass Storage Reset, a short settle,
    /// both bulk endpoints' halts cleared (toggle 0 on both sides). Stays due when a step fails.
    pub fn reset_recovery(&mut self) -> Result<(), BotError> {
        self.needs_reset = true;
        self.stats.resets += 1;
        RESETS.inc();
        self.link.abort_all();
        let r = self.link.bot_reset().and_then(|_| {
            self.link.sleep_ns(RESET_SETTLE_NS);
            let a = self.link.clear_halt(true);
            let b = self.link.clear_halt(false);
            a.and(b)
        });
        match r {
            Ok(()) => {
                self.needs_reset = false;
                klog::event!(Info, "usb", "msd.reset", device = self.name(), resets = self.stats.resets; "reset recovery done");
                Ok(())
            }
            Err(e) if e == UsbError::Gone || self.link.gone() => Err(BotError::Gone),
            Err(e) => {
                self.stats.reset_failures += 1;
                RESET_FAILURES.inc();
                klog::event!(Error, "usb", "msd.reset_failed", device = self.name(), error = e.as_str(); "reset recovery failed; tried again before the next command");
                Err(BotError::ResetFailed)
            }
        }
    }
}

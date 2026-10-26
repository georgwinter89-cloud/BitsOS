//! A mass storage device (`Storage`: one Bulk-Only interface, one command at a time) and its logical units
//! (`MsdDisk`: one `C: BlockDevice` per LUN that is a disk).
//!
//! Bring-up per LUN (V1 `usb/msdscsi.asm`, Linux sd/scsi_scan order): INQUIRY (direct-access disks only), TEST
//! UNIT READY until it passes (3 s; 15 s once the unit says it is becoming ready; UNIT ATTENTION and NOT READY
//! ask again after 50 ms; no medium at once), READ CAPACITY(10) and (16) when (10) says 0xFFFFFFFF, MODE SENSE(6)
//! for write protection and DPOFUA. Discard: only devices that claim SPC-3 or later are asked READ CAPACITY(16)
//! and the provisioning VPD pages (B0h, B2h); Linux asks USB devices neither (`try_rc_10_first`, skipped VPD
//! pages) because cheap sticks crash on unknown commands, so old-style sticks are never asked.
//!
//! Requests: split into commands of at most the device's transfer limit (`quirks::max_bytes`), up to
//! `CHAIN_BYTES` of them chained in one run of the link (one wake-up per run, K-2); between runs the device is
//! handed to the waiting request of the highest priority (`Gate`, B4). Errors: a transport error was already
//! followed by reset recovery in the BOT layer and the command is tried again (`ATTEMPTS` per position); a
//! failed command gets REQUEST SENSE and `scsi::classify` decides: UNIT ATTENTION retried (medium change: size
//! read again, new generation, and on removable media the request fails as Linux does), NOT READY waits, no
//! medium / write protected / illegal / medium errors end the request. An unplugged device ends every request
//! with `Gone`, never a panic.
//!
//! R8: writes (and discards) only when the device was attached with writes allowed (CONFIG `usb_msd_write=on`,
//! never set on the stick) and the medium is not write-protected; otherwise the unit says `read_only` and every
//! write is `Refused`. FS-6's write-refusal hook will sit above this.

use crate::bot::{Bot, BotError, Op, Outcome};
use crate::link::{Dir, Link};
use crate::scsi::{self, Action, Capacity, Cdb, Inquiry, ModeHeader, Sense};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use devmodel::DevError;
use ksync::{Mutex, SpinLock, WaitQueue};
use usb_core::{Buffer, Request, UsbError};

const MS: u64 = 1_000_000;
/// TEST UNIT READY: how long a unit may take to become ready, and once it says "becoming ready" (V1
/// `MSD_READY_MS`, `MSD_BECOMING_MS`), and the pause between tries (`MSD_RETRY_MS`).
pub const READY_NS: u64 = 3000 * MS;
pub const BECOMING_NS: u64 = 15_000 * MS;
pub const RETRY_PAUSE_NS: u64 = 50 * MS;
/// Tries of one command position (the first + 2 more) before the request fails. A bound against a broken
/// device, not a count of things.
pub const ATTEMPTS: u32 = 3;
/// Data moved by one run of chained commands; between runs a request of higher priority may take the device.
pub const CHAIN_BYTES: usize = 1 << 20;
/// How long a request waits for the device before it gives up (B4: no unbounded wait). A run of 1 MiB with
/// every retry at the CSW timeout stays well below it.
pub const GATE_TIMEOUT_NS: u64 = 120_000 * MS;

klog::counter!(READS, "usb.msd.reads");
klog::counter!(WRITES, "usb.msd.writes");
klog::counter!(READ_BYTES, "usb.msd.read_bytes");
klog::counter!(WRITTEN_BYTES, "usb.msd.written_bytes");
klog::counter!(RETRIES, "usb.msd.retries");
klog::counter!(IO_ERRORS, "usb.msd.io_errors");
klog::counter!(REFUSED, "usb.msd.writes_refused");

static NEXT_DISK: AtomicU64 = AtomicU64::new(0);

// ---- the priority gate (B4): one owner, waiters served by priority

struct GateState {
    busy: bool,
    waiting: [u32; 3],
}

pub struct Gate {
    st: SpinLock<GateState>,
    wq: WaitQueue,
}

pub struct GateGuard<'a>(&'a Gate);

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.0.st.lock().busy = false;
        self.0.wq.wake_all();
    }
}

fn prio_index(p: IoPriority) -> usize {
    match p {
        IoPriority::Interactive => 0,
        IoPriority::Normal => 1,
        IoPriority::Background => 2,
    }
}

impl Gate {
    pub fn new() -> Gate {
        Gate { st: SpinLock::named("usb.msd.gate", GateState { busy: false, waiting: [0; 3] }), wq: WaitQueue::new() }
    }

    /// Waits until the device is free and no request of a higher priority waits; at most `timeout_ns`.
    pub fn acquire(&self, prio: IoPriority, timeout_ns: u64) -> Result<GateGuard<'_>, DevError> {
        let p = prio_index(prio);
        self.st.lock().waiting[p] += 1;
        let ok = self.wq.wait_until_timeout(
            || {
                let mut s = self.st.lock();
                if !s.busy && s.waiting[..p].iter().all(|&w| w == 0) {
                    s.busy = true;
                    s.waiting[p] -= 1;
                    true
                } else {
                    false
                }
            },
            timeout_ns,
        );
        if ok {
            Ok(GateGuard(self))
        } else {
            self.st.lock().waiting[p] -= 1;
            // Waiters of lower priority may have been held back by this one.
            self.wq.wake_all();
            Err(DevError::Timeout)
        }
    }

    /// (busy, waiters per priority) for tests.
    pub fn state(&self) -> (bool, [u32; 3]) {
        let s = self.st.lock();
        (s.busy, s.waiting)
    }
}

impl Default for Gate {
    fn default() -> Self {
        Gate::new()
    }
}

// ---- the device

pub struct Io<L: Link> {
    pub bot: Bot<L>,
    /// Data buffer of the runs, kept between requests (grown when a request needs more, up to `CHAIN_BYTES`).
    buf: Option<Buffer>,
    sense: Option<Buffer>,
    /// `Bot::epoch` the cached buffers belong to.
    epoch: u64,
}

impl<L: Link> Io<L> {
    /// Drops cached buffers the controller may still hold (a run that was given up).
    fn fresh(&mut self) {
        let e = self.bot.epoch();
        if e != self.epoch {
            self.epoch = e;
            self.buf = None;
            self.sense = None;
            klog::event!(Warn, "usb", "msd.buffers_dropped", device = self.bot.link.name(); "a given-up run may still hold them");
        }
    }

    /// A data buffer of `want` bytes if memory allows, else of at least `min` (one command).
    fn buffer(&mut self, want: usize, min: usize) -> Result<Buffer, DevError> {
        self.fresh();
        if let Some(b) = &self.buf
            && b.len() >= want
        {
            return Ok(b.clone());
        }
        if let Ok(b) = self.bot.link.alloc(want) {
            self.buf = Some(b.clone());
            return Ok(b);
        }
        if let Some(b) = &self.buf
            && b.len() >= min
        {
            return Ok(b.clone());
        }
        let b = self.bot.link.alloc(min).map_err(|_| DevError::NoMemory)?;
        self.buf = Some(b.clone());
        Ok(b)
    }
}

pub struct Storage<L: Link> {
    /// Device path and interface, for logs ("xhci0-3.1 if0").
    pub path: String,
    io: Mutex<Io<L>>,
    gate: Gate,
    gone: AtomicBool,
    /// Bytes per READ/WRITE command (speed and quirks).
    pub max_bytes: usize,
    /// R8: CONFIG `usb_msd_write=on` when the device was attached.
    pub writes_allowed: bool,
    /// The bus speed ("high", "super", ...), for logs.
    pub speed: &'static str,
}

/// What a command answered after the transport did its part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Good(Outcome),
    Check(Sense),
}

impl<L: Link> Storage<L> {
    pub fn new(link: L, max_bytes: usize, writes_allowed: bool, ignore_residue: bool) -> Storage<L> {
        let path = link.name();
        let mut bot = Bot::new(link);
        bot.ignore_residue = ignore_residue;
        Storage {
            path,
            io: Mutex::named("usb.msd.io", Io { bot, buf: None, sense: None, epoch: 0 }),
            gate: Gate::new(),
            gone: AtomicBool::new(false),
            max_bytes: max_bytes.max(512),
            writes_allowed,
            speed: "?",
        }
    }

    pub fn with_speed(mut self, speed: &'static str) -> Self {
        self.speed = speed;
        self
    }

    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// The device was detached: every request from now on ends with `Gone` (the transfers under way end with
    /// `Gone` from the host already).
    pub fn set_gone(&self) {
        self.gone.store(true, Ordering::Release);
    }

    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    /// Runs `f` with the device to itself (gate at `prio`, then the I/O state).
    pub fn with_io<R>(&self, prio: IoPriority, f: impl FnOnce(&mut Io<L>) -> R) -> Result<R, DevError> {
        if self.is_gone() {
            return Err(DevError::Gone);
        }
        let _g = self.gate.acquire(prio, GATE_TIMEOUT_NS)?;
        let mut io = self.io.lock_timeout(GATE_TIMEOUT_NS).ok_or(DevError::Timeout)?;
        if self.is_gone() || io.bot.link.gone() {
            self.set_gone();
            return Err(DevError::Gone);
        }
        Ok(f(&mut io))
    }

    /// GET MAX LUN (Linux `usb_stor_Bulk_max_lun`, transport.c:1075): a stall, an empty answer or a value above
    /// 15 mean one LUN. `Err` only when the device is gone.
    pub fn max_lun(&self) -> Result<u8, DevError> {
        self.with_io(IoPriority::Normal, |io| match io.bot.link.max_lun() {
            Ok(n) if n < 16 => Ok(n),
            Ok(n) => {
                klog::event!(Info, "usb", "msd.max_lun_invalid", device = self.path.clone(), value = n; "using one LUN");
                Ok(0)
            }
            Err(UsbError::Gone) => Err(DevError::Gone),
            Err(e) => {
                klog::event!(Debug, "usb", "msd.max_lun_refused", device = self.path.clone(), error = e.as_str(); "one LUN");
                Ok(0)
            }
        })?
    }

    /// Finds the LUNs and brings each up. Units that are no disk are left out; disks without a medium (an empty
    /// card reader) are kept and looked at again on every request.
    pub fn scan(self: &Arc<Self>) -> Result<Vec<Arc<MsdDisk<L>>>, DevError> {
        let max = self.max_lun()?;
        let mut v = Vec::new();
        for lun in 0..=max {
            let d = Arc::new(MsdDisk::new(self.clone(), lun));
            match d.bring_up() {
                Ok(true) => v.push(d),
                Ok(false) => {}
                Err(DevError::Gone) => return Err(DevError::Gone),
                Err(e) => {
                    klog::event!(Warn, "usb", "msd.lun_failed", device = self.path.clone(), lun = lun, error = format!("{e:?}"));
                }
            }
        }
        Ok(v)
    }
}

// ---- commands with sense handling (need the device to themselves: `Io`)

fn map_bot(e: BotError) -> DevError {
    match e {
        BotError::Gone => DevError::Gone,
        BotError::Transport(_) | BotError::ResetFailed => DevError::Io,
    }
}

impl<L: Link> Io<L> {
    /// REQUEST SENSE (18 bytes). A device that cannot even say why gives empty sense.
    fn request_sense(&mut self, lun: u8) -> Result<Sense, BotError> {
        self.fresh();
        let buf = match &self.sense {
            Some(b) => b.clone(),
            None => match self.bot.link.alloc(64) {
                Ok(b) => {
                    self.sense = Some(b.clone());
                    b
                }
                Err(_) => return Ok(Sense::default()),
            },
        };
        let _ = buf.write(0, &[0u8; 18]);
        let o = self.bot.command(Op::read(lun, scsi::request_sense(18), Request::new(buf.clone(), 0, 18)))?;
        let mut b = [0u8; 18];
        let n = o.moved.min(18);
        if !o.passed || !buf.read(0, &mut b[..n]) {
            return Ok(Sense::default());
        }
        Ok(Sense::parse(&b[..n]).unwrap_or_default())
    }

    /// One command, tried again after a transport error (reset recovery done by the BOT layer), REQUEST SENSE
    /// after a failure. `Err` only for `Gone` or a transport that keeps failing.
    pub fn command(&mut self, lun: u8, cdb: Cdb, dir: Dir, data: Option<Request>) -> Result<Reply, DevError> {
        let mut last = DevError::Io;
        for _ in 0..ATTEMPTS {
            let op = Op { lun, cdb, dir, data: data.clone() };
            match self.bot.command(op) {
                Ok(o) if o.passed => return Ok(Reply::Good(o)),
                Ok(_) => return self.request_sense(lun).map(Reply::Check).map_err(map_bot),
                Err(BotError::Gone) => return Err(DevError::Gone),
                Err(e) => {
                    RETRIES.inc();
                    last = map_bot(e);
                }
            }
        }
        Err(last)
    }

    /// A command with a data-in answer of at most `len` bytes into a fresh buffer; UNIT ATTENTION and
    /// ABORTED COMMAND are asked again (bounded). Returns the bytes or the sense of the last try.
    fn ask(&mut self, lun: u8, cdb: Cdb, len: usize) -> Result<Result<Vec<u8>, Sense>, DevError> {
        let buf = self.bot.link.alloc(len.max(1)).map_err(|_| DevError::NoMemory)?;
        let mut sense = Sense::default();
        for _ in 0..ATTEMPTS {
            match self.command(lun, cdb, Dir::In, Some(Request::new(buf.clone(), 0, len)))? {
                Reply::Good(o) => {
                    let mut v = vec![0u8; o.moved.min(len)];
                    if !buf.read(0, &mut v) {
                        return Err(DevError::NoMemory);
                    }
                    return Ok(Ok(v));
                }
                Reply::Check(s) => {
                    sense = s;
                    if scsi::classify(s) != Action::Retry {
                        break;
                    }
                }
            }
        }
        Ok(Err(sense))
    }
}

// ---- a logical unit

#[derive(Clone, Debug, Default)]
pub struct UnitInfo {
    pub medium: bool,
    pub block_size: u32,
    pub blocks: u64,
    pub write_protected: bool,
    pub removable: bool,
    /// READ/WRITE(16) for every command (more than 2^32 blocks).
    pub use16: bool,
    pub dpofua: bool,
    /// (max blocks per UNMAP descriptor, descriptors per command) when the device offers UNMAP.
    pub unmap: Option<(u32, u32)>,
    /// SYNCHRONIZE CACHE came back ILLEGAL REQUEST: no cache to flush.
    pub no_sync_cache: bool,
    pub inquiry: Inquiry,
}

pub struct MsdDisk<L: Link> {
    name: String,
    lun: u8,
    storage: Arc<Storage<L>>,
    info: SpinLock<UnitInfo>,
    /// Bumped at every medium change (and when a medium appears): the file system above must look again.
    generation: AtomicU64,
}

/// Bring-up outcome of the readiness wait.
enum Ready {
    Yes,
    NoMedium,
    /// TEST UNIT READY never passed in its time.
    Never,
}

impl<L: Link> MsdDisk<L> {
    fn new(storage: Arc<Storage<L>>, lun: u8) -> MsdDisk<L> {
        let n = NEXT_DISK.fetch_add(1, Ordering::Relaxed);
        MsdDisk { name: format!("msd{n}"), lun, storage, info: SpinLock::named("usb.msd.unit", UnitInfo::default()), generation: AtomicU64::new(0) }
    }

    pub fn lun(&self) -> u8 {
        self.lun
    }
    pub fn storage(&self) -> &Arc<Storage<L>> {
        &self.storage
    }
    pub fn unit(&self) -> UnitInfo {
        self.info.lock().clone()
    }
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    pub fn is_gone(&self) -> bool {
        self.storage.is_gone()
    }
    /// Writes reach the medium (R8 switch on, not write-protected).
    pub fn writable(&self) -> bool {
        self.storage.writes_allowed && !self.info.lock().write_protected
    }

    /// INQUIRY, readiness, capacity, mode sense. `Ok(false)` if the LUN is no disk.
    pub fn bring_up(&self) -> Result<bool, DevError> {
        let r = self.storage.with_io(IoPriority::Normal, |io| self.bring_up_io(io))??;
        Ok(r)
    }

    fn bring_up_io(&self, io: &mut Io<L>) -> Result<bool, DevError> {
        let lun = self.lun;
        let inq = match io.ask(lun, scsi::inquiry(36), 36)? {
            Ok(b) => Inquiry::parse(&b),
            Err(s) => {
                klog::event!(Warn, "usb", "msd.inquiry_failed", device = self.storage.path.clone(), lun = lun,
                    key = s.key, asc = klog::Hex(s.asc as u64));
                None
            }
        };
        let Some(inq) = inq else {
            return Err(DevError::Io);
        };
        if !inq.is_disk() {
            klog::event!(Info, "usb", "msd.not_a_disk", device = self.storage.path.clone(), lun = lun,
                qualifier = inq.qualifier, device_type = klog::Hex(inq.device_type as u64); "LUN left out");
            return Ok(false);
        }
        {
            let mut i = self.info.lock();
            i.removable = inq.removable;
            i.inquiry = inq;
        }
        self.refresh(io, true)?;
        Ok(true)
    }

    /// Readiness, capacity and mode sense again (bring-up, medium change, a medium that appeared). Emits the
    /// unit's state. `first`: at bring-up (the event says `msd.unit_ready` instead of `msd.unit_changed`).
    fn refresh(&self, io: &mut Io<L>, first: bool) -> Result<(), DevError> {
        let lun = self.lun;
        match self.wait_ready(io)? {
            Ready::Yes => {}
            r => {
                let why = if matches!(r, Ready::NoMedium) { "no medium" } else { "not ready" };
                let mut i = self.info.lock();
                i.medium = false;
                i.blocks = 0;
                drop(i);
                klog::event!(Info, "usb", "msd.unit_no_medium", disk = self.name.clone(), device = self.storage.path.clone(),
                    lun = lun, why = why; "kept; looked at again on the next request");
                return Ok(());
            }
        }
        let version = self.info.lock().inquiry.version;
        let mut cap = match io.ask(lun, scsi::read_capacity10(), 8)? {
            Ok(b) => Capacity::parse10(&b),
            Err(s) => {
                klog::event!(Warn, "usb", "msd.capacity_failed", device = self.storage.path.clone(), lun = lun, key = s.key,
                    asc = klog::Hex(s.asc as u64));
                None
            }
        };
        let need16 = cap.is_some_and(|c| c.needs16());
        // SPC-3 and later: READ CAPACITY(16) is part of the command set (and tells about provisioning).
        if need16 || version >= 5 {
            match io.ask(lun, scsi::read_capacity16(), 32)? {
                Ok(b) => {
                    if let Some(c16) = Capacity::parse16(&b)
                        && (need16 || cap.is_none_or(|c| c.blocks() == c16.blocks() && c.block_size == c16.block_size))
                    {
                        cap = Some(c16);
                    }
                }
                Err(s) if need16 => {
                    klog::event!(Warn, "usb", "msd.capacity16_failed", device = self.storage.path.clone(), lun = lun, key = s.key,
                        asc = klog::Hex(s.asc as u64));
                    cap = None;
                }
                Err(_) => {}
            }
        }
        // (After a (10) answer of 0xFFFFFFFF, `cap` is the (16) answer or nothing.)
        let Some(cap) = cap.filter(|c| c.block_size_ok() && (need16 || !c.needs16())) else {
            let mut i = self.info.lock();
            i.medium = false;
            i.blocks = 0;
            drop(i);
            klog::event!(Warn, "usb", "msd.unit_unusable", disk = self.name.clone(), device = self.storage.path.clone(), lun = lun,
                capacity = format!("{cap:?}"); "no usable capacity (block size 512..4096 needed)");
            return Ok(());
        };
        let mode = match io.ask(lun, scsi::mode_sense6(192), 192)? {
            Ok(b) if b.len() >= 4 => ModeHeader::parse6(&b),
            _ => match io.ask(lun, scsi::mode_sense6(4), 4)? {
                Ok(b) if b.len() >= 4 => ModeHeader::parse6(&b),
                _ => None,
            },
        }
        .unwrap_or_default();
        let unmap = if cap.lbpme && version >= 5 { self.unmap_limits(io)? } else { None };
        // Update under the lock, then log from a snapshot with the lock released (no formatting under it).
        let i = {
            let mut g = self.info.lock();
            g.medium = true;
            g.block_size = cap.block_size;
            g.blocks = cap.blocks();
            g.use16 = cap.blocks() > 0xFFFF_FFFF;
            g.write_protected = mode.write_protected;
            g.dpofua = mode.dpofua;
            g.unmap = unmap;
            g.clone()
        };
        let name = if first { "msd.unit_ready" } else { "msd.unit_changed" };
        klog::event!(Info, "usb", name, disk = self.name.clone(), device = self.storage.path.clone(), lun = lun,
            vendor = i.inquiry.vendor.clone(), product = i.inquiry.product.clone(), revision = i.inquiry.revision.clone(),
            blocks = i.blocks, block_size = i.block_size, mib = i.blocks.saturating_mul(i.block_size as u64) >> 20,
            removable = i.removable, write_protected = i.write_protected, spc = i.inquiry.version,
            max_kib = self.storage.max_bytes / 1024, rw16 = i.use16, fua = i.dpofua, discard = i.unmap.is_some(),
            writes = self.storage.writes_allowed, generation = self.generation());
        Ok(())
    }

    /// VPD 00h must list B0h and B2h; B2h must say LBPU, B0h give non-zero UNMAP limits.
    fn unmap_limits(&self, io: &mut Io<L>) -> Result<Option<(u32, u32)>, DevError> {
        let lun = self.lun;
        let Ok(pages) = io.ask(lun, scsi::inquiry_vpd(0x00, 64), 64)? else { return Ok(None) };
        if !(scsi::vpd_lists(&pages, 0xB0) && scsi::vpd_lists(&pages, 0xB2)) {
            return Ok(None);
        }
        let Ok(lbp) = io.ask(lun, scsi::inquiry_vpd(0xB2, 8), 8)? else { return Ok(None) };
        if !scsi::lbp_unmap(&lbp) {
            return Ok(None);
        }
        let Ok(b0) = io.ask(lun, scsi::inquiry_vpd(0xB0, 64), 64)? else { return Ok(None) };
        Ok(scsi::BlockLimits::parse(&b0)
            .filter(|l| l.max_unmap_lba_count > 0 && l.max_unmap_descriptors > 0)
            .map(|l| (l.max_unmap_lba_count, l.max_unmap_descriptors)))
    }

    /// TEST UNIT READY until it passes (V1 `msd_ready`): 3 s, 15 s from the first "becoming ready"; a pause
    /// between tries; no medium at once.
    fn wait_ready(&self, io: &mut Io<L>) -> Result<Ready, DevError> {
        let link_now = |io: &Io<L>| io.bot.link.now_ns();
        let start = link_now(io);
        let mut deadline = start.saturating_add(READY_NS);
        let mut extended = false;
        let mut tries = 0u32;
        loop {
            tries += 1;
            match io.command(self.lun, scsi::test_unit_ready(), Dir::None, None)? {
                Reply::Good(_) => {
                    if tries > 1 {
                        klog::event!(Debug, "usb", "msd.ready_after", device = self.storage.path.clone(), lun = self.lun, tries = tries,
                            ms = (link_now(io) - start) / MS);
                    }
                    return Ok(Ready::Yes);
                }
                Reply::Check(s) => match scsi::classify(s) {
                    Action::NoMedium => return Ok(Ready::NoMedium),
                    Action::MediumChanged => {
                        self.generation.fetch_add(1, Ordering::AcqRel);
                    }
                    Action::WaitRetry if !extended => {
                        extended = true;
                        deadline = start.saturating_add(BECOMING_NS);
                    }
                    Action::Done => return Ok(Ready::Yes),
                    _ => {}
                },
            }
            if link_now(io) >= deadline {
                klog::event!(Warn, "usb", "msd.not_ready", device = self.storage.path.clone(), lun = self.lun, tries = tries,
                    ms = (link_now(io) - start) / MS);
                return Ok(Ready::Never);
            }
            io.bot.link.sleep_ns(RETRY_PAUSE_NS);
        }
    }

    /// Medium changed (UNIT ATTENTION 28h) or a medium appeared: new generation, everything read again.
    fn medium_changed(&self, io: &mut Io<L>) -> Result<(), DevError> {
        let g = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        klog::event!(Info, "usb", "msd.medium_changed", disk = self.name.clone(), device = self.storage.path.clone(),
            lun = self.lun, generation = g);
        self.refresh(io, false)
    }

    /// The unit has no medium: one TEST UNIT READY; a medium that appeared is brought up (new generation).
    fn check_medium(&self, io: &mut Io<L>) -> Result<(), DevError> {
        if self.info.lock().medium {
            return Ok(());
        }
        match io.command(self.lun, scsi::test_unit_ready(), Dir::None, None)? {
            Reply::Check(s) if scsi::classify(s) != Action::Done && scsi::classify(s) != Action::MediumChanged => Err(DevError::Gone),
            _ => {
                self.medium_changed(io)?;
                if self.info.lock().medium { Ok(()) } else { Err(DevError::Gone) }
            }
        }
    }

    fn validate(&self, lba: u64, len: usize) -> Result<(u32, u64), DevError> {
        let i = self.info.lock();
        if !i.medium {
            return Ok((i.block_size, 0));
        }
        let bs = i.block_size as usize;
        if bs == 0 || !len.is_multiple_of(bs) {
            return Err(DevError::InvalidArgument);
        }
        let n = (len / bs) as u64;
        if lba.checked_add(n).is_none_or(|end| end > i.blocks) {
            return Err(DevError::InvalidArgument);
        }
        Ok((i.block_size, n))
    }

    /// The I/O of one request: runs of chained commands, the gate taken per run.
    fn rw(&self, write: bool, lba: u64, mut data: Data<'_>, fua: bool, prio: IoPriority) -> Result<(), DevError> {
        let len = data.len();
        if len == 0 {
            return Ok(());
        }
        if self.storage.is_gone() {
            return Err(DevError::Gone);
        }
        if write && !self.storage.writes_allowed {
            REFUSED.inc();
            return Err(DevError::Refused);
        }
        let mut done = 0u64; // blocks
        let mut attempts = 0u32;
        loop {
            let (now_done, total) = self.storage.with_io(prio, |io| -> Result<(u64, u64), DevError> {
                self.check_medium(io)?;
                let (bs, total) = self.validate(lba, len)?;
                if total == 0 {
                    return Err(DevError::Gone);
                }
                if write && self.info.lock().write_protected {
                    REFUSED.inc();
                    return Err(DevError::Refused);
                }
                let n = self.run_once(io, write, lba, &mut data, bs as usize, total, done, fua, &mut attempts)?;
                Ok((n, total))
            })??;
            done = now_done;
            if done >= total {
                break;
            }
        }
        if write {
            WRITES.inc();
            WRITTEN_BYTES.add(len as u64);
            let (dpofua, sync) = {
                let i = self.info.lock();
                (i.dpofua, !i.no_sync_cache)
            };
            if fua && !dpofua && sync {
                self.flush_at(prio)?;
            }
        } else {
            READS.inc();
            READ_BYTES.add(len as u64);
        }
        Ok(())
    }

    /// One run from block `done` on; returns the blocks done after it (all of them = finished).
    #[allow(clippy::too_many_arguments)]
    fn run_once(&self, io: &mut Io<L>, write: bool, lba: u64, data: &mut Data<'_>, bs: usize, total: u64, done: u64, fua: bool,
                attempts: &mut u32) -> Result<u64, DevError> {
        let per_cmd = (self.storage.max_bytes / bs).max(1) as u64;
        let chain = (CHAIN_BYTES.max(self.storage.max_bytes) / bs).max(1) as u64;
        let left = total - done;
        let n = left.min(chain);
        let buf = io.buffer(n as usize * bs, per_cmd as usize * bs)?;
        let n = n.min((buf.len() / bs) as u64).max(1);
        let (use16, dpofua) = {
            let i = self.info.lock();
            (i.use16, i.dpofua)
        };
        if write && !buf.write(0, data.out_slice(done as usize * bs, n as usize * bs)) {
            return Err(DevError::NoMemory);
        }
        let mut ops = Vec::new();
        let mut at = 0u64;
        while at < n {
            let k = (n - at).min(per_cmd);
            let cdb = scsi::rw(write, lba + done + at, k as u32, fua && dpofua, use16);
            let req = Request::new(buf.clone(), at as usize * bs, k as usize * bs);
            ops.push(if write { Op::write(self.lun, cdb, req) } else { Op::read(self.lun, cdb, req) });
            at += k;
        }
        let results = io.bot.run(&ops);
        let mut good = 0u64; // blocks of this run that are done
        let mut failure: Option<Result<Outcome, BotError>> = None;
        for (i, r) in results.iter().enumerate() {
            let k = ops[i].data.as_ref().map_or(0, |d| d.len / bs) as u64;
            match r {
                Ok(o) if o.passed && o.residue == 0 => good += k,
                other => {
                    failure = Some(*other);
                    break;
                }
            }
        }
        if !write && good > 0 && !buf.read(0, data.in_slice(done as usize * bs, good as usize * bs)) {
            return Err(DevError::NoMemory);
        }
        if good > 0 {
            *attempts = 0;
        }
        let Some(f) = failure else {
            return Ok(done + good);
        };
        // The command at block `done + good` did not complete: decide.
        let at_lba = lba + done + good;
        let cmd_len = ops.get(results.len() - 1).and_then(|o| o.data.as_ref()).map_or(0, |d| d.len);
        // Whole blocks of the failed command that are good: what moved, less what the device says did not
        // happen (Linux: resid = max(not moved, dCSWDataResidue); the rest of the request goes again).
        let mut keep = |o: &Outcome, good: &mut u64| -> Result<bool, DevError> {
            let valid = cmd_len.saturating_sub(o.residue as usize).min(o.moved);
            let whole = (valid / bs) as u64;
            if !write && whole > 0 && !buf.read(*good as usize * bs, data.in_slice((done + *good) as usize * bs, whole as usize * bs)) {
                return Err(DevError::NoMemory);
            }
            *good += whole;
            Ok(whole as usize * bs == cmd_len)
        };
        let verdict = match f {
            Ok(o) if o.passed => {
                klog::event!(Debug, "usb", "msd.short", disk = self.name.clone(), lba = at_lba, moved = o.moved, residue = o.residue);
                keep(&o, &mut good)?;
                Action::Retry
            }
            Ok(o) => {
                let s = io.request_sense(self.lun).map_err(map_bot)?;
                let a = scsi::classify(s);
                if a != Action::Retry && a != Action::Done {
                    klog::event!(Info, "usb", "msd.check_condition", disk = self.name.clone(), lba = at_lba, write = write,
                        key = klog::Hex(s.key as u64), asc = klog::Hex(s.asc as u64), ascq = klog::Hex(s.ascq as u64),
                        action = format!("{a:?}"));
                }
                // RECOVERED ERROR: the command worked as far as its data moved.
                if a == Action::Done {
                    if keep(&o, &mut good)? {
                        *attempts = 0;
                        return Ok(done + good);
                    }
                    Action::Retry
                } else {
                    a
                }
            }
            Err(BotError::Gone) => {
                self.storage.set_gone();
                return Err(DevError::Gone);
            }
            Err(e) => {
                klog::event!(Debug, "usb", "msd.transport_retry", disk = self.name.clone(), lba = at_lba, why = e.as_str());
                Action::Retry
            }
        };
        match verdict {
            Action::Retry | Action::WaitRetry | Action::Done => {
                // `good` includes what `keep` took from the failed command: a run that moved the request on
                // starts the count for the new position (`ATTEMPTS` per position; review INT-A1d drivers 1).
                if good > 0 {
                    *attempts = 0;
                }
                *attempts += 1;
                RETRIES.inc();
                if *attempts >= ATTEMPTS {
                    IO_ERRORS.inc();
                    klog::event!(Warn, "usb", "msd.io_failed", disk = self.name.clone(), lba = at_lba, write = write, tries = *attempts);
                    return Err(DevError::Io);
                }
                if verdict == Action::WaitRetry {
                    io.bot.link.sleep_ns(RETRY_PAUSE_NS);
                }
                Ok(done + good)
            }
            Action::MediumChanged => {
                self.medium_changed(io)?;
                let removable = self.info.lock().removable;
                if removable {
                    // Linux (scsi_io_completion): a medium change on removable media fails the request; the file
                    // system must look at the new medium first.
                    IO_ERRORS.inc();
                    return Err(DevError::Io);
                }
                *attempts += 1;
                if *attempts >= ATTEMPTS {
                    return Err(DevError::Io);
                }
                Ok(done + good)
            }
            Action::NoMedium => {
                let mut i = self.info.lock();
                i.medium = false;
                i.blocks = 0;
                drop(i);
                self.generation.fetch_add(1, Ordering::AcqRel);
                klog::event!(Info, "usb", "msd.medium_removed", disk = self.name.clone(), device = self.storage.path.clone(), lun = self.lun);
                Err(DevError::Gone)
            }
            Action::WriteProtected => {
                self.info.lock().write_protected = true;
                klog::event!(Info, "usb", "msd.write_protected", disk = self.name.clone(); "DATA PROTECT: the medium refuses writes");
                REFUSED.inc();
                Err(DevError::Refused)
            }
            Action::Illegal => {
                IO_ERRORS.inc();
                Err(DevError::InvalidArgument)
            }
            Action::Fail => {
                IO_ERRORS.inc();
                Err(DevError::Io)
            }
        }
    }

    fn flush_at(&self, prio: IoPriority) -> Result<(), DevError> {
        if !self.storage.writes_allowed || self.info.lock().no_sync_cache {
            return Ok(());
        }
        self.storage.with_io(prio, |io| -> Result<(), DevError> {
            for _ in 0..ATTEMPTS {
                match io.command(self.lun, scsi::synchronize_cache10(), Dir::None, None)? {
                    Reply::Good(_) => return Ok(()),
                    Reply::Check(s) => match scsi::classify(s) {
                        Action::Illegal => {
                            self.info.lock().no_sync_cache = true;
                            klog::event!(Info, "usb", "msd.no_sync_cache", disk = self.name.clone(); "SYNCHRONIZE CACHE not supported: no cache");
                            return Ok(());
                        }
                        Action::Done => return Ok(()),
                        Action::Retry | Action::WaitRetry => {}
                        Action::NoMedium => return Err(DevError::Gone),
                        _ => return Err(DevError::Io),
                    },
                }
            }
            Err(DevError::Io)
        })?
    }

    /// Sends a raw command (ktests: sense paths). Returns the reply and the bytes read.
    pub fn raw(&self, cdb: Cdb, dir: Dir, len: usize) -> Result<(Reply, Vec<u8>), DevError> {
        self.storage.with_io(IoPriority::Normal, |io| -> Result<(Reply, Vec<u8>), DevError> {
            let buf = io.bot.link.alloc(len.max(1)).map_err(|_| DevError::NoMemory)?;
            let data = (len > 0).then(|| Request::new(buf.clone(), 0, len));
            let r = io.command(self.lun, cdb, if len > 0 { dir } else { Dir::None }, data)?;
            let n = match r {
                Reply::Good(o) => o.moved.min(len),
                Reply::Check(_) => 0,
            };
            let mut v = vec![0u8; n];
            let _ = buf.read(0, &mut v);
            Ok((r, v))
        })?
    }
}

/// The caller's side of a request.
enum Data<'a> {
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

impl<'a> Data<'a> {
    fn len(&self) -> usize {
        match self {
            Data::In(b) => b.len(),
            Data::Out(b) => b.len(),
        }
    }
    fn in_slice(&mut self, at: usize, len: usize) -> &mut [u8] {
        match self {
            Data::In(b) => &mut b[at..at + len],
            Data::Out(_) => &mut [],
        }
    }
    fn out_slice(&self, at: usize, len: usize) -> &[u8] {
        match self {
            Data::Out(b) => &b[at..at + len],
            Data::In(_) => &[],
        }
    }
}

impl<L: Link + 'static> BlockDevice for MsdDisk<L> {
    fn name(&self) -> &str {
        &self.name
    }
    fn info(&self) -> BlockInfo {
        let i = self.info.lock();
        // Without a medium the size is unknown: 512-byte blocks, none of them.
        let bs = if i.medium { i.block_size.max(512) } else { 512 };
        BlockInfo {
            block_size: bs,
            blocks: if i.medium { i.blocks } else { 0 },
            read_only: !self.storage.writes_allowed || i.write_protected,
            max_blocks_per_request: ((self.storage.max_bytes as u64 / bs as u64).max(1)).min(u32::MAX as u64) as u32,
            supports_discard: i.unmap.is_some() && self.storage.writes_allowed,
            has_volatile_cache: !i.no_sync_cache,
        }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        self.rw(false, lba, Data::In(buf), false, prio)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        self.rw(true, lba, Data::Out(buf), fua, prio)
    }
    fn flush(&self) -> Result<(), DevError> {
        if self.storage.is_gone() {
            return Err(DevError::Gone);
        }
        self.flush_at(IoPriority::Normal)
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        if blocks == 0 {
            return Ok(());
        }
        if !self.storage.writes_allowed {
            REFUSED.inc();
            return Err(DevError::Refused);
        }
        let (unmap, blocks_total, wp) = {
            let i = self.info.lock();
            (i.unmap, i.blocks, i.write_protected)
        };
        if wp {
            return Err(DevError::Refused);
        }
        if lba.checked_add(blocks).is_none_or(|e| e > blocks_total) {
            return Err(DevError::InvalidArgument);
        }
        let Some((max_lba, max_desc)) = unmap else {
            return Ok(()); // the device offers no UNMAP: ignored, as the trait's default
        };
        let mut ranges: Vec<(u64, u32)> = Vec::new();
        let mut at = lba;
        while at < lba + blocks {
            let k = (lba + blocks - at).min(max_lba as u64) as u32;
            ranges.push((at, k));
            at += k as u64;
        }
        // max_desc > 0 (unmap_limits filters zero); at most UNMAP_MAX_DESCRIPTORS so every length fits u16.
        let per_list = (max_desc as usize).clamp(1, scsi::UNMAP_MAX_DESCRIPTORS);
        for group in ranges.chunks(per_list) {
            let params = scsi::unmap_params(group).ok_or(DevError::InvalidArgument)?;
            let param_len = u16::try_from(params.len()).map_err(|_| DevError::InvalidArgument)?;
            self.storage.with_io(IoPriority::Background, |io| -> Result<(), DevError> {
                let buf = io.bot.link.alloc(params.len()).map_err(|_| DevError::NoMemory)?;
                if !buf.write(0, &params) {
                    return Err(DevError::NoMemory);
                }
                let req = Request::new(buf, 0, params.len());
                for _ in 0..ATTEMPTS {
                    match io.command(self.lun, scsi::unmap(param_len), Dir::Out, Some(req.clone()))? {
                        Reply::Good(_) => return Ok(()),
                        Reply::Check(s) => match scsi::classify(s) {
                            Action::Retry | Action::WaitRetry => {}
                            Action::Done => return Ok(()),
                            Action::Illegal => return Err(DevError::Unsupported),
                            Action::WriteProtected => return Err(DevError::Refused),
                            Action::NoMedium => return Err(DevError::Gone),
                            _ => return Err(DevError::Io),
                        },
                    }
                }
                Err(DevError::Io)
            })??;
        }
        Ok(())
    }
}

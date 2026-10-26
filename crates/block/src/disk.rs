//! A registered disk: one `C: BlockDevice` with its request queue (`queue::Elevator`), its dispatcher, its
//! partition-table verdict and its write-refusal hook.
//!
//! - **Submitting** (`submit`, or the waiting helpers `read_vec`, `write`, `read_pages`, `write_pages`,
//!   `flush`, `discard`): the request is checked (block size, range: a write past the end is `NoSpace`, a read
//!   `BeyondEnd`; writes, FUA/preflush writes and discards through the hook: `ReadOnly(why)` = EROFS), then
//!   queued with its priority. Every request **owns** its memory (a byte vector or page-cache pages), so a
//!   caller that gives up (deadline) never leaves the device writing into freed memory.
//! - **Dispatching:** one thread per disk (`blk:<name>`, class Interactive: it serves Interactive reads and
//!   sleeps while the device works) takes the next batch from the elevator (priority with aging, merges up to
//!   max_sectors), calls the device and completes the members. A merged batch that fails is retried member by
//!   member, so one bad block fails only the request that holds it. Without a thread (spawn failed, host tests)
//!   the waiting callers dispatch themselves, one at a time.
//! - **Waiting:** every member has a `Completion`; the waiter sleeps on it with a deadline (B4: default
//!   120 s, `set_timeout_ns`). On a timeout the request is taken out of the queue, or, if the device has it
//!   already, its answer is dropped (`Timeout` = EIO to the caller).
//! - **Scatter-gather into page-cache pages:** `read_pages`/`write_pages` take `PageSeg`s (FS-5's
//!   `CachePage`s); the dispatcher moves the bytes between the pages and the device in chunks of at least
//!   1 MiB (the trait takes a byte slice, and safe code reaches a cache frame only through its copy
//!   accessors, so this is one copy per byte: see the notes' change request for physical segments).
//!
//! Locks: `block.queue` (spin; the elevator and the request list; never held across a device call, a wait or
//! a klog event), `block.table`, `block.disk.gates`, a request's `block.req.data`/`block.req.result` (spin,
//! leaves). The dispatcher calls the device with no lock of this crate held.

use crate::error::{BlockError, Refusal};
use crate::policy::{self, DiskKind, StickMatch, WriteCheck, WriteGate};
use crate::queue::{Batch, Elevator, Entry, Kind, Limits};
use crate::table::{self, Scan};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use devmodel::DevError;
use ksync::{Completion, SpinLock, WaitQueue};
use vfs::pagecache::CachePage;

/// Default deadline of a request (B4).
pub const DEFAULT_TIMEOUT_NS: u64 = 120_000_000_000;
/// Default aging step: a request waiting this long competes one class higher.
pub const DEFAULT_AGE_NS: u64 = 250_000_000;
/// Bounce chunks are at least this big (usb-msd chains its commands up to 1 MiB per run).
pub const CHUNK_BYTES: u64 = 1 << 20;
/// How long the idle dispatcher sleeps before it looks again by itself.
const IDLE_NS: u64 = 1_000_000_000;
/// A waiter without a dispatcher thread looks at least this often whether it should dispatch.
const INLINE_POLL_NS: u64 = 10_000_000;

klog::counter!(SUBMITTED, "block.requests");
klog::counter!(MERGED, "block.merged");
klog::counter!(DISPATCHES, "block.dispatches");
klog::counter!(ERRORS, "block.errors");
klog::counter!(REFUSED, "block.writes_refused");
klog::counter!(TIMEOUTS, "block.timeouts");
klog::counter!(AGED, "block.aged");
klog::counter!(SPLIT_RETRIES, "block.merge_retries");

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// Part of a page-cache page: bytes `[off, off + len)` of `page`.
#[derive(Clone)]
pub struct PageSeg {
    pub page: Arc<CachePage>,
    pub off: usize,
    pub len: usize,
}

/// The memory a request owns.
#[derive(Default)]
pub enum Data {
    #[default]
    None,
    Bytes(Vec<u8>),
    Pages(Vec<PageSeg>),
}

impl Data {
    pub fn len(&self) -> usize {
        match self {
            Data::None => 0,
            Data::Bytes(b) => b.len(),
            Data::Pages(p) => p.iter().map(|s| s.len).sum(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `out.len()` bytes from offset `at` of the data into `out`.
    fn copy_out(&self, at: usize, out: &mut [u8]) {
        match self {
            Data::None => {}
            Data::Bytes(b) => {
                if let Some(s) = b.get(at..at + out.len()) {
                    out.copy_from_slice(s);
                }
            }
            Data::Pages(segs) => {
                let mut base = 0usize;
                for s in segs {
                    let (lo, hi) = (at.max(base), (at + out.len()).min(base + s.len));
                    if lo < hi {
                        s.page.read(s.off + (lo - base), &mut out[lo - at..hi - at]);
                    }
                    base += s.len;
                }
            }
        }
    }

    /// Copies `src` into the data at offset `at`.
    fn copy_in(&mut self, at: usize, src: &[u8]) {
        match self {
            Data::None => {}
            Data::Bytes(b) => {
                if let Some(s) = b.get_mut(at..at + src.len()) {
                    s.copy_from_slice(src);
                }
            }
            Data::Pages(segs) => {
                let mut base = 0usize;
                for s in segs.iter() {
                    let (lo, hi) = (at.max(base), (at + src.len()).min(base + s.len));
                    if lo < hi {
                        s.page.write(s.off + (lo - base), &src[lo - at..hi - at]);
                    }
                    base += s.len;
                }
            }
        }
    }
}

struct Req {
    id: u64,
    kind: Kind,
    lba: u64,
    blocks: u64,
    data: SpinLock<Data>,
    result: SpinLock<Option<Result<(), BlockError>>>,
    done: Completion,
}

impl Req {
    fn end(&self) -> u64 {
        self.lba + self.blocks
    }
}

struct Queue {
    elev: Elevator,
    /// Queued and in-flight requests.
    reqs: Vec<Arc<Req>>,
    stopped: bool,
}

struct TableState {
    scan: Scan,
    /// Disk size when the table was read (a different size later = a new medium: read again).
    blocks: u64,
    stick: StickMatch,
}

/// What the dispatcher does next (`Disk::next`).
enum Step {
    Run(Batch, Vec<Arc<Req>>),
    /// No memory to build a batch: its requests were failed with ENOMEM (logged by `drain`, outside the lock).
    NoMemory { kind: Kind, lba: u64, blocks: u64, members: usize },
}

const MODE_NONE: u8 = 0;
const MODE_THREAD: u8 = 1;
const MODE_INLINE: u8 = 2;

/// Counters of one disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskStats {
    pub requests: u64,
    /// Requests that went to the device inside another one's batch.
    pub merged: u64,
    pub dispatches: u64,
    pub errors: u64,
    pub refused: u64,
    pub timeouts: u64,
    pub aged: u64,
    pub merge_retries: u64,
    pub queued: u64,
    /// Bytes of successful reads and writes, counted at completion (RB-7b CR2).
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// Time with a request at the device (Linux /proc/diskstats `io_ticks`, in ns): one worker per disk runs
    /// the batches one at a time, so it is the time spent in `execute`.
    pub busy_ns: u64,
}

#[derive(Default)]
struct Stats {
    requests: AtomicU64,
    merged: AtomicU64,
    dispatches: AtomicU64,
    errors: AtomicU64,
    refused: AtomicU64,
    timeouts: AtomicU64,
    aged: AtomicU64,
    merge_retries: AtomicU64,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
    busy_ns: AtomicU64,
}

pub struct Disk {
    name: String,
    dev: Arc<dyn BlockDevice>,
    kind: DiskKind,
    block_size: u32,
    q: SpinLock<Queue>,
    work: WaitQueue,
    next_id: AtomicU64,
    table: SpinLock<TableState>,
    gates: SpinLock<Vec<Arc<dyn WriteGate>>>,
    mode: AtomicU8,
    dispatching: AtomicBool,
    worker_done: AtomicBool,
    age_ns: AtomicU64,
    timeout_ns: AtomicU64,
    stats: Stats,
    /// One bit per refusal reason already logged (one `write.refused` event per reason and disk).
    logged: AtomicU32,
}

/// A queued request; `wait` gives its data back (the bytes read, or the buffer written).
pub struct Ticket {
    disk: Arc<Disk>,
    req: Arc<Req>,
}

impl Ticket {
    pub fn id(&self) -> u64 {
        self.req.id
    }
    pub fn is_done(&self) -> bool {
        self.req.done.is_done()
    }
    /// Waits with the disk's deadline.
    pub fn wait(self) -> Result<Data, BlockError> {
        let t = self.disk.timeout_ns.load(Ordering::Relaxed);
        self.wait_timeout(t)
    }
    pub fn wait_timeout(self, timeout_ns: u64) -> Result<Data, BlockError> {
        let done = self.disk.wait_req(&self.req, timeout_ns);
        if !done && !self.disk.cancel(&self.req) && !self.req.done.is_done() {
            TIMEOUTS.inc();
            self.disk.stats.timeouts.fetch_add(1, Ordering::Relaxed);
            klog::event!(Warn, "block", "req.timeout", disk = self.disk.name.clone(), op = self.req.kind.as_str(), lba = self.req.lba,
                blocks = self.req.blocks, timeout_ms = timeout_ns / 1_000_000; "the device still has it: its answer will be dropped");
            return Err(BlockError::Timeout);
        }
        let r = self.req.result.lock().take().unwrap_or(Err(BlockError::Timeout));
        r.map(|()| core::mem::take(&mut *self.req.data.lock()))
    }
}

/// How a disk dispatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// A dispatcher thread (`blk:<name>`, Interactive).
    Thread,
    /// The waiting callers dispatch (host tests, or when no thread could be started).
    Inline,
}

impl Disk {
    /// Puts a queue in front of `dev` (not in the registry: `crate::register` does that), starts its
    /// dispatcher and reads its partition table.
    pub fn open(dev: Arc<dyn BlockDevice>, kind: DiskKind, dispatch: Dispatch) -> Result<Arc<Disk>, BlockError> {
        let info = dev.info();
        if !matches!(info.block_size, 512 | 1024 | 2048 | 4096) {
            klog::event!(Warn, "block", "disk.bad_block_size", disk = String::from(dev.name()), block_size = info.block_size);
            return Err(BlockError::Invalid);
        }
        let d = Arc::new(Disk {
            name: String::from(dev.name()),
            dev,
            kind,
            block_size: info.block_size,
            q: SpinLock::named("block.queue", Queue { elev: Elevator::new(), reqs: Vec::new(), stopped: false }),
            work: WaitQueue::new(),
            next_id: AtomicU64::new(1),
            table: SpinLock::named("block.table", TableState { scan: Scan::default(), blocks: info.blocks, stick: StickMatch::Unknown }),
            gates: SpinLock::named("block.disk.gates", Vec::new()),
            mode: AtomicU8::new(MODE_NONE),
            dispatching: AtomicBool::new(false),
            worker_done: AtomicBool::new(false),
            age_ns: AtomicU64::new(DEFAULT_AGE_NS),
            timeout_ns: AtomicU64::new(DEFAULT_TIMEOUT_NS),
            stats: Stats::default(),
            logged: AtomicU32::new(0),
        });
        d.start(dispatch);
        d.rescan();
        Ok(d)
    }

    fn start(self: &Arc<Self>, dispatch: Dispatch) {
        if dispatch == Dispatch::Thread {
            let me = self.clone();
            let name = format!("blk:{}", self.name);
            self.mode.store(MODE_THREAD, Ordering::Release);
            match sched::spawn(&name, sched::Class::Interactive, move || me.worker()) {
                Ok(_) => return,
                Err(e) => {
                    klog::event!(Warn, "block", "worker.inline", disk = self.name.clone(), error = format!("{e:?}");
                        "no dispatcher thread: the waiting callers dispatch")
                }
            }
        }
        self.worker_done.store(true, Ordering::Release);
        self.mode.store(MODE_INLINE, Ordering::Release);
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> DiskKind {
        self.kind
    }
    pub fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.dev
    }
    pub fn block_size(&self) -> u32 {
        self.block_size
    }
    /// The device's info now.
    pub fn dev_info(&self) -> BlockInfo {
        self.dev.info()
    }
    pub fn set_timeout_ns(&self, ns: u64) {
        self.timeout_ns.store(ns.max(1), Ordering::Relaxed);
    }
    pub fn set_age_ns(&self, ns: u64) {
        self.age_ns.store(ns.max(1), Ordering::Relaxed);
    }
    pub fn is_stopped(&self) -> bool {
        self.q.lock().stopped
    }
    /// The dispatcher thread ended (or never ran).
    pub fn worker_done(&self) -> bool {
        self.worker_done.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> DiskStats {
        let s = &self.stats;
        DiskStats {
            requests: s.requests.load(Ordering::Relaxed),
            merged: s.merged.load(Ordering::Relaxed),
            dispatches: s.dispatches.load(Ordering::Relaxed),
            errors: s.errors.load(Ordering::Relaxed),
            refused: s.refused.load(Ordering::Relaxed),
            timeouts: s.timeouts.load(Ordering::Relaxed),
            aged: s.aged.load(Ordering::Relaxed),
            merge_retries: s.merge_retries.load(Ordering::Relaxed),
            queued: self.q.lock().elev.len() as u64,
            read_bytes: s.read_bytes.load(Ordering::Relaxed),
            write_bytes: s.write_bytes.load(Ordering::Relaxed),
            busy_ns: s.busy_ns.load(Ordering::Relaxed),
        }
    }

    /// The partition-table look (copy).
    pub fn scan(&self) -> Scan {
        self.table.lock().scan.clone()
    }
    pub fn stick(&self) -> StickMatch {
        self.table.lock().stick
    }

    /// Whether one of this disk's own gates opens it as the boot stick now (FS-7 armed).
    pub fn stick_opened(&self) -> bool {
        self.gates.lock().iter().any(|g| g.opens_boot_stick())
    }

    /// Plugs a gate in for this disk only.
    pub fn add_gate(&self, g: Arc<dyn WriteGate>) {
        klog::event!(Info, "block", "gate.added", gate = g.name(), scope = self.name.clone());
        self.gates.lock().push(g);
    }

    /// Whether some write to this disk could pass the hook now (Ok) or why every write is refused: the
    /// disk-wide question behind `DiskDevice::info().read_only` and `disk.registered`. Gates are asked with
    /// `WriteCheck::range == None`, so a gate that allows only some ranges (FS-7's `\BITSOS2\` rule) does not
    /// make the whole disk look read-only.
    pub fn write_policy(self: &Arc<Self>) -> Result<(), Refusal> {
        self.check_write(Kind::Write { fua: false, preflush: false }, None)
    }

    /// Reads the partition table (again) and matches the boot stick.
    pub fn rescan(self: &Arc<Self>) {
        let info = self.dev.info();
        let bs = self.block_size as usize;
        let me = self.clone();
        let mut read = |lba: u64, n: u64| -> Option<Vec<u8>> {
            let len = usize::try_from(n).ok()?.checked_mul(bs)?;
            let mut tries = 0;
            loop {
                match me.read_vec(lba, len, IoPriority::Normal) {
                    Ok(v) => return Some(v),
                    Err(BlockError::Io | BlockError::Timeout) if tries == 0 => tries += 1,
                    Err(_) => return None,
                }
            }
        };
        let scan = table::scan(&mut read, self.block_size, info.blocks);
        let stick = policy::stick_match(&scan);
        let verdict = scan.verdict();
        klog::event!(Info, "block", "disk.table", disk = self.name.clone(), scheme = scan.scheme_str(), parts = scan.parts.len(),
            windows = scan.windows.first().cloned().unwrap_or_default(), efi = scan.efi.first().cloned().unwrap_or_default(),
            unknown = scan.unknown.clone().unwrap_or_default(), incomplete = scan.incomplete, stick = stick_str(stick));
        if verdict != table::Verdict::Clean {
            klog::event!(Info, "block", "disk.protected", disk = self.name.clone(), reason = format!("{verdict:?}");
                "writes refused for good (R8a)");
        }
        let mut t = self.table.lock();
        t.scan = scan;
        t.blocks = info.blocks;
        t.stick = stick;
    }

    /// The boot stick's identity changed (`crate::configure`): match again without I/O.
    pub(crate) fn rematch_stick(&self) -> StickMatch {
        let mut t = self.table.lock();
        t.stick = policy::stick_match(&t.scan);
        t.stick
    }

    /// The write-refusal hook (R8a): the block layer's rules, then this disk's and the global gates.
    /// `range`: `(lba, blocks)` of the request; None asks whether any write could pass (`write_policy`).
    pub fn check_write(self: &Arc<Self>, op: Kind, range: Option<(u64, u64)>) -> Result<(), Refusal> {
        let info = self.dev.info();
        if self.table.lock().blocks != info.blocks {
            // A new medium (the size changed): its table is read before anything is written to it.
            klog::event!(Info, "block", "disk.resized", disk = self.name.clone(), blocks = info.blocks; "partition table read again");
            self.rescan();
        }
        let (verdict, stick) = {
            let t = self.table.lock();
            (t.scan.verdict(), t.stick)
        };
        let own: Vec<Arc<dyn WriteGate>> = self.gates.lock().clone();
        let opens = own.iter().any(|g| g.opens_boot_stick());
        policy::opened(policy::decide(self.kind, info.read_only, &verdict, stick, policy::switches()), stick, opens)?;
        let w = WriteCheck { disk: &self.name, kind: self.kind, op, range };
        let mut gates = policy::global_gates();
        gates.extend(own);
        match gates.iter().find(|g| !g.allows(&w)) {
            Some(g) => Err(Refusal::Gate(g.name())),
            None => Ok(()),
        }
    }

    fn refused(&self, why: Refusal, op: Kind, lba: u64, blocks: u64) -> BlockError {
        REFUSED.inc();
        self.stats.refused.fetch_add(1, Ordering::Relaxed);
        let bit = 1u32 << refusal_bit(why);
        if self.logged.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            klog::event!(Info, "block", "write.refused", disk = self.name.clone(), kind = self.kind.as_str(), op = op.as_str(),
                lba = lba, blocks = blocks, reason = why.as_str(); "R8a: writes to this disk are refused (logged once per reason)");
        }
        BlockError::ReadOnly(why)
    }

    /// Queues a request. Reads and writes take their length from `data` (a multiple of the block size);
    /// discards take `blocks`; flushes neither.
    pub fn submit(self: &Arc<Self>, kind: Kind, lba: u64, blocks: u64, data: Data, prio: IoPriority) -> Result<Ticket, BlockError> {
        let bs = self.block_size as u64;
        let blocks = match kind {
            Kind::Read | Kind::Write { .. } => {
                let len = data.len() as u64;
                if len == 0 || !len.is_multiple_of(bs) {
                    return Err(BlockError::Invalid);
                }
                len / bs
            }
            Kind::Discard => {
                if blocks == 0 {
                    return Err(BlockError::Invalid);
                }
                blocks
            }
            Kind::Flush => 0,
        };
        let lba = if kind == Kind::Flush { 0 } else { lba };
        if kind != Kind::Flush {
            let total = self.dev.info().blocks;
            if lba.checked_add(blocks).is_none_or(|e| e > total) {
                return Err(if kind.writes() { BlockError::NoSpace } else { BlockError::BeyondEnd });
            }
        }
        if kind.writes() {
            self.check_write(kind, Some((lba, blocks))).map_err(|why| self.refused(why, kind, lba, blocks))?;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Arc::new(Req {
            id,
            kind,
            lba,
            blocks,
            data: SpinLock::named("block.req.data", data),
            result: SpinLock::named("block.req.result", None),
            done: Completion::new(),
        });
        {
            let mut q = self.q.lock();
            if q.stopped {
                return Err(BlockError::Gone);
            }
            if q.reqs.try_reserve(1).is_err() {
                return Err(BlockError::NoMemory);
            }
            if q.elev.push(Entry { id, kind, lba, blocks, prio, at_ns: now() }).is_err() {
                return Err(BlockError::NoMemory);
            }
            q.reqs.push(req.clone());
        }
        SUBMITTED.inc();
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        if self.mode.load(Ordering::Acquire) == MODE_THREAD {
            self.work.wake_one();
        }
        Ok(Ticket { disk: self.clone(), req })
    }

    // ---- waiting helpers

    /// Reads `len` bytes from `lba` into a new vector.
    pub fn read_vec(self: &Arc<Self>, lba: u64, len: usize, prio: IoPriority) -> Result<Vec<u8>, BlockError> {
        let mut v = Vec::new();
        v.try_reserve_exact(len).map_err(|_| BlockError::NoMemory)?;
        v.resize(len, 0);
        match self.submit(Kind::Read, lba, 0, Data::Bytes(v), prio)?.wait()? {
            Data::Bytes(v) => Ok(v),
            _ => Err(BlockError::Io),
        }
    }

    /// Reads into `buf` (one copy from the request's own buffer).
    pub fn read(self: &Arc<Self>, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), BlockError> {
        let v = self.read_vec(lba, buf.len(), prio)?;
        buf.copy_from_slice(&v);
        Ok(())
    }

    /// Writes `data` (owned) at `lba`; gives the buffer back.
    pub fn write_vec(self: &Arc<Self>, lba: u64, data: Vec<u8>, fua: bool, prio: IoPriority) -> Result<Vec<u8>, BlockError> {
        match self.submit(Kind::Write { fua, preflush: false }, lba, 0, Data::Bytes(data), prio)?.wait()? {
            Data::Bytes(v) => Ok(v),
            _ => Ok(Vec::new()),
        }
    }

    pub fn write(self: &Arc<Self>, lba: u64, data: &[u8], fua: bool, prio: IoPriority) -> Result<(), BlockError> {
        let mut v = Vec::new();
        v.try_reserve_exact(data.len()).map_err(|_| BlockError::NoMemory)?;
        v.extend_from_slice(data);
        self.write_vec(lba, v, fua, prio).map(|_| ())
    }

    /// Reads into page-cache pages (scatter): the segments together cover whole blocks from `lba`.
    pub fn read_pages(self: &Arc<Self>, lba: u64, segs: Vec<PageSeg>, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Read, lba, 0, Data::Pages(segs), prio)?.wait().map(|_| ())
    }

    /// Writes page-cache pages (gather). `fua`: durable on completion.
    pub fn write_pages(self: &Arc<Self>, lba: u64, segs: Vec<PageSeg>, fua: bool, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Write { fua, preflush: false }, lba, 0, Data::Pages(segs), prio)?.wait().map(|_| ())
    }

    /// The device cache flushed, then `data` written ("flush with data", a journal commit).
    pub fn write_preflush(self: &Arc<Self>, lba: u64, data: Vec<u8>, fua: bool, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Write { fua, preflush: true }, lba, 0, Data::Bytes(data), prio)?.wait().map(|_| ())
    }

    pub fn flush(self: &Arc<Self>, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Flush, 0, 0, Data::None, prio)?.wait().map(|_| ())
    }

    /// fsync's last step (INT-W12, VS-2b CR 3): every flush answered before is durable (`BlockDevice::settle`).
    /// Runs on the caller's thread, not through the dispatcher, so a device that waits here (bitsvol until a
    /// snapshot's commit) never holds up other requests.
    pub fn settle(&self) -> Result<(), BlockError> {
        self.dev.settle().map_err(BlockError::from_dev)
    }

    pub fn discard(self: &Arc<Self>, lba: u64, blocks: u64, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Discard, lba, blocks, Data::None, prio)?.wait().map(|_| ())
    }

    // ---- dispatching

    fn limits(&self) -> Limits {
        let info = self.dev.info();
        Limits {
            max_blocks: (info.max_blocks_per_request as u64).max(1),
            max_discard_blocks: u64::MAX >> 1,
            age_ns: self.age_ns.load(Ordering::Relaxed),
        }
    }

    fn has_work(&self) -> bool {
        let q = self.q.lock();
        !q.elev.is_empty() || q.stopped
    }

    /// The next step for the dispatcher; None when the queue is empty. Every step takes at least one request
    /// out of the queue, so `drain` ends even when memory has run out (no spinning on a queue it cannot serve).
    fn next(&self) -> Option<Step> {
        let lim = self.limits();
        let now = now();
        let mut q = self.q.lock();
        let b = match q.elev.pick(now, &lim)? {
            Ok(b) => b,
            Err(e) => {
                // No memory for the elevator's working lists: the chosen request fails with ENOMEM.
                let r = q.reqs.iter().position(|r| r.id == e.id).map(|i| q.reqs.remove(i));
                drop(q);
                if let Some(r) = r {
                    self.finish(&r, Err(BlockError::NoMemory));
                }
                return Some(Step::NoMemory { kind: e.kind, lba: e.lba, blocks: e.blocks, members: 1 });
            }
        };
        let mut reqs = Vec::new();
        if reqs.try_reserve_exact(b.members.len()).is_err() {
            // No memory for a list of pointers: the members fail with ENOMEM (nothing allocates here).
            for m in &b.members {
                if let Some(i) = q.reqs.iter().position(|r| r.id == m.id) {
                    let r = q.reqs.remove(i);
                    self.finish(&r, Err(BlockError::NoMemory));
                }
            }
            return Some(Step::NoMemory { kind: b.kind, lba: b.lba, blocks: b.blocks, members: b.members.len() });
        }
        for m in &b.members {
            if let Some(r) = q.reqs.iter().find(|r| r.id == m.id) {
                reqs.push(r.clone());
            }
        }
        Some(Step::Run(b, reqs))
    }

    /// Takes and runs batches until the queue is empty.
    fn drain(&self) {
        while let Some(step) = self.next() {
            match step {
                Step::Run(b, reqs) => self.execute(&b, &reqs),
                Step::NoMemory { kind, lba, blocks, members } => {
                    ERRORS.add(members as u64);
                    self.stats.errors.fetch_add(members as u64, Ordering::Relaxed);
                    klog::event!(Warn, "block", "req.nomem", disk = self.name.clone(), op = kind.as_str(), lba = lba, blocks = blocks,
                        members = members; "no memory to build the batch: its requests fail with ENOMEM");
                }
            }
        }
    }

    /// Host tests: the next `n` picks of the elevator find no memory.
    #[cfg(test)]
    pub(crate) fn inject_pick_oom(&self, n: u32) {
        self.q.lock().elev.oom_picks = n;
    }

    fn worker(self: Arc<Self>) {
        klog::event!(Debug, "block", "worker.started", disk = self.name.clone());
        loop {
            self.drain();
            if self.q.lock().stopped {
                break;
            }
            self.work.wait_until_timeout(|| self.has_work(), IDLE_NS);
        }
        self.fail_queued();
        self.worker_done.store(true, Ordering::Release);
        klog::event!(Debug, "block", "worker.ended", disk = self.name.clone());
    }

    /// Waits for `req` (inline: dispatching in turn with the other waiters). True when it completed.
    fn wait_req(&self, req: &Arc<Req>, timeout_ns: u64) -> bool {
        if self.mode.load(Ordering::Acquire) != MODE_INLINE {
            return req.done.wait_timeout(timeout_ns);
        }
        let start = now();
        let mut spins: u64 = 0;
        loop {
            if req.done.is_done() {
                return true;
            }
            if self.dispatching.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                self.drain();
                self.dispatching.store(false, Ordering::Release);
                continue;
            }
            // Without a clock (host) the deadline counts polls instead.
            let elapsed = match ksync::now_ns() {
                Some(n) => n.saturating_sub(start),
                None => {
                    spins += 1;
                    spins.saturating_mul(INLINE_POLL_NS)
                }
            };
            if elapsed >= timeout_ns {
                return req.done.is_done();
            }
            if req.done.wait_timeout(INLINE_POLL_NS.min(timeout_ns - elapsed)) {
                return true;
            }
        }
    }

    /// Takes a request that has not reached the device out of the queue. True when it was still queued.
    fn cancel(&self, req: &Arc<Req>) -> bool {
        let removed = {
            let mut q = self.q.lock();
            let r = q.elev.remove(req.id).is_some();
            if r {
                q.reqs.retain(|x| x.id != req.id);
            }
            r
        };
        if removed {
            TIMEOUTS.inc();
            self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
            klog::event!(Warn, "block", "req.timeout", disk = self.name.clone(), op = req.kind.as_str(), lba = req.lba, blocks = req.blocks;
                "taken out of the queue before the device got it");
            self.finish(req, Err(BlockError::Timeout));
        }
        removed
    }

    fn finish(&self, req: &Arc<Req>, r: Result<(), BlockError>) {
        *req.result.lock() = Some(r);
        req.done.complete();
    }

    fn execute(&self, b: &Batch, reqs: &[Arc<Req>]) {
        let t0 = now();
        self.execute_inner(b, reqs);
        self.stats.busy_ns.fetch_add(now().saturating_sub(t0), Ordering::Relaxed);
    }

    fn execute_inner(&self, b: &Batch, reqs: &[Arc<Req>]) {
        DISPATCHES.inc();
        self.stats.dispatches.fetch_add(1, Ordering::Relaxed);
        if reqs.len() > 1 {
            MERGED.add(reqs.len() as u64 - 1);
            self.stats.merged.fetch_add(reqs.len() as u64 - 1, Ordering::Relaxed);
        }
        if b.aged {
            AGED.inc();
            self.stats.aged.fetch_add(1, Ordering::Relaxed);
        }
        let r = self.run(b.kind, b.lba, b.blocks, b.prio, reqs);
        if r.is_err() && reqs.len() > 1 && b.kind != Kind::Flush {
            SPLIT_RETRIES.inc();
            self.stats.merge_retries.fetch_add(1, Ordering::Relaxed);
            klog::event!(Info, "block", "req.merged_failed", disk = self.name.clone(), op = b.kind.as_str(), lba = b.lba, blocks = b.blocks,
                members = reqs.len(), error = r.err().map_or("", |e| e.as_str()); "each member tried alone");
            for req in reqs {
                let one = self.run(req.kind, req.lba, req.blocks, b.prio, core::slice::from_ref(req));
                self.report(req.kind, req.lba, req.blocks, one);
                self.done(req, one);
            }
        } else {
            self.report(b.kind, b.lba, b.blocks, r);
            for req in reqs {
                self.done(req, r);
            }
        }
    }

    fn report(&self, kind: Kind, lba: u64, blocks: u64, r: Result<(), BlockError>) {
        if let Err(e) = r {
            ERRORS.inc();
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            klog::event!(Warn, "block", "req.failed", disk = self.name.clone(), op = kind.as_str(), lba = lba, blocks = blocks, error = e.as_str());
        }
    }

    fn done(&self, req: &Arc<Req>, r: Result<(), BlockError>) {
        if r.is_ok() {
            let bytes = req.blocks * self.block_size as u64;
            match req.kind {
                Kind::Read => self.stats.read_bytes.fetch_add(bytes, Ordering::Relaxed),
                Kind::Write { .. } => self.stats.write_bytes.fetch_add(bytes, Ordering::Relaxed),
                Kind::Discard | Kind::Flush => 0,
            };
        }
        self.q.lock().reqs.retain(|x| x.id != req.id);
        self.finish(req, r);
    }

    /// One device operation for `reqs` (one request, or the members of a merged batch in LBA order).
    fn run(&self, kind: Kind, lba: u64, blocks: u64, prio: IoPriority, reqs: &[Arc<Req>]) -> Result<(), BlockError> {
        let dev = |r: Result<(), DevError>| r.map_err(BlockError::from_dev);
        match kind {
            Kind::Flush => dev(self.dev.flush()),
            Kind::Discard => dev(self.dev.discard(lba, blocks)),
            Kind::Read | Kind::Write { .. } => {
                let (write, fua) = match kind {
                    Kind::Write { fua, preflush } => {
                        if preflush {
                            dev(self.dev.flush())?;
                        }
                        (true, fua)
                    }
                    _ => (false, false),
                };
                // The members' memory, taken out for the time of the device call.
                let mut datas: Vec<Data> = Vec::new();
                datas.try_reserve_exact(reqs.len()).map_err(|_| BlockError::NoMemory)?;
                for r in reqs {
                    datas.push(core::mem::take(&mut *r.data.lock()));
                }
                let res = self.move_data(write, fua, lba, blocks, prio, reqs, &mut datas);
                for (r, d) in reqs.iter().zip(datas) {
                    *r.data.lock() = d;
                }
                res
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn move_data(&self, write: bool, fua: bool, lba: u64, blocks: u64, prio: IoPriority, reqs: &[Arc<Req>], datas: &mut [Data]) -> Result<(), BlockError> {
        let bs = self.block_size as u64;
        // One request with a plain buffer: straight through.
        if let [Data::Bytes(b)] = datas {
            let r = if write { self.dev.write_blocks(lba, b, fua, prio) } else { self.dev.read_blocks(lba, b, prio) };
            return r.map_err(BlockError::from_dev);
        }
        let max = (self.dev.info().max_blocks_per_request as u64).max(1);
        let chunk = max.max(CHUNK_BYTES / bs).min(blocks.max(1));
        let mut bounce: Vec<u8> = Vec::new();
        let cap = usize::try_from(chunk * bs).map_err(|_| BlockError::NoMemory)?;
        bounce.try_reserve_exact(cap).map_err(|_| BlockError::NoMemory)?;
        let end = lba + blocks;
        let mut at = lba;
        while at < end {
            let n = chunk.min(end - at);
            let len = (n * bs) as usize;
            bounce.clear();
            bounce.resize(len, 0);
            let overlap = |r: &Req| -> Option<(u64, u64)> {
                let (lo, hi) = (r.lba.max(at), r.end().min(at + n));
                (lo < hi).then_some((lo, hi))
            };
            if write {
                for (r, d) in reqs.iter().zip(datas.iter()) {
                    if let Some((lo, hi)) = overlap(r) {
                        d.copy_out(((lo - r.lba) * bs) as usize, &mut bounce[((lo - at) * bs) as usize..((hi - at) * bs) as usize]);
                    }
                }
                self.dev.write_blocks(at, &bounce, fua, prio).map_err(BlockError::from_dev)?;
            } else {
                self.dev.read_blocks(at, &mut bounce, prio).map_err(BlockError::from_dev)?;
                for (r, d) in reqs.iter().zip(datas.iter_mut()) {
                    if let Some((lo, hi)) = overlap(r) {
                        d.copy_in(((lo - r.lba) * bs) as usize, &bounce[((lo - at) * bs) as usize..((hi - at) * bs) as usize]);
                    }
                }
            }
            at += n;
        }
        Ok(())
    }

    /// Ends the disk: queued requests fail with `Gone`, new ones are refused, the dispatcher thread ends.
    /// Requests the device has already finish normally.
    pub fn stop(&self) {
        {
            let mut q = self.q.lock();
            if q.stopped {
                return;
            }
            q.stopped = true;
        }
        self.fail_queued();
        self.work.wake_all();
    }

    fn fail_queued(&self) {
        let failed: Vec<Arc<Req>> = {
            let mut q = self.q.lock();
            let ids: Vec<u64> = q.elev.entries().iter().map(|e| e.id).collect();
            let mut out = Vec::new();
            for id in ids {
                q.elev.remove(id);
                if let Some(i) = q.reqs.iter().position(|r| r.id == id) {
                    out.push(q.reqs.remove(i));
                }
            }
            out
        };
        for r in &failed {
            self.finish(r, Err(BlockError::Gone));
        }
    }
}

fn refusal_bit(r: Refusal) -> u32 {
    match r {
        Refusal::DeviceReadOnly => 0,
        Refusal::WindowsPartition => 1,
        Refusal::EfiPartition => 2,
        Refusal::UnknownTable => 3,
        Refusal::BootStick => 4,
        Refusal::UsbWritesOff => 5,
        Refusal::NvmeWritesOff => 6,
        Refusal::UnknownKind => 7,
        Refusal::Gate(_) => 8,
    }
}

pub fn stick_str(s: StickMatch) -> &'static str {
    match s {
        StickMatch::Yes => "yes",
        StickMatch::No => "no",
        StickMatch::Unknown => "unknown",
    }
}

/// A `Disk` is a `C: BlockDevice` itself, so partitions, crypt and bitsvol stack on it and their writes pass
/// its hook. Byte-slice calls copy once through the request's own buffer.
pub struct DiskDevice(pub Arc<Disk>);

impl BlockDevice for DiskDevice {
    fn name(&self) -> &str {
        &self.0.name
    }
    fn info(&self) -> BlockInfo {
        let mut i = self.0.dev.info();
        i.read_only |= self.0.write_policy().is_err();
        i
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        self.0.read(lba, buf, prio).map_err(BlockError::to_dev)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        self.0.write(lba, buf, fua, prio).map_err(BlockError::to_dev)
    }
    fn flush(&self) -> Result<(), DevError> {
        self.0.flush(IoPriority::Normal).map_err(BlockError::to_dev)
    }
    fn settle(&self) -> Result<(), DevError> {
        self.0.settle().map_err(BlockError::to_dev)
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        self.0.discard(lba, blocks, IoPriority::Background).map_err(BlockError::to_dev)
    }
}

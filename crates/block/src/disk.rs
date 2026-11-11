//! A registered disk: one `C: BlockDevice` with its request queue (`queue::Elevator`), its partition-table
//! verdict and its write-refusal hook.
//!
//! - **Submitting** (`submit`, or the waiting helpers `read_vec`, `read`, `write`, `read_pages`, `read_frames`,
//!   `write_pages`, `flush`, `discard`): the request is checked (block size, range: a write past the end is
//!   `NoSpace`, a read `BeyondEnd`; writes, FUA/preflush writes and discards through the hook: `ReadOnly(why)` =
//!   EROFS), then queued with its priority. Every request **owns** its memory (a byte vector, or frames:
//!   page-cache pages by `Arc`), so a caller that gives up (deadline) never leaves the device writing into freed
//!   memory.
//! - **Dispatching (BL-2, Linux 7.x block/blk-mq.c `blk_mq_submit_bio`, plugging):** there is no dispatcher
//!   thread. What a caller submits waits in the queue until it waits for it (the plug: requests submitted together
//!   merge and go side by side); the waiting caller then takes one of the device's slots (`queue_depth`: an NVMe
//!   controller's tags, one for a device that takes one call at a time), picks the next batch by the elevator's
//!   rules (priority with aging, merging up to max_sectors, an older overlapping write first) and calls the device
//!   itself. Reads into frames go to the device together, as many as slots are free (one `read_frames` call: NVMe
//!   puts them all in flight). A dispatcher stops once its own request is answered and wakes the next sleeping
//!   waiter whenever a slot comes free, so nobody serves other callers' backlog for long (B4). A merged batch that
//!   fails is retried member by member, so one bad block fails only the request that holds it. An unwaited ticket
//!   is dispatched when dropped. `run_queue` dispatches without a request of one's own (tests holding a device).
//! - **Stacked disks** (partitions, bitsvol volumes, `DiskKind::Stacked`) only remap: no elevator, no merging, no
//!   slot limit; the caller hands its own requests to the device below (Linux `submit_bio_noacct`'s remap), the
//!   frames unchanged. Merging happens at the bottom only.
//! - **Waiting:** every request has a `Completion`; the waiter sleeps with a deadline (B4: default 120 s,
//!   `set_timeout_ns`), looking at least every `POLL_NS`. On a timeout the request is taken out of the queue, or,
//!   if the device has it already, its answer is dropped (`Timeout` = EIO to the caller).
//! - **Byte reads** (`read`) go straight into the caller's buffer when the caller can call the device at once (a
//!   stacked disk, or a free slot with nothing queued); otherwise through the request's own buffer (one copy,
//!   `block.copy_bytes.disk_copy`).
//!
//! Locks: `block.queue` (spin; the elevator, the request list and what is at the device; never held across a
//! device call, a wait or a klog event), `block.table`, `block.disk.gates`, a request's `block.req.data`/
//! `block.req.result` (spin, leaves). A device is called with no lock of this crate held.

mod dispatch;

use crate::error::{BlockError, Refusal};
use crate::frames::PageFrames;
use crate::policy::{self, DiskKind, StickMatch, WriteCheck, WriteGate};
use crate::queue::{Elevator, Entry, Kind};
use crate::table::{self, Scan};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use devmodel::block::{BlockDevice, BlockInfo, FrameIo, IoBuf, IoPriority};
use devmodel::DevError;
use ksync::{Completion, SpinLock, WaitQueue};

pub use crate::frames::PageSeg;

/// Default deadline of a request (B4).
pub const DEFAULT_TIMEOUT_NS: u64 = 120_000_000_000;
/// Default aging step: a request waiting this long competes one class higher.
pub const DEFAULT_AGE_NS: u64 = 250_000_000;
/// Bounce chunks are at least this big (usb-msd chains its commands up to 1 MiB per run).
pub const CHUNK_BYTES: u64 = 1 << 20;
/// A waiter looks this often by itself (a hand-off it missed, its deadline, a disk without a clock).
const POLL_NS: u64 = 10_000_000;

klog::counter!(BOUNCE_SHRUNK, "block.bounce_shrunk");
klog::counter!(SUBMITTED, "block.requests");
klog::counter!(MERGED, "block.merged");
klog::counter!(DISPATCHES, "block.dispatches");
klog::counter!(ERRORS, "block.errors");
klog::counter!(REFUSED, "block.writes_refused");
klog::counter!(TIMEOUTS, "block.timeouts");
klog::counter!(AGED, "block.aged");
klog::counter!(SPLIT_RETRIES, "block.merge_retries");
// BL-2: byte reads that went straight into the caller's buffer; groups of reads given to a device in one call.
vfs::pcpu_counter!(DIRECT_READS, "block.direct_reads", vfs::pcpu::EVENTS_BATCH);
klog::counter!(GROUPS, "block.read_groups");
// BL-0: bytes copied per copy site of the read path (plan/notes/bl-BL-0.md), per CPU.
vfs::pcpu_counter!(COPY_READVEC_ZERO, "block.copy_bytes.readvec_zero", vfs::pcpu::BYTES_BATCH);
vfs::pcpu_counter!(COPY_DISK, "block.copy_bytes.disk_copy", vfs::pcpu::BYTES_BATCH);
vfs::pcpu_counter!(COPY_BOUNCE, "block.copy_bytes.bounce", vfs::pcpu::BYTES_BATCH);
// BL-0: device operations of all disks under way now (a gauge: it goes down again) and the most one disk ever had
// at once (per disk: `DiskStats::inflight`, `inflight_max`).
klog::counter!(INFLIGHT, "block.inflight");
klog::counter!(INFLIGHT_MAX, "block.inflight_max");
static INFLIGHT_MAX_SEEN: AtomicU64 = AtomicU64::new(0);

/// BL-0: `n` device operations of a disk under way (one per batch), counted at the bottom of that disk, after the
/// elevator merged its requests: the queue depth the device sees. BL-2: also the disk's busy time (Linux
/// `io_ticks`: wall time with at least one operation at the device).
struct InFlight<'a>(&'a Stats, u64);

impl<'a> InFlight<'a> {
    fn new(s: &'a Stats, n: u64) -> InFlight<'a> {
        let before = s.inflight.fetch_add(n, Ordering::Relaxed);
        if before == 0 {
            s.busy_since.store(now(), Ordering::Relaxed);
        }
        let now = before + n;
        INFLIGHT.add(n);
        if now > s.inflight_max.load(Ordering::Relaxed) {
            s.inflight_max.fetch_max(now, Ordering::Relaxed);
            let seen = INFLIGHT_MAX_SEEN.fetch_max(now, Ordering::Relaxed);
            if now > seen {
                INFLIGHT_MAX.add(now - seen);
            }
        }
        InFlight(s, n)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if self.0.inflight.fetch_sub(self.1, Ordering::Relaxed) == self.1 {
            let since = self.0.busy_since.load(Ordering::Relaxed);
            self.0.busy_ns.fetch_add(now().saturating_sub(since), Ordering::Relaxed);
        }
        // The shared total goes down by wrapping (the counter only adds).
        INFLIGHT.add(self.1.wrapping_neg());
    }
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// The memory a request owns.
#[derive(Default)]
pub enum Data {
    #[default]
    None,
    Bytes(Vec<u8>),
    /// Page-cache pages; turned into `Frames` when the request is queued (`wait` gives `Frames` back).
    Pages(Vec<PageSeg>),
    /// BL-2: frames passed down by the layer above (or the pages above, as frames).
    Frames(IoBuf),
}

impl Data {
    pub fn len(&self) -> usize {
        match self {
            Data::None => 0,
            Data::Bytes(b) => b.len(),
            Data::Pages(p) => p.iter().map(|s| s.len).sum(),
            Data::Frames(f) => f.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `out.len()` bytes from offset `at` of the data into `out` (the bounce of a mixed batch).
    fn copy_out(&self, at: usize, out: &mut [u8]) {
        match self {
            Data::None | Data::Pages(_) => {}
            Data::Bytes(b) => {
                if let Some(s) = b.get(at..at + out.len()) {
                    out.copy_from_slice(s);
                }
            }
            Data::Frames(f) => {
                f.read(at, out);
            }
        }
    }

    /// Copies `src` into the data at offset `at`.
    fn copy_in(&mut self, at: usize, src: &[u8]) {
        match self {
            Data::None | Data::Pages(_) => {}
            Data::Bytes(b) => {
                if let Some(s) = b.get_mut(at..at + src.len()) {
                    s.copy_from_slice(src);
                }
            }
            Data::Frames(f) => {
                f.write(at, src);
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
    /// In the elevator now (not held back, not at the device, not answered).
    queued: AtomicBool,
    /// Its waiter sleeps on `kick` (a slot that comes free wakes such a one, BL-2).
    sleeping: AtomicBool,
    kick: WaitQueue,
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
    /// What the device has now (members of batches, direct reads): a younger request overlapping one of them
    /// with a write involved waits (the elevator's order rule across slots).
    inflight: Vec<Entry>,
    /// Byte reads straight into a caller's buffer under way (`Disk::read`; no request of their own).
    direct: usize,
    stopped: bool,
    /// RB-14: closed by the storage stop before a reset (`shutdown`): new requests are held back (`held`), what was
    /// queued before still runs (its waiters dispatch it), `reopen` undoes it.
    closed: bool,
    /// RB-14: requests submitted while closed, oldest first. They never reach the device before `reopen` queues
    /// them; their waiters keep their own deadline (`cancel` takes them out), `stop` fails them with `Gone`.
    /// Held rather than refused: a Reload that does not jump resumes, and a write refused in the stop's few
    /// milliseconds would have turned ext4 read-only (errors=remount-ro) for the rest of the boot.
    held: Vec<(Entry, Arc<Req>)>,
}

struct TableState {
    scan: Scan,
    /// Disk size when the table was read (a different size later = a new medium: read again).
    blocks: u64,
    stick: StickMatch,
}

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
    /// Time with at least one request at the device (Linux /proc/diskstats `io_ticks`, in ns).
    pub busy_ns: u64,
    /// BL-0: device operations of this disk under way now, and the most at once since it opened.
    pub inflight: u64,
    pub inflight_max: u64,
    /// BL-2: bytes this disk copied on reads (a zeroed request buffer, its copy to the caller, a bounce): the
    /// sites of `block.copy_bytes.{readvec_zero, disk_copy, bounce}`, for this disk alone.
    pub copied_bytes: u64,
}

#[derive(Default)]
struct Stats {
    inflight: AtomicU64,
    inflight_max: AtomicU64,
    busy_since: AtomicU64,
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
    copied: AtomicU64,
}

pub struct Disk {
    name: String,
    dev: Arc<dyn BlockDevice>,
    kind: DiskKind,
    block_size: u32,
    q: SpinLock<Queue>,
    /// BL-2: calls the device takes at once (`BlockDevice::queue_depth`; unbounded for a stacked disk), and how
    /// many dispatchers hold one now.
    slots: u32,
    active: AtomicU32,
    /// A stacked disk: no elevator, the caller hands its requests down itself.
    passthrough: bool,
    /// Grows whenever a request leaves the device (a waiter the order rule held back looks again).
    progress: AtomicU64,
    next_id: AtomicU64,
    table: SpinLock<TableState>,
    gates: SpinLock<Vec<Arc<dyn WriteGate>>>,
    /// NV-2p: the armed window `(first, blocks)`, only a `PartPort` of exactly this range writes (gpt/window.rs).
    /// While it is armed the whole-disk device refuses every write, inside the range too.
    window: SpinLock<Option<(u64, u64)>>,
    /// The EFI System partition `nvme_boot=` names, allowed beside the armed window (set with it).
    esp: SpinLock<Option<(u64, u64)>>,
    /// That ESP's own window, open while fs-fat has it as /boot (`open_boot_window`); only beside `window`.
    boot: SpinLock<Option<(u64, u64)>>,
    /// The layer below that keeps its own copy of the windows (the NVMe controller's command builder).
    lower: SpinLock<Option<Arc<dyn LowerWindow>>>,
    age_ns: AtomicU64,
    timeout_ns: AtomicU64,
    /// VS-H CR 5: the last failed request of this disk ended in a timeout, not in a media error (`take_timed_out`).
    timed_out: AtomicBool,
    stats: Stats,
    /// One bit per refusal reason already logged (one `write.refused` event per reason and disk).
    logged: AtomicU32,
    /// RB-14: a write completed since boot / since the last completed flush, and when the last one completed.
    written: AtomicBool,
    dirty: AtomicBool,
    last_write_ns: AtomicU64,
    /// Writes (not FUA) completed so far: a flush makes the cache clean only if none completed while it ran (with
    /// several slots a write may complete beside a flush).
    write_gen: AtomicU64,
}

/// A queued request; `wait` gives its data back (the bytes read, or the buffer written). Dropped without a wait,
/// it is dispatched and waited for there (BL-2: its submitter is its dispatcher).
pub struct Ticket {
    disk: Arc<Disk>,
    req: Arc<Req>,
    waited: bool,
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
    pub fn wait_timeout(mut self, timeout_ns: u64) -> Result<Data, BlockError> {
        self.waited = true;
        self.disk.settle_req(&self.req, timeout_ns)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.waited && !self.req.done.is_done() {
            let t = self.disk.timeout_ns.load(Ordering::Relaxed);
            let _ = self.disk.settle_req(&self.req, t);
        }
    }
}

/// How a disk dispatches. BL-2: always in its submitters (there is no dispatcher thread any more); both names are
/// kept so callers keep building, and mean the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// Formerly a `blk:<name>` thread; now as `Inline`.
    Thread,
    /// The waiting callers dispatch.
    Inline,
}

impl Disk {
    /// Puts a queue in front of `dev` (not in the registry: `crate::register` does that) and reads its partition
    /// table.
    pub fn open(dev: Arc<dyn BlockDevice>, kind: DiskKind, _dispatch: Dispatch) -> Result<Arc<Disk>, BlockError> {
        let info = dev.info();
        if !matches!(info.block_size, 512 | 1024 | 2048 | 4096) {
            klog::event!(Warn, "block", "disk.bad_block_size", disk = String::from(dev.name()), block_size = info.block_size);
            return Err(BlockError::Invalid);
        }
        let passthrough = kind == DiskKind::Stacked;
        let slots = if passthrough { u32::MAX } else { dev.queue_depth().max(1) };
        let d = Arc::new(Disk {
            name: String::from(dev.name()),
            dev,
            kind,
            block_size: info.block_size,
            q: SpinLock::named("block.queue", Queue { elev: Elevator::new(), reqs: Vec::new(), inflight: Vec::new(), direct: 0, stopped: false, closed: false, held: Vec::new() }),
            slots,
            active: AtomicU32::new(0),
            passthrough,
            progress: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            table: SpinLock::named("block.table", TableState { scan: Scan::default(), blocks: info.blocks, stick: StickMatch::Unknown }),
            gates: SpinLock::named("block.disk.gates", Vec::new()),
            window: SpinLock::named("block.disk.window", None),
            esp: SpinLock::named("block.disk.esp", None),
            boot: SpinLock::named("block.disk.boot", None),
            lower: SpinLock::named("block.disk.lower", None),
            age_ns: AtomicU64::new(DEFAULT_AGE_NS),
            timeout_ns: AtomicU64::new(DEFAULT_TIMEOUT_NS),
            timed_out: AtomicBool::new(false),
            stats: Stats::default(),
            logged: AtomicU32::new(0),
            written: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            last_write_ns: AtomicU64::new(0),
            write_gen: AtomicU64::new(0),
        });
        klog::event!(Debug, "block", "disk.dispatch", disk = d.name.clone(), slots = slots as u64, stacked = passthrough;
            "BL-2: requests are dispatched by their submitters");
        d.rescan();
        Ok(d)
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
    /// BL-4 CR 4: the device's largest request in 4 KiB pages (Linux `bdi->io_pages` from max_sectors,
    /// linux-7/block/blk-settings.c); NVMe: MDTS. A stacked disk reports its lowest device's.
    pub fn request_pages(&self) -> u64 {
        let i = self.dev.info();
        (i.max_blocks_per_request as u64).saturating_mul(i.block_size as u64) / 4096
    }
    /// BL-2: calls the device takes at once (u32::MAX for a stacked disk: it only remaps).
    pub fn slots(&self) -> u32 {
        self.slots
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
    /// The dispatcher thread ended (or never ran). BL-2: there is none, always true.
    pub fn worker_done(&self) -> bool {
        true
    }

    pub fn stats(&self) -> DiskStats {
        let s = &self.stats;
        let mut busy = s.busy_ns.load(Ordering::Relaxed);
        let inflight = s.inflight.load(Ordering::Relaxed);
        if inflight > 0 {
            busy += now().saturating_sub(s.busy_since.load(Ordering::Relaxed));
        }
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
            busy_ns: busy,
            inflight,
            inflight_max: s.inflight_max.load(Ordering::Relaxed),
            copied_bytes: s.copied.load(Ordering::Relaxed),
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
        // NV-2p: a table that no longer holds the armed window ends it; the next boot checks everything anyway. The
        // window is read into a local first: the lock guard of an `if let` scrutinee lives through the body, and the
        // body takes the same lock (this spun forever before 6.11.2026; a lost window had never happened yet).
        let armed = *self.window.lock();
        if let Some(w) = armed
            && (crate::gpt::window::foreign_esp(&scan, *self.esp.lock()) || !scan.gpt.as_ref().is_some_and(|g| g.entries.iter().any(|e| e.first == w.0 && e.last.checked_sub(e.first).and_then(|d| d.checked_add(1)) == Some(w.1))))
        {
            klog::event!(Warn, "block", "window.lost", disk = self.name.clone(), first = w.0, blocks = w.1; "the partition table changed under the armed window: the disk is read-only now");
            *self.window.lock() = None;
            self.close_boot_window();
        }
        let mut t = self.table.lock();
        t.scan = scan;
        t.blocks = info.blocks;
        t.stick = stick;
    }

    /// NV-2p: only the blocks `first..first + blocks` take writes, and only through a `PartPort` of that range
    /// (the driver calls this after `gpt::window::validate`, and arms its own command builder too).
    /// `esp`: the EFI System partition `nvme_boot=` names (never written, only allowed to be there).
    pub fn arm_window(&self, first: u64, blocks: u64, esp: Option<(u64, u64)>) {
        klog::event!(Info, "block", "window.armed", disk = self.name.clone(), first = first, blocks = blocks, esp = esp.is_some(); "NV-2p: one partition of this disk takes writes, everything else on it is refused");
        // A new window is checked anew: the boot ESP's is opened again only by its own check.
        self.close_boot_window();
        *self.esp.lock() = esp;
        *self.window.lock() = Some((first, blocks));
    }

    /// NV-2p: the disk is read-only as a whole again.
    pub fn disarm_window(&self) {
        self.close_boot_window();
        *self.esp.lock() = None;
        if self.window.lock().take().is_some() {
            klog::event!(Info, "block", "window.disarmed", disk = self.name.clone());
        }
    }

    pub fn window(&self) -> Option<(u64, u64)> {
        *self.window.lock()
    }

    /// The ESP `nvme_boot=` names, as the window was armed with it.
    pub fn named_esp(&self) -> Option<(u64, u64)> {
        *self.esp.lock()
    }

    /// The layer below that must open and close the boot ESP's window with this disk (the driver sets it).
    pub fn set_lower(&self, l: Arc<dyn LowerWindow>) {
        *self.lower.lock() = Some(l);
    }

    /// Opens the second window, exactly the named ESP `first..first + blocks`, for /boot's writes there (fs-fat,
    /// after `gpt::window::boot_esp` and its own identity rules passed). Only beside the armed window, only the ESP it
    /// was armed with, only when the layer below opens it too. Err: why it stays shut.
    pub fn open_boot_window(&self, first: u64, blocks: u64) -> Result<(), &'static str> {
        let Some(w) = self.window() else { return Err("no nvme_data= window is armed on this disk") };
        if self.named_esp() != Some((first, blocks)) {
            return Err("the range is not the EFI System partition nvme_boot= names");
        }
        if first < w.0.saturating_add(w.1) && w.0 < first.saturating_add(blocks) {
            return Err("the boot partition overlaps nvme_data='s window");
        }
        {
            let t = self.table.lock();
            if crate::gpt::window::foreign_esp(&t.scan, Some((first, blocks))) {
                return Err("the disk holds another EFI System partition");
            }
            // A port is known by its range only: a second listed entry of that range would share the window.
            if t.scan.parts.iter().filter(|p| (p.0, p.1) == (first, blocks)).count() != 1 {
                return Err("not exactly one partition of the disk has the boot partition's range");
            }
        }
        let lower = self.lower.lock().clone();
        if lower.is_some_and(|l| !l.open_boot(first, blocks)) {
            return Err("the controller did not open the boot partition's window");
        }
        *self.boot.lock() = Some((first, blocks));
        klog::event!(Info, "block", "window.boot_opened", disk = self.name.clone(), first = first, blocks = blocks; "the boot partition nvme_boot= names takes /boot's writes too");
        Ok(())
    }

    /// Closes the boot ESP's window (both layers); nothing when it is shut.
    pub fn close_boot_window(&self) {
        let was = self.boot.lock().take();
        let lower = self.lower.lock().clone();
        if let Some(l) = lower {
            l.close_boot();
        }
        if let Some((first, blocks)) = was {
            klog::event!(Info, "block", "window.boot_closed", disk = self.name.clone(), first = first, blocks = blocks);
        }
    }

    pub fn boot_window(&self) -> Option<(u64, u64)> {
        *self.boot.lock()
    }

    /// NV-2p: what a partition of `first..first + blocks` is stacked on (instead of `DiskDevice`): it writes when
    /// the armed window is exactly its range.
    pub fn part_port(self: &Arc<Self>, first: u64, blocks: u64) -> PartPort {
        PartPort { disk: self.clone(), port: (first, blocks) }
    }

    /// The hook for a request through a partition's port: `gpt::window::port_rule` (either window), an EFI System
    /// partition on the disk other than the one `nvme_boot=` names (whatever was armed), then the gates as for every write.
    fn check_port(self: &Arc<Self>, op: Kind, port: (u64, u64), range: Option<(u64, u64)>) -> Result<(), Refusal> {
        let read_only = self.dev.info().read_only;
        let (window, boot) = (*self.window.lock(), *self.boot.lock());
        crate::gpt::window::port_rule(window, boot, port, read_only, range)?;
        if crate::gpt::window::foreign_esp(&self.table.lock().scan, *self.esp.lock()) {
            return Err(Refusal::EfiPartition);
        }
        self.ask_gates(op, range, self.gates.lock().clone())
    }

    fn ask_gates(&self, op: Kind, range: Option<(u64, u64)>, own: Vec<Arc<dyn WriteGate>>) -> Result<(), Refusal> {
        let w = WriteCheck { disk: &self.name, kind: self.kind, op, range };
        let mut gates = policy::global_gates();
        gates.extend(own);
        match gates.iter().find(|g| !g.allows(&w)) {
            Some(g) => Err(Refusal::Gate(g.name())),
            None => Ok(()),
        }
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
        // NV-2p: under an armed window the whole-disk device stays refused, inside the window's range too.
        if self.window.lock().is_some() {
            return Err(Refusal::Gate(crate::gpt::window::KEY));
        }
        self.ask_gates(op, range, own)
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
        self.submit_via(None, kind, lba, blocks, data, prio)
    }

    /// `port`: the request comes through a partition's `PartPort` of this range (NV-2p), which has its own rule.
    fn submit_via(self: &Arc<Self>, port: Option<(u64, u64)>, kind: Kind, lba: u64, blocks: u64, data: Data, prio: IoPriority) -> Result<Ticket, BlockError> {
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
            let rule = match port {
                Some(p) => self.check_port(kind, p, Some((lba, blocks))),
                None => self.check_write(kind, Some((lba, blocks))),
            };
            rule.map_err(|why| self.refused(why, kind, lba, blocks))?;
        }
        // BL-2: page-cache pages travel as frames (the device below reads straight into them).
        let data = match data {
            Data::Pages(segs) => Data::Frames(IoBuf::new(Arc::new(PageFrames::new(segs).ok_or(BlockError::Invalid)?))),
            d => d,
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Arc::new(Req {
            id,
            kind,
            lba,
            blocks,
            data: SpinLock::named("block.req.data", data),
            result: SpinLock::named("block.req.result", None),
            done: Completion::new(),
            queued: AtomicBool::new(false),
            sleeping: AtomicBool::new(false),
            kick: WaitQueue::new(),
        });
        {
            let mut q = self.q.lock();
            if q.stopped {
                return Err(BlockError::Gone);
            }
            let entry = Entry { id, kind, lba, blocks, prio, at_ns: now() };
            if q.closed {
                if q.held.try_reserve(1).is_err() {
                    return Err(BlockError::NoMemory);
                }
                q.held.push((entry, req.clone()));
            } else {
                if q.reqs.try_reserve(1).is_err() || q.elev.push(entry).is_err() {
                    return Err(BlockError::NoMemory);
                }
                q.reqs.push(req.clone());
                req.queued.store(true, Ordering::SeqCst);
            }
        }
        SUBMITTED.inc();
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        Ok(Ticket { disk: self.clone(), req, waited: false })
    }

    // ---- waiting helpers

    /// Reads `len` bytes from `lba` into a new vector.
    pub fn read_vec(self: &Arc<Self>, lba: u64, len: usize, prio: IoPriority) -> Result<Vec<u8>, BlockError> {
        let mut v = Vec::new();
        v.try_reserve_exact(len).map_err(|_| BlockError::NoMemory)?;
        v.resize(len, 0);
        COPY_READVEC_ZERO.add(len as u64);
        self.stats.copied.fetch_add(len as u64, Ordering::Relaxed);
        match self.submit(Kind::Read, lba, 0, Data::Bytes(v), prio)?.wait()? {
            Data::Bytes(v) => Ok(v),
            _ => Err(BlockError::Io),
        }
    }

    /// Reads into `buf`: straight into it when the caller can call the device at once (BL-2, `direct`), else
    /// through a request of its own (one copy from the request's buffer).
    pub fn read(self: &Arc<Self>, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), BlockError> {
        let bs = self.block_size as u64;
        let len = buf.len() as u64;
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(BlockError::Invalid);
        }
        let blocks = len / bs;
        if lba.checked_add(blocks).is_none_or(|e| e > self.dev.info().blocks) {
            return Err(BlockError::BeyondEnd);
        }
        if let Some(slot) = self.direct(lba, blocks, prio) {
            SUBMITTED.inc();
            DIRECT_READS.inc();
            self.stats.requests.fetch_add(1, Ordering::Relaxed);
            self.stats.dispatches.fetch_add(1, Ordering::Relaxed);
            DISPATCHES.inc();
            let r = {
                let _op = InFlight::new(&self.stats, 1);
                self.dev.read_blocks(lba, buf, prio).map_err(BlockError::from_dev)
            };
            drop(slot);
            self.report(Kind::Read, lba, blocks, r);
            if r.is_ok() {
                self.stats.read_bytes.fetch_add(len, Ordering::Relaxed);
            }
            return r;
        }
        let v = self.read_vec(lba, buf.len(), prio)?;
        buf.copy_from_slice(&v);
        COPY_DISK.add(buf.len() as u64);
        self.stats.copied.fetch_add(buf.len() as u64, Ordering::Relaxed);
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

    /// BL-2: reads into frames, one request per `FrameIo`. All are queued before the first is waited for (the
    /// plug), so adjacent ones merge at the bottom and the device gets the rest side by side; a stacked disk hands
    /// them all down in one call. The first error is the answer, once every request ended.
    pub fn read_frames(self: &Arc<Self>, reqs: &[FrameIo], prio: IoPriority) -> Result<(), BlockError> {
        self.frames_via(None, reqs, prio)
    }

    fn frames_via(self: &Arc<Self>, port: Option<(u64, u64)>, reqs: &[FrameIo], prio: IoPriority) -> Result<(), BlockError> {
        if let [one] = reqs {
            return self.submit_via(port, Kind::Read, one.lba, 0, Data::Frames(one.buf.clone()), prio)?.wait().map(|_| ());
        }
        let mut tickets: Vec<Ticket> = Vec::new();
        tickets.try_reserve_exact(reqs.len()).map_err(|_| BlockError::NoMemory)?;
        let mut err = None;
        for r in reqs {
            match self.submit_via(port, Kind::Read, r.lba, 0, Data::Frames(r.buf.clone()), prio) {
                Ok(t) => tickets.push(t),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        if self.passthrough {
            let mut own: Vec<Arc<Req>> = Vec::new();
            if own.try_reserve_exact(tickets.len()).is_ok() {
                own.extend(tickets.iter().map(|t| t.req.clone()));
                self.run_own(&own);
            }
        }
        for t in tickets {
            if let Err(e) = t.wait() {
                err.get_or_insert(e);
            }
        }
        err.map_or(Ok(()), Err)
    }

    /// BL-2: writes `buf`'s frames at `lba` (no bounce on the way down; the bottom device gathers if it must).
    pub fn write_frames(self: &Arc<Self>, lba: u64, buf: &IoBuf, fua: bool, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Write { fua, preflush: false }, lba, 0, Data::Frames(buf.clone()), prio)?.wait().map(|_| ())
    }

    /// Writes page-cache pages (gather). `fua`: durable on completion.
    pub fn write_pages(self: &Arc<Self>, lba: u64, segs: Vec<PageSeg>, fua: bool, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Write { fua, preflush: false }, lba, 0, Data::Pages(segs), prio)?.wait().map(|_| ())
    }

    /// The device cache flushed, then `data` written ("flush with data", a journal commit). Tests only until NV-2
    /// makes /data on NVMe writable and ext4's disk commit uses it (FS-8b open point 2, C1).
    pub fn write_preflush(self: &Arc<Self>, lba: u64, data: Vec<u8>, fua: bool, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Write { fua, preflush: true }, lba, 0, Data::Bytes(data), prio)?.wait().map(|_| ())
    }

    pub fn flush(self: &Arc<Self>, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Flush, 0, 0, Data::None, prio)?.wait().map(|_| ())
    }

    /// fsync's last step (INT-W12, VS-2b CR 3): every flush answered before is durable (`BlockDevice::settle`).
    /// Runs on the caller's thread, not through the queue, so a device that waits here (bitsvol until a
    /// snapshot's commit) never holds up other requests.
    pub fn settle(&self) -> Result<(), BlockError> {
        self.dev.settle().map_err(BlockError::from_dev)
    }

    /// VS-H CR 5: whether the last failed request ended in a timeout, and forgets it. A file system tells by this
    /// a slow device, where the same write can go again, from a media error: both answer EIO to programs.
    pub fn take_timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Relaxed) && self.timed_out.swap(false, Ordering::Relaxed)
    }

    pub fn discard(self: &Arc<Self>, lba: u64, blocks: u64, prio: IoPriority) -> Result<(), BlockError> {
        self.submit(Kind::Discard, lba, blocks, Data::None, prio)?.wait().map(|_| ())
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
/// its hook. Byte-slice reads go straight into the caller's buffer when they can (`Disk::read`), frames pass
/// down unchanged (BL-2).
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
    fn read_frames(&self, reqs: &[FrameIo], prio: IoPriority) -> Result<(), DevError> {
        self.0.read_frames(reqs, prio).map_err(BlockError::to_dev)
    }
    fn write_frames(&self, lba: u64, buf: &IoBuf, fua: bool, prio: IoPriority) -> Result<(), DevError> {
        self.0.write_frames(lba, buf, fua, prio).map_err(BlockError::to_dev)
    }
    fn queue_depth(&self) -> u32 {
        self.0.slots
    }
}

/// The layer below a disk that keeps its own copy of the windows (the NVMe controller's command builder, NV-2p): the
/// boot ESP's window opens and closes there with the disk's, so both layers allow the same ranges.
pub trait LowerWindow: Send + Sync {
    /// False: it did not open (its own window is not the one this disk has).
    fn open_boot(&self, first: u64, blocks: u64) -> bool;
    fn close_boot(&self);
}

/// NV-2p: a partition's way down to its disk when that disk may hold a window (the driver stacks each NVMe
/// partition on one): reads as `DiskDevice`, writes and discards pass `Disk::check_port`, so only the partition
/// whose range the armed window is takes any.
pub struct PartPort {
    disk: Arc<Disk>,
    port: (u64, u64),
}

impl BlockDevice for PartPort {
    fn name(&self) -> &str {
        &self.disk.name
    }
    fn info(&self) -> BlockInfo {
        let mut i = self.disk.dev.info();
        i.read_only |= self.disk.check_port(Kind::Write { fua: false, preflush: false }, self.port, None).is_err();
        i
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        self.disk.read(lba, buf, prio).map_err(BlockError::to_dev)
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        let mut v = Vec::new();
        v.try_reserve_exact(buf.len()).map_err(|_| DevError::NoMemory)?;
        v.extend_from_slice(buf);
        let t = self.disk.submit_via(Some(self.port), Kind::Write { fua, preflush: false }, lba, 0, Data::Bytes(v), prio).map_err(BlockError::to_dev)?;
        t.wait().map(|_| ()).map_err(BlockError::to_dev)
    }
    fn flush(&self) -> Result<(), DevError> {
        self.disk.flush(IoPriority::Normal).map_err(BlockError::to_dev)
    }
    fn settle(&self) -> Result<(), DevError> {
        self.disk.settle().map_err(BlockError::to_dev)
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        let t = self.disk.submit_via(Some(self.port), Kind::Discard, lba, blocks, Data::None, IoPriority::Background).map_err(BlockError::to_dev)?;
        t.wait().map(|_| ()).map_err(BlockError::to_dev)
    }
    /// Reads stay unchecked, as `read_blocks`.
    fn read_frames(&self, reqs: &[FrameIo], prio: IoPriority) -> Result<(), DevError> {
        self.disk.read_frames(reqs, prio).map_err(BlockError::to_dev)
    }
    /// Through `Disk::check_port`, per request, as `write_blocks`.
    fn write_frames(&self, lba: u64, buf: &IoBuf, fua: bool, prio: IoPriority) -> Result<(), DevError> {
        let t = self.disk.submit_via(Some(self.port), Kind::Write { fua, preflush: false }, lba, 0, Data::Frames(buf.clone()), prio).map_err(BlockError::to_dev)?;
        t.wait().map(|_| ()).map_err(BlockError::to_dev)
    }
    fn queue_depth(&self) -> u32 {
        self.disk.slots
    }
}

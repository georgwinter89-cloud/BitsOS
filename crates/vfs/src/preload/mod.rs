//! FS-13 start-up preload (Georg 22./23.10.): which file pages a program reads in its first seconds is
//! recorded per executable path (`recorder`), kept in a small record on /data (`record`), and after the next
//! boot a Background task (vfs-sys `preload`) reads those pages into the page cache before the program is
//! started again. No Linux counterpart in the kernel (userland ureadahead/systemd-readahead traced boots the
//! same way); the reads go through the ordinary fill path (linux-6.6/mm/readahead.c
//! `force_page_cache_readahead` is the closest), so preloaded pages are ordinary clean page-cache pages.
//!
//! The hot paths pay one atomic load while no session records (`ACTIVE`): `note_fill` in `PageCache::fill_with`
//! and in the read-ahead queueing, `opened` in `Ctx::open`. While a program's window runs (20 s after its
//! exec) a fill or open asks for the current process (`set_current_pid`, installed by vfs-sys) and looks it up
//! in a lock-free filter of the recording pids (`FILTER`); only a process that may record takes the recorder's
//! lock, so other programs' faults and opens never queue on it.

pub mod ranges;
pub mod record;
pub mod recorder;

use crate::inode::Inode;
use crate::mount_ns::{MountNs, Path};
use alloc::sync::{Arc, Weak};
use crate::ops::Ctx;
use crate::pagecache::PageCache;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::Mutex;
pub use ranges::Ranges;
pub use record::Record;
pub use record::{FileRec, Ident};
pub use recorder::{Finished, Keep, RecStats, Recorder, Taken};

/// How long after its exec a program's reads are recorded (CONFIG `preload_window_ms`).
pub const WINDOW_NS_DEFAULT: u64 = 20_000_000_000;
/// `preload_max_mb=all`: no page budget; the pass stops at its free-memory floor (vfs-sys `memory_floor`), and
/// MM-9's reclaim gives the preloaded pages nobody used back first when memory is needed (FS-13b, Georg 23.10.).
pub const ALL: u64 = u64::MAX;
/// The preload budget per boot (CONFIG `preload_max_mb`, settingsd): `ALL` since FS-13b (was 1024 MB while V2
/// had no reclaim). 0 pauses the preload; a number of MB is an explicit cap.
pub const MAX_MB_DEFAULT: u64 = ALL;
/// The most an explicit `preload_max_mb` may say (1 TiB).
pub const MAX_MB_LIMIT: u64 = 1 << 20;
/// With `ALL`, what one program records at most before the frame allocator says how much RAM there is (host
/// tests): the old default.
const RECORD_MB_FALLBACK: u64 = 1024;
const PAGES_PER_MB: u64 = 256;
/// The record cap in MB that `record::bytes_for` maps to `LOAD_MAX_BYTES` (16 GiB).
const RECORD_LIMIT_MB: u64 = record::LOAD_MAX_BYTES as u64 / 1024;

static ENABLED: AtomicBool = AtomicBool::new(true);
static MAX_MB: AtomicU64 = AtomicU64::new(MAX_MB_DEFAULT);
static WINDOW_NS: AtomicU64 = AtomicU64::new(WINDOW_NS_DEFAULT);
/// Sessions recording now (the hot paths' gate).
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// FS-15: pages read while a session ran by kernel threads / by processes that record nothing (`RecStats`).
static KTHREAD_PAGES: AtomicU64 = AtomicU64::new(0);
static OTHER_PAGES: AtomicU64 = AtomicU64::new(0);
static REC: Mutex<Recorder<Held>> = Mutex::named("vfs.preload", Recorder::new());
static CURRENT_PID: ksync::Once<fn() -> Option<u32>> = ksync::Once::new();

klog::counter!(EXECS, "vfs.preload.execs");
klog::counter!(STARTS, "vfs.preload.starts");
klog::counter!(LOST, "vfs.preload.sessions_lost");

/// A bit per pid hash (pid mod 4096) for the processes that record now, rebuilt under `REC` whenever pids join
/// or leave a session. A set bit may be another pid's (the lock is then taken for
/// nothing); a recording pid's bit is never clear. A hash filter, not a bound on how many processes record.
const FILTER_WORDS: usize = 64;
static FILTER: [AtomicU64; FILTER_WORDS] = [const { AtomicU64::new(0) }; FILTER_WORDS];

fn filter_bit(pid: u32) -> (usize, u64) {
    let h = pid as usize % (FILTER_WORDS * 64);
    (h / 64, 1u64 << (h % 64))
}

fn may_record(pid: u32) -> bool {
    let (w, b) = filter_bit(pid);
    FILTER[w].load(Ordering::Acquire) & b != 0
}

/// Under `REC`: every word is replaced by one built from the pids still recording (a word is stored whole, so
/// a recording pid's bit stays set throughout).
fn filter_rebuild(r: &Recorder<Held>) {
    let mut words = [0u64; FILTER_WORDS];
    for pid in r.pids() {
        let (w, b) = filter_bit(pid);
        words[w] |= b;
    }
    for (f, w) in FILTER.iter().zip(words) {
        f.store(w, Ordering::Release);
    }
}

/// A session's hold on an opened file (INT-W13): a weak reference to its inode. Holding the `Path` kept the mount
/// and the file alive for the whole window: a snapshot mount could not be unmounted or deleted (EBUSY), and a
/// stick file replaced meanwhile (SYS\PIN) kept its clusters until the next Reload left them lost. The weak
/// reference keeps only the inode's memory, so the cache pointer (inside it) is not given to another file.
pub struct Held(Weak<Inode>);

impl Keep for Held {
    fn ident(&self) -> Option<Ident> {
        self.0.upgrade().map(|i| ident_of(&i))
    }
}

/// A file's identity for the record (size, modification time).
pub fn ident_of(i: &Inode) -> Ident {
    let m = i.meta.times().1;
    Ident { size: i.size(), mtime_sec: m.sec, mtime_nsec: m.nsec }
}

/// vfs-sys installs the process layer's "which process runs this thread" (None in a kernel thread).
pub fn set_current_pid(f: fn() -> Option<u32>) {
    CURRENT_PID.get_or_init(|| f);
}

fn current_pid() -> Option<u32> {
    CURRENT_PID.get().and_then(|f| f())
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Off: nothing is recorded any more (running sessions are dropped) and no preload starts.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if !on {
        drop(clear_sessions());
    }
}

/// Recording runs: switched on and a budget above 0 (0 pauses the preload and keeps the stored record as it
/// is).
pub fn recording() -> bool {
    enabled() && max_mb() > 0
}

/// The setting as stored: `ALL`, 0 (paused) or an explicit cap in MB.
pub fn max_mb() -> u64 {
    MAX_MB.load(Ordering::Relaxed)
}

/// Values above `MAX_MB_LIMIT` other than `ALL` are taken as `MAX_MB_LIMIT` (`parse_max_mb` refuses them).
pub fn set_max_mb(mb: u64) {
    MAX_MB.store(if mb == ALL { ALL } else { mb.min(MAX_MB_LIMIT) }, Ordering::Relaxed);
}

/// CONFIG `preload_max_mb=` and `/sys/bitsos/preload_max_mb`: `all` (any case), or 0 (paused) up to 1 TiB.
pub fn parse_max_mb(v: &str) -> Option<u64> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("all") {
        return Some(ALL);
    }
    v.parse::<u64>().ok().filter(|&n| n <= MAX_MB_LIMIT)
}

/// The pages one boot preloads at most: unbounded with `ALL` (the memory floor ends the pass).
pub fn max_pages() -> u64 {
    match max_mb() {
        ALL => u64::MAX,
        mb => mb.saturating_mul(PAGES_PER_MB),
    }
}

/// The pages one program's start records at most, and (`record_mb`) the size the stored record is cut to. With
/// `ALL` a finite bound is still needed for both: the machine's RAM (a start cannot preload more than that).
pub fn record_pages() -> u64 {
    record_pages_for(max_mb(), ram_pages())
}

/// `record_pages` for a setting and the RAM in pages (None: not known yet, host tests). Never more than the
/// stored record can hold at its planned density (`record::bytes_for`: 1 KiB per MB, at most
/// `LOAD_MAX_BYTES`), so what one start records in memory and what a save keeps stay of one size (fixer F1).
pub fn record_pages_for(max_mb: u64, ram: Option<u64>) -> u64 {
    let limit = RECORD_LIMIT_MB * PAGES_PER_MB;
    let want = match max_mb {
        ALL => ram.unwrap_or(RECORD_MB_FALLBACK * PAGES_PER_MB),
        mb => mb.saturating_mul(PAGES_PER_MB).min(ram.unwrap_or(u64::MAX)),
    };
    want.min(limit)
}

pub fn record_mb() -> u64 {
    record_pages() / PAGES_PER_MB
}

static RAM_PAGES: AtomicU64 = AtomicU64::new(0);

/// The RAM the frame allocator manages, looked up once (it is asked on every recorded fill).
fn ram_pages() -> Option<u64> {
    let n = RAM_PAGES.load(Ordering::Relaxed);
    if n != 0 {
        return Some(n);
    }
    let n = mm_core::frames().map(|f| f.stats().total_bytes / 4096).filter(|&n| n > 0)?;
    RAM_PAGES.store(n, Ordering::Relaxed);
    Some(n)
}

pub fn window_ns() -> u64 {
    WINDOW_NS.load(Ordering::Relaxed)
}

pub fn set_window_ns(ns: u64) {
    WINDOW_NS.store(ns.max(1), Ordering::Relaxed);
}

/// FS-15 fixer F2: the cache's own number, not its address: a file freed while a session or a pending fill still
/// names it must not lend its pages to a new file allocated at the same place.
fn cache_key(c: &PageCache) -> usize {
    c.ident() as usize
}

fn sync_active(r: &Recorder<Held>) {
    ACTIVE.store(r.active(), Ordering::Release);
}

/// AB's exec (and the kernel's spawn of a program file): process `pid` now runs `exe` (and its interpreter
/// `interp`), whose paths are taken in `ns` from `root`. Begins the program's session or joins the running
/// one; the exe and the interpreter count as opened by the session (exec opens them itself, not through
/// `Ctx::open`).
/// A chroot'ed process (its root is not its view's) records nothing: its paths would name other files when the
/// preload looks them up from the boot view's root. Another mount view (FS-11) records: its paths mostly name
/// the same files, and a file that is another one at preload time has another identity and is skipped.
pub fn exec_started(pid: u32, ns: &MountNs, root: &Path, exe: &Path, interp: Option<&Path>) {
    if !recording() {
        if ACTIVE.load(Ordering::Acquire) != 0 {
            exited(pid);
        }
        return;
    }
    EXECS.inc();
    let text = if root.same(&ns.root_path()) { ns.path_of(root, exe) } else { None };
    let Some(text) = text else {
        // An older session of this pid does not go on recording the new program.
        exited(pid);
        return;
    };
    let began = {
        let mut r = REC.lock();
        let b = r.exec(pid, &text, now(), window_ns());
        filter_rebuild(&r);
        sync_active(&r);
        b
    };
    if began {
        STARTS.inc();
    }
    note_path(pid, ns, root, exe);
    note_head(pid, exe);
    if let Some(i) = interp {
        note_path(pid, ns, root, i);
        note_head(pid, i);
    }
    let cap = record_pages();
    REC.lock().exec_done(pid, cap);
}

/// AB's fork: the child records for the parent's session while it runs.
pub fn forked(parent: u32, child: u32) {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    if !may_record(parent) && !may_record(child) {
        return;
    }
    let mut r = REC.lock();
    r.fork(parent, child);
    filter_rebuild(&r);
}

/// AB's process teardown: `pid` records nothing any more (its number may be given out again).
pub fn exited(pid: u32) {
    if ACTIVE.load(Ordering::Acquire) == 0 || !may_record(pid) {
        return;
    }
    let mut r = REC.lock();
    r.exited(pid);
    filter_rebuild(&r);
}

fn cache_of(i: &Inode) -> Option<&PageCache> {
    if !i.is_reg() {
        return None;
    }
    i.page_cache().filter(|c| c.has_backend())
}

fn note_path(pid: u32, ns: &MountNs, root: &Path, p: &Path) {
    let Some(c) = cache_of(&p.dentry.inode) else { return };
    let key = cache_key(c);
    if !REC.lock().wants(pid, key) {
        return;
    }
    let Some(text) = ns.path_of(root, p) else { return };
    // INT-W13: /run holds runtime mounts (snapshot mounts at /run/bitsos/snap/<id>) whose files are gone at the
    // next boot: nothing a later boot could preload.
    if text.starts_with(b"/run/") {
        return;
    }
    // A keep-alive the recorder did not take is dropped here, after its lock (it may be the file's last
    // reference, and dropping that may take the file system's locks).
    let back = REC.lock().open(pid, key, text, Held(Arc::downgrade(&p.dentry.inode)));
    drop(back);
}

/// `Ctx::open` of a regular file: noted when the opening process records.
pub(crate) fn opened(c: &Ctx<'_>, p: &Path) {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    let Some(pid) = current_pid() else { return };
    if !may_record(pid) {
        return;
    }
    note_path(pid, c.ns, &c.root, p);
}

/// The fill path read pages `[first, end)` of `cache` from its device for the current thread (not the
/// preload's own reads).
pub(crate) fn note_fill(cache: &PageCache, first: u64, end: u64) {
    if ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    // FS-15: what was read while a session ran but cannot be a program's, counted lock-free.
    let Some(pid) = current_pid() else {
        KTHREAD_PAGES.fetch_add(end.saturating_sub(first), Ordering::Relaxed);
        return;
    };
    if !may_record(pid) {
        OTHER_PAGES.fetch_add(end.saturating_sub(first), Ordering::Relaxed);
        return;
    }
    let cap = record_pages();
    REC.lock().fill(pid, cache_key(cache), first, end, cap);
}

/// FS-15: exec reads the head of the program and of its interpreter (`prepare_binprm`, the ELF and program
/// headers) before AB's exec hook runs, while the process still records for its old program or for none: those
/// fills were `unknown` and the head was missing from every record (each preloaded start read it from the
/// device). The run of cached pages from page 0, at most one fill window long (what that head read brought in),
/// is added to the new session as read now.
fn note_head(pid: u32, p: &Path) {
    let Some(c) = cache_of(&p.dentry.inode) else { return };
    let end = c.cached_run(0, c.read_window());
    if end > 0 {
        let cap = record_pages();
        REC.lock().fill(pid, cache_key(c), 0, end, cap);
    }
}

/// The sessions whose window is over (all with `all`): the service merges them into the record.
pub fn take_finished(all: bool) -> Vec<Finished> {
    let taken = {
        let mut r = REC.lock();
        let x = r.finish(now(), all);
        filter_rebuild(&r);
        sync_active(&r);
        x
    };
    // After the lock: sorting, allocating, and dropping the keep-alives.
    let mut out = Vec::new();
    if out.try_reserve_exact(taken.len()).is_err() {
        LOST.add(taken.len() as u64);
        return out;
    }
    for t in taken {
        match t.into_finished() {
            Ok(Some(f)) => out.push(f),
            Ok(None) => {}
            Err(()) => LOST.inc(),
        }
    }
    out
}

/// Drops every running session (their keep-alives are returned to be dropped without the lock).
fn clear_sessions() -> Vec<Taken<Held>> {
    let mut r = REC.lock();
    let k = r.clear();
    filter_rebuild(&r);
    sync_active(&r);
    k
}

/// Clears the running sessions (the record itself is the service's).
pub fn clear() {
    drop(clear_sessions());
}

pub fn stats() -> RecStats {
    let mut s = REC.lock().stats;
    s.other_pages += OTHER_PAGES.load(Ordering::Relaxed);
    s.kthread_pages = KTHREAD_PAGES.load(Ordering::Relaxed);
    s
}

pub fn active() -> usize {
    ACTIVE.load(Ordering::Acquire)
}

/// B4: the preload's pause for foreground I/O. Waits in steps of `step_ns` while `waiting()` (a foreground
/// request is queued), at most `max_ns` from the first look on the clock `now`; true when it waited.
pub fn yield_to(waiting: &dyn Fn() -> bool, now: &dyn Fn() -> u64, sleep: &mut dyn FnMut(u64), step_ns: u64, max_ns: u64) -> bool {
    let t0 = now();
    let mut waited = false;
    while waiting() && now().saturating_sub(t0) < max_ns {
        waited = true;
        sleep(step_ns);
    }
    waited
}

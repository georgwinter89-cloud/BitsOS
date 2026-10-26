//! FS-13 start-up preload, the service (`vfs::preload` records): one Background thread `vfs.preload` that
//! 1. waits until /data is mounted (`/run/bitsos/data-state`, polled once a second: /data may come late or not
//!    at all), loads `/data/system/preload/record` (a damaged or foreign one is ignored) and decays it once,
//! 2. reads the recorded ranges of the most-used programs into the page cache, most-used first, each file in
//!    the order its program read it: every device read goes as Background (FS-12), waits while an Interactive
//!    or Normal request is queued, and the pass stops when free memory falls below its floor (`memory_floor`:
//!    a quarter of RAM, at least MM-9's high watermark), when reclaim starts taking preloaded pages back
//!    (`Stop::Reclaim`), at an explicit `preload.max_mb` (default `all`: no page cap, FS-13b), when it is
//!    switched off or the record is cleared; one `vfs preload.done` line per boot says what it did. The pages
//!    it reads are marked preloaded (`PageCache::prefetch`): until a program uses them, MM-9's reclaim takes
//!    them before any other cached page,
//! 3. then merges the finished recording sessions into the record and writes it back (temp file + rename,
//!    at most every 5 s, only on a writable /data). During the pass finished sessions are taken out between
//!    files (their keep-alives go) and merged after it.
//! `preload.max_mb` 0 pauses it: no pass, nothing recorded, and the stored record is left as it is. A pass that
//! stopped because the preload was switched off runs again when it is switched on.
//! Settings: CONFIG `preload` (on/off), `preload_max_mb`, `preload_window_ms`; at run time
//! `/sys/bitsos/preload`, `/sys/bitsos/preload_max_mb` (settingsd `kernel.preload.*`), `/sys/bitsos/preload_clear`
//! (write 1: the record is emptied), `/sys/bitsos/preload_status` (one line); MCP tool and console command
//! `preload` (`preload_tool`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::SpinLock;
use uapi::errno::*;
use vfs::pagecache::PageCache;
use vfs::preload::record::{self, Record};
use vfs::preload::{Finished, Ranges};
use vfs::{Errno, MountNs};

const MS: u64 = 1_000_000;
const POLL_NS: u64 = 1000 * MS;
const SAVE_EVERY_NS: u64 = 5000 * MS;
/// A queued foreground request is waited for in steps of 2 ms, at most 250 ms per device read (then the
/// Background read goes into the queue anyway, where the elevator still serves the foreground first).
const YIELD_STEP_NS: u64 = 2 * MS;
const YIELD_MAX_NS: u64 = 250 * MS;
pub const DATA_STATE: &str = "/run/bitsos/data-state";
pub const DIR: &str = "/data/system/preload";
pub const FILE: &str = "/data/system/preload/record";
const NEW: &str = "/data/system/preload/record.new";

static CLEAR: AtomicBool = AtomicBool::new(false);
static STATUS: SpinLock<Status> = SpinLock::named("vfs.preload.status", Status::new());

klog::counter!(PAGES, "vfs.preload.pages");
klog::counter!(READS, "vfs.preload.reads");
klog::counter!(SAVES, "vfs.preload.saves");

/// Why a pass ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    End,
    Budget,
    Memory,
    /// FS-13b: reclaim took preloaded pages back while the pass ran (memory is needed elsewhere: reading on
    /// would only feed reclaim).
    Reclaim,
    Off,
    Cleared,
}

impl Stop {
    pub fn as_str(self) -> &'static str {
        match self {
            Stop::End => "end",
            Stop::Budget => "max_mb",
            Stop::Memory => "memory",
            Stop::Reclaim => "reclaim",
            Stop::Off => "off",
            Stop::Cleared => "cleared",
        }
    }
}

/// The preload's limits and what it did so far; `go` is asked before every device read.
pub struct Budget {
    pub max_pages: u64,
    /// Stop when free memory falls below this many bytes.
    pub floor_bytes: u64,
    /// The run-time switch is off / a clear is pending (the service's: the settings; the ktests' own, which
    /// must not depend on the settings).
    pub off: fn() -> bool,
    pub cleared: fn() -> bool,
    /// Preloaded pages reclaim had taken when the budget was made (`vfs::reclaim::preloaded_taken`).
    pub taken0: u64,
    pub pages: u64,
    pub reads: u64,
    pub yields: u64,
    pub stopped: Option<Stop>,
}

impl Budget {
    pub fn new(max_pages: u64, floor_bytes: u64, settings: bool) -> Budget {
        let (off, cleared): (fn() -> bool, fn() -> bool) = if settings { (|| !vfs::preload::enabled(), || CLEAR.load(Ordering::Acquire)) } else { (|| false, || false) };
        let taken0 = vfs::reclaim::preloaded_taken();
        Budget { max_pages, floor_bytes, off, cleared, taken0, pages: 0, reads: 0, yields: 0, stopped: None }
    }

    fn stop(&mut self, why: Stop) -> bool {
        self.stopped = Some(why);
        false
    }

    /// `more`: pages inserted by the read in progress that `pages` does not count yet.
    pub fn go(&mut self, more: u64) -> bool {
        if self.stopped.is_some() {
            return false;
        }
        if (self.off)() {
            return self.stop(Stop::Off);
        }
        if (self.cleared)() {
            return self.stop(Stop::Cleared);
        }
        // Reached: no further read (one read may bring a window past it).
        if self.pages.saturating_add(more) >= self.max_pages {
            return self.stop(Stop::Budget);
        }
        // The floor keeps free memory far above the watermarks, so this means pressure from elsewhere.
        if vfs::reclaim::preloaded_taken() > self.taken0 {
            return self.stop(Stop::Reclaim);
        }
        match mm_core::frames().map(|f| f.stats()) {
            Some(s) if s.free_bytes >= self.floor_bytes => {}
            _ => return self.stop(Stop::Memory),
        }
        // B4: Background yields to a waiting reader, bounded.
        let waited = vfs::preload::yield_to(&|| block::prio::foreground_waiting() > 0, &time::now_ns, &mut sched::sleep_ns, YIELD_STEP_NS, YIELD_MAX_NS);
        self.yields += waited as u64;
        true
    }
}

/// The pass's free-memory floor (FS-13b): a quarter of RAM, at least MM-9's high watermark. Only the pass
/// stops there; what is cached beyond it (later reads) is reclaim's. Kept that high so the pass never brings
/// free memory near the low watermark, where kswapd and direct reclaim would take the pages it just read (a
/// loop of reads for nothing), and so the programs started right after boot find free frames without direct
/// reclaim. u64::MAX without a frame allocator (nothing is read).
pub fn memory_floor() -> u64 {
    let Some(total) = mm_core::frames().map(|f| f.stats().total_bytes) else { return u64::MAX };
    memory_floor_for(total, mm::reclaim::watermarks().high.saturating_mul(4096))
}

/// `memory_floor` for a RAM size and MM-9's high watermark, both in bytes.
pub fn memory_floor_for(total: u64, high: u64) -> u64 {
    (total / 4).max(high)
}

/// `max_mb` as the settings say it: `all`, 0 or a number of MB.
pub fn max_mb_text() -> String {
    match vfs::preload::max_mb() {
        vfs::preload::ALL => String::from("all"),
        n => format!("{n}"),
    }
}

/// Reads the pages of `ranges` of `cache` that are not cached yet; the counts go to `b`.
pub fn prefetch_ranges(cache: &PageCache, ranges: &Ranges, b: &mut Budget) -> Result<(), Errno> {
    for &(first, end) in ranges.as_slice() {
        let (ins, reads) = cache.prefetch(first, end, &mut |more| b.go(more))?;
        b.pages += ins;
        b.reads += reads;
        PAGES.add(ins);
        READS.add(reads);
        if b.stopped.is_some() {
            break;
        }
    }
    Ok(())
}

/// What the boot's pass did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pass {
    pub programs: u64,
    pub files: u64,
    pub missing: u64,
    /// Files skipped because they changed since they were recorded.
    pub stale: u64,
    /// FS-15: changed files the start had read whole, read whole again at their new size (kept in the record).
    pub renewed: u64,
    pub errors: u64,
    pub pages: u64,
    pub reads: u64,
    pub yields: u64,
    pub ms: u64,
    /// The free-memory floor the pass ran with, in MB.
    pub floor_mb: u64,
    pub stopped: Option<Stop>,
}

/// One pass over `rec`, most-used programs first; `between` runs before every file. A file whose size or
/// modification time is not what was recorded (the program was updated) is skipped and its path noted in
/// `stale`, for the caller to forget.
pub fn run_pass(ns: &MountNs, rec: &Record, b: &mut Budget, between: &mut dyn FnMut(), stale: &mut Vec<Vec<u8>>) -> Pass {
    let t0 = time::now_ns();
    let mut p = Pass::default();
    'all: for prog in rec.by_use() {
        p.programs += 1;
        for f in &prog.files {
            if b.stopped.is_some() {
                break 'all;
            }
            between();
            let path = match ns.lookup_path(&f.path, true) {
                Ok(x) => x,
                Err(_) => {
                    p.missing += 1;
                    continue;
                }
            };
            let inode = &path.dentry.inode;
            let Some(cache) = inode.page_cache().filter(|_| inode.is_reg()) else {
                p.missing += 1;
                continue;
            };
            let now = vfs::preload::ident_of(inode);
            if now != f.id
                && let Some((first, end)) = f.whole_again(&now)
            {
                let mut whole = Ranges::new();
                if whole.add(first, end) {
                    if let Err(e) = prefetch_ranges(cache, &whole, b) {
                        p.errors += 1;
                        klog::event!(Warn, "vfs", "preload.read_failed", path = String::from_utf8_lossy(&f.path).as_ref(), errno = e; "the rest of this file is not preloaded");
                    }
                    p.renewed += 1;
                    p.files += 1;
                    continue;
                }
            }
            if now != f.id {
                p.stale += 1;
                let mut v = Vec::new();
                if v.try_reserve_exact(f.path.len()).is_ok() && stale.try_reserve(1).is_ok() {
                    v.extend_from_slice(&f.path);
                    stale.push(v);
                }
                continue;
            }
            if let Err(e) = prefetch_ranges(cache, &f.ranges, b) {
                p.errors += 1;
                klog::event!(Warn, "vfs", "preload.read_failed", path = String::from_utf8_lossy(&f.path).as_ref(), errno = e; "the rest of this file is not preloaded");
            }
            p.files += 1;
        }
    }
    p.pages = b.pages;
    p.reads = b.reads;
    p.yields = b.yields;
    p.stopped = Some(b.stopped.unwrap_or(Stop::End));
    p.floor_mb = b.floor_bytes >> 20;
    p.ms = time::now_ns().saturating_sub(t0) / MS;
    p
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DataState {
    Wait,
    Rw,
    Ro,
    None,
}

fn data_state(ns: &MountNs) -> DataState {
    let Ok(t) = ns.read_file(DATA_STATE, 256) else { return DataState::Wait };
    let t = String::from_utf8_lossy(&t);
    let mut w = t.split_whitespace();
    match w.next() {
        Some("mounted") if t.split_whitespace().any(|x| x == "rw") => DataState::Rw,
        Some("mounted") => DataState::Ro,
        Some("skipped" | "failed") => DataState::None,
        _ => DataState::Wait,
    }
}

struct Status {
    state: &'static str,
    pass: Option<Pass>,
    record_programs: u64,
    record_pages: u64,
    saved: u64,
    load: &'static str,
}

impl Status {
    const fn new() -> Status {
        Status { state: "starting", pass: None, record_programs: 0, record_pages: 0, saved: 0, load: "-" }
    }
}

fn set_state(s: &'static str) {
    STATUS.lock().state = s;
}

/// `/sys/bitsos/preload_status`: one line of `key=value`.
pub fn status_line() -> String {
    // The recorder's lock (a mutex) before the status spin lock, never under it.
    let (st, active) = (vfs::preload::stats(), vfs::preload::active());
    let s = STATUS.lock();
    let p = s.pass.unwrap_or_default();
    format!(
        "state={} enabled={} max_mb={} load={} programs={} files={} mb={} pages={} reads={} ms={} stopped={} yields={} missing={} record_programs={} record_mb={} sessions={} execs={} starts={} fills={} saved={} stale={} renewed={} unknown={} unknown_pages={} dropped={} dropped_pages={} other_pages={} kthread_pages={} floor_mb={}",
        s.state,
        if vfs::preload::enabled() { "on" } else { "off" },
        max_mb_text(),
        s.load,
        p.programs,
        p.files,
        p.pages / 256,
        p.pages,
        p.reads,
        p.ms,
        p.stopped.map_or("-", |x| x.as_str()),
        p.yields,
        p.missing,
        s.record_programs,
        s.record_pages / 256,
        active,
        st.execs,
        st.starts,
        st.fills,
        s.saved,
        p.stale,
        p.renewed,
        st.unknown,
        st.unknown_pages,
        st.dropped,
        st.dropped_pages,
        st.other_pages,
        st.kthread_pages,
        p.floor_mb
    )
}

fn load(ns: &MountNs) -> Record {
    // A fixed ceiling: a budget lowered since the record was written must not make it unreadable.
    match ns.read_file(FILE, record::LOAD_MAX_BYTES) {
        Ok(b) => match Record::decode(&b) {
            Ok(r) => {
                STATUS.lock().load = "ok";
                klog::event!(Info, "vfs", "preload.loaded", programs = r.programs.len() as u64, pages = r.pages(), bytes = b.len() as u64, boots = r.boots);
                r
            }
            Err(why) => {
                STATUS.lock().load = "ignored";
                klog::event!(Warn, "vfs", "preload.record_ignored", reason = why.as_str(), bytes = b.len() as u64; "a damaged or foreign record: starting a new one");
                Record::new()
            }
        },
        Err(ENOENT) => {
            STATUS.lock().load = "new";
            Record::new()
        }
        Err(e) => {
            STATUS.lock().load = "ignored";
            klog::event!(Warn, "vfs", "preload.record_unreadable", errno = e; "starting a new record");
            Record::new()
        }
    }
}

fn save(ns: &MountNs, rec: &Record) -> Result<usize, Errno> {
    let max = record::bytes_for(vfs::preload::record_mb());
    let (bytes, kept) = rec.encode_kept(max).ok_or(ENOMEM)?;
    let storable = rec.storable();
    if kept < storable {
        klog::event!(Warn, "vfs", "preload.record_cut", kept = kept as u64, left = (storable - kept) as u64, bytes = bytes.len() as u64, max_bytes = max as u64;
            "the record outgrew its size: the least used programs are not stored");
    }
    ns.mkdir_p_bytes(b"/data/system", 0o755)?;
    ns.mkdir_p_bytes(DIR.as_bytes(), 0o700)?;
    match ns.remove(NEW) {
        Ok(()) | Err(ENOENT) => {}
        Err(e) => return Err(e),
    }
    ns.create_file_bytes(NEW.as_bytes(), 0o600, &bytes)?;
    vfs::ops::Ctx::kernel(ns).rename(None, NEW.as_bytes(), None, FILE.as_bytes(), 0)?;
    Ok(bytes.len())
}

fn merge(rec: &mut Record, done: Vec<Finished>) -> bool {
    // Paused (max_mb 0): the stored record stays as it is.
    if !vfs::preload::recording() {
        return false;
    }
    let mut any = false;
    for f in done {
        // FS-15: what one start recorded, to set beside the device's read bytes of that start.
        let pages = f.files.iter().map(|x| x.ranges.pages()).sum::<u64>();
        klog::event!(Info, "vfs", "preload.session", exe = String::from_utf8_lossy(&f.exe).as_ref(), files = f.files.len() as u64, pages = pages);
        if !rec.add_start(&f.exe, f.files, vfs::preload::record_pages()) {
            klog::event!(Warn, "vfs", "preload.merge_short"; "no memory: part of a start was not recorded");
        }
        any = true;
    }
    any
}

/// Programs with something to preload (the stored record leaves the others out).
fn with_pages(rec: &Record) -> u64 {
    rec.programs.iter().filter(|p| p.pages() > 0).count() as u64
}

fn note_record(rec: &Record) {
    let (n, pages) = (with_pages(rec), rec.pages());
    let mut s = STATUS.lock();
    s.record_programs = n;
    s.record_pages = pages;
}

fn config() {
    if let Some(v) = pci::registry::config_value("preload") {
        match parse_switch(v.trim()) {
            Some(on) => vfs::preload::set_enabled(on),
            None => klog::event!(Warn, "vfs", "preload.config_bad", key = "preload", value = v.as_str()),
        }
    }
    if let Some(v) = pci::registry::config_value("preload_max_mb") {
        match vfs::preload::parse_max_mb(v.trim()) {
            Some(n) => vfs::preload::set_max_mb(n),
            None => klog::event!(Warn, "vfs", "preload.config_bad", key = "preload_max_mb", value = v.as_str()),
        }
    }
    if let Some(v) = pci::registry::config_value("preload_window_ms") {
        match v.trim().parse::<u64>() {
            Ok(n) if (100..=600_000).contains(&n) => vfs::preload::set_window_ns(n * MS),
            _ => klog::event!(Warn, "vfs", "preload.config_bad", key = "preload_window_ms", value = v.as_str()),
        }
    }
}

fn parse_switch(v: &str) -> Option<bool> {
    match v {
        "on" | "1" | "true" | "yes" => Some(true),
        "off" | "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

fn service() {
    let ns = vfs::root_ns();
    let mut rec: Option<Record> = None;
    let mut writable = false;
    let mut dirty = false;
    let mut passed = false;
    let mut last_save = 0u64;
    let mut pending: Vec<Finished> = Vec::new();
    // No /data: finished sessions are only dropped (nothing can keep them).
    let mut no_data = false;
    set_state("waiting_data");
    loop {
        if rec.is_none() {
            match data_state(&ns) {
                DataState::Wait => {}
                DataState::None if !no_data => {
                    config();
                    no_data = true;
                    pending.clear();
                    set_state("no_data");
                    klog::event!(Info, "vfs", "preload.no_data"; "no /data: nothing is kept across boots");
                }
                DataState::None => {}
                st => {
                    // CONFIG is read by now (it comes from the stick, like /data); sessions recorded before it
                    // said off are dropped.
                    config();
                    if !vfs::preload::enabled() {
                        pending.clear();
                    }
                    writable = st == DataState::Rw;
                    klog::event!(Info, "vfs", "preload.config", enabled = vfs::preload::enabled(), max_mb = max_mb_text().as_str(), floor_mb = memory_floor() >> 20, window_ms = vfs::preload::window_ns() / MS, data_writable = writable);
                    let mut r = load(&ns);
                    r.decay();
                    dirty = writable;
                    merge(&mut r, core::mem::take(&mut pending));
                    note_record(&r);
                    rec = Some(r);
                }
            }
        }
        // A clear asked for before /data is known waits for the record (then the loaded one is emptied);
        // what was recorded before it is dropped now.
        if rec.is_none() && CLEAR.load(Ordering::Acquire) {
            pending.clear();
        }
        if rec.is_some() && CLEAR.swap(false, Ordering::AcqRel) {
            vfs::preload::clear();
            pending.clear();
            if let Some(r) = rec.as_mut() {
                r.clear();
                note_record(r);
            }
            dirty = writable;
            last_save = 0;
            klog::event!(Info, "vfs", "preload.cleared");
        }
        let done = vfs::preload::take_finished(false);
        match rec.as_mut() {
            Some(r) => {
                if merge(r, done) {
                    dirty = writable;
                    note_record(r);
                }
            }
            None if no_data => drop(done),
            None => pending.extend(done),
        }
        if let Some(r) = rec.as_mut()
            && !passed
            && vfs::preload::enabled()
            && vfs::preload::max_pages() > 0
        {
            set_state("preloading");
            let mut b = Budget::new(vfs::preload::max_pages(), memory_floor(), true);
            let mut during: Vec<Finished> = Vec::new();
            let mut stale: Vec<Vec<u8>> = Vec::new();
            let p = run_pass(
                &ns,
                r,
                &mut b,
                &mut || {
                    let done = vfs::preload::take_finished(false);
                    if during.try_reserve(done.len()).is_ok() {
                        during.extend(done);
                    }
                },
                &mut stale,
            );
            // Switched off part way: it runs again when switched on (cached pages are not read twice).
            passed = p.stopped != Some(Stop::Off);
            klog::event!(Info, "vfs", "preload.done", programs = p.programs, files = p.files, mb = p.pages / 256, pages = p.pages, reads = p.reads, ms = p.ms, stopped = p.stopped.map_or("-", |x| x.as_str()), floor_mb = p.floor_mb, yields = p.yields, missing = p.missing, stale = p.stale, renewed = p.renewed, errors = p.errors);
            for path in &stale {
                r.forget_file(path);
            }
            if merge(r, during) || !stale.is_empty() {
                dirty = writable;
            }
            note_record(r);
            let mut s = STATUS.lock();
            s.pass = Some(p);
            s.state = if passed { "done" } else { "off" };
        }
        let now = time::now_ns();
        if dirty
            && vfs::preload::recording()
            && let Some(r) = rec.as_ref()
            && now.saturating_sub(last_save) >= SAVE_EVERY_NS
        {
            match save(&ns, r) {
                Ok(n) => {
                    SAVES.inc();
                    STATUS.lock().saved += 1;
                    let st = vfs::preload::stats();
                    klog::event!(Info, "vfs", "preload.saved", programs = with_pages(r), pages = r.pages(), bytes = n as u64, unknown_pages = st.unknown_pages, dropped_pages = st.dropped_pages, other_pages = st.other_pages, kthread_pages = st.kthread_pages);
                }
                Err(e) => klog::event!(Warn, "vfs", "preload.save_failed", errno = e; "tried again in 5 s"),
            }
            dirty = false;
            last_save = now;
        }
        sched::sleep_ns(POLL_NS);
    }
}

fn start() -> Result<(), &'static str> {
    vfs::preload::set_current_pid(current_pid);
    crate::bitsos::register("preload", sw_get, sw_set);
    crate::bitsos::register("preload_max_mb", mb_get, mb_set);
    crate::bitsos::register("preload_clear", clear_get, clear_set);
    crate::bitsos::register_ro("preload_status", status_line);
    match sched::spawn("vfs.preload", sched::Class::Background, service) {
        Ok(_) => Ok(()),
        Err(_) => {
            vfs::preload::set_enabled(false);
            klog::event!(Warn, "vfs", "preload.no_thread"; "start-up preload off");
            Err("vfs.preload: no thread")
        }
    }
}
kreg::initcall!(kreg::Level::Service, "vfs.preload", start);

fn current_pid() -> Option<u32> {
    proc::current().map(|p| p.pid)
}

fn sw_get() -> String {
    String::from(if vfs::preload::enabled() { "on" } else { "off" })
}

fn sw_set(v: &str) -> Result<(), Errno> {
    let on = parse_switch(v.trim()).ok_or(EINVAL)?;
    vfs::preload::set_enabled(on);
    klog::event!(Info, "vfs", "sys_bitsos.set", name = "preload", value = if on { "on" } else { "off" });
    Ok(())
}

fn mb_get() -> String {
    max_mb_text()
}

fn mb_set(v: &str) -> Result<(), Errno> {
    let n = vfs::preload::parse_max_mb(v.trim()).ok_or(EINVAL)?;
    vfs::preload::set_max_mb(n);
    klog::event!(Info, "vfs", "sys_bitsos.set", name = "preload_max_mb", value = max_mb_text().as_str());
    Ok(())
}

fn clear_get() -> String {
    String::from("0")
}

/// Writing 1 empties the record (the service writes the empty one back within a second or so).
fn clear_set(v: &str) -> Result<(), Errno> {
    if v.trim() != "1" {
        return Err(EINVAL);
    }
    request_clear("sysfs");
    Ok(())
}

/// Empties the record: running sessions now, the stored record within about a second (the service writes
/// the empty one back). `source` names who asked (sysfs, mcp, console).
pub fn request_clear(source: &'static str) {
    CLEAR.store(true, Ordering::Release);
    vfs::preload::clear();
    klog::event!(Info, "vfs", "preload.clear_requested", source = source);
}


#[cfg(test)]
mod tests {
    use super::memory_floor_for;

    const MIB: u64 = 1 << 20;

    #[test]
    fn the_pass_floor_is_a_quarter_of_ram_and_at_least_the_high_watermark() {
        // 32 GiB (Mini-PC, high watermark about 87 MB): the quarter wins.
        assert_eq!(memory_floor_for(32 << 30, 87 * MIB), 8 << 30);
        assert_eq!(memory_floor_for(512 * MIB, 4 * MIB), 128 * MIB);
        // A watermark above the quarter (tiny RAM, or a raised min_free) wins.
        assert_eq!(memory_floor_for(64 * MIB, 20 * MIB), 20 * MIB);
        assert_eq!(memory_floor_for(0, 0), 0);
    }
}

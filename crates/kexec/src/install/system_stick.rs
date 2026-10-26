//! RB-9, the stick side of `system_list` / `system_pin`: the listing with dates and kernel ids, read in a helper
//! thread with a deadline (B4: a stick that hangs must not hold an MCP worker or the console), and the PIN write
//! under the install lock.
//!
//! Kernel ids are the FNV-1a 64 of each KERNEL file. Reading a 20-30 MB file per generation costs time, so the
//! ids are kept in a small cache keyed by (generation, size, KERNEL's FAT date): a generation's files are written
//! once (RB-6), and the date tells a reused number apart (a newest generation removed from outside, then a new
//! install of the same size). An install primes the cache with the id it computed anyway. The helper first
//! publishes the listing without the missing ids, then reads them; a caller whose deadline passes gets the
//! listing so far, and the helper finishes the cache for the next call.

use super::plan::Gen;
use super::system::{Listing, SysGen};
use super::{LOCK, errno_name, gen_from};
use crate::Fnv;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use fs_fat::sysapi::Sys;
use ksync::{SpinLock, WaitQueue};

const SEC: u64 = 1_000_000_000;
/// How long one helper may spend on kernel ids (the rest waits for the next listing).
const IDS_NS: u64 = 120 * SEC;
const PIECE: usize = 1 << 20;

/// One cached kernel id.
struct Id {
    n: u32,
    bytes: u64,
    /// KERNEL's FAT date; None: noted by an install that could not read it back, bound by the next listing.
    mtime: Option<i64>,
    fnv: u64,
}

static IDS: SpinLock<Vec<Id>> = SpinLock::named("kexec.system.ids", Vec::new());
/// One helper reading the listing at a time (a stick whose folder reads hang refuses further listings instead of
/// collecting threads that hang on it too).
static LISTING: AtomicBool = AtomicBool::new(false);
/// When the helper that reads kernel ids started (0: none). One at a time; a later listing goes on without ids.
static READING_IDS: AtomicU64 = AtomicU64::new(0);

/// Clears `READING_IDS` when its reader ends, whichever way (only its own mark: never a later reader's).
struct IdsReader(u64);

impl Drop for IdsReader {
    fn drop(&mut self) {
        let _ = READING_IDS.compare_exchange(self.0, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// Becomes the kernel-id reader, or says how long the running one has been at it (ns).
fn ids_reader() -> Result<IdsReader, u64> {
    let now = time::now_ns().max(1);
    match READING_IDS.compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(IdsReader(now)),
        Err(since) => Err(now.saturating_sub(since)),
    }
}

fn kernel_mtime(s: &Sys<'_>, n: u32) -> Option<i64> {
    let dir = alloc::format!("{}/BITSOS2/SYS/{}", s.mount, n);
    s.ns.list_dir(&dir).ok()?.into_iter().find(|(name, _)| name.eq_ignore_ascii_case(b"KERNEL")).map(|(_, a)| a.mtime.sec)
}

/// An install wrote this KERNEL (its id is known without reading it back).
pub fn note_kernel_id(n: u32, bytes: u64, fnv: u64) {
    let mtime = fs_fat::stick::with_sys(|s| kernel_mtime(s, n)).ok().flatten();
    let mut ids = IDS.lock();
    ids.retain(|e| e.n != n);
    if ids.try_reserve(1).is_ok() {
        ids.push(Id { n, bytes, mtime, fnv });
    }
}

fn cached(n: u32, bytes: u64, mtime: Option<i64>) -> Option<u64> {
    let mut ids = IDS.lock();
    let e = ids.iter_mut().find(|e| e.n == n && e.bytes == bytes && (e.mtime.is_none() || e.mtime == mtime))?;
    if e.mtime.is_none() {
        e.mtime = mtime;
    }
    Some(e.fnv)
}

/// The generations with their dates (from the folder's entries) and the cached kernel ids.
fn read_listing(s: &Sys<'_>) -> Result<Listing, i64> {
    let mut gens = Vec::new();
    for g in s.generations()? {
        let size = |n: &str| g.files.iter().find(|f| f.0 == n).map(|f| f.1);
        let mut sg = SysGen { dir: gen_from(&g), kernel_bytes: size("KERNEL"), initrd_bytes: size("INITRD"), ..SysGen::default() };
        let dir = alloc::format!("{}/BITSOS2/SYS/{}", s.mount, g.n);
        // A folder that went away between the two listings shows no dates.
        for (name, a) in s.ns.list_dir(&dir).unwrap_or_default() {
            match String::from_utf8_lossy(&name).to_ascii_uppercase().as_str() {
                "KERNEL" => sg.installed = Some(a.mtime.sec),
                "OK" => sg.confirmed = Some(a.mtime.sec),
                _ => {}
            }
        }
        sg.kernel_id = sg.kernel_bytes.and_then(|b| cached(g.n, b, sg.installed));
        gens.push(sg);
    }
    // The cache follows the stick (bounded by its generations); an id noted by an install that finished after
    // this listing read the folder (a number above all listed ones) stays.
    let top = gens.iter().map(|g: &SysGen| g.dir.n).max().unwrap_or(0);
    IDS.lock().retain(|e| e.n > top || gens.iter().any(|g: &SysGen| g.dir.n == e.n));
    let (running, reload) = match crate::boot::boot_info() {
        Some((bi, _)) => (bi.generation(), bi.reload().map_or(0, |r| r.counter)),
        None => (None, 0),
    };
    Ok(Listing { gens, pin: s.pin()?, writable: s.vol.writable(), running, reload, running_kernel: crate::crash::running_kernel_id() })
}

/// RB-11: the generation whose kernel runs, for `system_pin now` (no KERNEL reads: after a Reload only the kernel
/// ids already read count, each checked against the folder's KERNEL size and date as the listing does; a
/// generation whose id is not read yet makes the answer None, and the Reload then goes ahead).
pub fn running_number(s: &Sys<'_>) -> Option<u32> {
    let (bi, _) = crate::boot::boot_info()?;
    let reload = bi.reload().map_or(0, |r| r.counter);
    let mut ids: Vec<(u32, Option<u64>)> = Vec::new();
    if reload > 0 {
        let gens = s.generations().ok()?;
        ids.try_reserve(gens.len()).ok()?;
        for g in gens {
            let Some(bytes) = g.files.iter().find(|f| f.0 == "KERNEL").map(|f| f.1) else { continue };
            ids.push((g.n, cached(g.n, bytes, kernel_mtime(s, g.n))));
        }
    }
    match super::system::identify(bi.generation(), reload, crate::crash::running_kernel_id(), &ids) {
        super::system::Running::Gen(n) => Some(n),
        _ => None,
    }
}

fn kernel_fnv(s: &Sys<'_>, n: u32, until_ns: u64) -> Result<u64, i64> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(PIECE).map_err(|_| uapi::errno::ENOMEM)?;
    buf.resize(PIECE, 0);
    let mut h = Fnv::new();
    let mut off = 0u64;
    loop {
        if time::now_ns() >= until_ns {
            return Err(uapi::errno::ETIMEDOUT);
        }
        let got = s.read_at(n, "KERNEL", off, &mut buf)?;
        if got == 0 {
            return Ok(h.finish());
        }
        h.update(&buf[..got]);
        off += got as u64;
    }
}

struct Slot {
    v: SpinLock<Option<Result<Listing, i64>>>,
    done: AtomicBool,
    wq: WaitQueue,
}

/// The listing, waiting at most `wait_ns` (Err: the text for the answer).
pub fn listing(wait_ns: u64) -> Result<Listing, String> {
    // Right after a start the stick mounts a moment after MCP is up.
    if fs_fat::stick::wait_mounted((wait_ns / 1_000_000).min(5_000)).is_none() {
        return Err(alloc::format!("no boot stick mounted at /boot ({})", fs_fat::stick::last_reason().unwrap_or("none recognised")));
    }
    if LISTING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        klog::event!(Warn, "kexec", "system.list_busy"; "an earlier listing still reads the stick: refused");
        return Err(String::from("an earlier listing is still reading the stick (a slow or hanging stick); ask again in a few seconds"));
    }
    let slot = Arc::new(Slot { v: SpinLock::named("kexec.system.list", None), done: AtomicBool::new(false), wq: WaitQueue::new() });
    let s2 = slot.clone();
    let started = sched::spawn("system-list", sched::Class::Normal, move || {
        let r = fs_fat::stick::with_sys(|s| {
            let l = read_listing(s);
            LISTING.store(false, Ordering::Release);
            let Ok(mut l) = l else { return l };
            let _reader = match ids_reader() {
                Ok(r) => r,
                Err(age) => {
                    // A reader past its own budget hangs inside one stick read: a second one would hang on the
                    // same KERNEL too, so the listing goes without ids until that read returns (the block layer's
                    // timeouts end it with an error).
                    if age > IDS_NS {
                        klog::event!(Warn, "kexec", "system.kernel_id_overdue", running_s = age / SEC; "an earlier kernel-id read has not returned");
                    }
                    return Ok(l);
                }
            };
            // The listing first: a caller whose deadline passes still gets it.
            *s2.v.lock() = Some(Ok(l.clone()));
            let until = time::now_ns().saturating_add(IDS_NS);
            for g in l.gens.iter_mut().filter(|g| g.kernel_id.is_none()) {
                let Some(bytes) = g.kernel_bytes else { continue };
                match kernel_fnv(s, g.dir.n, until) {
                    Ok(f) => {
                        note_kernel_id(g.dir.n, bytes, f);
                        g.kernel_id = Some(f);
                    }
                    Err(e) => {
                        klog::event!(Warn, "kexec", "system.kernel_id_failed", generation = g.dir.n, error = errno_name(e).as_str());
                        if e == uapi::errno::ETIMEDOUT {
                            break;
                        }
                    }
                }
            }
            Ok(l)
        });
        if r.is_err() {
            LISTING.store(false, Ordering::Release);
        }
        *s2.v.lock() = Some(r.unwrap_or_else(Err));
        s2.done.store(true, Ordering::Release);
        s2.wq.wake_all();
    });
    if started.is_err() {
        LISTING.store(false, Ordering::Release);
        return Err(String::from("no thread for the stick listing (no memory now)"));
    }
    let done = slot.wq.wait_until_timeout(|| slot.done.load(Ordering::Acquire), wait_ns);
    let got = slot.v.lock().clone();
    match got {
        Some(Ok(l)) => {
            if !done {
                klog::event!(Info, "kexec", "system.ids_pending", wait_ms = wait_ns / 1_000_000; "kernel ids are still being read");
            }
            Ok(l)
        }
        Some(Err(e)) => Err(alloc::format!("the stick could not be listed: {}", errno_name(e))),
        None => {
            klog::event!(Warn, "kexec", "system.list_timeout", wait_ms = wait_ns / 1_000_000; "the stick did not answer");
            Err(alloc::format!("the stick did not answer within {} s", wait_ns / SEC))
        }
    }
}

/// The generations and PIN as `install`'s planner sees them, then `f`, all under the install lock (the PIN
/// write and an install never interleave). Err: the text for the answer.
/// `lock_ns` bounds the wait for the lock (a call's deadline).
pub fn with_gens<R>(lock_ns: u64, f: impl FnOnce(&Sys<'_>, &[Gen], Option<u32>) -> Result<R, String>) -> Result<R, String> {
    let _l = LOCK.lock_timeout(lock_ns).ok_or_else(|| {
        klog::event!(Warn, "kexec", "system.lock_timeout", waited_ms = lock_ns / 1_000_000; "nothing on the stick changed");
        alloc::format!("the stick stayed busy for {} s (an install?); nothing changed", lock_ns / SEC)
    })?;
    fs_fat::stick::with_sys(|s| {
        let gens: Vec<Gen> = s.generations().map_err(|e| alloc::format!("listing \\BITSOS2\\SYS: {}", errno_name(e)))?.iter().map(gen_from).collect();
        let pin = s.pin().map_err(|e| alloc::format!("reading \\BITSOS2\\SYS\\PIN: {}", errno_name(e)))?;
        f(s, &gens, pin)
    })
    .map_err(|e| alloc::format!("the boot stick: {}", errno_name(e)))?
}

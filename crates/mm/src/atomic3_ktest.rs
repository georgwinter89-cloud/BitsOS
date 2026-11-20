//! ATOMIC-3 ktests: three lists that grew where the heap may not reclaim (detector `mm.heap.atomic_large`)
//! and now grow outside. Each drives its path past the size at which the old code asked
//! for two pages or more under a spin lock or with interrupts off, and judges by the detector's table: no request
//! of that kind may be noted meanwhile. The table is global, so another thread's site in the same moment would
//! make the test red too (not seen: after boot, the group's boots note nothing else).

use crate::linux::MAP_SHARED;
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use mm_core::grace;

/// Requests the detector noted so far (all chains, and those the full table could not list).
fn noted() -> u64 {
    let mut n = mm_core::callers::HEAP_ATOMIC_LARGE.dropped();
    mm_core::callers::HEAP_ATOMIC_LARGE.each(&mut |s| n += s.count);
    n
}

/// Items in one block of the grace queue (a page of 40-byte entries).
const GRACE_BLOCK: usize = 4096 / 40;

/// How long the deferred items may take to be freed once the grace period is let go.
const FREE_WITHIN_NS: u64 = 10_000_000_000;

/// 640 blocks' worth of items deferred under a spin lock while the grace period is held, so none drains: the queue
/// chains 640 blocks. The list of blocks it had before doubled under `mm.grace` (32 KiB at 1024 blocks).
fn grace_long_queue() -> TestResult {
    let n = GRACE_BLOCK * 640;
    let items: Vec<Arc<u64>> = (0..n as u64).map(Arc::new).collect();
    let (first, last) = (Arc::downgrade(&items[0]), Arc::downgrade(&items[n - 1]));
    let lock = ksync::SpinLock::named("mm.test.atomic3", ());
    let leaked0 = grace::stats().leaked;
    let hold = grace::hold_for_test().ok_or("no slot to hold the grace period")?;
    let before = noted();
    {
        let _g = lock.lock();
        for a in items {
            grace::defer_arc(a, 0);
        }
    }
    let during = noted() - before;
    let waiting = grace::stats().waiting;
    drop(hold);
    // Judged by the items themselves, within a time bound: `waiting()` is false as soon as a drain popped the last
    // item, and that drain (the reaper's, on the other CPU, maybe preempted) drops it a moment later. The desk saw
    // that window under load (int/atomic3, round 6); an item that is lost stays alive and ends the bound.
    let until = ksync::now_ns().ok_or("no clock")? + FREE_WITHIN_NS;
    let mut rounds = 0u64;
    while first.upgrade().is_some() || last.upgrade().is_some() {
        rounds += 1;
        let waits = grace::waiting();
        kassert!(ksync::now_ns().is_some_and(|t| t < until), "the deferred items are not freed within 10 s (round {rounds}, queue waiting: {waits})");
        if waits {
            grace::expedite();
        } else {
            sched::yield_now();
        }
    }
    let leaked = grace::stats().leaked - leaked0;
    klog::event!(Info, "mm", "ktest.grace_long_queue", deferred = n, waiting_at_once = waiting, noted = during, leaked = leaked, rounds = rounds);
    kassert!(leaked == 0, "{leaked} item(s) leaked: the queue had no room");
    kassert!(waiting >= n as u64, "only {waiting} items waited at once");
    kassert!(during == 0, "{during} heap request(s) of two pages or more where the heap may not reclaim");
    Ok(())
}
ktest!("mm.grace.long_queue", Safe, grace_long_queue);

/// One space maps `FILES` memfds, then forks: the reverse map gets two entries per file (16 bytes each). Its list
/// doubled under `mm.rmap` (16 KiB from a browser's fork on the Mini-PC); now it grows with the lock dropped.
fn rmap_fork_many_files() -> TestResult {
    const FILES: usize = 700;
    if vfs::root_ns().lookup("/dev/shm").is_err() {
        return ktest::fail("no /dev/shm in this image (memfds live there)");
    }
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))?;
    let mut files = Vec::with_capacity(FILES);
    for _ in 0..FILES {
        let f = vfs::memfd::create(b"mm-atomic3", 0, vfs::Cred::ROOT).map_err(|x| format!("memfd: {x}"))?;
        let i = f.inode().ok_or("no inode")?.clone();
        i.truncate(4096).map_err(|x| format!("size: {x}"))?;
        files.push((f, i));
    }
    let grown0 = crate::aspace::rmap::grown();
    let before = noted();
    for (f, _) in &files {
        a.sys_mmap_file(0, 4096, 3, MAP_SHARED, 0, &mut |len| vfs::mmap::check(f, 3, MAP_SHARED, 0, len)).map_err(|x| format!("mmap: {x}"))?;
    }
    let c = a.fork().map_err(|x| format!("fork: {x:?}"))?;
    let during = noted() - before;
    let grown = crate::aspace::rmap::grown() - grown0;
    let both = files.iter().filter(|(_, i)| crate::aspace::rmap::spaces_of(i) == 2).count();
    drop(c);
    drop(a);
    let left = files.iter().filter(|(_, i)| crate::aspace::rmap::spaces_of(i) != 0).count();
    klog::event!(Info, "mm", "ktest.rmap_fork_many_files", files = FILES, grown = grown, noted = during);
    kassert!(both == FILES, "{} of {FILES} files listed for both spaces", both);
    kassert!(left == 0, "{left} file(s) still listed after both spaces ended");
    kassert!(grown > 0, "the reverse map never grew: the test did not reach the path");
    kassert!(during == 0, "{during} heap request(s) of two pages or more where the heap may not reclaim");
    Ok(())
}
ktest!("mm.rmap.fork_many_files", Safe, rmap_fork_many_files);

static WIDE: hal::percpu::PerCpu<[u64; 1024]> = hal::percpu::PerCpu::new(|_| [0; 1024]);

/// A per-CPU container's first use makes 8 KiB per CPU (MSI's slot tables: 32 KiB on the Mini-PC, at a driver's
/// interrupt setup). They were made with interrupts off; now only the link is. In a thread of its own: the boot
/// thread's requests with only interrupts off are start-up to the detector and not counted.
fn percpu_first_use() -> TestResult {
    let noted_by_it = Arc::new(core::sync::atomic::AtomicU64::new(u64::MAX));
    let out = noted_by_it.clone();
    let h = sched::spawn("ktest.percpu", sched::Class::Normal, move || {
        let before = noted();
        let fresh = WIDE.get().iter().all(|&x| x == 0);
        let during = noted() - before;
        out.store(if fresh { during } else { u64::MAX - 1 }, core::sync::atomic::Ordering::Release);
    })
    .map_err(|x| format!("spawn: {x:?}"))?;
    kassert!(h.join_timeout(10_000_000_000), "the thread did not finish within 10 s");
    let during = noted_by_it.load(core::sync::atomic::Ordering::Acquire);
    kassert!(during != u64::MAX - 1, "the first value was not fresh");
    kassert!(WIDE.get_for(hal::percpu::possible_cpus().saturating_sub(1)).is_some(), "every possible CPU covered");
    kassert!(during == 0, "{during} heap request(s) of two pages or more where the heap may not reclaim");
    Ok(())
}
ktest!("mm.percpu.first_use", Safe, percpu_first_use);

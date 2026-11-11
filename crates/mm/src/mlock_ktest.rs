//! GM-2a CR 4: the mlock family's answers as `aspace::mlock` gives them (the caller's limit passed in); the same calls as a
//! program makes them are in user/abi-tests/c/memlock.c under `cargo xtask abi-diff`. Class Safe: only this test's own
//! address spaces.

use crate::linux::{MAP_ANONYMOUS, MAP_PRIVATE, madv};
use crate::{AddressSpace, Fill, LockLimit};
use crate::linux::Errno;
use alloc::format;
use alloc::string::String;
use hal::paging;
use ktest::{TestResult, kassert, ktest};

const P: u64 = 4096;
const RW: u64 = 3;
const AP: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const NOCAP: LockLimit = LockLimit { bytes: 4 * P, capable: false };
const CAP: LockLimit = LockLimit { bytes: 0, capable: true };

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn space() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(e)
}

fn limits_and_ranges() -> TestResult {
    let a = space()?;
    let m = a.sys_mmap(0, 8 * P, RW, AP, 0).map_err(e)?;
    kassert!(a.sys_mlock(m, P, 0, LockLimit { bytes: 0, capable: false }) == Err(Errno::Perm), "no limit, no capability");
    kassert!(a.sys_mlock(m, P, 2, NOCAP) == Err(Errno::Inval), "mlock2's flags before anything else");
    kassert!(a.sys_mlock(m, 5 * P, 0, NOCAP) == Err(Errno::NoMem), "above the limit");
    kassert!(a.locked_pages() == 0, "nothing locked by the refusals");

    kassert!(a.sys_mlock(m, 4 * P, 0, NOCAP) == Ok(()), "up to the limit");
    kassert!(a.locked_pages() == 4 && paging::count_user_leaves(a.root(), m, m + 8 * P) >= 4, "filled, counted");
    kassert!(a.sys_mlock(m, 4 * P, 0, NOCAP) == Ok(()), "locked pages do not count twice");
    kassert!(a.sys_mlock(m + 4 * P, P, 0, NOCAP) == Err(Errno::NoMem), "the limit counts what is locked");
    kassert!(a.sys_munlock(m + P, 2 * P) == Ok(()) && a.locked_pages() == 2, "munlock of the middle");

    // An unaligned start takes its page, a length of 0 at an unaligned start a whole one.
    kassert!(a.sys_munlock(m, 8 * P) == Ok(()) && a.locked_pages() == 0, "all unlocked");
    kassert!(a.sys_mlock(m + 16, 0, 0, CAP) == Ok(()) && a.locked_pages() == 1, "PAGE_ALIGN(len + offset)");
    kassert!(a.sys_munlock(m, P) == Ok(()), "unlock");
    kassert!(a.sys_mlock(m, 0, 0, NOCAP) == Ok(()) && a.locked_pages() == 0, "an empty range");
    kassert!(a.sys_mlock(m, u64::MAX - 8192, 0, CAP) == Err(Errno::Inval), "a range past 2^64");
    kassert!(a.sys_mlock(m, u64::MAX - 8192, 0, NOCAP) == Err(Errno::NoMem), "the limit comes first");

    // A hole: ENOMEM, what lies before it stays changed.
    a.sys_munmap(m + 2 * P, P).map_err(e)?;
    kassert!(a.sys_mlock(m, 4 * P, 0, CAP) == Err(Errno::NoMem) && a.locked_pages() == 2, "the hole");
    kassert!(a.sys_munlock(m, 4 * P) == Err(Errno::NoMem) && a.locked_pages() == 0, "munlock over the hole");

    // On fault: nothing filled, still counted.
    let o = a.sys_mmap(0, 4 * P, RW, AP, 0).map_err(e)?;
    kassert!(a.sys_mlock(o, 4 * P, 1, CAP) == Ok(()), "MLOCK_ONFAULT");
    kassert!(a.locked_pages() == 4 && paging::count_user_leaves(a.root(), o, o + 4 * P) == 0, "locked, not filled");
    kassert!(a.sys_munmap(o, 4 * P).is_ok() && a.locked_pages() == 0, "munmap ends it");

    // PROT_NONE cannot be filled: the flag is on and the answer ENOMEM, as Linux's populate gives.
    let n = a.sys_mmap(0, P, 0, AP, 0).map_err(e)?;
    kassert!(a.sys_mlock(n, P, 0, CAP) == Err(Errno::NoMem) && a.locked_pages() == 1, "PROT_NONE");
    Ok(())
}
ktest!("mm.mlock.limits", Safe, limits_and_ranges);

fn all_and_future() -> TestResult {
    let a = space()?;
    let m = a.sys_mmap(0, 8 * P, RW, AP, 0).map_err(e)?;
    for bad in [0, 8, 4] {
        kassert!(a.sys_mlockall(bad, NOCAP) == Err(Errno::Inval), "mlockall({bad})");
    }
    kassert!(a.sys_mlockall(1, LockLimit { bytes: 0, capable: false }) == Err(Errno::Perm), "no limit");
    kassert!(a.sys_mlockall(1, NOCAP) == Err(Errno::NoMem), "MCL_CURRENT above the limit");
    kassert!(a.sys_mlockall(1, CAP) == Ok(()) && a.locked_pages() == 8, "MCL_CURRENT");
    kassert!(paging::count_user_leaves(a.root(), m, m + 8 * P) == 8, "filled");
    kassert!(a.sys_munlockall() == Ok(()) && a.locked_pages() == 0, "munlockall");

    kassert!(a.sys_mlockall(2, NOCAP) == Ok(()) && a.locked_pages() == 0, "MCL_FUTURE locks nothing yet");
    let f = a.sys_mmap(0, 2 * P, RW, AP, 0).map_err(e)?;
    kassert!(a.locked_pages() == 2 && paging::count_user_leaves(a.root(), f, f + 2 * P) == 2, "a new mapping is locked and filled");
    kassert!(a.sys_mmap(0, 3 * P, RW, AP, 0) == Err(Errno::Again), "above the limit: EAGAIN");
    // The limit and the capability are the ones of the mapping's call, not of the mlockall.
    a.refresh_future_limit(|| CAP);
    kassert!(a.sys_mmap(0, 3 * P, RW, AP, 0).is_ok() && a.locked_pages() == 5, "a capability gained since");
    a.refresh_future_limit(|| NOCAP);
    kassert!(a.sys_mmap(0, P, RW, AP, 0) == Err(Errno::Again), "the capability dropped since");
    a.refresh_future_limit(|| LockLimit { bytes: 6 * P, capable: false });
    kassert!(a.sys_mmap(0, P, RW, AP, 0).is_ok() && a.locked_pages() == 6, "a limit raised since");
    kassert!(a.sys_munlockall() == Ok(()), "munlockall ends MCL_FUTURE");
    let mut asked = false;
    a.refresh_future_limit(|| {
        asked = true;
        CAP
    });
    kassert!(!asked, "no MCL_FUTURE: nobody asks for the limit");
    let g = a.sys_mmap(0, 16 * P, RW, AP, 0).map_err(e)?;
    kassert!(a.locked_pages() == 0 && paging::count_user_leaves(a.root(), g, g + 16 * P) == 0, "the next is plain");
    Ok(())
}
ktest!("mm.mlock.all", Safe, all_and_future);

fn effects() -> TestResult {
    let a = space()?;
    let m = a.sys_mmap(0, 4 * P, RW, AP, 0).map_err(e)?;
    a.sys_mlock(m, 2 * P, 0, CAP).map_err(e)?;
    kassert!(a.sys_madvise(m, P, madv::DONTNEED) == Err(Errno::Inval), "DONTNEED on a locked VMA");
    kassert!(a.sys_madvise(m, P, madv::DONTNEED_LOCKED) == Ok(()), "DONTNEED_LOCKED");
    kassert!(a.sys_madvise(m + 2 * P, P, madv::DONTNEED) == Ok(()), "the unlocked rest");
    kassert!(a.sys_msync(m, P, crate::aspace::MS_INVALIDATE) == Err(uapi::errno::EBUSY), "MS_INVALIDATE on a locked VMA");
    kassert!(a.sys_msync(m + 2 * P, P, crate::aspace::MS_INVALIDATE) == Ok(()), "and on the rest");
    let c = a.fork().map_err(e)?;
    kassert!(c.locked_pages() == 0 && a.locked_pages() == 2, "fork does not inherit");
    Ok(())
}
ktest!("mm.mlock.effects", Safe, effects);

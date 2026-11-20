//! SYS-A: the desktop session's RLIMIT_NICE, set by bitscomp while it is still root, right before it gives root up
//! (drop_root), so the session and every program it starts inherit it, as pam_limits gives a login session its
//! limits.conf values. Linux checks the target's own limit when the nice value goes down (kernel/sys.c
//! `set_one_prio` -> `can_nice(p, niceval)`, kernel/sched/syscalls.c: `nice_rlim = nice_to_rlimit(nice)` must be at
//! most `task_rlimit(p, RLIMIT_NICE)`), and V2 does the same (syscall sched_sys.rs `is_nice_reduction`). Without
//! a limit (0) the task manager could lower a program's Vorrang but never give it back: Normal (nice 0) from
//! Niedrig is a reduction too. 25 = 20 - (-5): Normal back and Hoch (-5) work, nothing beyond.

/// RLIMIT_NICE as `20 - nice`: down to nice -5.
pub const SESSION: u64 = 25;

/// Sets soft and hard limit; only root may raise the hard one (EPERM otherwise, then nothing changes).
pub fn session_nice() -> Result<(), String> {
    let r = libc::rlimit { rlim_cur: SESSION, rlim_max: SESSION };
    // SAFETY: setrlimit reads `r`, which outlives the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_NICE, &r) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error().to_string()) }
}

/// The limit now (soft), for `ctl system`'s checks.
pub fn current() -> Option<u64> {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes `r`, which outlives the call.
    (unsafe { libc::getrlimit(libc::RLIMIT_NICE, &mut r) } == 0).then_some(r.rlim_cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_allows_minus_five_and_no_more() {
        assert_eq!(20 - SESSION as i64, -5);
        assert_eq!(libc::RLIMIT_NICE as u32, 13, "linux-7.3.1 include/uapi/asm-generic/resource.h RLIMIT_NICE");
        assert!(current().is_some());
    }
}

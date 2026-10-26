//! Resource limits per process (`getrlimit`/`setrlimit`/`prlimit64`; linux-6.6/kernel/sys.c `do_prlimit`
//! :1455). The table has Linux's `RLIM_NLIMITS` entries (an ABI constant). Defaults are the kernel's
//! `INIT_RLIMITS` (include/asm-generic/resource.h) with NPROC and SIGPENDING computed from RAM as `fork_init`
//! does. RLIMIT_NOFILE acts on the fd table (FS), RLIMIT_NPROC on fork and clone (AB-4), RLIMIT_STACK on
//! execve's argument space (AB-4); the others are kept and reported (R1: the memory hook, not rlimits, bounds
//! memory).

use ksync::SpinLock;
use uapi::ab::rlimit::*;
use uapi::errno::*;
use vfs::Errno;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rlimit {
    pub cur: u64,
    pub max: u64,
}

impl Rlimit {
    pub fn from_bytes(b: &[u8; 16]) -> Self {
        Rlimit { cur: u64::from_le_bytes(b[0..8].try_into().unwrap_or([0; 8])), max: u64::from_le_bytes(b[8..16].try_into().unwrap_or([0; 8])) }
    }
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.cur.to_le_bytes());
        b[8..16].copy_from_slice(&self.max.to_le_bytes());
        b
    }
}

/// `max_threads` of `fork_init` (`/proc/sys/kernel/threads-max`) for `ram_pages` pages: RAM / (8 *
/// THREAD_SIZE), THREAD_SIZE 16 KiB, at least 20 (`MIN_THREADS`), at most `FUTEX_TID_MASK`.
pub fn max_threads_default(ram_pages: u64) -> u64 {
    (ram_pages.saturating_mul(4096) / (16384 * 8)).clamp(20, 0x3fff_ffff)
}

/// `max_threads / 2`: the RLIMIT_NPROC a process starts with.
pub fn nproc_default(ram_pages: u64) -> u64 {
    max_threads_default(ram_pages) / 2
}

/// The limits a new process starts with.
pub fn defaults(ram_pages: u64) -> [Rlimit; RLIM_NLIMITS as usize] {
    let inf = Rlimit { cur: RLIM_INFINITY, max: RLIM_INFINITY };
    let n = nproc_default(ram_pages);
    let mut t = [inf; RLIM_NLIMITS as usize];
    t[RLIMIT_STACK as usize] = Rlimit { cur: uapi::ab::exec::STACK_RLIMIT, max: RLIM_INFINITY };
    t[RLIMIT_CORE as usize] = Rlimit { cur: 0, max: RLIM_INFINITY };
    t[RLIMIT_NPROC as usize] = Rlimit { cur: n, max: n };
    t[RLIMIT_NOFILE as usize] = Rlimit { cur: INR_OPEN_CUR, max: INR_OPEN_MAX };
    t[RLIMIT_MEMLOCK as usize] = Rlimit { cur: MLOCK_LIMIT, max: MLOCK_LIMIT };
    t[RLIMIT_SIGPENDING as usize] = Rlimit { cur: n, max: n };
    t[RLIMIT_MSGQUEUE as usize] = Rlimit { cur: MQ_BYTES_MAX, max: MQ_BYTES_MAX };
    t[RLIMIT_NICE as usize] = Rlimit { cur: 0, max: 0 };
    t[RLIMIT_RTPRIO as usize] = Rlimit { cur: 0, max: 0 };
    t
}

/// `do_prlimit` on a table: EINVAL for an unknown resource or cur > max, EPERM for a NOFILE ceiling above
/// `nr_open` and for raising a hard limit without CAP_SYS_RESOURCE (`privileged`). Returns the old value.
pub fn apply(t: &mut [Rlimit; RLIM_NLIMITS as usize], resource: u32, new: Option<Rlimit>, nr_open: u64, privileged: bool) -> Result<Rlimit, Errno> {
    if resource >= RLIM_NLIMITS {
        return Err(EINVAL);
    }
    let r = &mut t[resource as usize];
    if let Some(n) = new {
        if n.cur > n.max {
            return Err(EINVAL);
        }
        if resource == RLIMIT_NOFILE && n.max > nr_open {
            return Err(EPERM);
        }
        if n.max > r.max && !privileged {
            return Err(EPERM);
        }
    }
    let old = *r;
    if let Some(n) = new {
        *r = n;
    }
    Ok(old)
}

pub struct Limits {
    t: SpinLock<[Rlimit; RLIM_NLIMITS as usize]>,
}

impl Limits {
    pub fn new(ram_pages: u64) -> Self {
        Limits { t: SpinLock::named("proc.rlimits", defaults(ram_pages)) }
    }
    pub fn get(&self, resource: u32) -> Option<Rlimit> {
        self.t.lock().get(resource as usize).copied()
    }
    pub fn prlimit(&self, resource: u32, new: Option<Rlimit>, nr_open: u64, privileged: bool) -> Result<Rlimit, Errno> {
        apply(&mut self.t.lock(), resource, new, nr_open, privileged)
    }
    /// fork: the child starts with the parent's limits (Linux `copy_signal` copies `rlim`).
    pub fn copy(&self) -> Self {
        Limits { t: SpinLock::named("proc.rlimits", *self.t.lock()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_defaults() {
        // 32 GiB: max_threads = 8,388,608 pages * 4096 / 131072 = 262,144; half of it.
        let t = defaults(8 << 20);
        assert_eq!(t[RLIMIT_NPROC as usize], Rlimit { cur: 131_072, max: 131_072 });
        assert_eq!(t[RLIMIT_NOFILE as usize], Rlimit { cur: 1024, max: 4096 });
        assert_eq!(t[RLIMIT_STACK as usize], Rlimit { cur: 8 << 20, max: RLIM_INFINITY });
        assert_eq!(t[RLIMIT_CORE as usize].cur, 0);
        assert_eq!(t[RLIMIT_CPU as usize].cur, RLIM_INFINITY);
        assert_eq!(nproc_default(0), 10, "MIN_THREADS 20, halved");
    }

    #[test]
    fn prlimit_rules() {
        let mut t = defaults(1 << 20);
        let nofile = RLIMIT_NOFILE;
        assert_eq!(apply(&mut t, 16, None, 1 << 20, false), Err(EINVAL));
        assert_eq!(apply(&mut t, nofile, Some(Rlimit { cur: 10, max: 5 }), 1 << 20, false), Err(EINVAL));
        assert_eq!(apply(&mut t, nofile, Some(Rlimit { cur: 10, max: 8192 }), 1 << 20, false), Err(EPERM), "raising the hard limit");
        assert_eq!(apply(&mut t, nofile, Some(Rlimit { cur: 10, max: 2 << 20 }), 1 << 20, true), Err(EPERM), "above nr_open even when privileged");
        assert_eq!(apply(&mut t, nofile, Some(Rlimit { cur: 256, max: 2048 }), 1 << 20, false), Ok(Rlimit { cur: 1024, max: 4096 }));
        assert_eq!(apply(&mut t, nofile, Some(Rlimit { cur: 2048, max: 2048 }), 1 << 20, false), Ok(Rlimit { cur: 256, max: 2048 }));
        assert_eq!(apply(&mut t, nofile, Some(Rlimit { cur: 1, max: 4096 }), 1 << 20, false), Err(EPERM), "a lowered hard limit stays lowered");
        assert_eq!(apply(&mut t, RLIMIT_CORE, None, 0, false), Ok(Rlimit { cur: 0, max: RLIM_INFINITY }));
        let b = Rlimit { cur: 7, max: 9 }.to_bytes();
        assert_eq!(Rlimit::from_bytes(&b), Rlimit { cur: 7, max: 9 });
    }
}

//! SYS-K: a thread's I/O accounting, Linux's `struct task_io_accounting` (linux-7.3.1/include/linux/task_io_accounting.h)
//! behind `/proc/<pid>/io`: bytes and calls of the read and write family (counted by the system calls) and the
//! bytes the thread made the storage read or write (counted by the block layer when it submits). Atomics: the
//! owner adds, `/proc` readers on other CPUs read.

use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoCounts {
    pub rchar: u64,
    pub wchar: u64,
    pub syscr: u64,
    pub syscw: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled_write_bytes: u64,
}

impl IoCounts {
    pub fn plus(self, o: IoCounts) -> IoCounts {
        IoCounts {
            rchar: self.rchar.wrapping_add(o.rchar),
            wchar: self.wchar.wrapping_add(o.wchar),
            syscr: self.syscr.wrapping_add(o.syscr),
            syscw: self.syscw.wrapping_add(o.syscw),
            read_bytes: self.read_bytes.wrapping_add(o.read_bytes),
            write_bytes: self.write_bytes.wrapping_add(o.write_bytes),
            cancelled_write_bytes: self.cancelled_write_bytes.wrapping_add(o.cancelled_write_bytes),
        }
    }
}

#[derive(Default)]
pub struct IoAcct([AtomicU64; 7]);

impl IoAcct {
    pub const fn new() -> IoAcct {
        IoAcct([const { AtomicU64::new(0) }; 7])
    }
    fn add(&self, i: usize, v: u64) {
        self.0[i].fetch_add(v, Ordering::Relaxed);
    }
    /// Adds a whole set (a thread that ended, to its group's tally).
    pub fn add_all(&self, c: IoCounts) {
        for (i, v) in [c.rchar, c.wchar, c.syscr, c.syscw, c.read_bytes, c.write_bytes, c.cancelled_write_bytes].into_iter().enumerate() {
            self.add(i, v);
        }
    }
    /// Used as a high-water mark: `c` raised field by field to what was reported before, and remembered. An
    /// ending thread leaves its group's list before its counts reach the group's tally, so a reader in between
    /// misses them once; tools take rates from differences and would see a dip, then a spike.
    pub fn raise_to(&self, c: IoCounts) -> IoCounts {
        let m = |i: usize, v: u64| self.0[i].fetch_max(v, Ordering::AcqRel).max(v);
        IoCounts {
            rchar: m(0, c.rchar),
            wchar: m(1, c.wchar),
            syscr: m(2, c.syscr),
            syscw: m(3, c.syscw),
            read_bytes: m(4, c.read_bytes),
            write_bytes: m(5, c.write_bytes),
            cancelled_write_bytes: m(6, c.cancelled_write_bytes),
        }
    }
    pub fn counts(&self) -> IoCounts {
        let r = |i: usize| self.0[i].load(Ordering::Relaxed);
        IoCounts { rchar: r(0), wchar: r(1), syscr: r(2), syscw: r(3), read_bytes: r(4), write_bytes: r(5), cancelled_write_bytes: r(6) }
    }
}

fn with_current(f: impl FnOnce(&IoAcct)) {
    if let Some(t) = crate::current_thread() {
        f(&t.io);
    }
}

/// A read-family call ended with `ret` (Linux `vfs_read`: `add_rchar` for what it read, `inc_syscr` always).
pub fn read_call(ret: i64) {
    with_current(|a| {
        a.add(2, 1);
        if ret > 0 {
            a.add(0, ret as u64);
        }
    });
}

/// A write-family call ended with `ret`.
pub fn write_call(ret: i64) {
    with_current(|a| {
        a.add(3, 1);
        if ret > 0 {
            a.add(1, ret as u64);
        }
    });
}

/// The calling thread made the storage read `bytes` (`task_io_account_read`).
pub fn storage_read(bytes: u64) {
    with_current(|a| a.add(4, bytes));
}

/// The calling thread sent `bytes` to the storage (`task_io_account_write`).
pub fn storage_write(bytes: u64) {
    with_current(|a| a.add(5, bytes));
}

/// The calling thread's counts.
pub fn current() -> IoCounts {
    crate::current_thread().map(|t| t.io.counts()).unwrap_or_default()
}

/// Thread `id`'s counts (None once it is gone).
pub fn of_thread(id: u64) -> Option<IoCounts> {
    crate::table::get(id).map(|t| t.io.counts())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_adds_up_field_by_field() {
        let a = IoAcct::new();
        a.add_all(IoCounts { rchar: 5, syscr: 1, write_bytes: 4096, ..Default::default() });
        a.add_all(IoCounts { rchar: 2, wchar: 9, syscw: 1, ..Default::default() });
        let c = a.counts();
        assert_eq!((c.rchar, c.wchar, c.syscr, c.syscw, c.write_bytes), (7, 9, 1, 1, 4096));
        assert_eq!(c.plus(c).rchar, 14);
    }

    #[test]
    fn a_read_never_goes_back() {
        let seen = IoAcct::new();
        assert_eq!(seen.raise_to(IoCounts { rchar: 100, wchar: 7, ..Default::default() }).rchar, 100);
        // A read while a thread moved from the list to the tally: its 60 bytes missing once.
        let b = seen.raise_to(IoCounts { rchar: 40, wchar: 9, ..Default::default() });
        assert_eq!((b.rchar, b.wchar), (100, 9));
        assert_eq!(seen.raise_to(IoCounts { rchar: 130, ..Default::default() }).rchar, 130);
    }
}

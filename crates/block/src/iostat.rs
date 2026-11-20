//! SYS-K: the per-direction numbers of Linux's `/proc/diskstats` and `/sys/block/<dev>/stat`
//! (linux-7.3.1/Documentation/admin-guide/iostats.rst): requests completed, merged, 512-byte sectors and the time
//! from submission to completion, for reads, writes, discards and flushes. A disk counts what it completed itself;
//! a partition (a stacked disk) counts its own and the disk below counts it again, as Linux counts a partition's
//! I/O for the partition and the whole disk.

use crate::queue::Kind;
use core::sync::atomic::{AtomicU64, Ordering};

pub const READ: usize = 0;
pub const WRITE: usize = 1;
pub const DISCARD: usize = 2;
pub const FLUSH: usize = 3;

/// Linux's `STAT_*` index of a request kind.
pub fn index(kind: Kind) -> usize {
    match kind {
        Kind::Read => READ,
        Kind::Write { .. } => WRITE,
        Kind::Discard => DISCARD,
        Kind::Flush => FLUSH,
    }
}

#[derive(Default)]
pub(crate) struct IoCounters {
    ios: [AtomicU64; 4],
    merges: [AtomicU64; 4],
    sectors: [AtomicU64; 4],
    nsecs: [AtomicU64; 4],
}

/// A copy for readers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoStat {
    pub ios: [u64; 4],
    pub merges: [u64; 4],
    pub sectors: [u64; 4],
    pub nsecs: [u64; 4],
}

impl IoCounters {
    /// One request of `kind` completed: `bytes` moved (none for a flush), `ns` since it was submitted.
    pub(crate) fn done(&self, kind: Kind, bytes: u64, ns: u64) {
        let i = index(kind);
        self.ios[i].fetch_add(1, Ordering::Relaxed);
        self.sectors[i].fetch_add(bytes / 512, Ordering::Relaxed);
        self.nsecs[i].fetch_add(ns, Ordering::Relaxed);
    }
    pub(crate) fn merged(&self, kind: Kind, n: u64) {
        self.merges[index(kind)].fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn snapshot(&self) -> IoStat {
        let r = |a: &[AtomicU64; 4]| core::array::from_fn(|i| a[i].load(Ordering::Relaxed));
        IoStat { ios: r(&self.ios), merges: r(&self.merges), sectors: r(&self.sectors), nsecs: r(&self.nsecs) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_land_in_their_direction() {
        let c = IoCounters::default();
        c.done(Kind::Read, 4096, 1_500_000);
        c.done(Kind::Write { fua: false, preflush: false }, 1024, 2_000_000);
        c.done(Kind::Flush, 0, 700_000);
        c.merged(Kind::Read, 2);
        let s = c.snapshot();
        assert_eq!((s.ios, s.sectors, s.merges[READ]), ([1, 1, 0, 1], [8, 2, 0, 0], 2));
        assert_eq!(s.nsecs, [1_500_000, 2_000_000, 0, 700_000]);
    }
}

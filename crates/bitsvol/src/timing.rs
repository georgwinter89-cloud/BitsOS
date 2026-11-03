//! VS-7: what a commit costs and when that is too slow. A commit holds the pool lock, so its time is what every
//! other read and write of the pool waits; on the Mini-PC the pool lives in an image file on a USB stick, where a
//! cache flush is the image's fsync (its dirty pages, the FAT, SYNCHRONIZE CACHE).
//!
//! Slow means slower than the stick can explain: `SLOW_BASE_NS` for the flushes plus the bytes the flushes had
//! to make durable at `SLOW_MIN_BPS`. Georg's stick writes ~1.8 MB/s on small files (desk-minipc-w7.md:6) and
//! its cache flush takes tens to hundreds of ms; the floor is about half that, so a warning means the stick or
//! something above it was clearly slower than it ever is on a normal day.

use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

/// Two cache flushes and the image file's file-system metadata on a cheap stick (the old fixed threshold).
pub const SLOW_BASE_NS: u64 = 1_000_000_000;
/// The slowest write rate a working stick reaches (bytes/s, about half of Georg's measured 1.8 MB/s).
pub const SLOW_MIN_BPS: u64 = 1 << 20;
/// Written bytes from which a commit flushes the device before it takes the pool lock (`PoolHandle::timed`):
/// ~125 ms of write-back on Georg's stick, more than an empty flush costs (tens of ms).
pub const PREFLUSH_BYTES: u64 = 256 << 10;
/// Commits shorter than this are never reported as the longest so far (noise of fast commits).
const LONGEST_FLOOR_NS: u64 = 100_000_000;

klog::counter!(COMMITS, "bitsvol.commit.count");
klog::counter!(HELD_NS, "bitsvol.commit.ns");
klog::counter!(LOCK_WAIT_NS, "bitsvol.commit.lock_wait_ns");
klog::counter!(IO_NS, "bitsvol.commit.io_ns");
klog::counter!(PRE_NS, "bitsvol.commit.preflush_ns");
klog::counter!(FLUSHES, "bitsvol.commit.flushes");
klog::counter!(BYTES, "bitsvol.commit.bytes");
klog::counter!(COVERED, "bitsvol.commit.covered_bytes");
klog::counter!(MAX_NS, "bitsvol.commit.max_ns");
klog::counter!(SLOW, "bitsvol.commit.slow");
static MAX: AtomicU64 = AtomicU64::new(0);

/// Monotonic ns: the kernel's clock; in host tests the process clock (ksync has none there).
pub(crate) fn now() -> Option<u64> {
    #[cfg(test)]
    {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        ksync::now_ns().or_else(|| Some(T0.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64))
    }
    #[cfg(not(test))]
    ksync::now_ns()
}

/// The device side of a pool's commits, counted since open (`Pool::io_stats`); `since` gives one commit's part.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoStats {
    /// Cache flushes and FUA writes.
    pub flushes: u64,
    /// ns in them.
    pub flush_ns: u64,
    /// Bytes the commits wrote themselves (metadata pages, superblock).
    pub bytes: u64,
    /// ns in the metadata runs (a FUA superblock's time is in `flush_ns`).
    pub write_ns: u64,
    /// Volume bytes written before a flush that the flush made durable (data the file system wrote).
    pub covered: u64,
}

impl IoStats {
    pub fn since(&self, before: &IoStats) -> IoStats {
        IoStats {
            flushes: self.flushes.saturating_sub(before.flushes),
            flush_ns: self.flush_ns.saturating_sub(before.flush_ns),
            bytes: self.bytes.saturating_sub(before.bytes),
            write_ns: self.write_ns.saturating_sub(before.write_ns),
            covered: self.covered.saturating_sub(before.covered),
        }
    }
}

/// One commit as `PoolHandle` measured it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitTime {
    /// What asked for it: "flush", "fua" (a FUA write) or "commit" (snapshots, settle).
    pub why: &'static str,
    /// Waiting for the pool lock before it.
    pub lock_wait_ns: u64,
    /// With the pool lock held (what other I/O of the pool waited).
    pub ns: u64,
    /// The flush before the lock (0: none) and the volume bytes it made durable.
    pub pre_ns: u64,
    pub pre_bytes: u64,
    pub io: IoStats,
}

impl CommitTime {
    /// Time in device calls (the rest of `ns` is the commit's own work: trees, checksums).
    pub fn io_ns(&self) -> u64 {
        self.io.flush_ns.saturating_add(self.io.write_ns)
    }

    /// What the stick may take for this commit, its flush before the lock included (module text).
    pub fn budget_ns(&self) -> u64 {
        let bytes = self.io.bytes.saturating_add(self.io.covered).saturating_add(self.pre_bytes);
        let carry = (bytes as u128 * 1_000_000_000 / SLOW_MIN_BPS as u128).min(u64::MAX as u128) as u64;
        SLOW_BASE_NS.saturating_add(carry)
    }

    pub fn slow(&self) -> bool {
        self.ns.saturating_add(self.pre_ns) > self.budget_ns()
    }
}

/// Counts `t` and logs it: Warn `commit.slow` over its budget, Info `commit.longest` when its held time is more
/// than a quarter above the longest one reported so far (at most ~26 lines per boot between 100 ms and 30 s),
/// Debug `commit.time` otherwise.
pub(crate) fn record(pool: &String, longest: &AtomicU64, t: &CommitTime) {
    COMMITS.inc();
    HELD_NS.add(t.ns);
    LOCK_WAIT_NS.add(t.lock_wait_ns);
    IO_NS.add(t.io_ns());
    PRE_NS.add(t.pre_ns);
    FLUSHES.add(t.io.flushes);
    BYTES.add(t.io.bytes);
    COVERED.add(t.io.covered);
    let prev = MAX.fetch_max(t.ns, Ordering::Relaxed);
    if t.ns > prev {
        MAX_NS.add(t.ns - prev);
    }
    macro_rules! ev {
        ($lvl:ident, $name:literal $(; $msg:literal)?) => {
            klog::event!($lvl, "bitsvol", $name, pool = pool.clone(), why = t.why, ns = t.ns, lock_wait_ns = t.lock_wait_ns,
                io_ns = t.io_ns(), flush_ns = t.io.flush_ns, flushes = t.io.flushes, bytes = t.io.bytes,
                covered = t.io.covered, pre_ns = t.pre_ns, pre_bytes = t.pre_bytes, budget_ns = t.budget_ns() $(; $msg)?)
        };
    }
    // `longest` is the longest commit a line reported: the pool's real longest is never more than a quarter
    // above the last line (`bitsvol.commit.max_ns` has the exact value).
    if t.slow() {
        SLOW.inc();
        longest.fetch_max(t.ns, Ordering::Relaxed);
        ev!(Warn, "commit.slow"; "slower than the stick explains");
    } else if t.ns >= LONGEST_FLOOR_NS && t.ns > longest.load(Ordering::Relaxed).saturating_mul(5) / 4 {
        longest.fetch_max(t.ns, Ordering::Relaxed);
        ev!(Info, "commit.longest");
    } else {
        ev!(Debug, "commit.time");
    }
}

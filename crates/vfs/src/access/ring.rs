//! The per-CPU rings of the access log. Each CPU's ring is made once (an initcall, where allocating is fine) and
//! never grows: a hook only moves its record into a free slot under this CPU's lock, or folds it into the last
//! record when it repeats it (a program opening the same file in a loop costs one slot), or counts it as lost
//! when the ring is full. A path is kept weakly and only for a new slot (a file's folder and name come from the
//! dentry's own lock, only tried under the ring's: no lock order between them); nothing is dropped under the lock.
//! The last `RESERVE` slots are only for exec and fork records: without them the reader would put what a new
//! program does under the program it replaced (or its parent), so they must survive a burst of file records.

use super::{Head, Kind, ObjRef, Rec};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use hal::percpu::{PerCpu, this_cpu};
use ksync::SpinLock;

/// Records one CPU keeps until the reader comes (about 96 KiB per CPU).
pub const CAP: usize = 1024;
/// Slots only exec and fork records may take.
pub const RESERVE: usize = 64;

/// The records that say which program a pid runs.
fn identity(k: Kind) -> bool {
    matches!(k, Kind::Exec | Kind::Fork)
}

/// An exec or fork record was lost since the reader last looked (it then checks pids against the live table).
static IDENTITY_LOST: AtomicBool = AtomicBool::new(false);

pub struct Ring {
    slots: Box<[Option<Rec>]>,
    head: usize,
    len: usize,
    /// Records this ring could not keep (the ktests count one CPU exactly).
    lost: u64,
}

impl Ring {
    fn new(cap: usize) -> Ring {
        Ring { slots: (0..cap).map(|_| None).collect(), head: 0, len: 0, lost: 0 }
    }

    fn cap(&self) -> usize {
        self.slots.len()
    }

    /// True: a new slot was taken.
    fn push(&mut self, h: &Head, obj: ObjRef<'_>) -> bool {
        if self.len > 0 {
            let i = (self.head + self.len - 1) % self.cap();
            if let Some(last) = self.slots[i].as_mut()
                && last.kind == h.kind
                && h.kind != Kind::NetClose
                && last.pid == h.pid
                && last.flags == h.flags
                && obj.same(&last.obj)
            {
                last.n = last.n.saturating_add(1);
                last.t1 = h.t;
                return false;
            }
        }
        let room = if identity(h.kind) { self.cap() } else { self.cap() - RESERVE };
        if self.len >= room {
            self.lost += 1;
            LOST.inc();
            if identity(h.kind) {
                IDENTITY_LOST.store(true, Ordering::Relaxed);
            }
            return false;
        }
        let i = (self.head + self.len) % self.cap();
        // The slot is empty (taken by the reader or never used): nothing is dropped here.
        self.slots[i] = Some(Rec { kind: h.kind, flags: h.flags, pid: h.pid, uid: h.uid, n: 1, t0: h.t, t1: h.t, tx: h.tx, rx: h.rx, obj: obj.owned() });
        self.len += 1;
        true
    }

    /// Moves every record into `out` (which has room for them: no allocation under the lock).
    fn take_into(&mut self, out: &mut Vec<Rec>) {
        while self.len > 0 && out.len() < out.capacity() {
            if let Some(r) = self.slots[self.head].take() {
                out.push(r);
            }
            self.head = (self.head + 1) % self.cap();
            self.len -= 1;
        }
    }
}

fn new_ring(_cpu: usize) -> SpinLock<Ring> {
    SpinLock::named("vfs.access.ring", Ring::new(CAP))
}

static RINGS: PerCpu<SpinLock<Ring>> = PerCpu::new(new_ring);
klog::counter!(LOST, "vfs.access.lost");
klog::counter!(KEPT, "vfs.access.records");

/// Makes every possible CPU's ring (the initcall; host tests call it first).
pub fn prepare() -> Result<(), &'static str> {
    RINGS.cover_possible();
    Ok(())
}
kreg::initcall!(kreg::Level::Subsys, "vfs.access_rings", prepare);

/// The hook's end: into this CPU's ring. A CPU without a ring yet (before the initcall) loses the record.
#[inline]
pub fn push(h: &Head, obj: ObjRef<'_>) {
    let Some(ring) = RINGS.find_for(this_cpu()) else {
        LOST.inc();
        if identity(h.kind) {
            IDENTITY_LOST.store(true, Ordering::Relaxed);
        }
        return;
    };
    if ring.lock().push(h, obj) {
        KEPT.inc();
    }
}

/// A record not kept (the log paused by a ktest).
pub(super) fn lost_one() {
    LOST.inc();
}

/// Whether an exec or fork record was lost since the last call.
pub fn take_identity_lost() -> bool {
    IDENTITY_LOST.swap(false, Ordering::Relaxed)
}

/// Records lost because a ring was full (or not made yet) since boot.
pub fn dropped() -> u64 {
    LOST.get()
}

/// Records kept since boot (a fold into the last record is not a new one).
pub fn kept() -> u64 {
    KEPT.get()
}

/// Every record waiting in every ring, oldest of each CPU first.
pub fn take_all() -> Vec<Rec> {
    let mut all = Vec::new();
    for ring in RINGS.iter_existing() {
        // Sized first, filled under the lock: what arrives in between waits for the next read.
        let n = ring.lock().len;
        if n == 0 {
            continue;
        }
        let mut v = Vec::with_capacity(n);
        ring.lock().take_into(&mut v);
        all.append(&mut v);
    }
    all
}

/// This CPU's ring for the ktests and host tests (callers keep the CPU: interrupts off).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    /// The storage's address and size (it never moves or grows).
    pub at: usize,
    pub cap: usize,
    pub len: usize,
    pub lost: u64,
    /// The newest record's count.
    pub last_n: Option<u32>,
}

pub fn look_this_cpu() -> Option<Look> {
    let r = RINGS.find_for(this_cpu())?;
    let g = r.lock();
    let last_n = (g.len > 0).then(|| g.slots[(g.head + g.len - 1) % g.cap()].as_ref().map_or(0, |r| r.n));
    Some(Look { at: g.slots.as_ptr() as usize, cap: g.cap(), len: g.len, lost: g.lost, last_n })
}


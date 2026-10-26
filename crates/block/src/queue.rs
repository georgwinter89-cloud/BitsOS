//! The request queue of one disk, as plain logic (host-tested): which pending requests the device gets next.
//!
//! - **Priority with aging (B4):** Interactive > Normal > Background. A request moves one class up for every
//!   `age_ns` it waited, so after `2 x age_ns` a Background request competes as Interactive; inside a class
//!   the oldest goes first. Nothing starves: a request's wait is bounded by `2 x age_ns` plus the requests
//!   that are older than it.
//! - **Order where it matters:** a request never overtakes an older one it overlaps when either writes
//!   (write/read, read/write, write/write, discards count as writes): the older one goes first. That keeps the
//!   answer of two overlapping requests the one their order gives, whatever their priorities. The older one
//!   then inherits the class of the request that waits for it (the device is told that priority).
//! - **Merging:** the chosen request takes along pending requests of the same kind that continue it at either
//!   end (front and back merges, repeated), while the whole stays within the device's `max_blocks`
//!   (`BlockInfo::max_blocks_per_request`, max_sectors). FUA writes merge only with FUA writes, a write with
//!   a flush before it never merges, discards merge up to `max_discard_blocks`, and every pending flush is
//!   answered by one device flush. A merge never takes a request past an older overlapping one (the rule above).
//!
//! Linux: blk-mq's merge rules (block/blk-merge.c `blk_rq_merge_ok`, `blk_try_merge`: back and front merge,
//! REQ_NOMERGE_FLAGS for flushes/FUA) and mq-deadline's FIFO expiry (block/mq-deadline.c) as the aging idea.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// FS-13: requests of Interactive or Normal priority queued on any disk now (not yet given to a device). The
/// preload pauses while one waits (`block::prio::foreground_waiting`).
#[cfg(not(test))]
static FOREGROUND_QUEUED: AtomicU64 = AtomicU64::new(0);

// Host tests run in parallel threads, each with its own elevators: one count per test thread.
#[cfg(test)]
std::thread_local! {
    static FOREGROUND_QUEUED: AtomicU64 = const { AtomicU64::new(0) };
}

#[cfg(not(test))]
fn counter<R>(f: impl FnOnce(&AtomicU64) -> R) -> R {
    f(&FOREGROUND_QUEUED)
}

#[cfg(test)]
fn counter<R>(f: impl FnOnce(&AtomicU64) -> R) -> R {
    FOREGROUND_QUEUED.with(|c| f(c))
}

pub fn foreground_queued() -> u64 {
    counter(|c| c.load(Ordering::Acquire))
}

fn note_queued(e: &Entry, up: bool) {
    if e.prio == IoPriority::Background {
        return;
    }
    counter(|c| {
        if up {
            c.fetch_add(1, Ordering::AcqRel);
        } else {
            let mut n = c.load(Ordering::Acquire);
            while n > 0 {
                match c.compare_exchange_weak(n, n - 1, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break,
                    Err(now) => n = now,
                }
            }
        }
    });
}
use devmodel::block::IoPriority;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    /// `fua`: durable when it completes; `preflush`: the device cache is flushed first ("flush with data").
    Write { fua: bool, preflush: bool },
    Discard,
    Flush,
}

impl Kind {
    /// Changes data on the medium (ordering and the write-refusal hook).
    pub fn writes(self) -> bool {
        matches!(self, Kind::Write { .. } | Kind::Discard)
    }
    fn has_range(self) -> bool {
        !matches!(self, Kind::Flush)
    }
    fn merges_with(self, o: Kind) -> bool {
        match (self, o) {
            (Kind::Read, Kind::Read) | (Kind::Discard, Kind::Discard) => true,
            (Kind::Write { fua: a, preflush: false }, Kind::Write { fua: b, preflush: false }) => a == b,
            _ => false,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Read => "read",
            Kind::Write { fua: false, preflush: false } => "write",
            Kind::Write { fua: true, preflush: false } => "write-fua",
            Kind::Write { fua: _, preflush: true } => "write-preflush",
            Kind::Discard => "discard",
            Kind::Flush => "flush",
        }
    }
}

/// One pending request. `id`s grow with arrival (the queue keeps arrival order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: u64,
    pub kind: Kind,
    pub lba: u64,
    pub blocks: u64,
    pub prio: IoPriority,
    /// When it was queued (ns, the caller's clock).
    pub at_ns: u64,
}

impl Entry {
    fn end(&self) -> u64 {
        self.lba.saturating_add(self.blocks)
    }
    fn overlaps(&self, o: &Entry) -> bool {
        self.kind.has_range() && o.kind.has_range() && self.lba < o.end() && o.lba < self.end()
    }
    /// `self` (older) must go before `o` (younger).
    fn orders_before(&self, o: &Entry) -> bool {
        self.overlaps(o) && (self.kind.writes() || o.kind.writes())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Largest merged read/write in blocks (at least 1).
    pub max_blocks: u64,
    pub max_discard_blocks: u64,
    /// Waiting this long moves a request one class up.
    pub age_ns: u64,
}

/// What the device gets next: `members` in LBA order (flushes: arrival order), together `blocks` from `lba`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    pub kind: Kind,
    pub lba: u64,
    pub blocks: u64,
    /// The best effective priority of the members (what the device is told).
    pub prio: IoPriority,
    pub members: Vec<Entry>,
    /// The chosen request was served above its own class because it had waited (aging).
    pub aged: bool,
}

fn class(p: IoPriority) -> u8 {
    match p {
        IoPriority::Interactive => 0,
        IoPriority::Normal => 1,
        IoPriority::Background => 2,
    }
}

fn prio_of(c: u8) -> IoPriority {
    match c {
        0 => IoPriority::Interactive,
        1 => IoPriority::Normal,
        _ => IoPriority::Background,
    }
}

/// The class a request competes in now: its own, one up per `age_ns` waited.
pub fn effective(e: &Entry, now_ns: u64, age_ns: u64) -> u8 {
    let waited = now_ns.saturating_sub(e.at_ns);
    let up = waited.checked_div(age_ns.max(1)).unwrap_or(0);
    class(e.prio).saturating_sub(up.min(2) as u8)
}

#[derive(Default)]
pub struct Elevator {
    /// Arrival order (ids grow).
    entries: Vec<Entry>,
    /// Host tests: the next this many picks find no memory.
    #[cfg(test)]
    pub(crate) oom_picks: u32,
}

impl Drop for Elevator {
    fn drop(&mut self) {
        for e in &self.entries {
            note_queued(e, false);
        }
    }
}

impl Elevator {
    pub const fn new() -> Elevator {
        Elevator {
            entries: Vec::new(),
            #[cfg(test)]
            oom_picks: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Queues `e` in id order (ids are taken before the queue lock, so two submitters may arrive swapped by a
    /// few nanoseconds). Err: no memory for the slot.
    pub fn push(&mut self, e: Entry) -> Result<(), Entry> {
        if self.entries.try_reserve(1).is_err() {
            return Err(e);
        }
        let at = self.entries.partition_point(|x| x.id < e.id);
        note_queued(&e, true);
        self.entries.insert(at, e);
        Ok(())
    }

    /// Takes a request back out (cancelled before the device got it).
    pub fn remove(&mut self, id: u64) -> Option<Entry> {
        let i = self.entries.iter().position(|e| e.id == id)?;
        Some(self.take(i))
    }

    pub fn contains(&self, id: u64) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    fn take(&mut self, i: usize) -> Entry {
        let e = self.entries.remove(i);
        note_queued(&e, false);
        e
    }

    /// The pending entries in arrival order (tests, the MCP view).
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Chooses the next batch and takes its members out of the queue. None: the queue is empty.
    /// `Some(Err(e))`: no memory for the batch's working lists; the chosen request `e` is taken out anyway
    /// (the caller fails it with ENOMEM), so every call makes progress and a dispatcher never spins on a
    /// queue it cannot serve.
    pub fn pick(&mut self, now_ns: u64, lim: &Limits) -> Option<Result<Batch, Entry>> {
        let age = lim.age_ns;
        let mut c = (0..self.entries.len()).min_by_key(|&i| (effective(&self.entries[i], now_ns, age), self.entries[i].id))?;
        // The class of the best request: one it waits for inherits it (the device hears this priority).
        let wanted = effective(&self.entries[c], now_ns, age);
        // An older request it overlaps (with a write involved) goes first; the oldest such, repeatedly.
        while let Some(i) = self.entries[..c].iter().position(|o| o.orders_before(&self.entries[c])) {
            c = i;
        }
        let head = self.entries[c];
        let aged = effective(&head, now_ns, age) < class(head.prio);
        #[cfg(test)]
        let oom = self.oom_picks > 0 && {
            self.oom_picks -= 1;
            true
        };
        #[cfg(not(test))]
        let oom = false;
        let mut take: Vec<usize> = Vec::new();
        if oom || take.try_reserve(1).is_err() {
            return Some(Err(self.take(c)));
        }
        take.push(c);
        let (mut lba, mut end) = (head.lba, head.end());
        if head.kind == Kind::Flush {
            for (i, e) in self.entries.iter().enumerate() {
                if i != c && e.kind == Kind::Flush && take.try_reserve(1).is_ok() {
                    take.push(i);
                }
            }
        } else {
            let limit = if head.kind == Kind::Discard { lim.max_discard_blocks } else { lim.max_blocks }.max(1);
            loop {
                let found = self.entries.iter().enumerate().position(|(i, e)| {
                    !take.contains(&i)
                        && e.kind.merges_with(head.kind)
                        && (e.lba == end || e.end() == lba)
                        && (end - lba).checked_add(e.blocks).is_some_and(|n| n <= limit)
                        // Not past an older overlapping request that stays behind.
                        && !self.entries[..i].iter().enumerate().any(|(j, o)| !take.contains(&j) && o.orders_before(e))
                });
                let Some(i) = found else { break };
                if take.try_reserve(1).is_err() {
                    break;
                }
                let e = self.entries[i];
                if e.lba == end {
                    end = e.end();
                } else {
                    lba = e.lba;
                }
                take.push(i);
            }
        }
        let mut members: Vec<Entry> = Vec::new();
        if members.try_reserve_exact(take.len()).is_err() {
            return Some(Err(self.take(c)));
        }
        members.extend(take.iter().map(|&i| self.entries[i]));
        if head.kind != Kind::Flush {
            members.sort_by_key(|e| e.lba);
        }
        let best = members.iter().map(|e| effective(e, now_ns, age)).min().unwrap_or(class(head.prio)).min(wanted);
        take.sort_unstable();
        for &i in take.iter().rev() {
            self.take(i);
        }
        let blocks = if head.kind == Kind::Flush { 0 } else { end - lba };
        Some(Ok(Batch { kind: head.kind, lba: if head.kind == Kind::Flush { 0 } else { lba }, blocks, prio: prio_of(best), members, aged }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use IoPriority::*;

    const AGE: u64 = 100;
    fn lim(max: u64) -> Limits {
        Limits { max_blocks: max, max_discard_blocks: 1 << 20, age_ns: AGE }
    }
    fn e(id: u64, kind: Kind, lba: u64, blocks: u64, prio: IoPriority, at: u64) -> Entry {
        Entry { id, kind, lba, blocks, prio, at_ns: at }
    }
    const W: Kind = Kind::Write { fua: false, preflush: false };
    const WF: Kind = Kind::Write { fua: true, preflush: false };
    const WP: Kind = Kind::Write { fua: false, preflush: true };
    const R: Kind = Kind::Read;

    /// `pick` where memory never runs out.
    fn pk(q: &mut Elevator, now: u64, l: &Limits) -> Option<Batch> {
        q.pick(now, l).map(|r| r.expect("no memory in a host test"))
    }

    fn ids(b: &Batch) -> Vec<u64> {
        b.members.iter().map(|m| m.id).collect()
    }

    #[test]
    fn adjacent_requests_merge_front_and_back_in_lba_order() {
        let mut q = Elevator::new();
        q.push(e(1, R, 10, 2, Normal, 0)).unwrap();
        q.push(e(2, R, 12, 3, Normal, 0)).unwrap(); // back
        q.push(e(3, R, 6, 4, Normal, 0)).unwrap(); // front
        q.push(e(4, R, 30, 1, Normal, 0)).unwrap(); // not adjacent
        let b = pk(&mut q,0, &lim(64)).unwrap();
        assert_eq!((b.kind, b.lba, b.blocks), (R, 6, 9));
        assert_eq!(ids(&b), [3, 1, 2]);
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [4]);
        assert!(pk(&mut q,0, &lim(64)).is_none());
    }

    #[test]
    fn merging_stops_at_max_blocks() {
        let mut q = Elevator::new();
        for i in 0..10 {
            q.push(e(i + 1, R, i * 8, 8, Normal, 0)).unwrap();
        }
        let mut sizes = Vec::new();
        while let Some(b) = pk(&mut q,0, &lim(32)) {
            assert!(b.blocks <= 32);
            sizes.push(b.blocks);
        }
        assert_eq!(sizes, [32, 32, 16]);
        // A single request above the limit still goes (the device splits), alone.
        q.push(e(20, R, 0, 100, Normal, 0)).unwrap();
        q.push(e(21, R, 100, 1, Normal, 0)).unwrap();
        assert_eq!(ids(&pk(&mut q,0, &lim(32)).unwrap()), [20]);
    }

    #[test]
    fn kinds_and_flags_decide_what_merges() {
        let mut q = Elevator::new();
        q.push(e(1, W, 0, 1, Normal, 0)).unwrap();
        q.push(e(2, R, 1, 1, Normal, 0)).unwrap();
        q.push(e(3, WF, 1, 1, Normal, 0)).unwrap();
        q.push(e(4, WP, 1, 1, Normal, 0)).unwrap();
        q.push(e(5, W, 1, 1, Normal, 0)).unwrap();
        let b = pk(&mut q,0, &lim(64)).unwrap();
        // 1 + 5 merge only if nothing older overlapping 5 with a write stays behind: 3 (FUA write at 1) is older
        // than 5 and overlaps it, so 5 must wait for 3.
        assert_eq!(ids(&b), [1]);
        let b = pk(&mut q,0, &lim(64)).unwrap();
        assert_eq!(ids(&b), [2]); // read at 1 (oldest now)
        let b = pk(&mut q,0, &lim(64)).unwrap();
        assert_eq!(ids(&b), [3]);
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [4]);
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [5]);
        // FUA with FUA merges; preflush never.
        q.push(e(6, WF, 0, 1, Normal, 0)).unwrap();
        q.push(e(7, WF, 1, 1, Normal, 0)).unwrap();
        q.push(e(8, WP, 2, 1, Normal, 0)).unwrap();
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [6, 7]);
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [8]);
    }

    #[test]
    fn discards_merge_and_flushes_coalesce() {
        let mut q = Elevator::new();
        q.push(e(1, Kind::Discard, 0, 1000, Background, 0)).unwrap();
        q.push(e(2, Kind::Flush, 0, 0, Normal, 0)).unwrap();
        q.push(e(3, Kind::Discard, 1000, 1000, Background, 0)).unwrap();
        q.push(e(4, Kind::Flush, 0, 0, Normal, 0)).unwrap();
        let b = pk(&mut q,0, &lim(8)).unwrap();
        assert_eq!((b.kind, ids(&b)), (Kind::Flush, alloc::vec![2, 4]));
        let b = pk(&mut q,0, &lim(8)).unwrap();
        assert_eq!((b.kind, b.lba, b.blocks, ids(&b)), (Kind::Discard, 0, 2000, alloc::vec![1, 3]));
    }

    #[test]
    fn priority_then_arrival() {
        let mut q = Elevator::new();
        q.push(e(1, R, 100, 1, Background, 0)).unwrap();
        q.push(e(2, R, 200, 1, Normal, 0)).unwrap();
        q.push(e(3, R, 300, 1, Interactive, 0)).unwrap();
        q.push(e(4, R, 400, 1, Normal, 0)).unwrap();
        q.push(e(5, R, 500, 1, Interactive, 0)).unwrap();
        let order: Vec<u64> = core::iter::from_fn(|| pk(&mut q,10, &lim(64))).map(|b| b.members[0].id).collect();
        assert_eq!(order, [3, 5, 2, 4, 1]);
    }

    #[test]
    fn aging_lifts_a_waiting_background_request() {
        let mut q = Elevator::new();
        q.push(e(1, R, 100, 1, Background, 0)).unwrap();
        q.push(e(2, R, 200, 1, Normal, 150)).unwrap();
        // At 150: the background request waited 150 (one class up = Normal) and is older: it goes first.
        let b = pk(&mut q,150, &lim(64)).unwrap();
        assert_eq!(ids(&b), [1]);
        assert!(b.aged);
        assert_eq!(b.prio, Normal);
        // The Normal one (queued at 150) has waited 900 by 1050: it competes as Interactive and is the oldest.
        q.push(e(3, R, 100, 1, Background, 1000)).unwrap();
        q.push(e(4, R, 300, 1, Interactive, 1050)).unwrap();
        assert_eq!(ids(&pk(&mut q,1050, &lim(64)).unwrap()), [2]);
        // A fresh Interactive request beats a Background one that waited less than one step.
        assert_eq!(ids(&pk(&mut q,1050, &lim(64)).unwrap()), [4]);
        // ... but after two steps the old one goes before newer Interactive ones.
        q.push(e(5, R, 400, 1, Interactive, 1250)).unwrap();
        assert_eq!(ids(&pk(&mut q,1250, &lim(64)).unwrap()), [3]);
        assert_eq!(ids(&pk(&mut q,1250, &lim(64)).unwrap()), [5]);
    }

    #[test]
    fn nothing_starves_under_a_stream_of_interactive_requests() {
        // One Interactive request arrives per time unit forever; the device serves one per unit. The
        // background request must be served within 2 x AGE plus the requests older than it.
        let mut q = Elevator::new();
        q.push(e(1, R, 0, 1, Background, 0)).unwrap();
        let mut served_at = None;
        for (id, now) in (2..).zip(0..10 * AGE) {
            q.push(e(id, R, 1000 + id * 10, 1, Interactive, now)).unwrap();
            let b = pk(&mut q,now, &lim(64)).unwrap();
            if b.members.iter().any(|m| m.id == 1) {
                served_at = Some(now);
                break;
            }
        }
        let t = served_at.expect("the background request starved");
        assert!(t <= 2 * AGE + 1, "served at {t}");
    }

    #[test]
    fn overlapping_requests_keep_their_order_across_priorities() {
        let mut q = Elevator::new();
        q.push(e(1, W, 10, 4, Background, 0)).unwrap();
        q.push(e(2, R, 12, 1, Interactive, 0)).unwrap(); // reads what 1 writes: after 1
        q.push(e(3, R, 50, 1, Interactive, 0)).unwrap(); // independent
        q.push(e(4, W, 13, 1, Interactive, 0)).unwrap(); // write after the read 2 and the write 1
        // The Interactive read 2 is the best, but it waits for the Background write 1, which therefore goes
        // first (it inherits 2's priority), then 2, then 3, then 4 (behind 1 and 2).
        let first = pk(&mut q,0, &lim(64)).unwrap();
        assert_eq!((ids(&first), first.prio), (alloc::vec![1], Interactive));
        let order: Vec<Vec<u64>> = core::iter::from_fn(|| pk(&mut q,0, &lim(64))).map(|b| ids(&b)).collect();
        assert_eq!(order, [alloc::vec![2], alloc::vec![3], alloc::vec![4]]);
        // Reads among themselves do not order.
        q.push(e(5, R, 10, 4, Background, 0)).unwrap();
        q.push(e(6, R, 12, 1, Interactive, 0)).unwrap();
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [6]);
    }

    #[test]
    fn a_merge_does_not_jump_an_older_overlapping_write() {
        let mut q = Elevator::new();
        q.push(e(1, R, 0, 4, Interactive, 0)).unwrap();
        q.push(e(2, W, 4, 4, Background, 0)).unwrap();
        q.push(e(3, R, 4, 4, Interactive, 0)).unwrap(); // continues 1 but must wait for the write 2
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [1]);
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [2]);
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [3]);
    }

    #[test]
    fn no_memory_takes_the_chosen_request_out() {
        let mut q = Elevator::new();
        q.push(e(1, R, 0, 1, Normal, 0)).unwrap();
        q.push(e(2, R, 1, 1, Normal, 0)).unwrap();
        q.push(e(3, R, 50, 1, Interactive, 0)).unwrap();
        q.oom_picks = 1;
        // The best request goes out as the failed one (progress, no spin); the rest is served normally.
        assert_eq!(q.pick(0, &lim(64)).map(|r| r.err().map(|x| x.id)), Some(Some(3)));
        assert_eq!(q.entries().iter().map(|x| x.id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(ids(&pk(&mut q, 0, &lim(64)).unwrap()), [1, 2]);
        assert!(q.pick(0, &lim(64)).is_none());
    }

    #[test]
    fn remove_takes_a_request_back() {
        let mut q = Elevator::new();
        q.push(e(1, R, 0, 1, Normal, 0)).unwrap();
        q.push(e(2, R, 1, 1, Normal, 0)).unwrap();
        assert!(q.remove(1).is_some());
        assert!(q.remove(1).is_none());
        assert_eq!(ids(&pk(&mut q,0, &lim(64)).unwrap()), [2]);
        assert!(q.is_empty());
    }

    #[test]
    fn random_sequences_keep_every_rule() {
        // Pseudo-random queues: every request is served exactly once, batches stay within the limit, merged
        // members are contiguous and of one kind, and no request is served before an older one it must follow.
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _round in 0..300 {
            let mut q = Elevator::new();
            let n = 1 + rnd(40);
            let kinds = [R, W, WF, WP, Kind::Discard, Kind::Flush];
            let prios = [Interactive, Normal, Background];
            let mut all = Vec::new();
            for id in 1..=n {
                let k = kinds[rnd(6) as usize];
                let x = e(id, k, rnd(64), 1 + rnd(8), prios[rnd(3) as usize], rnd(500));
                all.push(x);
            }
            all.sort_by_key(|x| x.id);
            for x in &all {
                q.push(*x).unwrap();
            }
            let l = lim(1 + rnd(16));
            let mut served: Vec<u64> = Vec::new();
            let mut now = 0;
            while let Some(b) = pk(&mut q,now, &l) {
                now += rnd(80);
                if b.kind != Kind::Flush {
                    let limit = if b.kind == Kind::Discard { l.max_discard_blocks } else { l.max_blocks };
                    assert!(b.members.len() == 1 || b.blocks <= limit);
                    let mut at = b.lba;
                    for m in &b.members {
                        assert_eq!(m.kind, b.kind);
                        assert_eq!(m.lba, at);
                        at += m.blocks;
                    }
                    assert_eq!(at, b.lba + b.blocks);
                }
                for m in &b.members {
                    // Every older request it must follow was served already.
                    for o in all.iter().filter(|o| o.id < m.id && o.orders_before(m)) {
                        assert!(served.contains(&o.id) || b.members.iter().any(|x| x.id == o.id), "{} before {}", m.id, o.id);
                    }
                }
                served.extend(b.members.iter().map(|m| m.id));
            }
            served.sort_unstable();
            assert_eq!(served, (1..=n).collect::<Vec<_>>());
        }
    }

    /// FS-13: `foreground_queued` counts queued Interactive and Normal requests, not Background ones; it falls
    /// when they are picked, taken back or the elevator goes.
    #[test]
    fn foreground_count_follows_the_queue() {
        let base = foreground_queued();
        let mut q = Elevator::new();
        q.push(e(1, R, 0, 1, Background, 0)).unwrap();
        assert_eq!(foreground_queued(), base);
        q.push(e(2, R, 100, 1, Interactive, 0)).unwrap();
        q.push(e(3, R, 200, 1, Normal, 0)).unwrap();
        q.push(e(4, R, 300, 1, Normal, 0)).unwrap();
        assert_eq!(foreground_queued(), base + 3);
        let b = pk(&mut q, 0, &lim(64)).unwrap();
        assert_eq!(ids(&b), [2]);
        assert_eq!(foreground_queued(), base + 2);
        assert!(q.remove(3).is_some());
        assert_eq!(foreground_queued(), base + 1);
        assert!(q.remove(1).is_some()); // Background: no change
        assert_eq!(foreground_queued(), base + 1);
        drop(q);
        assert_eq!(foreground_queued(), base);
    }
}

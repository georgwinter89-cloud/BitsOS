//! The run queue of one CPU (SC-3; pure, host-tested): KernelRt threads by strict priority, FIFO inside a
//! priority; the fair class (Interactive, Normal, Background) by virtual runtime. Both lists are sorted vectors
//! with the best entry last, so picking is a `pop` and inserting a binary search plus a move of the entries
//! behind it (a CPU rarely has more than a handful waiting). The vectors only grow (R1: no limit); memory is
//! taken under the queue lock in thread context only, never in an interrupt handler (the scheduler queues
//! interrupt-time wake-ups elsewhere), and fallibly: a push without memory hands the item back (the scheduler
//! then keeps the thread on the CPU's lock-free wake list and retries).

use alloc::vec::Vec;

/// One waiting KernelRt thread: higher `prio` first, then lower `seq` (earlier) first.
struct RtEntry<T> {
    prio: u8,
    seq: i64,
    id: u64,
    item: T,
}

/// One waiting fair-class thread: lower virtual runtime first (id breaks ties).
struct FairEntry<T> {
    vruntime: u64,
    id: u64,
    item: T,
}

pub struct RunQueue<T> {
    /// Ascending by (prio, -seq): the best is last.
    rt: Vec<RtEntry<T>>,
    /// Descending by (vruntime, id): the smallest is last.
    fair: Vec<FairEntry<T>>,
    /// Monotonic floor of the fair class's virtual time on this CPU: new and waking threads are placed
    /// relative to it, so a thread that slept long does not get the CPU for as long as it slept.
    pub min_vruntime: u64,
    back_seq: i64,
    front_seq: i64,
}

impl<T> Default for RunQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Which list an entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    Rt,
    /// With the virtual runtime it had in the queue.
    Fair(u64),
}

impl<T> RunQueue<T> {
    pub const fn new() -> Self {
        RunQueue { rt: Vec::new(), fair: Vec::new(), min_vruntime: 0, back_seq: 0, front_seq: 0 }
    }

    pub fn len(&self) -> usize {
        self.rt.len() + self.fair.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn rt_len(&self) -> usize {
        self.rt.len()
    }
    pub fn fair_len(&self) -> usize {
        self.fair.len()
    }

    /// Queues a KernelRt thread behind the others of its priority, or (`front`, a preempted thread) before them.
    /// `Err(item)`: no memory to grow the queue.
    pub fn push_rt(&mut self, prio: u8, front: bool, id: u64, item: T) -> Result<(), T> {
        if self.rt.try_reserve(1).is_err() {
            return Err(item);
        }
        let seq = if front {
            self.front_seq -= 1;
            self.front_seq
        } else {
            self.back_seq += 1;
            self.back_seq
        };
        // Ascending by (prio, -seq).
        let key = (prio, -seq);
        let pos = self.rt.partition_point(|e| (e.prio, -e.seq) < key);
        self.rt.insert(pos, RtEntry { prio, seq, id, item });
        Ok(())
    }

    /// Queues a fair-class thread at virtual runtime `vruntime`. `Err(item)`: no memory to grow the queue.
    pub fn push_fair(&mut self, vruntime: u64, id: u64, item: T) -> Result<(), T> {
        if self.fair.try_reserve(1).is_err() {
            return Err(item);
        }
        let key = (vruntime, id);
        // Descending: entries greater than the key stay in front.
        let pos = self.fair.partition_point(|e| (e.vruntime, e.id) > key);
        self.fair.insert(pos, FairEntry { vruntime, id, item });
        Ok(())
    }

    /// Priority of the best waiting KernelRt thread.
    pub fn best_rt_prio(&self) -> Option<u8> {
        self.rt.last().map(|e| e.prio)
    }
    /// Virtual runtime of the first fair thread in line.
    pub fn best_fair_vruntime(&self) -> Option<u64> {
        self.fair.last().map(|e| e.vruntime)
    }

    /// The first fair thread in line.
    pub fn peek_fair(&self) -> Option<&T> {
        self.fair.last().map(|e| &e.item)
    }

    pub fn pop_rt(&mut self) -> Option<T> {
        self.rt.pop().map(|e| e.item)
    }
    pub fn pop_fair(&mut self) -> Option<T> {
        self.fair.pop().map(|e| e.item)
    }

    /// The thread to run next: KernelRt first (strict priority), then the fair thread with the least virtual
    /// runtime.
    pub fn pop_best(&mut self) -> Option<T> {
        self.pop_rt().or_else(|| self.pop_fair())
    }

    /// Raises `min_vruntime` towards the smallest virtual runtime on this CPU (the running thread's `curr`
    /// included); it never goes back.
    pub fn update_min(&mut self, curr: Option<u64>) {
        let left = self.best_fair_vruntime();
        let m = match (curr, left) {
            (Some(c), Some(l)) => c.min(l),
            (Some(c), None) => c,
            (None, Some(l)) => l,
            (None, None) => return,
        };
        self.min_vruntime = self.min_vruntime.max(m);
    }

    /// Takes a thread another CPU may run (`movable`): the best waiting KernelRt one (it should run as soon as
    /// possible anywhere), else the fair one last in line (largest virtual runtime: it would wait longest here
    /// and is the least likely to have warm caches).
    pub fn take_movable(&mut self, movable: impl Fn(&T) -> bool) -> Option<(T, Taken)> {
        if let Some(i) = self.rt.iter().rposition(|e| movable(&e.item)) {
            return Some((self.rt.remove(i).item, Taken::Rt));
        }
        if let Some(i) = self.fair.iter().position(|e| movable(&e.item)) {
            let e = self.fair.remove(i);
            return Some((e.item, Taken::Fair(e.vruntime)));
        }
        None
    }

    /// Takes the entry of thread `id` out (wherever it waits).
    pub fn remove_id(&mut self, id: u64) -> Option<(T, Taken)> {
        if let Some(i) = self.rt.iter().position(|e| e.id == id) {
            return Some((self.rt.remove(i).item, Taken::Rt));
        }
        let i = self.fair.iter().position(|e| e.id == id)?;
        let e = self.fair.remove(i);
        Some((e.item, Taken::Fair(e.vruntime)))
    }

    /// Whether thread `id` waits here.
    pub fn contains(&self, id: u64) -> bool {
        self.rt.iter().any(|e| e.id == id) || self.fair.iter().any(|e| e.id == id)
    }

    /// Ids in the order they would run (tests, statistics).
    pub fn order(&self) -> Vec<u64> {
        self.rt.iter().rev().map(|e| e.id).chain(self.fair.iter().rev().map(|e| e.id)).collect()
    }

    /// Every queued item (statistics).
    pub fn items(&self) -> impl Iterator<Item = &T> {
        self.rt.iter().map(|e| &e.item).chain(self.fair.iter().map(|e| &e.item))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn rt_is_strict_priority_and_fifo_inside_one() {
        let mut q: RunQueue<u64> = RunQueue::new();
        let _ = q.push_rt(50, false, 1, 1);
        let _ = q.push_rt(50, false, 2, 2);
        let _ = q.push_rt(90, false, 3, 3);
        let _ = q.push_rt(10, false, 4, 4);
        let _ = q.push_rt(50, false, 5, 5);
        assert_eq!(q.order(), vec![3, 1, 2, 5, 4]);
        // A preempted thread goes before the others of its priority.
        let _ = q.push_rt(50, true, 6, 6);
        assert_eq!(q.order(), vec![3, 6, 1, 2, 5, 4]);
        assert_eq!(q.best_rt_prio(), Some(90));
        let got: Vec<u64> = core::iter::from_fn(|| q.pop_best()).collect();
        assert_eq!(got, vec![3, 6, 1, 2, 5, 4]);
    }

    #[test]
    fn fair_runs_the_least_virtual_runtime_first_and_rt_before_it() {
        let mut q: RunQueue<u64> = RunQueue::new();
        let _ = q.push_fair(300, 1, 1);
        let _ = q.push_fair(100, 2, 2);
        let _ = q.push_fair(200, 3, 3);
        let _ = q.push_fair(100, 4, 4);
        let _ = q.push_rt(1, false, 9, 9);
        assert_eq!(q.order(), vec![9, 2, 4, 3, 1]);
        assert_eq!(q.best_fair_vruntime(), Some(100));
        assert_eq!(q.pop_best(), Some(9));
        assert_eq!(q.pop_best(), Some(2));
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn min_vruntime_only_grows() {
        let mut q: RunQueue<u64> = RunQueue::new();
        q.update_min(Some(50));
        assert_eq!(q.min_vruntime, 50);
        let _ = q.push_fair(40, 1, 1);
        q.update_min(Some(60));
        assert_eq!(q.min_vruntime, 50, "never back");
        q.pop_fair();
        q.update_min(Some(70));
        assert_eq!(q.min_vruntime, 70);
        q.update_min(None);
        assert_eq!(q.min_vruntime, 70);
    }

    #[test]
    fn stealing_takes_rt_first_then_the_last_fair_one_and_skips_pinned() {
        let mut q: RunQueue<u64> = RunQueue::new();
        let _ = q.push_fair(10, 1, 1);
        let _ = q.push_fair(30, 2, 2);
        let _ = q.push_fair(20, 3, 3);
        let _ = q.push_rt(5, false, 7, 7);
        let _ = q.push_rt(9, false, 8, 8);
        // 8 is pinned: the best movable RT thread is 7.
        assert_eq!(q.take_movable(|&t| t != 8), Some((7, Taken::Rt)));
        assert_eq!(q.take_movable(|&t| t != 8 && t != 2), Some((3, Taken::Fair(20))));
        assert_eq!(q.take_movable(|&t| t == 2), Some((2, Taken::Fair(30))));
        assert_eq!(q.take_movable(|_| false), None);
        assert_eq!(q.order(), vec![8, 1]);
        assert_eq!(q.remove_id(1), Some((1, Taken::Fair(10))));
        assert!(q.contains(8) && !q.contains(1));
    }

    #[test]
    fn many_entries_stay_sorted() {
        let mut q: RunQueue<u64> = RunQueue::new();
        let mut x = 12345u64;
        for id in 0..2000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let _ = q.push_fair(x >> 40, id, id);
        }
        let mut last = 0;
        let mut n = 0;
        while let Some(id) = q.fair.last().map(|e| e.vruntime) {
            assert!(id >= last);
            last = id;
            q.pop_fair();
            n += 1;
        }
        assert_eq!(n, 2000);
    }
}

//! WY-16: the acquire waits of one surface, oldest commit first. Every commit with an unsignalled acquire point
//! gets a blocker flag here, but only the oldest still-waiting point gets an eventfd (`syncobj::arm`): a client that
//! commits in a loop with points it never signals costs bitscomp one fd per surface, not one per commit (the fd
//! table would run out and bitscomp could no longer accept clients). Commits of one surface apply in order anyway
//! (Smithay's transaction queue), so nothing behind the head could be applied before it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Queue<P> {
    items: VecDeque<(P, Arc<AtomicBool>)>,
}

impl<P> Default for Queue<P> {
    fn default() -> Self {
        Queue { items: VecDeque::new() }
    }
}

impl<P> Queue<P> {
    /// A new waiting commit; its blocker reads the returned flag.
    pub fn push(&mut self, p: P) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.items.push_back((p, flag.clone()));
        flag
    }

    /// Releases the commits at the head whose point has signalled, in order; the first one still waiting stays
    /// the head. Ok(true) when at least one was released. On an error the failing commit and everything behind it
    /// stay blocked (the caller retries): a point the kernel cannot read never lets a buffer through unsynchronised.
    pub fn advance<E>(&mut self, mut signalled: impl FnMut(&P) -> Result<bool, E>) -> Result<bool, E> {
        let mut released = false;
        while let Some((p, flag)) = self.items.front() {
            if !signalled(p)? {
                break;
            }
            flag.store(true, Ordering::Release);
            released = true;
            self.items.pop_front();
        }
        Ok(released)
    }

    /// The oldest commit still waiting (the one an eventfd is armed for).
    pub fn head(&self) -> Option<&P> {
        self.items.front().map(|(p, _)| p)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_in_order_and_stops_at_the_first_waiting_point() {
        let mut q = Queue::default();
        let (a, b, c) = (q.push(1u64), q.push(2), q.push(3));
        let reached = 2;
        assert_eq!(q.advance(|p| Ok::<_, ()>(*p <= reached)), Ok(true));
        assert!(a.load(Ordering::Acquire) && b.load(Ordering::Acquire) && !c.load(Ordering::Acquire));
        assert_eq!(q.head(), Some(&3));
        assert_eq!(q.advance(|p| Ok::<_, ()>(*p <= reached)), Ok(false), "nothing new");
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn a_later_signalled_point_waits_behind_the_head() {
        let mut q = Queue::default();
        let (a, b) = (q.push(5u64), q.push(1));
        assert_eq!(q.advance(|p| Ok::<_, ()>(*p == 1)), Ok(false));
        assert!(!a.load(Ordering::Acquire) && !b.load(Ordering::Acquire), "commits apply in order");
    }

    #[test]
    fn an_error_keeps_everything_blocked() {
        let mut q = Queue::default();
        let (a, b) = (q.push(1u64), q.push(2));
        assert_eq!(q.advance(|p| if *p == 1 { Ok(true) } else { Err("broken") }), Err("broken"));
        assert!(a.load(Ordering::Acquire), "released before the error");
        assert!(!b.load(Ordering::Acquire), "never let through on an error");
        assert_eq!(q.head(), Some(&2));
    }

    #[test]
    fn many_commits_keep_one_head() {
        let mut q = Queue::default();
        let flags: Vec<_> = (1..=10_000u64).map(|p| q.push(p)).collect();
        assert_eq!(q.advance(|_| Ok::<_, ()>(false)), Ok(false));
        assert_eq!((q.len(), q.head()), (10_000, Some(&1)));
        assert_eq!(q.advance(|_| Ok::<_, ()>(true)), Ok(true));
        assert!(q.is_empty() && flags.iter().all(|f| f.load(Ordering::Acquire)));
    }
}

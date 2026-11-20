//! The inodes the VFS let go of, waiting for the volume lock (`Volume::forget`, `Volume::lock`).
//!
//! `forget` runs in a drop, under whatever lock the dropper holds, so it may not ask the heap for anything:
//! under a spin lock the heap cannot reclaim, and a list that grew there (8 KiB when a mount's dentry tree was
//! pruned) ends the kernel when no run of frames is free. An inode's place on the list is therefore made where
//! it is taken (`Volume::hold`, thread context) and travels with its holder as a `Slot`; noting it is two
//! pointer moves. No list has to have room, so nothing is sized, grown or kept after the inodes went.

use alloc::boxed::Box;

struct Link {
    ino: u32,
    next: Option<Box<Link>>,
}

/// A held inode's place on the list of dropped inodes: made by `Volume::hold`, used up by `Volume::forget`.
pub struct Slot(Box<Link>);

impl Slot {
    pub fn new(ino: u32) -> Slot {
        Slot(Box::new(Link { ino, next: None }))
    }
    pub fn ino(&self) -> u32 {
        self.0.ino
    }
}

/// The noted inodes, newest first.
pub struct Dropped {
    head: Option<Box<Link>>,
}

impl Dropped {
    pub const fn new() -> Dropped {
        Dropped { head: None }
    }

    /// Allocates nothing: callable under any lock.
    pub fn note(&mut self, slot: Slot) {
        let mut l = slot.0;
        l.next = self.head.take();
        self.head = Some(l);
    }

    /// Everything noted so far (one move; walk it with the list's lock released).
    pub fn take(&mut self) -> Gone {
        Gone { head: self.head.take() }
    }
}

/// Slots taken off the list.
pub struct Gone {
    head: Option<Box<Link>>,
}

impl Gone {
    /// In the order they were noted.
    pub fn oldest_first(mut self) -> Gone {
        let mut rest = self.head.take();
        let mut out = None;
        while let Some(mut l) = rest {
            rest = l.next.take();
            l.next = out;
            out = Some(l);
        }
        Gone { head: out }
    }
}

impl Iterator for Gone {
    type Item = Slot;
    fn next(&mut self) -> Option<Slot> {
        let mut l = self.head.take()?;
        self.head = l.next.take();
        Some(Slot(l))
    }
}

// A chain goes link by link: the derived drop would recurse once per link, on a kernel stack of 64 KiB.
impl Drop for Gone {
    fn drop(&mut self) {
        for _ in self.by_ref() {}
    }
}

impl Drop for Dropped {
    fn drop(&mut self) {
        drop(self.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn slots_come_back_in_the_order_they_were_noted() {
        let mut d = Dropped::new();
        (1..=5).for_each(|i| d.note(Slot::new(i)));
        let got: Vec<u32> = d.take().oldest_first().map(|s| s.ino()).collect();
        assert_eq!(got, [1, 2, 3, 4, 5]);
        assert!(d.take().next().is_none(), "taken once");
        // A slot noted again (its inode's step at the device still runs) is a slot like any other.
        let mut it = { d.note(Slot::new(7)); d.take().oldest_first() };
        let s = it.next().unwrap();
        d.note(s);
        d.note(Slot::new(8));
        assert_eq!(d.take().oldest_first().map(|s| s.ino()).collect::<Vec<_>>(), [7, 8]);
    }

    /// A million links dropped at once, taken or not, half walked or whole: no recursion (the test thread's
    /// stack would not hold it).
    #[test]
    fn a_long_list_is_dropped_without_recursion() {
        const N: u32 = 1_000_000;
        let fill = || {
            let mut d = Dropped::new();
            (0..N).for_each(|i| d.note(Slot::new(i)));
            d
        };
        drop(fill());
        drop(fill().take());
        let mut half = fill().take().oldest_first();
        assert_eq!(half.by_ref().take(10).map(|s| s.ino()).sum::<u32>(), 45);
        drop(half);
    }
}

//! A sparse index from page numbers to values (FS-5): a radix tree with 64 slots per node, like Linux's
//! XArray. The page cache keeps its pages here (by file page), the file mappings their pins (by frame number).
//!
//! Every node is a `Vec` whose room is reserved with `try_reserve_exact` before anything changes, so running
//! out of kernel heap is an `ENOMEM` for the asking call and never an allocation abort (R2). No count is fixed
//! (R1): the tree grows a level whenever an index does not fit, up to the 64-bit index space (11 levels).
//! Lookups and inserts cost one step per level; listing walks only present subtrees.

use alloc::vec::Vec;

const BITS: u32 = 6;
const FAN: usize = 1 << BITS;
const MASK: u64 = FAN as u64 - 1;

/// The heap had no room for a node; nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoRoom;

enum Node<T> {
    Inner(Vec<Option<Node<T>>>),
    Leaf(Vec<Option<T>>),
}

fn slots<X>() -> Result<Vec<Option<X>>, NoRoom> {
    let mut v: Vec<Option<X>> = Vec::new();
    v.try_reserve_exact(FAN).map_err(|_| NoRoom)?;
    v.resize_with(FAN, || None);
    Ok(v)
}

impl<T> Node<T> {
    fn is_empty(&self) -> bool {
        match self {
            Node::Inner(c) => c.iter().all(Option::is_none),
            Node::Leaf(c) => c.iter().all(Option::is_none),
        }
    }
}

pub struct PageIndex<T> {
    root: Option<Node<T>>,
    /// Levels of the tree (1: the root is a leaf). Indices below `FAN^height` fit.
    height: u32,
    len: usize,
}

impl<T> Default for PageIndex<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Bits of an index at `level` (1 = leaf level).
fn digit(i: u64, level: u32) -> usize {
    let shift = BITS * (level - 1);
    if shift >= 64 { 0 } else { ((i >> shift) & MASK) as usize }
}

/// Whether index `i` fits a tree of `height` levels.
fn fits(i: u64, height: u32) -> bool {
    let bits = BITS * height;
    bits >= 64 || i >> bits == 0
}

impl<T> PageIndex<T> {
    pub const fn new() -> Self {
        PageIndex { root: None, height: 0, len: 0 }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: u64) -> Option<&T> {
        if !fits(i, self.height) {
            return None;
        }
        let mut n = self.root.as_ref()?;
        let mut level = self.height;
        loop {
            match n {
                Node::Inner(c) => n = c[digit(i, level)].as_ref()?,
                Node::Leaf(c) => return c[digit(i, 1)].as_ref(),
            }
            level -= 1;
        }
    }

    pub fn get_mut(&mut self, i: u64) -> Option<&mut T> {
        if !fits(i, self.height) {
            return None;
        }
        let mut n = self.root.as_mut()?;
        let mut level = self.height;
        loop {
            match n {
                Node::Inner(c) => n = c[digit(i, level)].as_mut()?,
                Node::Leaf(c) => return c[digit(i, 1)].as_mut(),
            }
            level -= 1;
        }
    }

    pub fn contains(&self, i: u64) -> bool {
        self.get(i).is_some()
    }

    /// Room for an insert at `i`: grows the tree and makes the nodes on the way (all fallible). After `Ok`,
    /// `insert(i, ..)` cannot fail.
    pub fn prepare(&mut self, i: u64) -> Result<(), NoRoom> {
        if self.root.is_none() {
            self.root = Some(Node::Leaf(slots()?));
            self.height = 1;
        }
        while !fits(i, self.height) {
            let mut c = slots()?;
            c[0] = self.root.take().filter(|r| !r.is_empty());
            self.root = Some(Node::Inner(c));
            self.height += 1;
        }
        let mut level = self.height;
        let mut n = self.root.as_mut().ok_or(NoRoom)?;
        while level > 1 {
            let Node::Inner(c) = n else { return Err(NoRoom) };
            let slot = &mut c[digit(i, level)];
            if slot.is_none() {
                *slot = Some(if level == 2 { Node::Leaf(slots()?) } else { Node::Inner(slots()?) });
            }
            n = slot.as_mut().ok_or(NoRoom)?;
            level -= 1;
        }
        Ok(())
    }

    /// Puts `v` at `i`, returning the old value.
    pub fn insert(&mut self, i: u64, v: T) -> Result<Option<T>, NoRoom> {
        self.prepare(i)?;
        let mut level = self.height;
        let mut n = self.root.as_mut().ok_or(NoRoom)?;
        loop {
            match n {
                Node::Inner(c) => n = c[digit(i, level)].as_mut().ok_or(NoRoom)?,
                Node::Leaf(c) => {
                    let old = c[digit(i, 1)].replace(v);
                    if old.is_none() {
                        self.len += 1;
                    }
                    return Ok(old);
                }
            }
            level -= 1;
        }
    }

    /// Takes the value at `i` out; empty nodes on the way are freed.
    pub fn remove(&mut self, i: u64) -> Option<T> {
        if !fits(i, self.height) {
            return None;
        }
        fn rec<T>(n: &mut Node<T>, i: u64, level: u32) -> Option<T> {
            match n {
                Node::Leaf(c) => c[digit(i, 1)].take(),
                Node::Inner(c) => {
                    let d = digit(i, level);
                    let child = c[d].as_mut()?;
                    let v = rec(child, i, level - 1);
                    if v.is_some() && child.is_empty() {
                        c[d] = None;
                    }
                    v
                }
            }
        }
        let v = rec(self.root.as_mut()?, i, self.height);
        if v.is_some() {
            self.len -= 1;
            if self.len == 0 {
                self.root = None;
                self.height = 0;
            }
        }
        v
    }

    /// The first present index at or after `from`, with its value.
    pub fn next_from(&self, from: u64) -> Option<(u64, &T)> {
        // u128 arithmetic: the top level of an 11-level tree spans more than 64 bits.
        fn rec<T>(n: &Node<T>, base: u128, level: u32, from: u64) -> Option<(u64, &T)> {
            let span_bits = BITS * (level - 1);
            for d in 0..FAN {
                let lo = base + ((d as u128) << span_bits);
                if lo > u64::MAX as u128 {
                    return None;
                }
                let hi = lo + (1u128 << span_bits) - 1;
                if hi < from as u128 {
                    continue;
                }
                match n {
                    Node::Leaf(c) => {
                        if let Some(v) = c[d].as_ref() {
                            return Some((lo as u64, v));
                        }
                    }
                    Node::Inner(c) => {
                        if let Some(ch) = c[d].as_ref()
                            && let Some(r) = rec(ch, lo, level - 1, from)
                        {
                            return Some(r);
                        }
                    }
                }
            }
            None
        }
        let root = self.root.as_ref()?;
        if !fits(from, self.height) {
            return None;
        }
        rec(root, 0, self.height, from)
    }

    /// Calls `f` for every entry in index order (stops when `f` returns false).
    pub fn for_each(&self, f: &mut dyn FnMut(u64, &T) -> bool) {
        let mut at = 0u64;
        while let Some((i, v)) = self.next_from(at) {
            if !f(i, v) {
                return;
            }
            match i.checked_add(1) {
                Some(n) => at = n,
                None => return,
            }
        }
    }

    /// Removes every entry at or after `from`, handing each to `f` in index order. Never allocates.
    pub fn drain_from(&mut self, from: u64, f: &mut dyn FnMut(u64, T)) {
        loop {
            let Some((i, _)) = self.next_from(from) else { return };
            if let Some(v) = self.remove(i) {
                f(i, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    #[test]
    fn against_a_model() {
        let mut t: PageIndex<u64> = PageIndex::new();
        let mut m: BTreeMap<u64, u64> = BTreeMap::new();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for step in 0..40_000u64 {
            let r = rnd();
            // Mostly small indices (dense files), some huge ones (sparse files, frame numbers).
            let i = match r % 7 {
                0 => rnd(),
                1 => rnd() >> 20,
                _ => rnd() % 5000,
            };
            match (r >> 8) % 4 {
                0 | 1 => assert_eq!(t.insert(i, step).unwrap(), m.insert(i, step)),
                2 => assert_eq!(t.remove(i), m.remove(&i)),
                _ => {
                    let want = m.range(i..).next().map(|(&k, &v)| (k, v));
                    assert_eq!(t.next_from(i).map(|(k, &v)| (k, v)), want);
                }
            }
            assert_eq!(t.len(), m.len());
            assert_eq!(t.get(i), m.get(&i));
        }
        let mut seen = alloc::vec::Vec::new();
        t.for_each(&mut |k, &v| {
            seen.push((k, v));
            true
        });
        assert_eq!(seen, m.iter().map(|(&k, &v)| (k, v)).collect::<alloc::vec::Vec<_>>());
        let cut = 2500;
        let mut gone = alloc::vec::Vec::new();
        t.drain_from(cut, &mut |k, _| gone.push(k));
        let want: alloc::vec::Vec<u64> = m.range(cut..).map(|(&k, _)| k).collect();
        assert_eq!(gone, want);
        m.retain(|&k, _| k < cut);
        assert_eq!(t.len(), m.len());
        t.drain_from(0, &mut |_, _| {});
        assert!(t.is_empty() && t.root.is_none(), "an empty tree frees its nodes");
    }

    #[test]
    fn edges() {
        let mut t: PageIndex<&str> = PageIndex::new();
        assert_eq!(t.get(0), None);
        assert_eq!(t.next_from(0), None);
        t.insert(u64::MAX, "top").unwrap();
        t.insert(0, "zero").unwrap();
        assert_eq!(t.get(u64::MAX), Some(&"top"));
        assert_eq!(t.next_from(1), Some((u64::MAX, &"top")));
        assert_eq!(t.remove(u64::MAX), Some("top"));
        assert_eq!(t.next_from(1), None);
        assert_eq!(t.remove(0), Some("zero"));
        assert!(t.is_empty());
    }
}

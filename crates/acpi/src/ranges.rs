//! A set of half-open physical ranges, merged on insert, so a lookup is one ordered-map search however many
//! ranges a hostile table names (an MADT of a million I/O APIC entries stays O(log n) per lookup).

use alloc::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct RangeSet {
    /// start -> end, non-overlapping and non-touching.
    map: BTreeMap<u64, u64>,
}

impl RangeSet {
    pub fn new() -> RangeSet {
        RangeSet { map: BTreeMap::new() }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Adds `start..end` (empty ranges are ignored), merging it with every range it overlaps or touches.
    pub fn insert(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        let (mut s, mut e) = (start, end);
        // The range starting at or before `start` may reach into the new one.
        if let Some((&ps, &pe)) = self.map.range(..=s).next_back()
            && pe >= s
        {
            s = ps;
            e = e.max(pe);
        }
        // Ranges starting inside the new one are swallowed.
        while let Some((&ns, &ne)) = self.map.range(s..=e).next() {
            self.map.remove(&ns);
            e = e.max(ne);
        }
        self.map.insert(s, e);
    }

    /// True if any byte of `start..end` is in the set.
    pub fn overlaps(&self, start: u64, end: u64) -> bool {
        if end <= start {
            return false;
        }
        self.map.range(..end).next_back().is_some_and(|(_, &pe)| pe > start)
    }
}

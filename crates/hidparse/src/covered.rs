//! A set of usage intervals, so a usage that a field declares again is configured only once.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Disjoint inclusive intervals, keyed by their first usage.
#[derive(Default)]
pub(crate) struct Covered(BTreeMap<u32, u32>);

impl Covered {
    /// Adds `a..=b` and returns its parts that were not in the set yet, in ascending order.
    /// Amortised O(log n): every interval it walks over is merged away.
    pub(crate) fn add(&mut self, a: u32, b: u32) -> Vec<(u32, u32)> {
        let mut gaps = Vec::new();
        let (mut lo, mut hi) = (a, b);
        let mut next = a as u64;
        let from = match self.0.range(..=a).next_back() {
            Some((&s, &e)) if e as u64 + 1 >= a as u64 => s,
            _ => a,
        };
        let touching: Vec<(u32, u32)> = self
            .0
            .range(from..)
            .take_while(|&(&s, _)| s as u64 <= b as u64 + 1)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in touching {
            if s as u64 > next && next <= b as u64 {
                gaps.push((next as u32, (s - 1).min(b)));
            }
            next = next.max(e as u64 + 1);
            (lo, hi) = (lo.min(s), hi.max(e));
            self.0.remove(&s);
        }
        if next <= b as u64 {
            gaps.push((next as u32, b));
        }
        self.0.insert(lo, hi);
        gaps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_are_the_new_parts() {
        let mut c = Covered::default();
        assert_eq!(c.add(10, 20), [(10, 20)]);
        assert_eq!(c.add(15, 18), []);
        assert_eq!(c.add(30, 40), [(30, 40)]);
        assert_eq!(c.add(5, 50), [(5, 9), (21, 29), (41, 50)]);
        assert_eq!(c.add(51, 51), [(51, 51)]);
        assert_eq!(c.add(0, u32::MAX), [(0, 4), (52, u32::MAX)]);
        assert_eq!(c.add(0, u32::MAX), []);
        assert_eq!(c.0.len(), 1);
    }
}

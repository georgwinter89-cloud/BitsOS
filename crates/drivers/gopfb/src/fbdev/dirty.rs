//! Dirty pages of the fbdev buffer (deferred io's page list, Linux fb_defio.c `pagereflist`): one bit per 4 KiB
//! page, sized from the buffer (R1), and what a set of pages means in rows of the picture. Host-tested.

use alloc::vec::Vec;

pub struct Dirty {
    bits: Vec<u64>,
    pages: u64,
}

impl Dirty {
    /// A map for `pages` pages; None if the words cannot be allocated.
    pub fn new(pages: u64) -> Option<Dirty> {
        let words = pages.div_ceil(64) as usize;
        let mut bits = Vec::new();
        bits.try_reserve_exact(words).ok()?;
        bits.resize(words, 0);
        Some(Dirty { bits, pages })
    }
    pub fn pages(&self) -> u64 {
        self.pages
    }
    /// Marks page `p`; false if it was marked already (or lies outside).
    pub fn set(&mut self, p: u64) -> bool {
        if p >= self.pages {
            return false;
        }
        let (w, b) = ((p / 64) as usize, p % 64);
        let was = self.bits[w] & (1 << b) != 0;
        self.bits[w] |= 1 << b;
        !was
    }
    pub fn is_set(&self, p: u64) -> bool {
        p < self.pages && self.bits[(p / 64) as usize] & (1 << (p % 64)) != 0
    }
    /// Marks pages `first..=last` (clipped to the map).
    pub fn set_range(&mut self, first: u64, last: u64) {
        let last = last.min(self.pages.saturating_sub(1));
        for p in first..=last {
            self.set(p);
        }
    }
    /// Whether any page in `first..=last` is marked.
    pub fn any_in(&self, first: u64, last: u64) -> bool {
        let last = last.min(self.pages.saturating_sub(1));
        (first..=last).any(|p| self.is_set(p))
    }
    /// Clears the marks in `first..=last` and calls `run(a, b)` for every run of pages that were marked.
    pub fn take_runs(&mut self, first: u64, last: u64, mut run: impl FnMut(u64, u64)) {
        if self.pages == 0 || first >= self.pages {
            return;
        }
        let last = last.min(self.pages - 1);
        let mut start: Option<u64> = None;
        let mut p = first;
        while p <= last {
            let (w, b) = ((p / 64) as usize, p % 64);
            // Skip a clean word at once.
            if b == 0 && self.bits[w] == 0 && p + 63 <= last {
                if let Some(s) = start.take() {
                    run(s, p - 1);
                }
                p += 64;
                continue;
            }
            if self.bits[w] & (1 << b) != 0 {
                self.bits[w] &= !(1 << b);
                start.get_or_insert(p);
            } else if let Some(s) = start.take() {
                run(s, p - 1);
            }
            p += 1;
        }
        if let Some(s) = start {
            run(s, last);
        }
    }
    /// Clears every mark.
    pub fn clear(&mut self) {
        self.bits.fill(0);
    }
}

/// The rows of a `line`-byte-per-row picture that pages `a..=b` touch, clipped to the window of `height` rows
/// starting at buffer row `top`: (first row, row count) in window rows, or None if they miss the window.
pub fn rows_of_pages(a: u64, b: u64, line: u64, top: u64, height: u64) -> Option<(u64, u64)> {
    if line == 0 || height == 0 {
        return None;
    }
    let first_byte = a * 4096;
    let end_byte = (b + 1) * 4096;
    let r0 = (first_byte / line).max(top);
    let r1 = ((end_byte - 1) / line).min(top + height - 1);
    (r0 <= r1).then(|| (r0 - top, r1 - r0 + 1))
}

/// The pages a window of `height` rows at buffer row `top` covers.
pub fn pages_of_rows(top: u64, height: u64, line: u64) -> (u64, u64) {
    let first = top * line / 4096;
    let last = ((top + height) * line).saturating_sub(1) / 4096;
    (first, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_are_taken_and_cleared() {
        let mut d = Dirty::new(200).unwrap();
        for p in [3, 4, 5, 64, 65, 130, 199] {
            assert!(d.set(p));
        }
        assert!(!d.set(4), "marked twice");
        assert!(!d.set(200), "outside");
        let mut runs = Vec::new();
        d.take_runs(0, 150, |a, b| runs.push((a, b)));
        assert_eq!(runs, [(3, 5), (64, 65), (130, 130)]);
        assert!(!d.any_in(0, 150));
        assert!(d.any_in(151, 500), "199 stays, outside the taken window");
        runs.clear();
        d.take_runs(0, 1000, |a, b| runs.push((a, b)));
        assert_eq!(runs, [(199, 199)]);
        d.set_range(10, 1000);
        assert!(d.is_set(10) && d.is_set(199));
        runs.clear();
        d.take_runs(0, 199, |a, b| runs.push((a, b)));
        assert_eq!(runs, [(10, 199)], "a run across word boundaries and to the end");
    }

    #[test]
    fn a_long_clean_stretch_is_skipped_by_words() {
        let mut d = Dirty::new(1 << 16).unwrap();
        d.set(65_535);
        let mut runs = Vec::new();
        d.take_runs(0, u64::MAX, |a, b| runs.push((a, b)));
        assert_eq!(runs, [(65_535, 65_535)]);
        let mut e = Dirty::new(0).unwrap();
        e.take_runs(0, 10, |_, _| panic!("no pages"));
    }

    #[test]
    fn pages_map_to_rows_of_the_window() {
        // 1440p: 10240-byte rows, so a page holds 0.4 rows; window at row 1440 (the second buffer).
        let line = 10_240;
        assert_eq!(rows_of_pages(0, 0, line, 0, 1440), Some((0, 1)));
        assert_eq!(rows_of_pages(2, 2, line, 0, 1440), Some((0, 2)), "bytes 8192..12287 touch rows 0 and 1");
        assert_eq!(rows_of_pages(0, 3599, line, 0, 1440), Some((0, 1440)));
        assert_eq!(rows_of_pages(0, 3599, line, 1440, 1440), None, "the first buffer is hidden");
        assert_eq!(rows_of_pages(3599, 3600, line, 1440, 1440), Some((0, 1)));
        assert_eq!(pages_of_rows(0, 1440, line), (0, 3599));
        assert_eq!(pages_of_rows(1440, 1440, line), (3600, 7199));
        // 1024x768: a row is exactly one page.
        assert_eq!(rows_of_pages(5, 9, 4096, 0, 768), Some((5, 5)));
        assert_eq!(rows_of_pages(5, 9, 4096, 7, 768), Some((0, 3)));
        assert_eq!(rows_of_pages(0, 0, 0, 0, 10), None);
    }
}

//! A field's usage list, kept as runs so a Usage Minimum/Maximum pair of any width costs one entry
//! (R1: no cap on how many usages a field has, and memory stays proportional to the descriptor).

use alloc::vec::Vec;

/// `count` consecutive 32-bit usages (page << 16 | id) starting at `first`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageSpan {
    pub first: u32,
    pub count: u64,
    /// True for usages written with a 4-byte item (page included, never re-paged).
    pub extended: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usages {
    spans: Vec<UsageSpan>,
    /// starts[i] = usage index of spans[i].first.
    starts: Vec<u64>,
    total: u64,
}

impl Usages {
    pub(crate) fn push(&mut self, mut span: UsageSpan) {
        // A run never crosses usage 0xFFFFFFFF, so `get` and the mapping agree on every usage.
        span.count = span.count.min((1u64 << 32) - span.first as u64);
        if span.count == 0 {
            return;
        }
        self.starts.push(self.total);
        self.total += span.count;
        self.spans.push(span);
    }

    pub(crate) fn spans_mut(&mut self) -> &mut [UsageSpan] {
        &mut self.spans
    }

    pub fn spans(&self) -> &[UsageSpan] {
        &self.spans
    }

    /// Usage index where span `i` starts.
    pub fn span_start(&self, i: usize) -> u64 {
        self.starts[i]
    }

    /// Number of usages declared.
    pub fn len(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Usage `i` as Linux `hid_add_field` sees it: past the declared ones the last one repeats.
    pub fn get(&self, i: u64) -> Option<u32> {
        if self.total == 0 {
            return None;
        }
        let i = i.min(self.total - 1);
        let s = self.starts.partition_point(|&start| start <= i) - 1;
        let span = &self.spans[s];
        // In range: `push` keeps every run below 2^32.
        Some((span.first as u64 + (i - self.starts[s])) as u32)
    }
}

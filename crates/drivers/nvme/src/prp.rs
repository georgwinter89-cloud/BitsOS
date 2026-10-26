//! PRP entries for a transfer from a page-aligned contiguous DMA buffer (NVMe 1.4 §4.3; the cases of
//! linux-6.6/drivers/nvme/host/pci.c:579 `nvme_pci_setup_prps`): up to one page PRP1 only, up to two pages
//! PRP1 + PRP2, more: PRP2 points at a list page holding the addresses of pages 2..n. One list page holds
//! `PAGE / 8` entries, so a transfer is bounded by `max_bytes` (a single request's size, not a count).

use crate::regs::PAGE;

/// Largest transfer one list page describes: PRP1's page plus a full list.
pub const fn max_bytes() -> usize {
    (PAGE / 8 + 1) * PAGE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prps {
    pub prp1: u64,
    pub prp2: u64,
    /// Entries to write into the list page at `list_addr` (0 = no list).
    pub list_len: usize,
}

/// `base` (page-aligned) + `len` bytes; `list_addr` = the list page's device address. None: `len` is 0, too
/// long for one list or `base` not aligned.
pub fn plan(base: u64, len: usize, list_addr: u64) -> Option<Prps> {
    if len == 0 || len > max_bytes() || !base.is_multiple_of(PAGE as u64) {
        return None;
    }
    let pages = len.div_ceil(PAGE);
    Some(match pages {
        1 => Prps { prp1: base, prp2: 0, list_len: 0 },
        2 => Prps { prp1: base, prp2: base + PAGE as u64, list_len: 0 },
        n => Prps { prp1: base, prp2: list_addr, list_len: n - 1 },
    })
}

/// The list's i-th entry: page i+1 of the buffer.
pub fn list_entry(base: u64, i: usize) -> u64 {
    base + ((i + 1) * PAGE) as u64
}

//! PRP entries for one transfer (NVMe 1.4 §4.3; the cases of linux-7/drivers/nvme/host/pci.c:1028
//! `nvme_pci_setup_data_prp`): PRP1 = the first byte (dword aligned, any offset in its page); a transfer within two
//! pages has PRP2 = the second page; a longer one has PRP2 = a list page holding the addresses of pages 2..n. The
//! pages after the first need not be contiguous (BL-1: the caller's own memory, or bounce pages from the queue's pool).
//! One list page holds `PAGE / 8` entries, so a transfer is bounded by `max_bytes` (one request's size, not a count).

use crate::regs::PAGE;

/// Largest transfer one list page describes from a page-aligned start: PRP1's page plus a full list.
pub const fn max_bytes() -> usize {
    (PAGE / 8 + 1) * PAGE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prps {
    pub prp1: u64,
    pub prp2: u64,
    /// Entries to write into the list page at `list_addr` (0 = no list): page(1) ..= page(list_len).
    pub list_len: usize,
}

/// Pages `len` bytes from device address `first` touch.
pub fn pages(first: u64, len: usize) -> usize {
    ((first % PAGE as u64) as usize).saturating_add(len).div_ceil(PAGE)
}

/// Page `k` (k >= 1) of a physically contiguous transfer from `first`.
pub fn contig_page(first: u64, k: usize) -> u64 {
    (first & !(PAGE as u64 - 1)) + (k * PAGE) as u64
}

/// The PRPs of `len` bytes from `first`; `second` = the transfer's second page (used when it has exactly two),
/// `list_addr` = the list page (used when it has more). None: `len` is 0, `first` is not dword aligned, `second`
/// not page aligned, or one list page is too short.
pub fn plan(first: u64, len: usize, second: u64, list_addr: u64) -> Option<Prps> {
    let n = pages(first, len);
    if len == 0 || !first.is_multiple_of(4) || n - 1 > PAGE / 8 {
        return None;
    }
    Some(match n {
        1 => Prps { prp1: first, prp2: 0, list_len: 0 },
        2 if second.is_multiple_of(PAGE as u64) => Prps { prp1: first, prp2: second, list_len: 0 },
        2 => return None,
        n => Prps { prp1: first, prp2: list_addr, list_len: n - 1 },
    })
}

//! Where the slots lie on the disk and the check of every block a panic writes (CRASHDISK; pure, host-tested).
//! The file's runs come from fs-fat (its cluster chain) in volume sectors; here they become disk LBAs and are
//! checked once at boot: the volume is exactly the partition the device may write (the ESP `nvme_boot=` names),
//! every run lies in the volume's data area, no two overlap, together they hold the whole file, and nothing else
//! on the volume links into its clusters.

use super::device::Run;
use alloc::vec::Vec;

/// Runs the area may be split into (a fresh 1 MiB file is one or a few; more is a badly fragmented volume).
pub const MAX_RUNS: usize = 64;

/// What fs-fat says about the file and its volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFacts<'a> {
    /// (first volume sector, sectors), in file order.
    pub runs: &'a [(u64, u64)],
    pub size: u64,
    pub bps: usize,
    pub data_start: u64,
    pub vol_total: u64,
    /// The volume's partition on the disk: (first LBA, blocks).
    pub part: (u64, u64),
    /// Where the volume starts inside its partition (0 on the NVMe: the volume is the partition's own disk).
    pub vol_offset: u64,
    /// Links into the file's chain from another chain or directory entry (fs-fat's cross-link scan).
    pub foreign: u64,
}

/// The file's blocks on the disk, in file order: (first LBA, blocks).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extents {
    runs: Vec<(u64, u64)>,
    bs: usize,
}

fn inside(w: (u64, u64), lba: u64, n: u64) -> bool {
    n > 0 && lba >= w.0 && lba.checked_add(n).is_some_and(|e| e <= w.0.saturating_add(w.1))
}

/// Checks the facts against the device's block size and its one writable range `allowed`, for a file of
/// `want` bytes. Err: the first rule that failed, in words for the log.
pub fn resolve(f: &FileFacts<'_>, bs: usize, allowed: (u64, u64), want: u64) -> Result<Extents, &'static str> {
    if f.bps != bs || bs == 0 {
        return Err("the volume's sector size is not the disk's block size");
    }
    if f.part != allowed {
        return Err("the boot volume is not exactly the partition the disk lets the panic write");
    }
    if f.vol_offset.checked_add(f.vol_total).is_none_or(|e| e > f.part.1) || f.data_start >= f.vol_total {
        return Err("the volume's size does not fit its partition");
    }
    if f.foreign != 0 {
        return Err("another file or folder shares the area's clusters (a cross-linked FAT)");
    }
    if f.size != want {
        return Err("the area file does not have its size");
    }
    if f.runs.is_empty() || f.runs.len() > MAX_RUNS {
        return Err("the area file lies in no run or in too many pieces");
    }
    let mut sum = 0u64;
    let mut runs = Vec::new();
    for &(s, n) in f.runs {
        if n == 0 || s < f.data_start || !s.checked_add(n).is_some_and(|e| e <= f.vol_total) {
            return Err("a run of the area lies outside the volume's data area");
        }
        let lba = f.part.0.checked_add(f.vol_offset).and_then(|b| b.checked_add(s)).ok_or("a run of the area lies past the disk")?;
        if !inside(f.part, lba, n) {
            return Err("a run of the area lies outside its partition");
        }
        sum += n;
        runs.push((lba, n));
    }
    let mut sorted = runs.clone();
    sorted.sort_unstable();
    if sorted.windows(2).any(|w| w[0].0 + w[0].1 > w[1].0) {
        return Err("two runs of the area overlap (a damaged cluster chain)");
    }
    if sum.saturating_mul(bs as u64) < want {
        return Err("the area's runs hold less than the file");
    }
    Ok(Extents { runs, bs })
}

impl Extents {
    pub fn runs(&self) -> &[(u64, u64)] {
        &self.runs
    }

    /// The disk LBA of the file's byte `off` (a block start); None past the runs.
    pub fn lba_of(&self, off: u64) -> Option<u64> {
        let mut left = off / self.bs as u64;
        for &(lba, n) in &self.runs {
            if left < n {
                return Some(lba + left);
            }
            left -= n;
        }
        None
    }

    /// The writes of the file's bytes `off..off + len` (whole blocks) from buffer offset `buf_off` into `out`;
    /// the count. None: not whole blocks, past the file's runs, or more pieces than `out` holds. No allocation.
    pub fn map(&self, off: u64, len: u64, buf_off: usize, out: &mut [Run]) -> Option<usize> {
        let bs = self.bs as u64;
        if len == 0 || !off.is_multiple_of(bs) || !len.is_multiple_of(bs) {
            return None;
        }
        let (mut skip, mut want, mut at, mut k) = (off / bs, len / bs, buf_off, 0);
        for &(lba, n) in &self.runs {
            if want == 0 {
                break;
            }
            if skip >= n {
                skip -= n;
                continue;
            }
            let take = (n - skip).min(want);
            *out.get_mut(k)? = Run { lba: lba + skip, off: at, blocks: u32::try_from(take).ok()? };
            k += 1;
            at += (take * bs) as usize;
            want -= take;
            skip = 0;
        }
        (want == 0).then_some(k)
    }

    /// `lba..lba + n` lies inside one run (the check right before a write).
    pub fn holds(&self, lba: u64, n: u64) -> bool {
        self.runs.iter().any(|&r| inside(r, lba, n))
    }
}

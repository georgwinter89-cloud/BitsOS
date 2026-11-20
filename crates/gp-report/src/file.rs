//! The report as a file on the boot stick (only with `gp_stick=on`, through the guarded stick path below
//! `\BITSOS2\`): one file per boot in `\BITSOS2\REPORT\`, named `GP000001.TXT`, `GP000002.TXT`, ... (8.3 names, so
//! any FAT reader shows them), the newest [`KEEP`] kept. A round without network still leaves its evidence there.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The folder, as the kernel's VFS names it (the stick is mounted at /boot).
pub const DIR: &str = "/boot/BITSOS2/REPORT";

/// Reports kept: this boot's and the two before it.
pub const KEEP: usize = 3;

/// The number in a report's name (`GP000042.TXT` -> 42, any case); None for every other name.
pub fn number(name: &str) -> Option<u32> {
    let up = name.to_ascii_uppercase();
    let digits = up.strip_prefix("GP")?.strip_suffix(".TXT")?;
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit())).then(|| digits.parse().ok()).flatten()
}

pub fn name_of(n: u32) -> String {
    format!("GP{:06}.TXT", n % 1_000_000)
}

/// What to write and what to remove, given the names in [`DIR`]: the new file takes the next number after the
/// highest one there, and the oldest reports go so that [`KEEP`] remain once it is written. Names that are not
/// reports are never touched.
pub fn plan(existing: &[&str]) -> (String, Vec<String>) {
    let mut nums: Vec<(u32, &str)> = existing.iter().filter_map(|n| number(n).map(|k| (k, *n))).collect();
    nums.sort_unstable();
    let next = nums.last().map_or(1, |(k, _)| if *k >= 999_999 { 1 } else { k + 1 });
    let drop = nums.len().saturating_sub(KEEP - 1);
    (name_of(next), nums.iter().take(drop).map(|(_, n)| String::from(*n)).collect())
}

/// The measurement run's protocols (`gpu-messung`), in the same folder: `GPUM0001.TXT`, `GPUM0002.TXT`, ... (8.3).
/// None is ever removed or replaced: every run gets a name no file has. [`plan`] never touches them (its names
/// are `GP` and six digits).
pub fn messung_number(name: &str) -> Option<u32> {
    let up = name.to_ascii_uppercase();
    let digits = up.strip_prefix("GPUM")?.strip_suffix(".TXT")?;
    (digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_digit())).then(|| digits.parse().ok()).flatten().filter(|n| *n > 0)
}

pub fn messung_name_of(n: u32) -> String {
    format!("GPUM{:04}.TXT", n)
}

/// The next protocol's name: the number after the highest one there, past 9999 the lowest one no file has;
/// None when all 9999 are taken.
pub fn messung_next(existing: &[&str]) -> Option<String> {
    let used: alloc::collections::BTreeSet<u32> = existing.iter().filter_map(|n| messung_number(n)).collect();
    let after = used.last().map_or(1, |n| n + 1);
    let n = if after <= 9999 { Some(after) } else { (1..=9999).find(|n| !used.contains(n)) };
    n.map(messung_name_of)
}

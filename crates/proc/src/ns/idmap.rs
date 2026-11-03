//! SX-2: the id maps of a user namespace (`/proc/<pid>/{uid,gid,projid}_map`): the text a writer gives, Linux's
//! rules for it (linux-6.6/kernel/user_namespace.c `map_write` :920, `mappings_overlap` :731) and the lookups in
//! both directions (`map_id_range_down` / `map_id_up`). Pure: no locks, no process; host-tested.
//!
//! An extent maps `count` ids starting at `first` (inside the namespace) onto ids starting at `lower`. Once a
//! map is installed, `lower` is a kernel id (the parent's map was applied at the write, as Linux does), so one
//! lookup goes straight from a namespace's id to the kernel's.

use alloc::vec::Vec;
use uapi::errno::EINVAL;
use vfs::Errno;

/// `UID_GID_MAP_MAX_EXTENTS`: the most lines a map may have (an ABI value: the 341st line is EINVAL).
pub const MAX_EXTENTS: usize = 340;
/// `UID_GID_MAP_MAX_BASE_EXTENTS`: up to this many extents Linux shows them in the order written, above it
/// sorted by `first` (`sort_idmaps`).
const BASE_EXTENTS: usize = 5;
/// A write must be shorter than a page (`count >= PAGE_SIZE` is EINVAL).
pub const MAX_WRITE: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub first: u32,
    pub lower: u32,
    pub count: u32,
}

impl Extent {
    fn last(&self) -> u32 {
        self.first.wrapping_add(self.count).wrapping_sub(1)
    }
    fn lower_last(&self) -> u32 {
        self.lower.wrapping_add(self.count).wrapping_sub(1)
    }
}

/// An installed map, in the order `/proc` shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdMap {
    ext: Vec<Extent>,
}

impl IdMap {
    /// The initial user namespace's map: `0 0 4294967295` (every id but -1 is itself).
    pub fn identity() -> IdMap {
        IdMap { ext: alloc::vec![Extent { first: 0, lower: 0, count: u32::MAX }] }
    }
    /// A map from checked extents (`parse` and the parent's mapping already applied).
    pub fn from_extents(mut ext: Vec<Extent>) -> IdMap {
        if ext.len() > BASE_EXTENTS {
            ext.sort_by_key(|e| e.first);
        }
        IdMap { ext }
    }
    pub fn extents(&self) -> &[Extent] {
        &self.ext
    }
    /// `map_id_range_down`: the lower id of `id..id+count-1` when one extent holds the whole range.
    pub fn down_range(&self, id: u32, count: u32) -> Option<u32> {
        let last = id.wrapping_add(count).wrapping_sub(1);
        self.ext.iter().find(|e| id >= e.first && id <= e.last() && last >= e.first && last <= e.last()).map(|e| id - e.first + e.lower)
    }
    /// `map_id_down`: the lower (kernel) id of `id`.
    pub fn down(&self, id: u32) -> Option<u32> {
        self.down_range(id, 1)
    }
    /// `map_id_up`: the namespace's id of lower (kernel) id `lower`.
    pub fn up(&self, lower: u32) -> Option<u32> {
        self.ext.iter().find(|e| lower >= e.lower && lower <= e.lower_last()).map(|e| lower - e.lower + e.first)
    }
}

/// The kernel's `isspace` (lib/ctype.c: space, \t \n \v \f \r, and Latin-1's no-break space 0xa0).
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0xa0)
}

fn skip_spaces(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    i
}

/// `simple_strtoul(.., 10)`: decimal digits from `i`, wrapping on overflow as the kernel's does (no digits:
/// 0 and the position unchanged); the value as the `u32` field it is stored in.
fn strtoul(s: &[u8], mut i: usize) -> (u32, usize) {
    let mut v: u64 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        v = v.wrapping_mul(10).wrapping_add((s[i] - b'0') as u64);
        i += 1;
    }
    (v as u32, i)
}

/// `mappings_overlap`: the upper or the lower range of `e` meets one of `prev`'s.
fn overlaps(prev: &[Extent], e: &Extent) -> bool {
    prev.iter().any(|p| (p.first <= e.last() && p.last() >= e.first) || (p.lower <= e.lower_last() && p.lower_last() >= e.lower))
}

/// One line: "first lower count" with the separators and checks of `map_write`.
fn parse_line(s: &[u8]) -> Result<Extent, Errno> {
    let i = skip_spaces(s, 0);
    let (first, i) = strtoul(s, i);
    if i >= s.len() || !is_space(s[i]) {
        return Err(EINVAL);
    }
    let i = skip_spaces(s, i);
    let (lower, i) = strtoul(s, i);
    if i >= s.len() || !is_space(s[i]) {
        return Err(EINVAL);
    }
    let i = skip_spaces(s, i);
    let (count, i) = strtoul(s, i);
    if i < s.len() && !is_space(s[i]) {
        return Err(EINVAL);
    }
    if skip_spaces(s, i) != s.len() {
        return Err(EINVAL);
    }
    if first == u32::MAX || lower == u32::MAX {
        return Err(EINVAL);
    }
    // A zero count, or a range that wraps.
    if first.wrapping_add(count) <= first || lower.wrapping_add(count) <= lower {
        return Err(EINVAL);
    }
    Ok(Extent { first, lower, count })
}

/// The extents of a map write, in the order written (lower ids still in the parent's numbering). The text ends
/// at its first NUL (`memdup_user_nul` gives the parser a C string). EINVAL for anything Linux refuses: a bad
/// line, an empty map, overlapping ranges, more than `MAX_EXTENTS` lines.
pub fn parse(text: &[u8]) -> Result<Vec<Extent>, Errno> {
    let text = &text[..text.iter().position(|&b| b == 0).unwrap_or(text.len())];
    let mut out: Vec<Extent> = Vec::new();
    let mut rest = Some(text);
    while let Some(pos) = rest {
        let (line, next) = match pos.iter().position(|&b| b == b'\n') {
            Some(n) => (&pos[..n], if n + 1 == pos.len() { None } else { Some(&pos[n + 1..]) }),
            None => (pos, None),
        };
        let e = parse_line(line)?;
        if overlaps(&out, &e) {
            return Err(EINVAL);
        }
        if out.len() + 1 == MAX_EXTENTS && next.is_some() {
            return Err(EINVAL);
        }
        out.try_reserve(1).map_err(|_| uapi::errno::ENOMEM)?;
        out.push(e);
        rest = next;
    }
    if out.is_empty() {
        return Err(EINVAL);
    }
    Ok(out)
}

/// `uid_m_show`'s line: first, the lower id as the reader sees it (-1 when it cannot), count; `%10u` each.
pub fn show_line(first: u32, lower: Option<u32>, count: u32) -> alloc::string::String {
    alloc::format!("{:>10} {:>10} {:>10}\n", first, lower.unwrap_or(u32::MAX), count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(first: u32, lower: u32, count: u32) -> Extent {
        Extent { first, lower, count }
    }

    #[test]
    fn lines_like_linux() {
        assert_eq!(parse(b"0 1000 1\n"), Ok(alloc::vec![e(0, 1000, 1)]));
        assert_eq!(parse(b"0 1000 1"), Ok(alloc::vec![e(0, 1000, 1)]));
        assert_eq!(parse(b"  0\t1000   1  \n"), Ok(alloc::vec![e(0, 1000, 1)]));
        assert_eq!(parse(b"1000 1000 1\n0 0 1\n"), Ok(alloc::vec![e(1000, 1000, 1), e(0, 0, 1)]));
        // An empty line after the last one is a line of its own, and a bad one.
        assert_eq!(parse(b"0 1000 1\n\n"), Err(EINVAL));
        assert_eq!(parse(b""), Err(EINVAL));
        assert_eq!(parse(b"\n"), Err(EINVAL));
        assert_eq!(parse(b"0 1000"), Err(EINVAL), "the second number must be followed by a space");
        assert_eq!(parse(b"0 1000 "), Err(EINVAL), "count 0");
        assert_eq!(parse(b"0 1000 1 x"), Err(EINVAL), "trailing junk");
        assert_eq!(parse(b"0 1000 1x"), Err(EINVAL));
        assert_eq!(parse(b"a 1000 1"), Err(EINVAL), "no digits: 0, then 'a' is no space");
        assert_eq!(parse(b"-1 1000 1"), Err(EINVAL));
        assert_eq!(parse(b"0 1000 1\0junk"), Ok(alloc::vec![e(0, 1000, 1)]), "the text ends at its NUL");
        assert_eq!(parse(b"0x1 1000 1"), Err(EINVAL), "base 10 only");
    }

    #[test]
    fn values_and_ranges() {
        assert_eq!(parse(b"4294967295 1000 1"), Err(EINVAL), "first -1");
        assert_eq!(parse(b"0 4294967295 1"), Err(EINVAL), "lower -1");
        assert_eq!(parse(b"0 0 4294967295"), Ok(alloc::vec![e(0, 0, u32::MAX)]));
        assert_eq!(parse(b"1 0 4294967295"), Err(EINVAL), "wraps");
        assert_eq!(parse(b"0 0 0"), Err(EINVAL));
        // simple_strtoul wraps; the u32 field keeps the low half: 2^32 + 5 is 5.
        assert_eq!(parse(b"4294967301 1000 1"), Ok(alloc::vec![e(5, 1000, 1)]));
        assert_eq!(parse(b"0 1000 10\n5 2000 1"), Err(EINVAL), "upper ranges overlap");
        assert_eq!(parse(b"0 1000 10\n20 1005 1"), Err(EINVAL), "lower ranges overlap");
        assert_eq!(parse(b"0 1000 10\n10 1010 10"), Ok(alloc::vec![e(0, 1000, 10), e(10, 1010, 10)]));
    }

    #[test]
    fn extent_limit() {
        let mut t = alloc::string::String::new();
        for i in 0..MAX_EXTENTS as u32 {
            t.push_str(&alloc::format!("{} {} 1\n", i, 1000 + i));
        }
        assert_eq!(parse(t.as_bytes()).map(|v| v.len()), Ok(MAX_EXTENTS));
        t.push_str("400 2000 1\n");
        assert_eq!(parse(t.as_bytes()), Err(EINVAL));
    }

    #[test]
    fn lookups() {
        let m = IdMap::from_extents(alloc::vec![e(0, 1000, 1), e(1, 100000, 65536)]);
        assert_eq!(m.down(0), Some(1000));
        assert_eq!(m.down(1), Some(100000));
        assert_eq!(m.down(65536), Some(165535));
        assert_eq!(m.down(65537), None);
        assert_eq!(m.down_range(0, 2), None, "a range must lie in one extent");
        assert_eq!(m.down_range(1, 65536), Some(100000));
        assert_eq!(m.up(1000), Some(0));
        assert_eq!(m.up(100005), Some(6));
        assert_eq!(m.up(999), None);
        let id = IdMap::identity();
        assert_eq!((id.down(0), id.down(u32::MAX - 1), id.down(u32::MAX), id.up(u32::MAX)), (Some(0), Some(u32::MAX - 1), None, None));
        // More than five extents are kept sorted by `first` (Linux's shown order).
        let many: Vec<Extent> = (0..6).rev().map(|i| e(i * 10, 1000 + i * 10, 1)).collect();
        assert_eq!(IdMap::from_extents(many).extents()[0].first, 0);
        let few: Vec<Extent> = (0..5).rev().map(|i| e(i * 10, 1000 + i * 10, 1)).collect();
        assert_eq!(IdMap::from_extents(few).extents()[0].first, 40);
        assert_eq!(show_line(0, Some(1000), 1), "         0       1000          1\n");
        assert_eq!(show_line(0, None, 1), "         0 4294967295          1\n");
    }
}

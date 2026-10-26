//! Reading a directory's raw entries the way Windows does (V1 `spec/usb.md` §6.1, `fat-tricky.mjs`):
//! - a long name counts only when its parts come in order (last part first, flagged 0x40, down to part 1), all
//!   carry the checksum of the short entry right after them, and nothing else sits between; otherwise (parts
//!   missing, out of order, mixed checksums, no short entry) the long name is ignored and the short name shown;
//! - the NT lower-case flags of a short-only name are honoured;
//! - the first entry whose first byte is 0x00 ends the directory: entries behind it are not shown;
//! - deleted entries (0xE5) and half-deleted sets are skipped; the volume label lives only in the root.

use crate::dirent::{self, Entry, ATTR_LFN, ATTR_VOLUME, DELETED, ENTRY};
use crate::name;
use alloc::vec::Vec;

/// One visible entry.
#[derive(Clone, Debug)]
pub struct Item {
    /// The name shown (UTF-8): the long name, else the short one.
    pub name: Vec<u8>,
    pub e: Entry,
    /// Index of the short entry in the directory.
    pub slot: u32,
    /// The valid LFN entries right before it that belong to it (removed with it).
    pub lfn: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Listing {
    pub items: Vec<Item>,
    /// The root's volume label entry.
    pub label: Option<[u8; 11]>,
    /// Index of the end mark (or the number of slots when there is none).
    pub end: u32,
}

struct Acc {
    sum: u8,
    next: u8,
    units: Vec<u16>,
    count: u32,
}

/// A name the VFS can hold: not empty, not "." or "..", no '/' or NUL.
fn usable(n: &[u8]) -> bool {
    !n.is_empty() && n != b"." && n != b".." && !n.iter().any(|&b| b == b'/' || b == 0)
}

pub fn parse(raw: &[u8]) -> Listing {
    let mut out = Listing { end: (raw.len() / ENTRY) as u32, ..Default::default() };
    let mut acc: Option<Acc> = None;
    for (i, e) in raw.chunks_exact(ENTRY).enumerate() {
        let first = e[0];
        if first == 0 {
            out.end = i as u32;
            break;
        }
        if first == DELETED {
            acc = None;
            continue;
        }
        let attr = e[11];
        if attr & 0x3F == ATTR_LFN {
            let ord = first & 0x1F;
            if first & 0x40 != 0 && ord >= 1 {
                let mut units = alloc::vec![0xFFFFu16; ord as usize * 13];
                units[(ord as usize - 1) * 13..ord as usize * 13].copy_from_slice(&dirent::lfn_units(e));
                acc = Some(Acc { sum: e[13], next: ord - 1, units, count: 1 });
            } else {
                acc = match acc.take() {
                    Some(mut a) if ord >= 1 && first & 0x40 == 0 && ord == a.next && e[13] == a.sum => {
                        a.units[(ord as usize - 1) * 13..ord as usize * 13].copy_from_slice(&dirent::lfn_units(e));
                        a.next -= 1;
                        a.count += 1;
                        Some(a)
                    }
                    _ => None,
                };
            }
            continue;
        }
        let a = acc.take();
        if attr & ATTR_VOLUME != 0 {
            if out.label.is_none() {
                let mut l = [0u8; 11];
                l.copy_from_slice(&e[..11]);
                out.label = Some(l);
            }
            continue;
        }
        if first == b'.' {
            continue;
        }
        let ent = Entry::parse(e);
        let long = a.filter(|a| a.next == 0 && a.sum == name::checksum(&ent.name)).and_then(|a| {
            let len = a.units.iter().position(|&w| w == 0).unwrap_or(a.units.len());
            let n = name::from_utf16(&a.units[..len]);
            usable(&n).then_some((n, a.count))
        });
        let (shown, lfn) = match long {
            Some((n, c)) => (n, c),
            None => (name::short_display(&ent.name, ent.ntres), 0),
        };
        if !usable(&shown) {
            continue;
        }
        out.items.push(Item { name: shown, e: ent, slot: i as u32, lfn });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirent::{lfn_entries, stamp, ATTR_ARCHIVE};
    use alloc::vec;

    fn short(n: &[u8; 11]) -> [u8; ENTRY] {
        Entry::new(*n, ATTR_ARCHIVE, 0, stamp(0)).bytes()
    }
    fn long(l: &str, s: &[u8; 11]) -> Vec<[u8; ENTRY]> {
        let u: Vec<u16> = l.encode_utf16().collect();
        let mut v = lfn_entries(&u, name::checksum(s));
        v.push(short(s));
        v
    }
    fn names(raw: &[[u8; ENTRY]]) -> Vec<String> {
        let b: Vec<u8> = raw.iter().flatten().copied().collect();
        parse(&b).items.into_iter().map(|i| String::from_utf8(i.name).unwrap()).collect()
    }
    use alloc::string::String;

    #[test]
    fn windows_rules() {
        let mut raw = long("This is file one.txt", b"THISIS~1TXT");
        assert_eq!(names(&raw), ["This is file one.txt"]);
        // orphan with a wrong checksum
        let mut o = lfn_entries(&"Ghost.txt".encode_utf16().collect::<Vec<_>>(), 0x11);
        o.push(short(b"ORPHAN  TXT"));
        assert_eq!(names(&o), ["ORPHAN.TXT"]);
        // part 2 of 3 missing
        let g = long("Gap set with three parts in it.txt", b"GAPSET~1TXT");
        assert_eq!(names(&[g[0], g[2], g[3]]), ["GAPSET~1.TXT"]);
        // first part missing
        let f = long("Incomplete long name.txt", b"INCOMP~1TXT");
        assert_eq!(names(&[f[1], f[2]]), ["INCOMP~1.TXT"]);
        // mixed checksums
        let mut m = long("Mixed checksum parts.txt", b"MIXSUM~1TXT");
        m[1][13] ^= 0x5A;
        assert_eq!(names(&m), ["MIXSUM~1.TXT"]);
        // deleted and half deleted
        let mut d = long("Deleted.txt", b"DELETE~1TXT");
        for x in d.iter_mut() {
            x[0] = DELETED;
        }
        let mut h = long("Half deleted.txt", b"HALFDE~1TXT");
        let last = h.len() - 1;
        h[last][0] = DELETED;
        let mut all = d.clone();
        all.extend(h);
        all.extend(raw.clone());
        assert_eq!(names(&all), ["This is file one.txt"]);
        // the end mark hides what follows
        raw.push([0u8; ENTRY]);
        raw.push(short(b"GHOST   TXT"));
        let b: Vec<u8> = raw.iter().flatten().copied().collect();
        let l = parse(&b);
        assert_eq!((l.items.len(), l.end, l.items[0].slot, l.items[0].lfn), (1, 3, 2, 2));
        // NT lower case, dot entries, label
        let mut nt = short(b"DEMO    ASM");
        nt[12] = 0x18;
        let mut lab = short(b"BITSOS     ");
        lab[11] = ATTR_VOLUME;
        let dot = short(b".          ");
        assert_eq!(names(&[lab, dot, nt]), ["demo.asm"]);
        let b: Vec<u8> = [lab].iter().flatten().copied().collect();
        assert_eq!(parse(&b).label, Some(*b"BITSOS     "));
        // a long name with '/' falls back to the short one
        assert_eq!(names(&long("a/b", b"AB~1       ")), ["AB~1"]);
        assert!(names(&vec![]).is_empty());
    }
}

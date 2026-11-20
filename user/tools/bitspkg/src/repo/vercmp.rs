//! pacman's version order, bit for bit (pacman/lib/libalpm/version.c:39 parseEVR, :83 rpmvercmp, :219
//! alpm_pkg_vercmp). The C code walks NUL-terminated copies and writes NULs at segment ends; here a segment end is
//! an index, and "the byte at i" is 0 past the end, which is what the C code reads there. ctype is the C locale
//! (vercmp sets none): only ASCII letters and digits count, every other byte separates.

use std::cmp::Ordering;

fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `[epoch:]version[-release]` (version.c:39). A missing epoch is "0"; no '-' means no release.
fn parse_evr(evr: &[u8]) -> (&[u8], &[u8], Option<&[u8]>) {
    let mut s = 0;
    while s < evr.len() && evr[s].is_ascii_digit() {
        s += 1;
    }
    // strrchr(s, '-'): the last '-' at or after the epoch's end.
    let se = evr[s..].iter().rposition(|&b| b == b'-').map(|p| p + s);
    let (epoch, version) = if at(evr, s) == b':' {
        let e = &evr[..s];
        (if e.is_empty() { &b"0"[..] } else { e }, s + 1)
    } else {
        (&b"0"[..], 0)
    };
    match se {
        // A '-' before the version start cannot be (se >= s, and version starts at s or s + 1 with evr[s] = ':').
        Some(se) if se >= version => (epoch, &evr[version..se], Some(&evr[se + 1..])),
        _ => (epoch, &evr[version..], None),
    }
}

/// version.c:83. 1: a newer, 0 same, -1 b newer.
fn rpmvercmp(a: &[u8], b: &[u8]) -> i32 {
    if a == b {
        return 0;
    }
    let (mut one, mut two) = (0usize, 0usize);
    let (mut ptr1, mut ptr2) = (0usize, 0usize);
    while at(a, one) != 0 && at(b, two) != 0 {
        while at(a, one) != 0 && !at(a, one).is_ascii_alphanumeric() {
            one += 1;
        }
        while at(b, two) != 0 && !at(b, two).is_ascii_alphanumeric() {
            two += 1;
        }
        if !(at(a, one) != 0 && at(b, two) != 0) {
            break;
        }
        if one - ptr1 != two - ptr2 {
            return if one - ptr1 < two - ptr2 { -1 } else { 1 };
        }
        ptr1 = one;
        ptr2 = two;
        let isnum = at(a, ptr1).is_ascii_digit();
        if isnum {
            while at(a, ptr1).is_ascii_digit() {
                ptr1 += 1;
            }
            while at(b, ptr2).is_ascii_digit() {
                ptr2 += 1;
            }
        } else {
            while at(a, ptr1).is_ascii_alphabetic() {
                ptr1 += 1;
            }
            while at(b, ptr2).is_ascii_alphabetic() {
                ptr2 += 1;
            }
        }
        if one == ptr1 {
            return -1;
        }
        if two == ptr2 {
            return if isnum { 1 } else { -1 };
        }
        let (mut o, mut t) = (one, two);
        if isnum {
            while o < ptr1 && a[o] == b'0' {
                o += 1;
            }
            while t < ptr2 && b[t] == b'0' {
                t += 1;
            }
            if ptr1 - o > ptr2 - t {
                return 1;
            }
            if ptr2 - t > ptr1 - o {
                return -1;
            }
        }
        match a[o..ptr1].cmp(&b[t..ptr2]) {
            Ordering::Less => return -1,
            Ordering::Greater => return 1,
            Ordering::Equal => {}
        }
        one = ptr1;
        two = ptr2;
    }
    let (c1, c2) = (at(a, one), at(b, two));
    if c1 == 0 && c2 == 0 {
        return 0;
    }
    if (c1 == 0 && !c2.is_ascii_alphabetic()) || c1.is_ascii_alphabetic() { -1 } else { 1 }
}

/// alpm_pkg_vercmp (version.c:219): epoch, then version, then the release only when both have one.
pub fn vercmp(a: &str, b: &str) -> i32 {
    // C strings end at the first NUL; a Rust string may hold one.
    let cut = |s: &str| -> Vec<u8> { s.bytes().take_while(|&c| c != 0).collect() };
    let (a, b) = (cut(a), cut(b));
    if a == b {
        return 0;
    }
    let (e1, v1, r1) = parse_evr(&a);
    let (e2, v2, r2) = parse_evr(&b);
    let mut ret = rpmvercmp(e1, e2);
    if ret == 0 {
        ret = rpmvercmp(v1, v2);
        if ret == 0
            && let (Some(r1), Some(r2)) = (r1, r2)
        {
            ret = rpmvercmp(r1, r2);
        }
    }
    ret
}

pub fn order(a: &str, b: &str) -> Ordering {
    vercmp(a, b).cmp(&0)
}

#[cfg(test)]
mod tests {
    use super::vercmp;

    #[test]
    fn hand_cases() {
        for (a, b, want) in [
            ("1.0", "1.0", 0),
            ("1.0", "1.0.0", -1),
            ("1:1.0", "2.0", 1),
            ("1.0a", "1.0", -1),
            ("1.0", "1.0-1", 0),
            ("1.0-1", "1.0-2", -1),
            ("1.0.a", "1.0.1", -1),
            ("", "0", -1),
            (":1", "0", 1),
        ] {
            assert_eq!(vercmp(a, b), want, "{a} vs {b}");
        }
    }
}

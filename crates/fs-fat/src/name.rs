//! Names on FAT: long names (UTF-16 in LFN entries) and their 8.3 aliases.
//!
//! Linux 6.6 vfat is the reference (`fs/fat/namei_vfat.c`): trailing dots are not part of a name
//! (`vfat_striptail_len`), `vfat_bad_char` characters and a trailing space are EINVAL, a name longer than 255
//! UTF-16 units is ENAMETOOLONG, lookups are case-insensitive (`vfat_ci_dentry_ops`; with the utf8 option only
//! ASCII letters fold, as here). New aliases follow `vfat_create_shortname`'s idea: a plain upper-case 8.3 name
//! is stored alone; a name that only differs in case from a free 8.3 alias keeps it without a tail
//! ("Readme.TXT" -> README.TXT, as Windows); anything else gets BASE~n.EXT with the lowest free n (V1's rule).

use alloc::vec::Vec;
use uapi::errno::*;

/// An 8.3 name as stored: 8 bytes base, 3 bytes extension, space padded.
pub type Short = [u8; 11];

/// The key two names compare by: trailing dots stripped, ASCII upper case.
pub fn fold(name: &[u8]) -> Vec<u8> {
    let mut n = strip_dots(name).to_vec();
    n.make_ascii_uppercase();
    n
}

pub fn strip_dots(name: &[u8]) -> &[u8] {
    let mut l = name.len();
    while l > 0 && name[l - 1] == b'.' {
        l -= 1;
    }
    &name[..l]
}

fn bad_char(w: u16) -> bool {
    w < 0x20 || matches!(w, 0x2A | 0x3F | 0x3C | 0x3E | 0x7C | 0x22 | 0x3A | 0x2F | 0x5C)
}

/// A name a program gives (UTF-8) as the UTF-16 units of its LFN entries, with Linux's checks.
pub fn to_utf16(name: &[u8]) -> Result<Vec<u16>, i64> {
    let n = strip_dots(name);
    if n.is_empty() {
        return Err(ENOENT);
    }
    let s = core::str::from_utf8(n).map_err(|_| EINVAL)?;
    let mut u: Vec<u16> = Vec::new();
    for c in s.chars() {
        let mut b = [0u16; 2];
        u.extend_from_slice(c.encode_utf16(&mut b));
    }
    if u.len() > 255 {
        return Err(ENAMETOOLONG);
    }
    if u.iter().any(|&w| bad_char(w)) || u.last() == Some(&0x20) {
        return Err(EINVAL);
    }
    Ok(u)
}

/// UTF-16 from an LFN as UTF-8 (unpaired surrogates become U+FFFD).
pub fn from_utf16(u: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in core::char::decode_utf16(u.iter().copied()) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        let mut b = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
    }
    out
}

/// Code page 437, bytes 0x80..0xFF (short names written by DOS/Windows with bytes above ASCII).
const CP437: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢',
    '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖',
    '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙',
    '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩',
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// NTRes bits Windows NT sets for an 8.3 name shown in lower case.
pub const LOWER_BASE: u8 = 0x08;
pub const LOWER_EXT: u8 = 0x10;

/// How an 8.3 entry without a long name is shown: "BASE.EXT", lower case by NTRes, 0x05 as 0xE5, bytes
/// above ASCII through code page 437.
pub fn short_display(s: &Short, ntres: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let mut push = |b: u8, lower: bool| {
        if b < 0x80 {
            out.push(if lower { b.to_ascii_lowercase() } else { b });
        } else {
            let mut t = [0u8; 4];
            out.extend_from_slice(CP437[(b - 0x80) as usize].encode_utf8(&mut t).as_bytes());
        }
    };
    let base_len = s[..8].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    for (i, &b) in s[..base_len].iter().enumerate() {
        push(if i == 0 && b == 0x05 { 0xE5 } else { b }, ntres & LOWER_BASE != 0);
    }
    let ext_len = s[8..].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    if ext_len > 0 {
        push(b'.', false);
        for &b in &s[8..8 + ext_len] {
            push(b, ntres & LOWER_EXT != 0);
        }
    }
    out
}

/// The LFN checksum of an 8.3 name (FAT spec, `fat_checksum`).
pub fn checksum(s: &Short) -> u8 {
    s.iter().fold(0u8, |sum, &b| (((sum & 1) << 7) | (sum >> 1)).wrapping_add(b))
}

fn short_ok(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b"$%'`-@{}~!#()&_^".contains(&b)
}

/// The 8.3 form of a name that *is* a plain upper-case 8.3 name (stored without a long name), else None.
pub fn plain_short(name: &[u8]) -> Option<Short> {
    let (base, ext) = match name.iter().position(|&b| b == b'.') {
        Some(p) => (&name[..p], &name[p + 1..]),
        None => (name, &name[..0]),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || !base.iter().chain(ext).all(|&b| short_ok(b)) {
        return None;
    }
    let mut s = [b' '; 11];
    s[..base.len()].copy_from_slice(base);
    s[8..8 + ext.len()].copy_from_slice(ext);
    Some(s)
}

/// The alias characters of a name: base and extension in upper case with Linux's substitutions, and whether
/// nothing was lost (no character skipped, replaced or cut).
fn alias_parts(u: &[u16]) -> (Vec<u8>, Vec<u8>, bool) {
    let dot = u.iter().rposition(|&w| w == b'.' as u16).filter(|&p| p > 0);
    let (b, e) = match dot {
        Some(p) => (&u[..p], &u[p + 1..]),
        None => (u, &u[..0]),
    };
    let mut lossless = true;
    let mut conv = |src: &[u16], max: usize| -> Vec<u8> {
        let mut out = Vec::new();
        for &w in src {
            let c = if w == b'.' as u16 || w == b' ' as u16 {
                lossless = false;
                continue;
            } else if w < 0x80 && b"[];,+=".contains(&(w as u8)) {
                lossless = false;
                b'_'
            } else if w < 0x80 {
                (w as u8).to_ascii_uppercase()
            } else {
                lossless = false;
                b'_'
            };
            if out.len() == max {
                lossless = false;
                break;
            }
            out.push(c);
        }
        out
    };
    let base = conv(b, 8);
    let ext = conv(e, 3);
    (base, ext, lossless)
}

/// A new alias for the long name `u`, free in the directory (`taken` says whether an alias is in use).
/// EEXIST when no tail up to ~999999 is free (Linux gives up much earlier, also with EEXIST).
pub fn make_alias(u: &[u16], taken: &dyn Fn(&Short) -> bool) -> Result<Short, i64> {
    let (mut base, ext, lossless) = alias_parts(u);
    if base.is_empty() {
        base.push(b'_');
    }
    let mut s = [b' '; 11];
    s[8..8 + ext.len()].copy_from_slice(&ext);
    if lossless && base.iter().chain(&ext).all(|&b| short_ok(b)) {
        s[..base.len()].copy_from_slice(&base);
        if !taken(&s) {
            return Ok(s);
        }
    }
    let mut digits = [0u8; 8];
    for n in 1u32..=999_999 {
        let mut k = 0;
        let mut m = n;
        while m > 0 {
            digits[k] = b'0' + (m % 10) as u8;
            m /= 10;
            k += 1;
        }
        let keep = base.len().min(8 - 1 - k);
        s[..8].fill(b' ');
        s[..keep].copy_from_slice(&base[..keep]);
        s[keep] = b'~';
        for i in 0..k {
            s[keep + 1 + i] = digits[k - 1 - i];
        }
        if s[0] == 0xE5 {
            s[0] = 0x05;
        }
        if !taken(&s) {
            return Ok(s);
        }
    }
    Err(EEXIST)
}

/// A short name as the key a lookup by alias compares with ("BASE.EXT", upper case).
pub fn short_key(s: &Short) -> Vec<u8> {
    let mut k = short_display(s, 0);
    k.make_ascii_uppercase();
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }
    fn none(_: &Short) -> bool {
        false
    }

    #[test]
    fn checks_like_linux() {
        assert_eq!(to_utf16(b"a.txt...").unwrap(), u("a.txt"));
        assert_eq!(to_utf16(b"..."), Err(ENOENT));
        assert_eq!(to_utf16(b"a:b"), Err(EINVAL));
        assert_eq!(to_utf16(b"a\\b"), Err(EINVAL));
        assert_eq!(to_utf16(b"trailing "), Err(EINVAL));
        assert_eq!(to_utf16(&[0xFF, 0x41]), Err(EINVAL));
        assert_eq!(to_utf16("x".repeat(256).as_bytes()), Err(ENAMETOOLONG));
        assert_eq!(to_utf16("ä".repeat(255).as_bytes()).unwrap().len(), 255);
        assert_eq!(to_utf16("😀".as_bytes()).unwrap().len(), 2, "outside the BMP: a surrogate pair");
        assert_eq!(from_utf16(&u("Länge 😀.txt")), "Länge 😀.txt".as_bytes());
        assert_eq!(from_utf16(&[0xD800, 0x41]), "\u{fffd}A".as_bytes());
        assert_eq!(fold(b"Readme.txt."), b"README.TXT");
    }

    #[test]
    fn short_names() {
        assert_eq!(&plain_short(b"KERNEL").unwrap(), b"KERNEL     ");
        assert_eq!(&plain_short(b"STICK.ID").unwrap(), b"STICK   ID ");
        assert!(plain_short(b"stick.id").is_none());
        assert!(plain_short(b"TOOLONGNAME").is_none());
        assert!(plain_short(b"A.B.C").is_none());
        assert_eq!(short_display(b"DEMO    ASM", LOWER_BASE | LOWER_EXT), b"demo.asm");
        assert_eq!(short_display(b"L\x8eNGE   TXT", 0), "LÄNGE.TXT".as_bytes());
        assert_eq!(short_display(b"\x05BC        ", 0), "σBC".as_bytes(), "0x05 stands for 0xE5 (σ in 437)");
        assert_eq!(checksum(b"ORPHAN  TXT"), {
            let mut s = 0u8;
            for &b in b"ORPHAN  TXT" {
                s = (((s & 1) << 7) | (s >> 1)).wrapping_add(b);
            }
            s
        });
    }

    #[test]
    fn aliases() {
        assert_eq!(&make_alias(&u("Readme.TXT"), &none).unwrap(), b"README  TXT");
        assert_eq!(&make_alias(&u("This is file one.txt"), &none).unwrap(), b"THISIS~1TXT");
        assert_eq!(&make_alias(&u("Länge.txt"), &none).unwrap(), b"L_NGE~1 TXT");
        assert_eq!(&make_alias(&u(".bashrc"), &none).unwrap(), b"BASHRC~1   ");
        assert_eq!(&make_alias(&u("a+b.c"), &none).unwrap(), b"A_B~1   C  ");
        assert_eq!(&make_alias(&u("archive.tar.gz"), &none).unwrap(), b"ARCHIV~1GZ ");
        let taken = vec![*b"THISIS~1TXT", *b"THISIS~2TXT"];
        assert_eq!(&make_alias(&u("This is file three.txt"), &|s: &Short| taken.contains(s)).unwrap(), b"THISIS~3TXT");
        assert_eq!(&make_alias(&u("longname"), &|s: &Short| !s.starts_with(b"LONGN~10")).unwrap(), b"LONGN~10   ");
        assert_eq!(&make_alias(&u("Readme.TXT"), &|s: &Short| s == b"README  TXT").unwrap(), b"README~1TXT");
        assert_eq!(make_alias(&u("x"), &|_: &Short| true), Err(EEXIST));
    }
}

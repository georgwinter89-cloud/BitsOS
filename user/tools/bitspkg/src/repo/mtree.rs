//! A package's `.MTREE` (gzip'd mtree text, written by makepkg through bsdtar `--format=mtree`): every member's
//! type, size, sha256 and link target, read as libarchive's mtree reader reads it (`/set` and `/unset` defaults,
//! `keyword=value` per line, names in vis(3) octal escapes). pacman reads it only for the file list
//! (pacman/lib/libalpm/be_package.c:447 build_filelist_from_mtree) and checks it with `-Qkk`; PK-6d checks every
//! unpacked file against it (plan 1.6: sha256 and size of every file).

use std::collections::BTreeMap;

/// One input's bound (R1): a package's file list as its .MTREE says it, at most 16 MiB of text unpacked.
pub const MTREE_MAX: u64 = 16 << 20;

#[derive(Clone, Debug, PartialEq)]
pub enum MKind {
    File { size: u64, sha256: String },
    Dir,
    Link(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MEntry {
    pub kind: MKind,
    pub mode: u32,
}

/// vis(3) as libarchive's mtree writer uses it: `\ooo` octal, `\\`, and the C escapes libarchive's reader knows.
pub fn unvis(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let rest = &b[i + 1..];
        if rest.len() >= 3 && rest[..3].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (rest[0] - b'0') as u32 * 64 + (rest[1] - b'0') as u32 * 8 + (rest[2] - b'0') as u32;
            out.push(u8::try_from(v).map_err(|_| format!(".MTREE: Escape \\{} zu groß", String::from_utf8_lossy(&rest[..3])))?);
            i += 4;
            continue;
        }
        let c = match rest.first() {
            Some(b'\\') => b'\\',
            Some(b's') => b' ',
            Some(b't') => b'\t',
            Some(b'n') => b'\n',
            Some(b'r') => b'\r',
            Some(b'0') => 0,
            _ => return Err(format!(".MTREE: unbekannte Escape-Folge in {s:?}")),
        };
        out.push(c);
        i += 2;
    }
    String::from_utf8(out).map_err(|_| format!(".MTREE: Name {s:?} ist kein UTF-8"))
}

/// Path as the tar reader gives it: no `./`, no trailing `/`.
fn norm(p: &str) -> String {
    p.trim_start_matches("./").trim_end_matches('/').to_string()
}

/// The entries by path (metadata files like `.PKGINFO` included; the caller skips them).
pub fn parse(text: &str) -> Result<BTreeMap<String, MEntry>, String> {
    let mut set: BTreeMap<String, String> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let mut words = l.split_whitespace();
        let first = words.next().unwrap_or("");
        if first == "/set" || first == "/unset" {
            for w in words {
                let (k, v) = w.split_once('=').unwrap_or((w, ""));
                if first == "/set" {
                    set.insert(k.to_string(), v.to_string());
                } else if k == "all" {
                    set.clear();
                } else {
                    set.remove(k);
                }
            }
            continue;
        }
        if first.starts_with('/') {
            return Err(format!(".MTREE Zeile {}: unbekannter Befehl {first}", n + 1));
        }
        let path = norm(&unvis(first)?);
        if path.is_empty() || path == "." {
            continue;
        }
        let mut kv = set.clone();
        for w in words {
            let (k, v) = w.split_once('=').unwrap_or((w, ""));
            kv.insert(k.to_string(), v.to_string());
        }
        let bad = |what: &str| format!(".MTREE Zeile {}: {path}: {what}", n + 1);
        let mode = match kv.get("mode") {
            Some(m) => u32::from_str_radix(m, 8).map_err(|_| bad("Modus ist keine Oktalzahl"))? & 0o7777,
            None => 0o644,
        };
        let kind = match kv.get("type").map(String::as_str).unwrap_or("file") {
            "file" => {
                let size = kv.get("size").ok_or_else(|| bad("size fehlt"))?.parse().map_err(|_| bad("size ist keine Zahl"))?;
                let sha256 = kv.get("sha256digest").or_else(|| kv.get("sha256")).ok_or_else(|| bad("sha256digest fehlt"))?.to_ascii_lowercase();
                crate::names::check_sha256(&sha256).map_err(|e| bad(&e))?;
                MKind::File { size, sha256 }
            }
            "dir" => MKind::Dir,
            "link" => MKind::Link(unvis(kv.get("link").ok_or_else(|| bad("link fehlt"))?)?),
            t => return Err(bad(&format!("Art {t} wird in Paketen nicht angenommen"))),
        };
        if out.insert(path.clone(), MEntry { kind, mode }).is_some() {
            return Err(bad("doppelt"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_makepkg_mtree() {
        let t = "#mtree\n/set type=file uid=0 gid=0 mode=644\n./.PKGINFO time=1.0 size=458 sha256digest=AB".to_string()
            + &"ab".repeat(31)
            + "\n/set mode=755\n./usr time=1.0 type=dir\n./usr/bin/jq time=1.0 size=30800 sha256digest="
            + &"cd".repeat(32)
            + "\n/set mode=777\n./usr/lib/libjq.so time=1.0 type=link link=libjq.so.1\n./usr/share/a\\040b time=1.0 mode=644 size=1 sha256digest="
            + &"ef".repeat(32)
            + "\n";
        let m = parse(&t).unwrap();
        assert_eq!(m["usr/bin/jq"], MEntry { kind: MKind::File { size: 30800, sha256: "cd".repeat(32) }, mode: 0o755 });
        assert_eq!(m["usr"].kind, MKind::Dir);
        assert_eq!(m["usr/lib/libjq.so"].kind, MKind::Link("libjq.so.1".into()));
        assert!(m.contains_key("usr/share/a b"));
        assert!(matches!(&m[".PKGINFO"].kind, MKind::File { sha256, .. } if sha256 == &"ab".repeat(32)));
    }

    #[test]
    fn refuses_hostile_lines() {
        for t in ["./x size=1\n", "./x type=block\n", "./x type=file size=a sha256digest=00\n", "/frobnicate\n", "./a\\9 type=dir\n"] {
            assert!(parse(t).is_err(), "{t}");
        }
        let dup = "./x type=dir\n./x/ type=dir\n";
        assert!(parse(dup).unwrap_err().contains("doppelt"));
    }
}

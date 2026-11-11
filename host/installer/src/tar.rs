//! POSIX tar, as far as release assets need it: regular files and directories, long names as pax `path`
//! records (GNU long names are read too), everything owned by 0 at time 0 so two packings of the same tree are the
//! same bytes. Links are refused: the stick is FAT, which has none. Paths are checked before anything uses them.

use std::io::{self, Read, Write};

const BLOCK: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mode: u32,
}

/// A relative `/`-separated path without `.`/`..` components, backslashes, NULs or control characters.
pub fn safe_path(p: &str) -> Option<String> {
    let p = p.trim_start_matches("./").trim_end_matches('/');
    if p.is_empty() || p.starts_with('/') {
        return None;
    }
    for c in p.split('/') {
        if c.is_empty() || c == "." || c == ".." || c.chars().any(|ch| ch == '\\' || ch.is_control()) {
            return None;
        }
    }
    Some(p.to_string())
}

fn octal(field: &mut [u8], v: u64) {
    let digits = field.len() - 1;
    let s = format!("{v:0digits$o}");
    field[..digits].copy_from_slice(s.as_bytes());
    field[digits] = 0;
}

fn checksum(h: &mut [u8; BLOCK]) {
    h[148..156].fill(b' ');
    let sum: u64 = h.iter().map(|&b| b as u64).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
}

fn header(name: &[u8], prefix: &[u8], size: u64, mode: u32, typ: u8) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name);
    octal(&mut h[100..108], mode as u64);
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[156] = typ;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix);
    checksum(&mut h);
    h
}

/// Splits a path into ustar's prefix (155) and name (100), if it fits.
fn split(p: &str) -> Option<(&str, &str)> {
    if p.len() <= 100 {
        return Some(("", p));
    }
    p.match_indices('/').map(|(i, _)| i).filter(|&i| i <= 155 && p.len() - i - 1 <= 100 && p.len() - i - 1 > 0).last().map(|i| (&p[..i], &p[i + 1..]))
}

pub struct Writer<W: Write> {
    out: W,
}

impl<W: Write> Writer<W> {
    pub fn new(out: W) -> Self {
        Writer { out }
    }

    fn head(&mut self, path: &str, size: u64, mode: u32, typ: u8) -> io::Result<()> {
        let p = safe_path(path).ok_or_else(|| io::Error::other(format!("tar: unsafe path {path:?}")))?;
        let p = if typ == b'5' { format!("{p}/") } else { p };
        match split(&p) {
            Some((pre, name)) => self.out.write_all(&header(name.as_bytes(), pre.as_bytes(), size, mode, typ)),
            None => {
                // A pax record "<len> path=<p>\n" whose length counts its own digits.
                let body = format!(" path={p}\n");
                let mut n = body.len() + 1;
                while format!("{n}{body}").len() != n {
                    n += 1;
                }
                let rec = format!("{n}{body}");
                self.out.write_all(&header(b"././@PaxHeader", b"", rec.len() as u64, 0o644, b'x'))?;
                self.out.write_all(rec.as_bytes())?;
                self.pad(rec.len() as u64)?;
                let short: String = p.chars().rev().take(90).collect::<String>().chars().rev().collect();
                self.out.write_all(&header(short.trim_start_matches('/').as_bytes(), b"", size, mode, typ))
            }
        }
    }

    fn pad(&mut self, n: u64) -> io::Result<()> {
        let r = (n as usize) % BLOCK;
        if r != 0 {
            self.out.write_all(&[0u8; BLOCK][..BLOCK - r])?;
        }
        Ok(())
    }

    pub fn dir(&mut self, path: &str) -> io::Result<()> {
        self.head(path, 0, 0o755, b'5')
    }

    /// A file of exactly `size` bytes from `data`.
    pub fn file(&mut self, path: &str, mode: u32, size: u64, mut data: impl Read) -> io::Result<()> {
        self.head(path, size, mode, b'0')?;
        let n = io::copy(&mut (&mut data).take(size), &mut self.out)?;
        if n != size {
            return Err(io::Error::other(format!("tar: {path} ended after {n} of {size} bytes")));
        }
        self.pad(size)
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.out.write_all(&[0u8; BLOCK * 2])?;
        self.out.flush()?;
        Ok(self.out)
    }
}

pub struct Reader<R: Read> {
    inp: R,
    /// Bytes of the current entry's data not read yet, then its padding.
    left: u64,
    pad: u64,
}

fn parse_octal(f: &[u8]) -> io::Result<u64> {
    if f.first().is_some_and(|b| b & 0x80 != 0) {
        // GNU base-256 for large sizes.
        let mut v: u64 = (f[0] & 0x7f) as u64;
        for &b in &f[1..] {
            v = v.checked_mul(256).ok_or_else(|| io::Error::other("tar: number too large"))? | b as u64;
        }
        return Ok(v);
    }
    let s: String = f.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| io::Error::other("tar: bad octal number"))
}

fn cstr(f: &[u8]) -> String {
    String::from_utf8_lossy(&f[..f.iter().position(|&b| b == 0).unwrap_or(f.len())]).into_owned()
}

impl<R: Read> Reader<R> {
    pub fn new(inp: R) -> Self {
        Reader { inp, left: 0, pad: 0 }
    }

    fn skip(&mut self, mut n: u64) -> io::Result<()> {
        let mut buf = [0u8; 8192];
        while n > 0 {
            let k = self.inp.read(&mut buf[..(n as usize).min(8192)])?;
            if k == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "tar: archive ends inside an entry"));
            }
            n -= k as u64;
        }
        Ok(())
    }

    fn small_body(&mut self, size: u64) -> io::Result<Vec<u8>> {
        if size > 1 << 20 {
            return Err(io::Error::other("tar: extended header too large"));
        }
        let mut v = vec![0u8; size as usize];
        self.inp.read_exact(&mut v)?;
        self.skip((BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64)?;
        Ok(v)
    }

    /// The next file or directory (data of the previous one is skipped). None at the end of the archive.
    pub fn next_entry(&mut self) -> io::Result<Option<Entry>> {
        self.skip(self.left + self.pad)?;
        self.left = 0;
        self.pad = 0;
        let mut long: Option<String> = None;
        let mut pax_size: Option<u64> = None;
        loop {
            let mut h = [0u8; BLOCK];
            self.inp.read_exact(&mut h)?;
            if h.iter().all(|&b| b == 0) {
                return Ok(None);
            }
            let mut c = h;
            c[148..156].fill(b' ');
            let sum: u64 = c.iter().map(|&b| b as u64).sum();
            if sum != parse_octal(&h[148..156])? {
                return Err(io::Error::other("tar: header checksum wrong (not a tar archive, or damaged)"));
            }
            let size = parse_octal(&h[124..136])?;
            match h[156] {
                b'x' => {
                    let body = self.small_body(size)?;
                    for rec in String::from_utf8_lossy(&body).split('\n') {
                        let Some((_, kv)) = rec.split_once(' ') else { continue };
                        if let Some(p) = kv.strip_prefix("path=") {
                            long = Some(p.to_string());
                        } else if let Some(s) = kv.strip_prefix("size=") {
                            pax_size = Some(s.parse().map_err(|_| io::Error::other("tar: bad pax size"))?);
                        }
                    }
                }
                b'g' => {
                    self.small_body(size)?;
                }
                b'L' => long = Some(cstr(&self.small_body(size)?)),
                t @ (b'0' | 0 | b'5') => {
                    let mut path = cstr(&h[..100]);
                    if &h[257..262] == b"ustar" && h[345] != 0 {
                        path = format!("{}/{path}", cstr(&h[345..500]));
                    }
                    let path = long.take().unwrap_or(path);
                    // The archive's own root (`./`, as dpkg packs data.tar) names nothing to create.
                    if t == b'5' && path.trim_start_matches("./").trim_end_matches('/').is_empty() {
                        pax_size = None;
                        continue;
                    }
                    let p = safe_path(&path).ok_or_else(|| io::Error::other(format!("tar: unsafe path {path:?}")))?;
                    let size = pax_size.take().unwrap_or(size);
                    let kind = if t == b'5' || path.ends_with('/') { EntryKind::Dir } else { EntryKind::File };
                    let size = if kind == EntryKind::Dir { 0 } else { size };
                    self.left = size;
                    self.pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
                    return Ok(Some(Entry { path: p, kind, size, mode: parse_octal(&h[100..108])? as u32 }));
                }
                b'1' | b'2' => return Err(io::Error::other(format!("tar: {} is a link; the stick's FAT has none", cstr(&h[..100])))),
                t => return Err(io::Error::other(format!("tar: entry type {:?} is not supported", t as char))),
            }
        }
    }
}

impl<R: Read> Read for Reader<R> {
    /// Reads the current entry's data.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.left) as usize;
        let n = self.inp.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "tar: archive ends inside a file"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_long_names() {
        let long = format!("BITSOS2/libreoffice/{}/{}", "share/config/soffice.cfg/modules/swriter/ui".repeat(2), "x".repeat(120));
        let mut w = Writer::new(Vec::new());
        w.dir("BITSOS2/mesa").unwrap();
        w.file("BITSOS2/mesa/a", 0o755, 3, &b"abc"[..]).unwrap();
        w.file(&long, 0o644, 600, &[7u8; 600][..]).unwrap();
        w.file("e", 0o644, 0, &b""[..]).unwrap();
        let bytes = w.finish().unwrap();
        let mut r = Reader::new(&bytes[..]);
        let mut got = Vec::new();
        while let Some(e) = r.next_entry().unwrap() {
            let mut d = Vec::new();
            r.read_to_end(&mut d).unwrap();
            got.push((e.path, e.kind, d.len(), e.mode));
        }
        assert_eq!(got, vec![
            ("BITSOS2/mesa".into(), EntryKind::Dir, 0, 0o755),
            ("BITSOS2/mesa/a".into(), EntryKind::File, 3, 0o755),
            (long, EntryKind::File, 600, 0o644),
            ("e".into(), EntryKind::File, 0, 0o644),
        ]);
    }

    #[test]
    fn system_tar_reads_our_archives() {
        // GNU tar (or bsdtar) must list what we write: the assets can be unpacked by hand.
        let Ok(o) = std::process::Command::new("tar").arg("--version").output() else { panic!("no tar on this host") };
        assert!(o.status.success());
        let d = std::env::temp_dir().join(format!("bi-tar-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let long = format!("t/{}/f", "d".repeat(130));
        let mut w = Writer::new(Vec::new());
        w.file(&long, 0o644, 2, &b"hi"[..]).unwrap();
        std::fs::write(d.join("a.tar"), w.finish().unwrap()).unwrap();
        let o = std::process::Command::new("tar").arg("-tf").arg(d.join("a.tar")).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), long);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unsafe_paths_are_refused() {
        for p in ["/etc/passwd", "a/../b", "..", "a//b", "a\\b", ""] {
            assert!(safe_path(p).is_none(), "{p}");
        }
        assert_eq!(safe_path("./a/b/").as_deref(), Some("a/b"));
        let mut root = header(b"./", b"", 0, 0o755, b'5').to_vec();
        root.extend_from_slice(&header(b"./x", b"", 0, 0o644, b'0'));
        root.extend_from_slice(&[0u8; 1024]);
        assert_eq!(Reader::new(&root[..]).next_entry().unwrap().unwrap().path, "x", "a dpkg root entry is skipped");
        let mut h = header(b"../x", b"", 0, 0o644, b'0').to_vec();
        h.extend_from_slice(&[0u8; 1024]);
        assert!(Reader::new(&h[..]).next_entry().is_err());
    }
}

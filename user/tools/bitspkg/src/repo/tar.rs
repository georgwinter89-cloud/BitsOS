//! A tar reader for sync databases and (PK-6d) packages: ustar, pax (`path`, `linkpath`, `size`) and GNU long
//! names (`L`, `K`); regular files, folders, symlinks and hard links. Refused with a reason: devices, FIFOs,
//! sparse and other GNU kinds, absolute names and `..`, a bad header checksum, a truncated archive. Nothing here
//! touches the disk: the caller decides what an entry becomes. The path rules follow host/installer/src/tar.rs
//! (lane/INST), which writes release assets and refuses links; libarchive's reader is the behaviour model
//! (pacman reads every archive through it).

use std::io::Read;

const BLOCK: usize = 512;
/// A pax or GNU long-name record is a few hundred bytes in real archives; this bound only stops a hostile one.
pub const META_MAX: u64 = 64 << 10;
/// No archive can hold more than this (a pax `size` or base-256 size above it is hostile).
pub const ENTRY_MAX: u64 = i64::MAX as u64;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    File,
    Dir,
    Symlink(String),
    HardLink(String),
    /// A device file or FIFO, only with `keep_specials` (PK-6d drops and reports them; a database refuses them).
    Special(&'static str),
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// Relative, normalised: no leading `./`, no trailing `/`, never empty, no `..`.
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
    /// Owner as the archive says it (pax `uid`/`gid` over the header's): PK-6d reports and drops a non-root one.
    pub uid: u64,
    pub gid: u64,
    /// The names of pax `SCHILY.xattr.*` records (file capabilities, ACLs): never applied, PK-6d reports them.
    pub xattrs: Vec<String>,
}

pub struct Tar<R: Read> {
    r: R,
    specials: bool,
    /// Bytes of the current entry's data (plus padding) not consumed yet.
    left: u64,
    pad: u64,
    done: bool,
}

fn octal(f: &[u8], what: &str) -> Result<u64, String> {
    // GNU base-256: the high bit of the first byte set, the rest a big-endian number (sizes over 8 GiB).
    if let Some(&b0) = f.first()
        && b0 & 0x80 != 0
    {
        if b0 & 0x40 != 0 {
            return Err(format!("tar: negative {what}"));
        }
        let mut v: u64 = (b0 & 0x3f) as u64;
        for &b in &f[1..] {
            v = v.checked_mul(256).and_then(|v| v.checked_add(b as u64)).ok_or_else(|| format!("tar: {what} too large"))?;
        }
        return Ok(v);
    }
    let t: Vec<u8> = f.iter().copied().take_while(|&b| b != 0).collect();
    let t = String::from_utf8_lossy(&t);
    let t = t.trim_matches(|c| c == ' ');
    if t.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(t, 8).map_err(|_| format!("tar: {what} is not a number ({t:?})"))
}

fn cstr(f: &[u8]) -> Vec<u8> {
    f.iter().copied().take_while(|&b| b != 0).collect()
}

/// Checks a member name: relative, no `..`, no NUL; `./` and trailing `/` are dropped.
pub fn clean_path(raw: &[u8]) -> Result<String, String> {
    let s = std::str::from_utf8(raw).map_err(|_| format!("tar: name is not UTF-8 ({})", String::from_utf8_lossy(raw)))?;
    if s.starts_with('/') {
        return Err(format!("tar: absolute name {s:?} refused"));
    }
    let mut parts = Vec::new();
    for p in s.split('/') {
        match p {
            "" | "." => {}
            ".." => return Err(format!("tar: name {s:?} leaves the tree (..)")),
            _ => parts.push(p),
        }
    }
    if parts.is_empty() {
        return Err(format!("tar: empty name {s:?}"));
    }
    if s.bytes().any(|b| b < 0x20) {
        return Err(format!("tar: control character in name {s:?}"));
    }
    Ok(parts.join("/"))
}

/// A link target: a hard link names a member (same rules as a name); a symlink may be relative with `..` (the
/// caller checks it stays in its tree, PK-6d) but never absolute here either: an absolute target means the host's
/// tree, never the package's.
fn link_target(raw: &[u8], hard: bool) -> Result<String, String> {
    if hard {
        return clean_path(raw);
    }
    let s = std::str::from_utf8(raw).map_err(|_| "tar: link target is not UTF-8".to_string())?;
    if s.is_empty() || s.bytes().any(|b| b < 0x20) {
        return Err(format!("tar: bad link target {s:?}"));
    }
    Ok(s.to_string())
}

/// pax records: `<len> <key>=<value>\n` (POSIX pax, libarchive archive_read_support_format_tar.c pax_header).
fn pax(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let sp = data[i..].iter().position(|&b| b == b' ').ok_or("tar: pax record without length")? + i;
        let len: usize = std::str::from_utf8(&data[i..sp]).ok().and_then(|t| t.parse().ok()).ok_or("tar: bad pax length")?;
        if len <= sp - i + 1 || i + len > data.len() || data[i + len - 1] != b'\n' {
            return Err("tar: bad pax record".into());
        }
        let rec = &data[sp + 1..i + len - 1];
        let eq = rec.iter().position(|&b| b == b'=').ok_or("tar: pax record without =")?;
        out.push((String::from_utf8_lossy(&rec[..eq]).into_owned(), rec[eq + 1..].to_vec()));
        i += len;
    }
    Ok(out)
}

impl<R: Read> Tar<R> {
    pub fn new(r: R) -> Tar<R> {
        Tar { r, specials: false, left: 0, pad: 0, done: false }
    }

    /// Device files and FIFOs come as `Kind::Special` instead of refusing the archive (a package: PK-6d).
    pub fn keep_specials(mut self) -> Tar<R> {
        self.specials = true;
        self
    }

    fn fill(&mut self, buf: &mut [u8]) -> Result<bool, String> {
        let mut got = 0;
        while got < buf.len() {
            match self.r.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("tar: {e}")),
            }
        }
        if got == 0 {
            return Ok(false);
        }
        if got < buf.len() {
            return Err("tar: archive ends inside a block (truncated)".into());
        }
        Ok(true)
    }

    fn skip(&mut self, mut n: u64) -> Result<(), String> {
        let mut buf = [0u8; 8192];
        while n > 0 {
            let k = n.min(buf.len() as u64) as usize;
            if !self.fill(&mut buf[..k])? {
                return Err("tar: archive ends inside an entry (truncated)".into());
            }
            n -= k as u64;
        }
        Ok(())
    }

    /// Skips what is left of the current entry.
    fn finish(&mut self) -> Result<(), String> {
        let n = self.left + self.pad;
        self.left = 0;
        self.pad = 0;
        self.skip(n)
    }

    fn data_of(&mut self, size: u64) -> Result<Vec<u8>, String> {
        if size > META_MAX {
            return Err(format!("tar: extended header of {size} bytes (at most {META_MAX})"));
        }
        let mut v = vec![0u8; size as usize];
        if size > 0 && !self.fill(&mut v)? {
            return Err("tar: archive ends inside an extended header".into());
        }
        let pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
        self.skip(pad)?;
        Ok(v)
    }

    /// The next entry, or None at the end. Its data is read with `read_data`/`data` before the next call, or
    /// skipped by it.
    pub fn next_entry(&mut self) -> Result<Option<Entry>, String> {
        if self.done {
            return Ok(None);
        }
        self.finish()?;
        let (mut long_name, mut long_link): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
        let mut pax_path: Option<Vec<u8>> = None;
        let mut pax_link: Option<Vec<u8>> = None;
        let mut pax_size: Option<u64> = None;
        let (mut pax_uid, mut pax_gid): (Option<u64>, Option<u64>) = (None, None);
        let mut xattrs = Vec::new();
        loop {
            let mut h = [0u8; BLOCK];
            if !self.fill(&mut h)? {
                // libarchive accepts an archive without the two zero blocks; so does pacman.
                self.done = true;
                return if long_name.is_some() || pax_path.is_some() { Err("tar: extended header without its entry".into()) } else { Ok(None) };
            }
            if h.iter().all(|&b| b == 0) {
                self.done = true;
                return Ok(None);
            }
            let want = octal(&h[148..156], "checksum")?;
            let unsigned: u64 = h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { b as u64 }).sum();
            let signed: i64 = h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { b as i8 as i64 }).sum();
            if want != unsigned && want as i64 != signed {
                return Err("tar: header checksum wrong (not a tar archive, or damaged)".into());
            }
            let ty = h[156];
            let hsize = octal(&h[124..136], "size")?;
            let size = if matches!(ty, b'x' | b'g' | b'L' | b'K') { hsize } else { pax_size.take().unwrap_or(hsize) };
            if size > ENTRY_MAX {
                // Bounds the arithmetic (size plus padding) for every caller, not only those under catch_unwind.
                return Err(format!("tar: entry of {size} bytes (at most {ENTRY_MAX})"));
            }
            match ty {
                b'x' => {
                    for (k, v) in pax(&self.data_of(size)?)? {
                        match k.as_str() {
                            "path" => pax_path = Some(v),
                            "linkpath" => pax_link = Some(v),
                            "size" => {
                                let s = std::str::from_utf8(&v).ok().and_then(|t| t.parse().ok()).ok_or("tar: bad pax size")?;
                                pax_size = Some(s);
                            }
                            "uid" => pax_uid = std::str::from_utf8(&v).ok().and_then(|t| t.parse().ok()),
                            "gid" => pax_gid = std::str::from_utf8(&v).ok().and_then(|t| t.parse().ok()),
                            k if k.starts_with("SCHILY.xattr.") => xattrs.push(k["SCHILY.xattr.".len()..].to_string()),
                            _ => {}
                        }
                    }
                    continue;
                }
                // A global header sets defaults for the rest; bsdtar writes none, and none of its keys we use
                // would be safe to apply to every member, so it is read and dropped.
                b'g' => {
                    self.data_of(size)?;
                    continue;
                }
                b'L' => {
                    long_name = Some(cstr(&self.data_of(size)?));
                    continue;
                }
                b'K' => {
                    long_link = Some(cstr(&self.data_of(size)?));
                    continue;
                }
                _ => {}
            }
            let ustar = &h[257..263] == b"ustar\0";
            let mut name = cstr(&h[0..100]);
            if ustar {
                let prefix = cstr(&h[345..500]);
                if !prefix.is_empty() {
                    let mut p = prefix;
                    p.push(b'/');
                    p.extend_from_slice(&name);
                    name = p;
                }
            }
            let raw_name = pax_path.take().or(long_name.take()).unwrap_or(name);
            let raw_link = pax_link.take().or(long_link.take()).unwrap_or_else(|| cstr(&h[157..257]));
            let shown = String::from_utf8_lossy(&raw_name).into_owned();
            let path = clean_path(&raw_name)?;
            let kind = match ty {
                b'0' | 0 | b'7' => Kind::File,
                b'5' => Kind::Dir,
                b'2' => Kind::Symlink(link_target(&raw_link, false)?),
                b'1' => Kind::HardLink(link_target(&raw_link, true)?),
                b'3' | b'4' if self.specials => Kind::Special("Gerätedatei"),
                b'6' if self.specials => Kind::Special("FIFO"),
                b'3' | b'4' => return Err(format!("tar: {shown}: device file refused")),
                b'6' => return Err(format!("tar: {shown}: FIFO refused")),
                b'S' => return Err(format!("tar: {shown}: sparse file refused")),
                t => return Err(format!("tar: {shown}: entry type {:?} refused", t as char)),
            };
            // Old tars mark folders by a trailing '/' on a regular entry.
            let kind = if kind == Kind::File && raw_name.ends_with(b"/") { Kind::Dir } else { kind };
            let data = if kind == Kind::File { size } else { 0 };
            if kind != Kind::File && size > 0 {
                // A folder or link with data: skipped, as libarchive does.
                self.left = size;
                self.pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
                self.finish()?;
            } else {
                self.left = data;
                self.pad = (BLOCK as u64 - data % BLOCK as u64) % BLOCK as u64;
            }
            let mode = octal(&h[100..108], "mode")? as u32 & 0o7777;
            let mtime = octal(&h[136..148], "mtime")? as i64;
            let uid = match pax_uid.take() {
                Some(u) => u,
                None => octal(&h[108..116], "uid")?,
            };
            let gid = match pax_gid.take() {
                Some(g) => g,
                None => octal(&h[116..124], "gid")?,
            };
            return Ok(Some(Entry { path, kind, size: data, mode, mtime, uid, gid, xattrs: std::mem::take(&mut xattrs) }));
        }
    }

    /// Reads up to buf.len() bytes of the current entry's data; 0 at its end.
    pub fn read_data(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        if self.left == 0 {
            return Ok(0);
        }
        let k = (self.left.min(buf.len() as u64)) as usize;
        let mut got = 0;
        while got < k {
            match self.r.read(&mut buf[got..k]) {
                Ok(0) => return Err("tar: archive ends inside an entry (truncated)".into()),
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("tar: {e}")),
            }
        }
        self.left -= k as u64;
        Ok(k)
    }

    pub fn into_inner(self) -> R {
        self.r
    }
}

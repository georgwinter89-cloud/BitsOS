//! The signed manifest of one version (`MANIFEST`, its signature `MANIFEST.SIG`). Plain text, one fact per line,
//! so the desk can read what it signs:
//!
//! ```text
//! bitsos-update 1
//! version 165
//! date 2026-11-16
//! min_previous 164          (optional: an older BitsOS must install this one first)
//! file KERNEL 41234567 <sha256 hex>
//! file INITRD 87654321 <sha256 hex>
//! file BOOTX64.EFI 1234567 <sha256 hex>
//! file WASISTNEU.TXT 812 <sha256 hex>
//! ```
//!
//! Parsing is strict (every line known, each file once, sizes in bounds, KERNEL and INITRD present): the bytes
//! that were signed are the bytes that are read, and nothing in them is guessed.
//!
//! UPD-4: form 2 (`MANIFEST2`, head `bitsos-update 2`) adds the version's parts under the same one signature:
//!
//! ```text
//! bin bitscomp 31234567 <sha256 hex>        every file of \BITSOS2\bin, by name (byte order)
//! tree mesa <hash> <files> <dirs> <bytes> 1     a program tree: its content hash (parts_rules), its size
//! piece mesa 1 123456789 <sha256 hex>           the tree's archives (tar + zstd, each below 2 GiB), in order
//! ```
//!
//! A release keeps form 1 in `MANIFEST` too (KERNEL, INITRD, BOOTX64.EFI, WASISTNEU.TXT), signed on its own, for
//! the BitsOS versions before UPD-4: they install the kernel and keep the medium's own folders.

/// The files a version may carry, the largest each may be.
pub const FILES: [(&str, u64); 4] = [("KERNEL", 1 << 30), ("INITRD", 1 << 30), ("BOOTX64.EFI", 64 << 20), (NOTES, 1 << 20)];
pub const NOTES: &str = "WASISTNEU.TXT";
pub const MANIFEST: &str = "MANIFEST";
pub const SIGNATURE: &str = "MANIFEST.SIG";
pub const MANIFEST2: &str = "MANIFEST2";
pub const SIGNATURE2: &str = "MANIFEST2.SIG";
const HEAD: &str = "bitsos-update 1";
const HEAD2: &str = "bitsos-update 2";
/// The largest manifest of form 1 (a few hundred bytes in practice).
pub const MANIFEST_MAX: usize = 4096;
/// The largest of form 2: a line per file of \BITSOS2\bin and per tree piece (a few KiB in practice).
pub const MANIFEST2_MAX: usize = 4 << 20;
/// GitHub's limit per release asset (`bitsos_install::manifest::MAX_ASSET`): every piece stays below it.
pub const ASSET_MAX: u64 = (2 << 30) - 1;
/// The largest single file of \BITSOS2\bin (it is downloaded as one asset).
pub const BIN_MAX: u64 = ASSET_MAX;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// A program tree of form 2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    pub name: String,
    /// Its content hash (64 hex digits, `parts_rules`).
    pub hash: String,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Its archives: `name` is the asset's (`tree.<name>.<16 hex>.<k>.tar.zst`).
    pub pieces: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub version: u32,
    pub date: String,
    pub min_previous: Option<u32>,
    pub files: Vec<Entry>,
    /// Form 2: each file of \BITSOS2\bin (`name` is the file's).
    pub bins: Vec<Entry>,
    pub trees: Vec<Tree>,
}

/// The asset a file of \BITSOS2\bin comes as.
pub fn bin_asset(name: &str) -> String {
    format!("bin.{name}")
}

/// The asset of piece `k` (from 1) of tree `name`.
pub fn piece_asset(name: &str, hash: &str, k: usize) -> String {
    format!("tree.{name}.{}.{k}.tar.zst", &hash[..16.min(hash.len())])
}

/// A bin file's name: one FAT name that is also an asset name GitHub keeps as it is.
fn bin_name_ok(n: &str) -> bool {
    crate::parts_rules::path_ok(n, true).is_ok() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) && !n.starts_with('.')
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn number(s: &str, max: u64) -> Option<u64> {
    (!s.is_empty() && s.len() <= 12 && s.bytes().all(|b| b.is_ascii_digit()) && !(s.len() > 1 && s.starts_with('0'))).then(|| s.parse().ok()).flatten().filter(|&n| n <= max)
}

fn date_ok(d: &str) -> bool {
    d.len() == 10 && d.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() })
}

impl Manifest {
    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.files.iter().find(|e| e.name == name)
    }

    /// Form 2: the version carries its parts.
    pub fn has_parts(&self) -> bool {
        !self.bins.is_empty() || !self.trees.is_empty()
    }

    /// The text that is signed (one form only: `parse(render(m)) == m` and `render(parse(t)) == t`).
    pub fn render(&self) -> String {
        let head = if self.has_parts() { HEAD2 } else { HEAD };
        let mut t = format!("{head}\nversion {}\ndate {}\n", self.version, self.date);
        if let Some(p) = self.min_previous {
            t.push_str(&format!("min_previous {p}\n"));
        }
        for e in &self.files {
            t.push_str(&format!("file {} {} {}\n", e.name, e.size, hex(&e.sha256)));
        }
        for e in &self.bins {
            t.push_str(&format!("bin {} {} {}\n", e.name, e.size, hex(&e.sha256)));
        }
        for tr in &self.trees {
            t.push_str(&format!("tree {} {} {} {} {} {}\n", tr.name, tr.hash, tr.files, tr.dirs, tr.bytes, tr.pieces.len()));
            for (k, p) in tr.pieces.iter().enumerate() {
                t.push_str(&format!("piece {} {} {} {}\n", tr.name, k + 1, p.size, hex(&p.sha256)));
            }
        }
        t
    }

    pub fn parse(b: &[u8]) -> Result<Manifest, String> {
        let form2 = b.starts_with(HEAD2.as_bytes());
        let max = if form2 { MANIFEST2_MAX } else { MANIFEST_MAX };
        if b.len() > max {
            return Err(format!("the manifest has {} bytes, more than {max}", b.len()));
        }
        let t = std::str::from_utf8(b).map_err(|_| String::from("the manifest is no UTF-8 text"))?;
        if !t.ends_with('\n') || t.contains('\r') {
            return Err(String::from("the manifest's lines end in a single newline each"));
        }
        let mut lines = t.lines();
        let head = lines.next();
        if head != Some(HEAD) && head != Some(HEAD2) {
            return Err(format!("the manifest does not start with \"{HEAD}\" or \"{HEAD2}\""));
        }
        let (mut version, mut date, mut min_previous, mut files) = (None, None, None, Vec::<Entry>::new());
        let (mut bins, mut trees) = (Vec::<Entry>::new(), Vec::<Tree>::new());
        let mut pending_pieces = 0u64;
        for l in lines {
            let w: Vec<&str> = l.split(' ').collect();
            if pending_pieces > 0 && w.first() != Some(&"piece") {
                return Err(String::from("a tree has fewer pieces than announced"));
            }
            match w.as_slice() {
                ["version", v] if version.is_none() => version = Some(number(v, 99_999_999).filter(|&n| n > 0).ok_or("bad version line")? as u32),
                ["date", d] if date.is_none() && date_ok(d) => date = Some(d.to_string()),
                ["min_previous", v] if min_previous.is_none() => min_previous = Some(number(v, 99_999_999).ok_or("bad min_previous line")? as u32),
                ["file", name, size, sha] if bins.is_empty() && trees.is_empty() => {
                    let Some(&(_, max)) = FILES.iter().find(|(n, _)| n == name) else { return Err(format!("unknown file {name}")) };
                    if files.iter().any(|e| e.name == *name) {
                        return Err(format!("{name} named twice"));
                    }
                    let size = number(size, max).ok_or_else(|| format!("{name}: bad size (at most {max} bytes)"))?;
                    let sha256 = unhex32(sha).ok_or_else(|| format!("{name}: bad SHA-256"))?;
                    files.push(Entry { name: name.to_string(), size, sha256 });
                }
                ["bin", name, size, sha] if form2 && trees.is_empty() => {
                    if !bin_name_ok(name) {
                        return Err(format!("bin {name:?}: not a plain file name"));
                    }
                    if bins.last().is_some_and(|e| e.name.as_str() >= *name) || bins.iter().any(|e| e.name.eq_ignore_ascii_case(name)) {
                        return Err(format!("bin {name}: out of order or named twice"));
                    }
                    let size = number(size, BIN_MAX).ok_or_else(|| format!("bin {name}: bad size"))?;
                    let sha256 = unhex32(sha).ok_or_else(|| format!("bin {name}: bad SHA-256"))?;
                    bins.push(Entry { name: name.to_string(), size, sha256 });
                }
                ["tree", name, hash, f, d, by, n] if form2 => {
                    crate::parts_rules::name_ok(name)?;
                    if *name == crate::parts_rules::BIN || !crate::parts_rules::hex_ok(hash) {
                        return Err(format!("tree {name}: a tree's own name and a 64-digit hash"));
                    }
                    if trees.last().is_some_and(|last| last.name.as_str() >= *name) {
                        return Err(format!("tree {name}: out of order or named twice"));
                    }
                    let (Some(files), Some(dirs), Some(bytes), Some(pieces)) = (number(f, u64::MAX), number(d, u64::MAX), number(by, u64::MAX), number(n, u64::MAX)) else {
                        return Err(format!("tree {name}: bad counts"));
                    };
                    if pieces == 0 || files == 0 {
                        return Err(format!("tree {name}: no files or no piece"));
                    }
                    // `pieces` is checked against the piece lines that follow (bounded by the manifest's size).
                    trees.push(Tree { name: name.to_string(), hash: hash.to_string(), files, dirs, bytes, pieces: Vec::with_capacity(pieces.min(64) as usize) });
                    pending_pieces = pieces;
                }
                ["piece", name, k, size, sha] if form2 => {
                    let Some(tr) = trees.last_mut().filter(|t| t.name == *name) else { return Err(format!("piece {name}: not right after its tree")) };
                    if pending_pieces == 0 || number(k, u64::MAX) != Some(tr.pieces.len() as u64 + 1) {
                        return Err(format!("piece {name} {k}: out of order or more than announced"));
                    }
                    let size = number(size, ASSET_MAX).filter(|&s| s > 0).ok_or_else(|| format!("piece {name} {k}: bad size (at most {ASSET_MAX})"))?;
                    let sha256 = unhex32(sha).ok_or_else(|| format!("piece {name} {k}: bad SHA-256"))?;
                    let asset = piece_asset(name, &tr.hash, tr.pieces.len() + 1);
                    tr.pieces.push(Entry { name: asset, size, sha256 });
                    pending_pieces -= 1;
                }
                _ => return Err(format!("unknown or repeated line {l:?}")),
            }
        }
        if pending_pieces > 0 {
            return Err(String::from("a tree has fewer pieces than announced"));
        }
        let m = Manifest {
            version: version.ok_or("no version line")?,
            date: date.ok_or("no date line")?,
            min_previous,
            files,
            bins,
            trees,
        };
        if form2 && m.bins.is_empty() {
            return Err(String::from("form 2 without the files of \\BITSOS2\\bin"));
        }
        for need in ["KERNEL", "INITRD"] {
            if m.entry(need).is_none() {
                return Err(format!("no {need} in the manifest"));
            }
        }
        if m.render().as_bytes() != b {
            return Err(String::from("the manifest is not in its one written form"));
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample() -> Manifest {
        Manifest {
            version: 165,
            date: "2026-11-16".into(),
            min_previous: Some(164),
            files: vec![
                Entry { name: "KERNEL".into(), size: 1000, sha256: [0xab; 32] },
                Entry { name: "INITRD".into(), size: 2000, sha256: [2; 32] },
                Entry { name: NOTES.into(), size: 30, sha256: [3; 32] },
            ],
            bins: vec![],
            trees: vec![],
        }
    }

    #[test]
    fn round_trip_and_one_form() {
        let m = sample();
        let t = m.render();
        assert_eq!(Manifest::parse(t.as_bytes()), Ok(m.clone()));
        assert!(t.starts_with("bitsos-update 1\nversion 165\ndate 2026-11-16\nmin_previous 164\nfile KERNEL 1000 abab"));
        let no_min = Manifest { min_previous: None, ..m };
        assert_eq!(Manifest::parse(no_min.render().as_bytes()), Ok(no_min));
    }

    #[test]
    fn everything_else_is_refused() {
        let t = sample().render();
        let bad = [
            t.replace("bitsos-update 1", "bitsos-update 2"),
            t.replace("version 165", "version 0"),
            t.replace("version 165", "version 0165"),
            t.replace("version 165", "version 165\nversion 166"),
            t.replace("date 2026-11-16", "date 16.11.2026"),
            t.replace("KERNEL 1000", "KERNEL 1e3"),
            t.replace("KERNEL 1000", "KERNEL 99999999999"),
            t.replace("file KERNEL", "file VMLINUZ"),
            t.replace("file INITRD 2000", "file KERNEL 2000"),
            t.replace(&format!("file INITRD 2000 {}\n", hex(&[2; 32])), ""),
            t.replace(&hex(&[0xab; 32]), &hex(&[0xab; 32]).to_uppercase()),
            t.replace('\n', "\r\n"),
            t.replace("file ", "file  "),
            format!("{t}extra line\n"),
            t.trim_end().to_string(),
        ];
        for b in bad {
            assert!(Manifest::parse(b.as_bytes()).is_err(), "{b:?}");
        }
        assert!(Manifest::parse(&vec![b'a'; MANIFEST_MAX + 1]).is_err());
        assert!(Manifest::parse(b"\xff\xfe").is_err());
    }
}

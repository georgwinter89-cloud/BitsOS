//! FS-13: the preload record, `/data/system/preload/record`: per program (executable path) a use score and the
//! page ranges of the files it read in its first seconds. Small and versioned, with a CRC-32 over it: a file
//! that is cut, damaged or of another version is ignored as a whole (an empty record; never a panic). It stays
//! on the device and is never sent anywhere.
//!
//! Scores: a start adds `ONE`; every boot multiplies all scores by 3/4, and a program below `FADE` (about seven
//! boots without a start after a single one) is dropped, so old habits fade. The record's size follows the
//! preload budget (`bytes_for`): programs are written most-used first until it is full.
//!
//! Every file carries its identity when recorded (size and modification time): a file that changed since
//! (a program update) is not preloaded with its old ranges, and its next recorded start replaces them.

use super::ranges::Ranges;
use alloc::vec::Vec;

/// One start.
pub const ONE: u32 = 1024;
/// Below this a program is forgotten.
pub const FADE: u32 = ONE / 8;
pub const MAGIC: &[u8; 4] = b"BPRL";
/// 2: file identity (size, mtime) per file.
pub const VERSION: u16 = 2;
/// The largest stored record taken, whatever `preload.max_mb` is now (a lowered budget must not make the
/// learned record unreadable): `bytes_for` of 16 GiB.
pub const LOAD_MAX_BYTES: usize = 16 << 20;

/// What a file was when it was recorded: its size and modification time (FAT and ext4 keep both on their
/// disk, so they are the same after a reboot unless the file was written).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ident {
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRec {
    pub path: Vec<u8>,
    pub id: Ident,
    pub ranges: Ranges,
}

impl FileRec {
    /// FS-15: the pages to preload of this file now that its identity is `now` (it changed since the start was
    /// recorded). A file the start read whole (its ranges are one run over every page it had) is read whole again
    /// at its new size: a program rewrites such files itself (Brave's Preferences and Local State on /data at its
    /// end), so they were stale at every boot and each start read them from the device. Bounded by twice the
    /// recorded size plus 1 MiB (a file that grew far is not the one recorded). None: skip it, it is stale.
    pub fn whole_again(&self, now: &Ident) -> Option<(u64, u64)> {
        let had = self.id.size.div_ceil(4096);
        let has = now.size.div_ceil(4096);
        let whole = had > 0 && matches!(self.ranges.as_slice(), [(0, e)] if *e >= had);
        (whole && has > 0 && has <= had.saturating_mul(2).saturating_add(256)).then_some((0, has))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub exe: Vec<u8>,
    pub score: u32,
    /// In the order the program first read them.
    pub files: Vec<FileRec>,
}

impl Program {
    pub fn pages(&self) -> u64 {
        self.files.iter().fold(0u64, |n, f| n.saturating_add(f.ranges.pages()))
    }

    /// Keeps the first `max` pages (files in reading order, each from its start); empty files go.
    fn cut_to(&mut self, max: u64) {
        let mut left = max;
        for f in self.files.iter_mut() {
            let n = f.ranges.pages();
            if n <= left {
                left -= n;
            } else {
                f.ranges.cut_to(left);
                left = 0;
            }
        }
        self.files.retain(|f| !f.ranges.is_empty());
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// Boots this record has seen (each one decayed it once).
    pub boots: u32,
    pub programs: Vec<Program>,
}

/// Why a stored record was not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    Short,
    Magic,
    Version(u16),
    Checksum,
    Length,
    NoMemory,
}

impl Bad {
    pub fn as_str(self) -> &'static str {
        match self {
            Bad::Short => "short",
            Bad::Magic => "magic",
            Bad::Version(_) => "version",
            Bad::Checksum => "checksum",
            Bad::Length => "length",
            Bad::NoMemory => "no_memory",
        }
    }
}

/// A file's identity in the stored form: size u64, mtime seconds i64, nanoseconds u32.
const IDENT_BYTES: usize = 20;

/// The record's largest encoded size for a record cap of `max_mb` (`vfs::preload::record_mb`): 1 KiB per MB (12
/// bytes per range: a range per 340 KiB read), at least 64 KiB, at most `LOAD_MAX_BYTES` (FS-13b: a cap of the
/// whole RAM must not write a record the next boot refuses to load).
pub fn bytes_for(max_mb: u64) -> usize {
    (max_mb.saturating_mul(1024)).clamp(64 * 1024, LOAD_MAX_BYTES as u64) as usize
}

impl Record {
    pub const fn new() -> Record {
        Record { boots: 0, programs: Vec::new() }
    }

    pub fn pages(&self) -> u64 {
        self.programs.iter().fold(0u64, |n, p| n.saturating_add(p.pages()))
    }

    /// Forgets the recorded ranges of `path` in every program (the file changed or is gone).
    pub fn forget_file(&mut self, path: &[u8]) {
        for p in self.programs.iter_mut() {
            p.files.retain(|f| f.path != path);
        }
    }

    pub fn clear(&mut self) {
        self.programs.clear();
    }

    /// One boot: every score times 3/4, programs below `FADE` forgotten.
    pub fn decay(&mut self) {
        self.boots = self.boots.saturating_add(1);
        for p in self.programs.iter_mut() {
            p.score = (p.score as u64 * 3 / 4) as u32;
        }
        self.programs.retain(|p| p.score >= FADE);
    }

    /// A start of `exe` that read `files` (path, page ranges) in its first seconds: the score grows by `ONE`, the
    /// ranges join the program's (new files after the known ones; a file whose identity changed has its old
    /// ranges replaced), at most `max_pages` pages per program. False when memory ran out (what was merged
    /// stays).
    pub fn add_start(&mut self, exe: &[u8], files: Vec<FileRec>, max_pages: u64) -> bool {
        let i = match self.programs.iter().position(|p| p.exe == exe) {
            Some(i) => i,
            None => {
                let mut name = Vec::new();
                if self.programs.try_reserve(1).is_err() || name.try_reserve_exact(exe.len()).is_err() {
                    return false;
                }
                name.extend_from_slice(exe);
                self.programs.push(Program { exe: name, score: 0, files: Vec::new() });
                self.programs.len() - 1
            }
        };
        let p = &mut self.programs[i];
        p.score = p.score.saturating_add(ONE);
        let mut ok = true;
        for f in files {
            match p.files.iter().position(|x| x.path == f.path) {
                Some(k) if p.files[k].id == f.id => ok &= p.files[k].ranges.add_all(&f.ranges),
                Some(k) => p.files[k] = f,
                None if p.files.try_reserve(1).is_ok() => p.files.push(f),
                None => ok = false,
            }
        }
        p.cut_to(max_pages);
        ok
    }

    /// The programs most-used first (ties by name, so the order is stable).
    pub fn by_use(&self) -> Vec<&Program> {
        let mut v: Vec<&Program> = Vec::new();
        if v.try_reserve_exact(self.programs.len()).is_err() {
            return v;
        }
        v.extend(self.programs.iter());
        // In place, no allocation (a stable sort would allocate).
        v.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.exe.cmp(&b.exe)));
        v
    }

    /// The stored form, at most `max_bytes` long: programs most-used first while they fit (a program without
    /// pages has nothing to preload and is left out). None: no memory for the buffer.
    pub fn encode(&self, max_bytes: usize) -> Option<Vec<u8>> {
        self.encode_kept(max_bytes).map(|(b, _)| b)
    }

    /// The programs `encode` writes: those with pages and a path that fits (the rest are never stored).
    pub fn storable(&self) -> usize {
        self.programs.iter().filter(|p| p.pages() > 0 && p.exe.len() <= u16::MAX as usize).count()
    }

    /// `encode`, and how many programs it kept: below `storable` when `max_bytes` cut the least used ones.
    pub fn encode_kept(&self, max_bytes: usize) -> Option<(Vec<u8>, usize)> {
        let mut out: Vec<u8> = Vec::new();
        out.try_reserve(max_bytes.min(4096)).ok()?;
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.boots.to_le_bytes());
        let count_at = out.len();
        out.extend_from_slice(&0u32.to_le_bytes());
        let mut n = 0u32;
        for p in self.by_use() {
            if p.pages() == 0 || p.exe.len() > u16::MAX as usize {
                continue;
            }
            let size = 4 + 2 + p.exe.len() + 4 + p.files.iter().filter(|f| f.path.len() <= u16::MAX as usize).map(|f| 2 + f.path.len() + IDENT_BYTES + 4 + 12 * f.ranges.as_slice().len()).sum::<usize>();
            if out.len() + size + 4 > max_bytes {
                break;
            }
            out.try_reserve(size).ok()?;
            out.extend_from_slice(&p.score.to_le_bytes());
            out.extend_from_slice(&(p.exe.len() as u16).to_le_bytes());
            out.extend_from_slice(&p.exe);
            let nf = p.files.iter().filter(|f| f.path.len() <= u16::MAX as usize).count();
            out.extend_from_slice(&(nf as u32).to_le_bytes());
            for f in p.files.iter().filter(|f| f.path.len() <= u16::MAX as usize) {
                out.extend_from_slice(&(f.path.len() as u16).to_le_bytes());
                out.extend_from_slice(&f.path);
                out.extend_from_slice(&f.id.size.to_le_bytes());
                out.extend_from_slice(&f.id.mtime_sec.to_le_bytes());
                out.extend_from_slice(&f.id.mtime_nsec.to_le_bytes());
                out.extend_from_slice(&(f.ranges.as_slice().len() as u32).to_le_bytes());
                for &(a, b) in f.ranges.as_slice() {
                    out.extend_from_slice(&a.to_le_bytes());
                    out.extend_from_slice(&((b - a).min(u32::MAX as u64) as u32).to_le_bytes());
                }
            }
            n += 1;
        }
        out[count_at..count_at + 4].copy_from_slice(&n.to_le_bytes());
        out.try_reserve(4).ok()?;
        let c = crc32(&out);
        out.extend_from_slice(&c.to_le_bytes());
        Some((out, n as usize))
    }

    /// A stored record; any damage is an Err (the caller starts with an empty record).
    pub fn decode(b: &[u8]) -> Result<Record, Bad> {
        if b.len() < 16 + 4 {
            return Err(Bad::Short);
        }
        if &b[..4] != MAGIC {
            return Err(Bad::Magic);
        }
        let (body, tail) = b.split_at(b.len() - 4);
        let mut r = Reader { b: body, at: 4 };
        let version = r.u16()?;
        if version != VERSION {
            return Err(Bad::Version(version));
        }
        if crc32(body) != u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) {
            return Err(Bad::Checksum);
        }
        let _flags = r.u16()?;
        let boots = r.u32()?;
        let n = r.u32()? as usize;
        let mut rec = Record { boots, programs: Vec::new() };
        // Every program takes at least 10 bytes: a count past what is left is damage, not an allocation.
        rec.programs.try_reserve(n.min(r.left() / 10)).map_err(|_| Bad::NoMemory)?;
        for _ in 0..n {
            let score = r.u32()?;
            let exe = r.bytes16()?;
            let nf = r.u32()? as usize;
            let mut files = Vec::new();
            files.try_reserve(nf.min(r.left() / (6 + IDENT_BYTES))).map_err(|_| Bad::NoMemory)?;
            for _ in 0..nf {
                let path = r.bytes16()?;
                let id = Ident { size: r.u64()?, mtime_sec: r.u64()? as i64, mtime_nsec: r.u32()? };
                let nr = r.u32()? as usize;
                if nr > r.left() / 12 {
                    return Err(Bad::Length);
                }
                let mut ranges = Ranges::new();
                for _ in 0..nr {
                    let a = r.u64()?;
                    let len = r.u32()? as u64;
                    if !ranges.add(a, a.saturating_add(len)) {
                        return Err(Bad::NoMemory);
                    }
                }
                // Sanitised, not refused: a path the preload could not look up from / (relative, with a NUL)
                // or a file without pages is left out; overlapping or unsorted ranges were merged above.
                if !ranges.is_empty() && path.first() == Some(&b'/') && !path.contains(&0) && files.len() < files.capacity() {
                    files.push(FileRec { path, id, ranges });
                }
            }
            if rec.programs.len() < rec.programs.capacity() {
                rec.programs.push(Program { exe, score, files });
            }
        }
        if r.left() != 0 {
            return Err(Bad::Length);
        }
        Ok(rec)
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn left(&self) -> usize {
        self.b.len() - self.at
    }
    fn take(&mut self, n: usize) -> Result<&[u8], Bad> {
        if self.left() < n {
            return Err(Bad::Length);
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, Bad> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32, Bad> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64, Bad> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }
    fn bytes16(&mut self) -> Result<Vec<u8>, Bad> {
        let n = self.u16()? as usize;
        let s = self.take(n)?;
        let mut v = Vec::new();
        v.try_reserve_exact(n).map_err(|_| Bad::NoMemory)?;
        v.extend_from_slice(s);
        Ok(v)
    }
}

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC: [u32; 256] = crc_table();

/// CRC-32 (IEEE 802.3, as zlib's `crc32`).
pub fn crc32(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c = CRC[((c ^ x as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

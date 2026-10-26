//! The journal on disk: segment files `j-<start unix s, 11 digits>.bj` in `/data/system/journal/`, each a row of
//! blocks. A block = a 40-byte head (`BJ1\n`, raw length, compressed length, lines, boot id, first and last wall
//! time in ms, little endian) + the lines of one boot as zlib (its Adler-32 tells a damaged block). Blocks are
//! appended and made durable with one fsync per block (every few seconds, never per event: the jbd2 commit and
//! the snapshot freeze stay cheap, desk-INT-W12.md). A torn tail (power cut while appending) is cut off when the
//! segment is opened again, and skipped by readers until then. Retention deletes whole segments, oldest first.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: [u8; 4] = *b"BJ1\n";
pub const HEAD: usize = 40;
/// Largest raw block (a block of more is refused when read: a damaged length).
pub const RAW_MAX: usize = 16 << 20;
/// The writer never makes a block above this many raw bytes (well below `RAW_MAX`, so its own reader takes it).
pub const CHUNK_RAW: usize = 1 << 20;
/// 2020-01-01 UTC: a time below it comes from a boot without a wall clock (seconds since boot, not since 1970).
pub const SANE_S: u64 = 1_577_836_800;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockHead {
    pub raw_len: u32,
    pub comp_len: u32,
    pub lines: u32,
    pub boot: u64,
    pub first_ms: u64,
    pub last_ms: u64,
}

impl BlockHead {
    pub fn encode(&self) -> [u8; HEAD] {
        let mut h = [0u8; HEAD];
        h[..4].copy_from_slice(&MAGIC);
        h[4..8].copy_from_slice(&self.raw_len.to_le_bytes());
        h[8..12].copy_from_slice(&self.comp_len.to_le_bytes());
        h[12..16].copy_from_slice(&self.lines.to_le_bytes());
        h[16..24].copy_from_slice(&self.boot.to_le_bytes());
        h[24..32].copy_from_slice(&self.first_ms.to_le_bytes());
        h[32..40].copy_from_slice(&self.last_ms.to_le_bytes());
        h
    }

    pub fn decode(h: &[u8]) -> Option<BlockHead> {
        if h.len() < HEAD || h[..4] != MAGIC {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap_or_default());
        let u64_at = |i: usize| u64::from_le_bytes(h[i..i + 8].try_into().unwrap_or_default());
        let b = BlockHead { raw_len: u32_at(4), comp_len: u32_at(8), lines: u32_at(12), boot: u64_at(16), first_ms: u64_at(24), last_ms: u64_at(32) };
        (b.raw_len as usize <= RAW_MAX && b.comp_len as usize <= RAW_MAX + RAW_MAX / 8 + 64).then_some(b)
    }

    /// Whether the block may hold lines stamped `since..=until` (ms). A block without wall times (no wall
    /// clock on that boot) always may.
    pub fn overlaps(&self, since: u64, until: u64) -> bool {
        self.first_ms == 0 || (self.first_ms <= until && self.last_ms >= since)
    }
}

/// The wall time (ms) a stored line carries, if any.
pub fn wall_of(line: &str) -> Option<u64> {
    match crate::line::parse(line)? {
        crate::line::Item::Event { wall, .. } | crate::line::Item::Trace { wall, .. } | crate::line::Item::Metric { wall, .. } | crate::line::Item::Boot { wall, .. } => wall,
        _ => None,
    }
}

/// One block's bytes (head + zlib) of `lines` of boot `boot`.
pub fn encode_block(boot: u64, lines: &[String]) -> Vec<u8> {
    let mut raw = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    let (mut first, mut last) = (u64::MAX, 0u64);
    for l in lines {
        raw.push_str(l);
        raw.push('\n');
        if let Some(w) = wall_of(l) {
            first = first.min(w);
            last = last.max(w);
        }
    }
    let comp = miniz_oxide::deflate::compress_to_vec_zlib(raw.as_bytes(), 6);
    let head = BlockHead { raw_len: raw.len() as u32, comp_len: comp.len() as u32, lines: lines.len() as u32, boot, first_ms: if first == u64::MAX { 0 } else { first }, last_ms: last };
    let mut out = Vec::with_capacity(HEAD + comp.len());
    out.extend_from_slice(&head.encode());
    out.extend_from_slice(&comp);
    out
}

/// The block's text, None when it is damaged.
pub fn decode_body(h: &BlockHead, comp: &[u8]) -> Option<String> {
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(comp, h.raw_len as usize).ok()?;
    if raw.len() != h.raw_len as usize {
        return None;
    }
    String::from_utf8(raw).ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub path: PathBuf,
    pub start_s: u64,
    pub bytes: u64,
    /// The last append (the file's mtime, UTC seconds; 0 unknown).
    pub mtime_s: u64,
}

pub fn segment_name(start_s: u64) -> String {
    format!("j-{start_s:011}.bj")
}

fn start_of(name: &str) -> Option<u64> {
    let d = name.strip_prefix("j-")?.strip_suffix(".bj")?;
    (d.len() == 11 && d.bytes().all(|b| b.is_ascii_digit())).then(|| d.parse().ok()).flatten()
}

/// The segments of `dir`, oldest first.
pub fn segments(dir: &Path) -> io::Result<Vec<Segment>> {
    let mut v = Vec::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name();
        let Some(start_s) = name.to_str().and_then(start_of) else { continue };
        let m = e.metadata().ok();
        let bytes = m.as_ref().map_or(0, |m| m.len());
        let mtime_s = m.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        v.push(Segment { path: e.path(), start_s, bytes, mtime_s });
    }
    v.sort_by_key(|s| s.start_s);
    Ok(v)
}

/// Walks the blocks of a segment: `f(head, offset of the head)`; stops at the end or a torn or bad block and
/// returns the length of the good part.
pub fn walk(path: &Path, mut f: impl FnMut(&BlockHead, u64, &mut File) -> io::Result<bool>) -> io::Result<u64> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut at = 0u64;
    let mut h = [0u8; HEAD];
    while at + HEAD as u64 <= len {
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut h)?;
        let Some(head) = BlockHead::decode(&h) else { break };
        let end = at + HEAD as u64 + head.comp_len as u64;
        if end > len {
            break;
        }
        if !f(&head, at, &mut file)? {
            return Ok(end);
        }
        at = end;
    }
    Ok(at)
}

/// Reads the body of the block whose head is at `at`.
pub fn read_body(file: &mut File, head: &BlockHead, at: u64) -> io::Result<Option<String>> {
    let mut comp = vec![0u8; head.comp_len as usize];
    file.seek(SeekFrom::Start(at + HEAD as u64))?;
    file.read_exact(&mut comp)?;
    Ok(decode_body(head, &comp))
}

/// Cuts a torn or damaged tail off a segment (after a power cut): returns the bytes cut.
pub fn repair(path: &Path) -> io::Result<u64> {
    let len = fs::metadata(path)?.len();
    let mut last: Option<(BlockHead, u64)> = None;
    let mut good = walk(path, |h, at, _| {
        last = Some((*h, at));
        Ok(true)
    })?;
    // The last whole block may still be damaged (pages written out of order): check its body.
    if let Some((h, at)) = last {
        let mut f = File::open(path)?;
        if read_body(&mut f, &h, at)?.is_none() {
            good = at;
        }
    }
    if good < len {
        OpenOptions::new().write(true).open(path)?.set_len(good)?;
        return Ok(len - good);
    }
    Ok(0)
}

/// The segment being appended to.
pub struct Writer {
    dir: PathBuf,
    file: Option<(File, Segment)>,
    /// Rotate after this many bytes.
    pub seg_max: u64,
    pub fsyncs: u64,
    pub written: u64,
}

impl Writer {
    pub fn new(dir: &Path, seg_max: u64) -> Writer {
        Writer { dir: dir.to_path_buf(), file: None, seg_max, fsyncs: 0, written: 0 }
    }

    /// Appends one block and makes it durable. A new segment when there is none, the current one is full, or
    /// the UTC day changed (`now_s`); true when a new segment was begun (time for the retention rules).
    pub fn append(&mut self, block: &[u8], now_s: u64) -> io::Result<bool> {
        let rotate = match &self.file {
            None => true,
            Some((_, s)) => s.bytes >= self.seg_max || s.start_s / 86_400 != now_s / 86_400,
        };
        if rotate {
            self.open_new(now_s)?;
        }
        let Some((f, s)) = self.file.as_mut() else { return Err(io::Error::other("no segment")) };
        if let Err(e) = f.write_all(block).and_then(|_| f.sync_data()) {
            // ENOSPC, EIO, a jbd2 abort: cut what part of the block got in, so the retry does not append behind a
            // torn block (readers stop there). If even that fails, the segment is opened again (and repaired)
            // before the next append.
            if f.set_len(s.bytes).is_err() {
                self.file = None;
            }
            return Err(e);
        }
        s.bytes += block.len() as u64;
        self.fsyncs += 1;
        self.written += block.len() as u64;
        Ok(rotate)
    }

    /// Continues the newest segment of today (after a restart) or starts one.
    fn open_new(&mut self, now_s: u64) -> io::Result<()> {
        let segs = segments(&self.dir)?;
        if self.file.is_none()
            && let Some(s) = segs.last()
            && s.start_s / 86_400 == now_s / 86_400
            && s.bytes < self.seg_max
        {
            repair(&s.path)?;
            let f = OpenOptions::new().append(true).open(&s.path)?;
            let bytes = f.metadata()?.len();
            self.file = Some((f, Segment { bytes, ..s.clone() }));
            return Ok(());
        }
        let mut start = now_s;
        while segs.iter().any(|s| s.start_s == start) || self.dir.join(segment_name(start)).exists() {
            start += 1;
        }
        let path = self.dir.join(segment_name(start));
        let f = OpenOptions::new().create_new(true).append(true).open(&path)?;
        self.file = Some((f, Segment { path, start_s: start, bytes: 0, mtime_s: now_s }));
        Ok(())
    }

    pub fn current(&self) -> Option<&Segment> {
        self.file.as_ref().map(|(_, s)| s)
    }
}

/// Retention: segments last written more than `keep_s` before `now_s` go (their mtime; without one, where the
/// next begins), then the oldest until the folder is at most `max_bytes`; the newest is never deleted. A segment
/// of a boot without a wall clock (its times below `SANE_S`) has no known age: the age rule leaves it, only the
/// size rule may take it (oldest first, as it sorts first). Returns what was deleted.
pub fn retain(dir: &Path, now_s: u64, keep_s: u64, max_bytes: u64) -> io::Result<Vec<Segment>> {
    let segs = segments(dir)?;
    let mut gone = Vec::new();
    let mut total: u64 = segs.iter().map(|s| s.bytes).sum();
    let n = segs.len();
    for (i, s) in segs.iter().enumerate() {
        if i + 1 >= n {
            break;
        }
        let ended = if s.mtime_s != 0 { s.mtime_s } else { segs[i + 1].start_s };
        let too_old = ended >= SANE_S && now_s >= SANE_S && ended.saturating_add(keep_s) < now_s;
        if too_old || total > max_bytes {
            fs::remove_file(&s.path)?;
            total -= s.bytes;
            gone.push(s.clone());
        }
    }
    Ok(gone)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitsjournal-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn lines(n: usize, wall0: u64) -> Vec<String> {
        (0..n).map(|i| format!("E {i} {} {} I 0 1 test t.line i={i}", i * 1000, wall0 + i as u64)).collect()
    }

    #[test]
    fn blocks_round_trip_and_a_torn_tail_is_cut() {
        let d = tmp("blocks");
        let mut w = Writer::new(&d, 1 << 20);
        const DAY: u64 = 1_792_713_600;
        w.append(&encode_block(7, &lines(50, DAY * 1000)), DAY).unwrap();
        w.append(&encode_block(7, &lines(10, DAY * 1000 + 500)), DAY + 5).unwrap();
        let seg = w.current().unwrap().clone();
        let mut heads = Vec::new();
        walk(&seg.path, |h, at, f| {
            heads.push((*h, read_body(f, h, at).unwrap().unwrap()));
            Ok(true)
        })
        .unwrap();
        assert_eq!(heads.len(), 2);
        assert_eq!((heads[0].0.lines, heads[0].0.boot, heads[0].0.first_ms, heads[0].0.last_ms), (50, 7, DAY * 1000, DAY * 1000 + 49));
        assert!(heads[1].1.starts_with("E 0 0 ") && heads[1].1.ends_with("i=9\n"));
        assert!(heads[0].0.overlaps(DAY * 1000 + 10, DAY * 1000 + 20) && !heads[0].0.overlaps(0, DAY * 1000 - 1));
        // A power cut in the middle of the third block: readers stop before it, the next append cuts it.
        let good = fs::metadata(&seg.path).unwrap().len();
        let third = encode_block(7, &lines(5, DAY * 1000 + 900));
        OpenOptions::new().append(true).open(&seg.path).unwrap().write_all(&third[..third.len() / 2]).unwrap();
        assert_eq!(walk(&seg.path, |_, _, _| Ok(true)).unwrap(), good);
        // A whole block with a damaged body is cut too.
        let mut bad = encode_block(7, &lines(5, DAY * 1000 + 900));
        let n = bad.len();
        bad[n - 3] ^= 0xff;
        fs::OpenOptions::new().write(true).open(&seg.path).unwrap().set_len(good).unwrap();
        OpenOptions::new().append(true).open(&seg.path).unwrap().write_all(&bad).unwrap();
        assert_eq!(repair(&seg.path).unwrap(), bad.len() as u64);
        assert_eq!(fs::metadata(&seg.path).unwrap().len(), good);
        // After a restart the same day's segment is continued; the next day starts a new one.
        let mut w2 = Writer::new(&d, 1 << 20);
        w2.append(&encode_block(8, &lines(3, DAY * 1000 + 999)), DAY + 60).unwrap();
        assert_eq!(w2.current().unwrap().path, seg.path);
        w2.append(&encode_block(8, &lines(3, (DAY + 86_400) * 1000)), DAY + 86_400).unwrap();
        assert_eq!(segments(&d).unwrap().len(), 2);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_failed_append_leaves_no_torn_block() {
        let d = tmp("append-fail");
        let mut w = Writer::new(&d, 1 << 20);
        const DAY: u64 = 1_792_713_600;
        w.append(&encode_block(7, &lines(5, DAY * 1000)), DAY).unwrap();
        let good = w.current().unwrap().bytes;
        // A read-only handle makes write_all fail, as a full or broken /data does.
        let path = w.current().unwrap().path.clone();
        let ro = File::open(&path).unwrap();
        let (_, seg) = w.file.take().unwrap();
        w.file = Some((ro, seg));
        assert!(w.append(&encode_block(7, &lines(5, DAY * 1000 + 10)), DAY + 1).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), good, "nothing of the failed block stays");
        // set_len on a read-only handle fails too: the writer drops it and reopens (and repairs) next time.
        assert!(w.file.is_none());
        w.append(&encode_block(7, &lines(5, DAY * 1000 + 20)), DAY + 2).unwrap();
        assert_eq!(w.current().unwrap().path, path, "the same day's segment is continued");
        let mut n = 0;
        walk(&path, |_, _, _| {
            n += 1;
            Ok(true)
        })
        .unwrap();
        assert_eq!(n, 2, "both good blocks are readable");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn damaged_middle_garbage_files_and_hostile_heads() {
        let d = tmp("hostile");
        const DAY: u64 = 1_792_713_600;
        let mut w = Writer::new(&d, 1 << 20);
        for i in 0..3 {
            w.append(&encode_block(7, &lines(5, DAY * 1000 + i * 100)), DAY + i).unwrap();
        }
        let path = w.current().unwrap().path.clone();
        // Flip a byte in the body of the middle block: the walk goes on, only that body is refused.
        let mut bytes = fs::read(&path).unwrap();
        let first = HEAD + BlockHead::decode(&bytes).unwrap().comp_len as usize;
        let mid = BlockHead::decode(&bytes[first..]).unwrap();
        bytes[first + HEAD + mid.comp_len as usize / 2] ^= 0x55;
        fs::write(&path, &bytes).unwrap();
        let (mut good, mut bad) = (0, 0);
        walk(&path, |h, at, f| {
            if read_body(f, h, at)?.is_some() { good += 1 } else { bad += 1 }
            Ok(true)
        })
        .unwrap();
        assert_eq!((good, bad), (2, 1));
        // A head that claims a huge raw length is refused (no allocation by it), and so is a short or foreign one.
        let mut huge = BlockHead { raw_len: 0, comp_len: 10, lines: 1, boot: 1, first_ms: 1, last_ms: 1 }.encode();
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(BlockHead::decode(&huge), None);
        huge[4..8].copy_from_slice(&1u32.to_le_bytes());
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(BlockHead::decode(&huge), None);
        assert_eq!(BlockHead::decode(&huge[..20]), None);
        assert_eq!(BlockHead::decode(b"XXXXaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), None);
        // A body that inflates to more than its head says: refused, not grown.
        let raw = "x".repeat(1 << 16);
        let comp = miniz_oxide::deflate::compress_to_vec_zlib(raw.as_bytes(), 6);
        let lie = BlockHead { raw_len: 100, comp_len: comp.len() as u32, lines: 1, boot: 1, first_ms: 0, last_ms: 0 };
        assert_eq!(decode_body(&lie, &comp), None);
        // Garbage in the folder: foreign names are not segments, a garbage segment reads as empty and is cut.
        fs::write(d.join("j-123.bj"), b"junk").unwrap();
        fs::write(d.join("notes.txt"), b"junk").unwrap();
        let junk = d.join(segment_name(DAY + 86_400 * 3));
        fs::write(&junk, vec![0xa5u8; 5000]).unwrap();
        fs::write(d.join(segment_name(DAY + 86_400 * 4)), b"").unwrap();
        assert_eq!(segments(&d).unwrap().len(), 3);
        assert_eq!(walk(&junk, |_, _, _| Ok(true)).unwrap(), 0);
        assert_eq!(repair(&junk).unwrap(), 5000);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn segments_of_a_boot_without_wall_clock_have_no_age() {
        let d = tmp("clockless");
        const T: u64 = 1_792_713_600;
        // Written on a boot without a wall clock: named and stamped in seconds since boot (1970).
        let p = d.join(segment_name(42));
        fs::write(&p, vec![0u8; 100]).unwrap();
        File::options().write(true).open(&p).unwrap().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(90)).unwrap();
        fs::write(d.join(segment_name(T)), vec![0u8; 100]).unwrap();
        assert!(retain(&d, T + 400 * 86_400, 60 * 86_400, u64::MAX).unwrap().is_empty(), "the age rule leaves it");
        let gone = retain(&d, T, 60 * 86_400, 150).unwrap();
        assert_eq!(gone.iter().map(|s| s.start_s).collect::<Vec<_>>(), [42], "the size rule still bounds the folder");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn retention_by_age_then_by_size() {
        let d = tmp("retain");
        const T: u64 = 1_792_713_600;
        for (i, size) in [(0u64, 100usize), (1, 200), (2, 300), (3, 400)] {
            let p = d.join(segment_name(T + i * 86_400));
            fs::write(&p, vec![0u8; size]).unwrap();
            // Last written at the end of its day.
            let end = std::time::UNIX_EPOCH + std::time::Duration::from_secs(T + (i + 1) * 86_400 - 1);
            File::options().write(true).open(&p).unwrap().set_modified(end).unwrap();
        }
        // Keep 2 days at T + 4 days + 1 s: the first two ended at T+1d and T+2d, more than 2 days ago.
        let gone = retain(&d, T + 4 * 86_400 + 1, 2 * 86_400, u64::MAX).unwrap();
        assert_eq!(gone.iter().map(|s| s.start_s).collect::<Vec<_>>(), [T, T + 86_400]);
        // 600 bytes at most: the oldest left (300) goes; the newest is never deleted even when alone too big.
        let gone = retain(&d, T, u64::MAX / 2, 600).unwrap();
        assert_eq!(gone.iter().map(|s| s.start_s).collect::<Vec<_>>(), [T + 2 * 86_400]);
        assert!(retain(&d, T, u64::MAX / 2, 1).unwrap().is_empty());
        assert_eq!(segments(&d).unwrap().len(), 1);
        fs::remove_dir_all(&d).unwrap();
    }
}

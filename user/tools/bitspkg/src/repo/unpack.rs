//! The compression of a sync database (or package): gzip (Arch's `.db`, through miniz_oxide), zstd (CachyOS's
//! `.db` and every `.pkg.tar.zst`, through ruzstd) or none, found by the magic bytes as libarchive does
//! (pacman/lib/libalpm/util.c _alpm_open_archive uses archive_read_support_filter_all). Several gzip members and
//! several zstd frames in a row are one stream (gzip(1) and zstd(1) both read them so). `Bounded` ends a stream
//! that unpacks to more than a bound (R1: one input's bound, a database at most 256 MiB).

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};
use std::io::{self, BufRead, BufReader, Read};

pub const GZIP: [u8; 2] = [0x1f, 0x8b];
pub const ZSTD: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

fn bad(m: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.into())
}

/// The unpacked bytes of `r`, whatever its compression.
pub fn open<R: Read + 'static>(r: R) -> io::Result<Box<dyn Read>> {
    let mut b = BufReader::with_capacity(64 << 10, r);
    let head = b.fill_buf()?.to_vec();
    if head.starts_with(&GZIP) {
        Ok(Box::new(Gz::new(b)))
    } else if head.starts_with(&ZSTD) {
        Ok(Box::new(Zstd::new(b)))
    } else if head.len() >= 6 && head.starts_with(b"\xfd7zXZ\0") {
        Err(bad("xz-Kompression wird nicht unterstützt (nur gzip, zstd oder keine)"))
    } else if head.starts_with(b"BZh") {
        Err(bad("bzip2-Kompression wird nicht unterstützt (nur gzip, zstd oder keine)"))
    } else {
        Ok(Box::new(b))
    }
}

// ---- gzip (RFC 1952) ----

fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for (i, e) in t.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *e = c;
    }
    t
}

/// CRC-32 (IEEE, gzip's), for tests that build gzip members.
pub fn crc32(data: &[u8]) -> u32 {
    let t = crc_table();
    !data.iter().fold(!0u32, |c, &b| t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

pub struct Gz<R: BufRead> {
    r: R,
    st: Box<InflateState>,
    table: [u32; 256],
    crc: u32,
    size: u32,
    in_member: bool,
    members: u64,
    done: bool,
}

impl<R: BufRead> Gz<R> {
    pub fn new(r: R) -> Gz<R> {
        Gz { r, st: InflateState::new_boxed(DataFormat::Raw), table: crc_table(), crc: !0, size: 0, in_member: false, members: 0, done: false }
    }

    fn byte(&mut self) -> io::Result<u8> {
        let mut b = [0u8; 1];
        self.r.read_exact(&mut b).map_err(|_| bad("gzip: Kopf abgeschnitten"))?;
        Ok(b[0])
    }

    /// Reads a member header; false at the end of the stream (trailing zero bytes are padding, as gzip(1) says).
    fn header(&mut self) -> io::Result<bool> {
        loop {
            let buf = self.r.fill_buf()?;
            if buf.is_empty() {
                return Ok(false);
            }
            let zeros = buf.iter().take_while(|&&b| b == 0).count();
            if zeros == 0 {
                break;
            }
            self.r.consume(zeros);
        }
        let mut h = [0u8; 10];
        self.r.read_exact(&mut h).map_err(|_| bad("gzip: Kopf abgeschnitten"))?;
        if h[..2] != GZIP || h[2] != 8 {
            return Err(bad(if self.members > 0 { "gzip: fremde Daten nach dem Ende" } else { "gzip: kein gzip-Kopf" }));
        }
        let flg = h[3];
        if flg & 0xe0 != 0 {
            return Err(bad("gzip: reservierte Kopf-Bits gesetzt"));
        }
        if flg & 4 != 0 {
            let n = self.byte()? as usize | (self.byte()? as usize) << 8;
            for _ in 0..n {
                self.byte()?;
            }
        }
        for bit in [8u8, 16] {
            if flg & bit != 0 {
                // A zero-terminated name or comment; bounded like a field (64 KiB).
                let mut n = 0;
                while self.byte()? != 0 {
                    n += 1;
                    if n > 64 << 10 {
                        return Err(bad("gzip: Name im Kopf zu lang"));
                    }
                }
            }
        }
        if flg & 2 != 0 {
            self.byte()?;
            self.byte()?;
        }
        self.st.reset(DataFormat::Raw);
        self.crc = !0;
        self.size = 0;
        self.in_member = true;
        self.members += 1;
        Ok(true)
    }

    fn trailer(&mut self) -> io::Result<()> {
        let mut t = [0u8; 8];
        self.r.read_exact(&mut t).map_err(|_| bad("gzip: Ende abgeschnitten"))?;
        let crc = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
        let size = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
        if crc != !self.crc || size != self.size {
            return Err(bad("gzip: Prüfsumme falsch (Datei beschädigt)"));
        }
        self.in_member = false;
        Ok(())
    }
}

impl<R: BufRead> Read for Gz<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.done {
                return Ok(0);
            }
            if !self.in_member && !self.header()? {
                self.done = true;
                return Ok(0);
            }
            let input = self.r.fill_buf()?;
            let empty = input.is_empty();
            let res = inflate(&mut self.st, input, out, MZFlush::None);
            self.r.consume(res.bytes_consumed);
            let n = res.bytes_written;
            for &b in &out[..n] {
                self.crc = self.table[((self.crc ^ b as u32) & 0xff) as usize] ^ (self.crc >> 8);
            }
            self.size = self.size.wrapping_add(n as u32);
            match res.status {
                Ok(MZStatus::StreamEnd) => {
                    self.trailer()?;
                    if n > 0 {
                        return Ok(n);
                    }
                }
                Ok(_) | Err(MZError::Buf) => {
                    if n > 0 {
                        return Ok(n);
                    }
                    if empty {
                        return Err(bad("gzip: Datei abgeschnitten"));
                    }
                    if res.bytes_consumed == 0 {
                        return Err(bad("gzip: kein Fortschritt (beschädigt)"));
                    }
                }
                Err(_) => return Err(bad("gzip: Daten beschädigt")),
            }
        }
    }
}

// ---- zstd (RFC 8878) ----

type Frame<R> = ruzstd::decoding::StreamingDecoder<R, ruzstd::decoding::FrameDecoder>;

/// Several frames in a row; each frame's content checksum (when its header has one) is checked at its end, as
/// libarchive's zstd filter does through libzstd.
pub struct Zstd<R: BufRead> {
    idle: Option<R>,
    frame: Option<Frame<R>>,
    frames: u64,
    hash: super::xxh64::Xxh64,
}

impl<R: BufRead> Zstd<R> {
    pub fn new(r: R) -> Zstd<R> {
        Zstd { idle: Some(r), frame: None, frames: 0, hash: Default::default() }
    }
}

impl<R: BufRead> Read for Zstd<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(f) = &mut self.frame {
                let n = f.read(out).map_err(|e| bad(format!("zstd: {e}")))?;
                if n > 0 || out.is_empty() {
                    self.hash.update(&out[..n]);
                    return Ok(n);
                }
                let f = self.frame.take().ok_or_else(|| bad("zstd"))?;
                let (r, dec) = f.into_parts();
                let got = std::mem::take(&mut self.hash).finish() as u32;
                if let Some(want) = dec.get_checksum_from_data()
                    && want != got
                {
                    return Err(bad(format!("zstd: Prüfsumme des Rahmens falsch ({got:08x} statt {want:08x}): beschädigt")));
                }
                self.idle = Some(r);
            }
            let Some(mut r) = self.idle.take() else { return Ok(0) };
            if r.fill_buf()?.is_empty() {
                return if self.frames == 0 { Err(bad("zstd: leer")) } else { Ok(0) };
            }
            if !r.fill_buf()?.starts_with(&ZSTD) {
                return Err(bad("zstd: fremde Daten nach dem Ende"));
            }
            self.frames += 1;
            self.frame = Some(Frame::new(r).map_err(|e| bad(format!("zstd: {e}")))?);
        }
    }
}

/// Ends the stream with an error once more than `max` bytes came through.
pub struct Bounded<R> {
    r: R,
    pub got: u64,
    max: u64,
    what: String,
}

impl<R: Read> Bounded<R> {
    pub fn new(r: R, max: u64, what: &str) -> Bounded<R> {
        Bounded { r, got: 0, max, what: what.to_string() }
    }
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.r.read(out)?;
        self.got += n as u64;
        if self.got > self.max {
            return Err(bad(format!("{} entpackt mehr als {} MiB (Grenze je Datenbank)", self.what, self.max >> 20)));
        }
        Ok(n)
    }
}

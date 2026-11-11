//! Byte-granular reads, writes and seeks on a device that only takes whole sectors from a buffer aligned to its
//! sector (Windows refuses anything else on `\\.\PhysicalDriveN` opened without buffering). Partial sectors at the
//! edges are read, patched and written back; every transfer goes through one aligned bounce buffer, so a caller's
//! buffer never needs any alignment.

use super::BlockDev;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// A device that is read and written in whole sectors only.
pub trait Raw {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

/// Largest transfer in one call; larger reads and writes return early and the caller's loop goes on.
const CHUNK: usize = 8 << 20;
/// Alignment of the bounce buffer: at least the sector, at least a page.
const PAGE: usize = 4096;

pub struct Aligned<R: Raw> {
    raw: R,
    size: u64,
    sector: u64,
    pos: u64,
    store: Vec<u8>,
    /// Where the aligned part of `store` begins.
    start: usize,
}

impl<R: Raw> Aligned<R> {
    /// `size` is the device's size in bytes, a whole number of `sector`s (a power of two from 512 to 64 KiB).
    pub fn new(raw: R, size: u64, sector: u64) -> io::Result<Aligned<R>> {
        if !sector.is_power_of_two() || !(512..=65536).contains(&sector) || !size.is_multiple_of(sector) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unusable disk geometry: {size} bytes in sectors of {sector}")));
        }
        let align = (sector as usize).max(PAGE);
        let store = vec![0u8; CHUNK + 2 * sector as usize + align];
        let start = store.as_ptr().align_offset(align);
        Ok(Aligned { raw, size, sector, pos: 0, store, start })
    }

    /// The aligned sectors that cover up to `want` bytes at `pos`, at most one chunk and never past the end:
    /// (first byte, length, bytes of the caller's).
    fn span(&self, want: usize) -> (u64, usize, usize) {
        let first = self.pos - self.pos % self.sector;
        let n = (want.min(CHUNK) as u64).min(self.size - self.pos);
        let end = (self.pos + n).div_ceil(self.sector) * self.sector;
        (first, (end - first) as usize, n as usize)
    }

    fn bounce(&mut self, len: usize) -> &mut [u8] {
        &mut self.store[self.start..self.start + len]
    }
}

impl<R: Raw> Read for Aligned<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        let (first, len, n) = self.span(buf.len());
        let head = (self.pos - first) as usize;
        let start = self.start;
        self.raw.read_at(first, &mut self.store[start..start + len])?;
        buf[..n].copy_from_slice(&self.bounce(len)[head..head + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Raw> Write for Aligned<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.size {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write past the end of the disk"));
        }
        let (first, len, n) = self.span(buf.len());
        let head = (self.pos - first) as usize;
        let sec = self.sector as usize;
        let start = self.start;
        // Partial edge sectors keep the bytes around the new ones.
        if head != 0 {
            self.raw.read_at(first, &mut self.store[start..start + sec])?;
        }
        if !(head + n).is_multiple_of(sec) && (head == 0 || len > sec) {
            let last = len - sec;
            self.raw.read_at(first + last as u64, &mut self.store[start + last..start + len])?;
        }
        self.bounce(len)[head..head + n].copy_from_slice(&buf[..n]);
        self.raw.write_at(first, &self.store[start..start + len])?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.raw.flush()
    }
}

impl<R: Raw> Seek for Aligned<R> {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        let to = match p {
            SeekFrom::Start(o) => Some(o),
            SeekFrom::End(d) => self.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = to.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start of the disk"))?;
        Ok(self.pos)
    }
}

impl<R: Raw> BlockDev for Aligned<R> {
    fn size(&self) -> u64 {
        self.size
    }
    fn sync(&mut self) -> io::Result<()> {
        self.raw.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disk that fails every unaligned transfer, as a raw Windows disk does.
    struct Strict {
        data: Vec<u8>,
        sector: usize,
        calls: usize,
    }

    impl Strict {
        fn check(&self, off: u64, ptr: *const u8, len: usize) -> io::Result<()> {
            let ok = off as usize % self.sector == 0 && len % self.sector == 0 && ptr as usize % self.sector == 0 && off as usize + len <= self.data.len();
            if ok { Ok(()) } else { Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unaligned: {off} {len} {:p}", ptr))) }
        }
    }

    impl Raw for Strict {
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
            self.check(off, buf.as_ptr(), buf.len())?;
            self.calls += 1;
            buf.copy_from_slice(&self.data[off as usize..off as usize + buf.len()]);
            Ok(())
        }
        fn write_at(&mut self, off: u64, buf: &[u8]) -> io::Result<()> {
            self.check(off, buf.as_ptr(), buf.len())?;
            self.calls += 1;
            self.data[off as usize..off as usize + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn disk(sector: usize, size: usize) -> Aligned<Strict> {
        let data: Vec<u8> = (0..size).map(|i| (i * 7 % 251) as u8).collect();
        Aligned::new(Strict { data, sector, calls: 0 }, size as u64, sector as u64).unwrap()
    }

    #[test]
    fn unaligned_writes_keep_the_bytes_around_them() {
        for sector in [512, 4096] {
            let size = 64 * sector;
            let mut d = disk(sector, size);
            let mut model = d.raw.data.clone();
            // Inside one sector, across a boundary, a whole aligned run, the very end, and odd buffers.
            for (at, len) in [(3, 10), (sector - 5, 11), (2 * sector, 3 * sector), (size - 7, 7), (17, 5 * sector + 9), (0, 512)] {
                let src: Vec<u8> = (0..len + 1).map(|i| (i * 13 + at) as u8).collect();
                let src = &src[1..]; // never aligned
                d.seek(SeekFrom::Start(at as u64)).unwrap();
                d.write_all(src).unwrap();
                model[at..at + len].copy_from_slice(src);
                assert_eq!(d.raw.data, model, "sector {sector} at {at} len {len}");
                let mut back = vec![0u8; len + 3];
                d.seek(SeekFrom::Start(at as u64)).unwrap();
                d.read_exact(&mut back[3..]).unwrap();
                assert_eq!(&back[3..], src);
            }
        }
    }

    #[test]
    fn large_transfers_are_split_and_the_end_is_the_end() {
        let sector = 512;
        let size = CHUNK * 2 + 4 * sector;
        let mut d = disk(sector, size);
        let src = vec![0xa5u8; CHUNK + 1000];
        d.seek(SeekFrom::Start(100)).unwrap();
        d.write_all(&src).unwrap();
        assert!(d.raw.data[100..100 + src.len()].iter().all(|b| *b == 0xa5));
        assert_eq!(d.seek(SeekFrom::End(0)).unwrap(), size as u64);
        assert_eq!(d.read(&mut [0u8; 8]).unwrap(), 0);
        assert!(d.write(&[1]).is_err());
        d.seek(SeekFrom::End(-3)).unwrap();
        let mut tail = [0u8; 8];
        assert_eq!(d.read(&mut tail).unwrap(), 3);
        assert!(d.seek(SeekFrom::Current(-(size as i64) - 1)).is_err());
    }

    #[test]
    fn aligned_writes_read_nothing_first() {
        let mut d = disk(512, 1 << 20);
        d.write_all(&vec![1u8; 64 << 10]).unwrap();
        assert_eq!(d.raw.calls, 1);
    }

    #[test]
    fn odd_geometry_is_refused() {
        let s = || Strict { data: vec![], sector: 512, calls: 0 };
        assert!(Aligned::new(s(), 1000, 512).is_err());
        assert!(Aligned::new(s(), 1536, 768).is_err());
        assert!(Aligned::new(s(), 4096, 256).is_err());
    }
}

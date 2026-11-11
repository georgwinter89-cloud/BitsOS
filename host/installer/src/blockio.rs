//! A window of a disk (the FAT32 partition) as a file for the FAT code, in whole aligned blocks: small reads and
//! writes (FAT entries, directory slots) go through a cache of 4 KiB blocks, whole aligned blocks go straight to the
//! disk, and consecutive direct writes are joined into large ones. Windows refuses unaligned I/O on a physical
//! drive; on Linux the joining keeps a stick's write rate.

use crate::devices::BlockDev;
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom, Write};

pub const BLOCK: u64 = 4096;
const CACHE_BLOCKS: usize = 8192;
const JOIN_MAX: usize = 8 << 20;

pub struct Window<'a> {
    dev: &'a mut dyn BlockDev,
    base: u64,
    len: u64,
    pos: u64,
    /// Block index -> (bytes, dirty).
    cache: BTreeMap<u64, (Vec<u8>, bool)>,
    /// Direct writes not yet sent: (offset in the window, bytes), whole blocks only.
    pending: Option<(u64, Vec<u8>)>,
}

impl<'a> Window<'a> {
    pub fn new(dev: &'a mut dyn BlockDev, base: u64, len: u64) -> Window<'a> {
        assert!(base % BLOCK == 0 && len % BLOCK == 0, "window not block aligned");
        Window { dev, base, len, pos: 0, cache: BTreeMap::new(), pending: None }
    }

    fn dev_write(&mut self, off: u64, data: &[u8]) -> io::Result<()> {
        self.dev.seek(SeekFrom::Start(self.base + off))?;
        self.dev.write_all(data)
    }

    fn dev_read(&mut self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.dev.seek(SeekFrom::Start(self.base + off))?;
        self.dev.read_exact(buf)
    }

    fn send_pending(&mut self) -> io::Result<()> {
        if let Some((off, data)) = self.pending.take() {
            self.dev_write(off, &data)?;
        }
        Ok(())
    }

    fn overlaps_pending(&self, off: u64, n: u64) -> bool {
        self.pending.as_ref().is_some_and(|(p, d)| off < p + d.len() as u64 && *p < off + n)
    }

    fn flush_cache(&mut self) -> io::Result<()> {
        self.send_pending()?;
        let dirty: Vec<u64> = self.cache.iter().filter(|(_, (_, d))| *d).map(|(b, _)| *b).collect();
        for b in dirty {
            let data = self.cache.get(&b).map(|(d, _)| d.clone()).expect("listed");
            self.dev_write(b * BLOCK, &data)?;
            self.cache.get_mut(&b).expect("listed").1 = false;
        }
        Ok(())
    }

    fn block(&mut self, b: u64) -> io::Result<&mut (Vec<u8>, bool)> {
        if !self.cache.contains_key(&b) {
            if self.cache.len() >= CACHE_BLOCKS {
                self.flush_cache()?;
                self.cache.clear();
            }
            if self.overlaps_pending(b * BLOCK, BLOCK) {
                self.send_pending()?;
            }
            let mut v = vec![0u8; BLOCK as usize];
            self.dev_read(b * BLOCK, &mut v)?;
            self.cache.insert(b, (v, false));
        }
        Ok(self.cache.get_mut(&b).expect("inserted"))
    }

    fn direct_write(&mut self, off: u64, data: &[u8]) -> io::Result<()> {
        // A cached copy of these blocks would be stale: it goes.
        let (first, last) = (off / BLOCK, (off + data.len() as u64) / BLOCK);
        let stale: Vec<u64> = self.cache.range(first..last).map(|(b, _)| *b).collect();
        for b in stale {
            self.cache.remove(&b);
        }
        match &mut self.pending {
            Some((p, d)) if *p + d.len() as u64 == off && d.len() + data.len() <= JOIN_MAX => d.extend_from_slice(data),
            _ => {
                self.send_pending()?;
                self.pending = Some((off, data.to_vec()));
            }
        }
        Ok(())
    }

    /// Sends everything to the disk and asks it to keep it.
    pub fn sync(&mut self) -> io::Result<()> {
        self.flush_cache()?;
        self.dev.sync()
    }
}

impl Read for Window<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        let mut done = 0;
        while done < n {
            let off = self.pos + done as u64;
            let b = off / BLOCK;
            let in_blk = (off % BLOCK) as usize;
            let whole = in_blk == 0 && n - done >= BLOCK as usize;
            if whole && !self.cache.contains_key(&b) {
                // Whole blocks not in the cache: straight from the disk, as many as are not cached.
                let mut k = (n - done) / BLOCK as usize;
                if let Some((&c, _)) = self.cache.range(b..b + k as u64).next() {
                    k = (c - b) as usize;
                }
                let len = k * BLOCK as usize;
                if self.overlaps_pending(off, len as u64) {
                    self.send_pending()?;
                }
                self.dev_read(off, &mut buf[done..done + len])?;
                done += len;
                continue;
            }
            let take = (BLOCK as usize - in_blk).min(n - done);
            let blk = self.block(b)?;
            buf[done..done + take].copy_from_slice(&blk.0[in_blk..in_blk + take]);
            done += take;
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for Window<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.pos + data.len() as u64 > self.len {
            return Err(io::Error::other("write past the end of the partition"));
        }
        let mut done = 0;
        while done < data.len() {
            let off = self.pos + done as u64;
            let in_blk = (off % BLOCK) as usize;
            if in_blk == 0 && data.len() - done >= BLOCK as usize {
                let len = (data.len() - done) / BLOCK as usize * BLOCK as usize;
                self.direct_write(off, &data[done..done + len])?;
                done += len;
                continue;
            }
            let take = (BLOCK as usize - in_blk).min(data.len() - done);
            let blk = self.block(off / BLOCK)?;
            blk.0[in_blk..in_blk + take].copy_from_slice(&data[done..done + take]);
            blk.1 = true;
            done += take;
        }
        self.pos += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_cache()
    }
}

impl Seek for Window<'_> {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        let to = match p {
            SeekFrom::Start(o) => o as i128,
            SeekFrom::End(d) => self.len as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if to < 0 {
            return Err(io::Error::other("seek before the start of the partition"));
        }
        self.pos = to as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Mem(Cursor<Vec<u8>>, usize);
    impl Read for Mem {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            assert!(self.0.position() % BLOCK == 0 && b.len() as u64 % BLOCK == 0, "unaligned read");
            self.0.read(b)
        }
    }
    impl Write for Mem {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            assert!(self.0.position() % BLOCK == 0 && b.len() as u64 % BLOCK == 0, "unaligned write");
            self.1 += 1;
            self.0.write(b)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Mem {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.0.seek(p)
        }
    }
    impl BlockDev for Mem {
        fn size(&self) -> u64 {
            self.0.get_ref().len() as u64
        }
        fn sync(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn mixed_io_is_aligned_and_reads_back() {
        let mut m = Mem(Cursor::new(vec![0u8; 1 << 20]), 0);
        let mut model = vec![0u8; (1 << 20) - 8192];
        {
            let mut w = Window::new(&mut m, 8192, model.len() as u64);
            let mut x: u32 = 7;
            for _ in 0..400 {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let off = (x as usize >> 3) % (model.len() - 20_000);
                let len = 1 + (x as usize >> 7) % 19_000;
                let data: Vec<u8> = (0..len).map(|i| (i as u32 ^ x) as u8).collect();
                w.seek(SeekFrom::Start(off as u64)).unwrap();
                w.write_all(&data).unwrap();
                model[off..off + len].copy_from_slice(&data);
                let r = (x as usize >> 11) % (model.len() - 30_000);
                let mut back = vec![0u8; 1 + (x as usize >> 13) % 25_000];
                w.seek(SeekFrom::Start(r as u64)).unwrap();
                w.read_exact(&mut back).unwrap();
                assert!(back == model[r..r + back.len()], "read-back differs");
            }
            w.sync().unwrap();
        }
        assert!(m.0.get_ref()[8192..] == model[..], "disk differs after sync");
        assert!(m.0.get_ref()[..8192].iter().all(|&b| b == 0), "wrote outside the window");
    }

    #[test]
    fn consecutive_whole_blocks_are_joined() {
        let mut m = Mem(Cursor::new(vec![0u8; 4 << 20]), 0);
        {
            let mut w = Window::new(&mut m, 0, 4 << 20);
            for i in 0..256u32 {
                w.write_all(&[i as u8; 16384]).unwrap();
            }
            w.sync().unwrap();
        }
        assert_eq!(m.1, 1, "one write for 4 MiB of consecutive clusters");
        assert_eq!(m.0.get_ref()[16384 * 5], 5);
    }
}

//! A RAM `BlockDevice` that can lose power: everything written since the last flush is a log, and a crash
//! image keeps any subset of it in order (FUA writes always), with the write in flight torn after a random
//! number of blocks. Used by the host tests and the ktests; it refuses requests above its own size limit, so
//! a missing split above shows up as an error.

use alloc::string::String;
use alloc::vec::Vec;
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use ksync::SpinLock;

struct LogW {
    off: usize,
    data: Vec<u8>,
    fua: bool,
}

struct St {
    cur: Vec<u8>,
    base: Vec<u8>,
    log: Vec<LogW>,
    writes: u64,
    last_off: usize,
    flushes: u64,
    fail_write: Option<u64>,
    rng: u64,
    capture: bool,
    images: Vec<Vec<u8>>,
}

pub struct MemDev {
    name: String,
    bs: u32,
    blocks: u64,
    max: u32,
    discard: bool,
    read_only: bool,
    st: SpinLock<St>,
}

pub(crate) fn xorshift(s: &mut u64) -> u64 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *s = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

impl MemDev {
    pub fn new(name: &str, bs: u32, blocks: u64, max_blocks_per_request: u32) -> Option<MemDev> {
        let n = usize::try_from(blocks.checked_mul(bs as u64)?).ok()?;
        let mut cur = Vec::new();
        cur.try_reserve_exact(n).ok()?;
        cur.resize(n, 0);
        MemDev::from_image(name, bs, max_blocks_per_request, cur)
    }

    pub fn from_image(name: &str, bs: u32, max_blocks_per_request: u32, image: Vec<u8>) -> Option<MemDev> {
        if bs == 0 || image.len() % bs as usize != 0 {
            return None;
        }
        let mut base = Vec::new();
        base.try_reserve_exact(image.len()).ok()?;
        base.extend_from_slice(&image);
        Some(MemDev {
            name: name.into(),
            bs,
            blocks: (image.len() / bs as usize) as u64,
            max: max_blocks_per_request.max(1),
            discard: true,
            read_only: false,
            st: SpinLock::new(St {
                cur: image,
                base,
                log: Vec::new(),
                writes: 0,
                last_off: 0,
                flushes: 0,
                fail_write: None,
                rng: 0x9E37_79B9_7F4A_7C15,
                capture: false,
                images: Vec::new(),
            }),
        })
    }

    /// Reports itself read-only and refuses writes (a read-only stick).
    pub fn read_only(mut self) -> MemDev {
        self.read_only = true;
        self
    }

    /// What a reader sees now.
    pub fn image(&self) -> Vec<u8> {
        self.st.lock().cur.clone()
    }

    /// After a power cut that kept nothing beyond the last flush except FUA writes.
    pub fn durable(&self) -> Vec<u8> {
        let st = self.st.lock();
        let mut img = st.base.clone();
        for w in st.log.iter().filter(|w| w.fua) {
            img[w.off..w.off + w.data.len()].copy_from_slice(&w.data);
        }
        img
    }

    /// From now on every write first records two crash images (cut just before it lands, and a torn and
    /// reordered variant); `take_images` hands them out.
    pub fn set_capture(&self, on: bool, seed: u64) {
        let mut st = self.st.lock();
        st.capture = on;
        st.rng = seed | 1;
    }

    pub fn take_images(&self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.st.lock().images)
    }

    /// The `n`-th write from now on fails with `Io` (0: the next one).
    pub fn fail_write_after(&self, n: Option<u64>) {
        let mut st = self.st.lock();
        st.fail_write = n.map(|n| st.writes + n);
    }

    pub fn writes(&self) -> u64 {
        self.st.lock().writes
    }
    /// Byte offset of the latest write.
    pub fn last_write_offset(&self) -> usize {
        self.st.lock().last_off
    }
    pub fn flushes(&self) -> u64 {
        self.st.lock().flushes
    }

    fn crash(st: &mut St, pending: Option<(usize, &[u8])>, random: bool, bs: usize) -> Vec<u8> {
        let mut img = st.base.clone();
        let n = st.log.len();
        for i in 0..n {
            let keep = st.log[i].fua || (random && xorshift(&mut st.rng) % 2 == 0);
            if !keep {
                continue;
            }
            let w = &st.log[i];
            let mut len = w.data.len();
            if random && !w.fua && i + 1 == n {
                len = (xorshift(&mut st.rng) as usize % (len / bs + 1)) * bs;
            }
            img[w.off..w.off + len].copy_from_slice(&w.data[..len]);
        }
        if let Some((off, data)) = pending {
            let len = (xorshift(&mut st.rng) as usize % (data.len() / bs + 1)) * bs;
            img[off..off + len].copy_from_slice(&data[..len]);
        }
        img
    }

    fn check(&self, lba: u64, len: usize) -> Result<usize, DevError> {
        let bs = self.bs as usize;
        if len % bs != 0 || len / bs > self.max as usize {
            return Err(DevError::InvalidArgument);
        }
        let end = lba.checked_add((len / bs) as u64).ok_or(DevError::InvalidArgument)?;
        if end > self.blocks {
            return Err(DevError::InvalidArgument);
        }
        Ok(lba as usize * bs)
    }

    fn put(&self, off: usize, data: &[u8], fua: bool) -> Result<(), DevError> {
        let mut st = self.st.lock();
        if st.fail_write == Some(st.writes) {
            st.writes += 1;
            st.fail_write = None;
            return Err(DevError::Io);
        }
        st.writes += 1;
        if st.capture {
            let bs = self.bs as usize;
            let a = Self::crash(&mut st, Some((off, data)), false, bs);
            let b = Self::crash(&mut st, Some((off, data)), true, bs);
            st.images.push(a);
            st.images.push(b);
        }
        st.cur[off..off + data.len()].copy_from_slice(data);
        st.last_off = off;
        st.log.push(LogW { off, data: data.into(), fua });
        Ok(())
    }
}

impl BlockDevice for MemDev {
    fn name(&self) -> &str {
        &self.name
    }
    fn info(&self) -> BlockInfo {
        BlockInfo {
            block_size: self.bs,
            blocks: self.blocks,
            read_only: self.read_only,
            max_blocks_per_request: self.max,
            supports_discard: self.discard,
            has_volatile_cache: true,
        }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], _prio: IoPriority) -> Result<(), DevError> {
        let off = self.check(lba, buf.len())?;
        buf.copy_from_slice(&self.st.lock().cur[off..off + buf.len()]);
        Ok(())
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, _prio: IoPriority) -> Result<(), DevError> {
        if self.read_only {
            return Err(DevError::Refused);
        }
        let off = self.check(lba, buf.len())?;
        self.put(off, buf, fua)
    }
    fn flush(&self) -> Result<(), DevError> {
        let mut st = self.st.lock();
        st.flushes += 1;
        let St { base, log, .. } = &mut *st;
        for w in log.drain(..) {
            base[w.off..w.off + w.data.len()].copy_from_slice(&w.data);
        }
        Ok(())
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        let bs = self.bs as usize;
        let end = lba.checked_add(blocks).ok_or(DevError::InvalidArgument)?;
        if end > self.blocks {
            return Err(DevError::InvalidArgument);
        }
        let mut st = self.st.lock();
        let (off, len) = (lba as usize * bs, blocks as usize * bs);
        st.cur[off..off + len].fill(0);
        let mut z = Vec::new();
        z.resize(len, 0u8);
        st.log.push(LogW { off, data: z, fua: false });
        Ok(())
    }
}

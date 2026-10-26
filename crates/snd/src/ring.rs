//! A client's ring of stereo frames at the client's rate (mono is doubled on the way in). The writer and the
//! mixer each hold the lock only for one bounded copy; the mixer never waits for it longer than that.

use alloc::vec::Vec;

pub struct Ring {
    buf: Vec<[i16; 2]>,
    /// Absolute frame counts: read <= write <= read + capacity.
    read: u64,
    write: u64,
    /// `read` at the last `clear` (ALSA's hw_ptr restarts at 0 after drop/prepare).
    base: u64,
}

impl Ring {
    /// None if the memory cannot be had (the size comes from a program: never an infallible allocation).
    pub fn try_new(frames: usize) -> Option<Ring> {
        let n = frames.max(1);
        let mut buf = Vec::new();
        buf.try_reserve_exact(n).ok()?;
        buf.resize(n, [0; 2]);
        Some(Ring { buf, read: 0, write: 0, base: 0 })
    }
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }
    pub fn len(&self) -> usize {
        (self.write - self.read) as usize
    }
    pub fn is_empty(&self) -> bool {
        self.write == self.read
    }
    pub fn free(&self) -> usize {
        self.capacity() - self.len()
    }
    /// Drops what is queued; `position` restarts at 0.
    pub fn clear(&mut self) {
        self.read = self.write;
        self.base = self.read;
    }
    /// Frames taken since open or the last `clear` (read under the same lock as the pop, so a concurrent
    /// `clear` never leaves a stale count behind).
    pub fn position(&self) -> u64 {
        self.read - self.base
    }

    /// Appends whole frames of 16-bit little-endian samples with `channels` (1 or 2) per frame; returns the
    /// frames taken (at most `max` and the free space).
    pub fn push_bytes(&mut self, data: &[u8], channels: usize, max: usize) -> usize {
        let fb = 2 * channels;
        let n = (data.len() / fb).min(self.free()).min(max);
        let cap = self.capacity() as u64;
        for i in 0..n {
            let s = |c: usize| i16::from_le_bytes([data[i * fb + 2 * c], data[i * fb + 2 * c + 1]]);
            let f = if channels == 1 { [s(0), s(0)] } else { [s(0), s(1)] };
            self.buf[((self.write + i as u64) % cap) as usize] = f;
        }
        self.write += n as u64;
        n
    }

    pub fn push_frames(&mut self, frames: &[[i16; 2]]) -> usize {
        let n = frames.len().min(self.free());
        let cap = self.capacity() as u64;
        for (i, f) in frames[..n].iter().enumerate() {
            self.buf[((self.write + i as u64) % cap) as usize] = *f;
        }
        self.write += n as u64;
        n
    }

    /// Moves up to `n` frames into `out` (appended); returns how many.
    pub fn pop_into(&mut self, out: &mut Vec<[i16; 2]>, n: usize) -> usize {
        let n = n.min(self.len());
        let cap = self.capacity() as u64;
        out.extend((0..n as u64).map(|i| self.buf[((self.read + i) % cap) as usize]));
        self.read += n as u64;
        n
    }
}

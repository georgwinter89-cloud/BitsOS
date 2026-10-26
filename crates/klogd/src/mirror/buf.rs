//! The mirror's byte ring (V1's `MIRROR`/`log_count` idea): every byte ever written has a position in the
//! stream; the ring keeps the newest `capacity` of them. `sent` is how far the stream has gone out; what the
//! ring overwrote before it could go is skipped, and the jump in positions is exactly what the listener shows
//! as `[... n bytes lost]`. Fixed capacity (a memory budget sized from RAM, R1), never allocates after `new`.

use alloc::vec::Vec;

pub struct ByteRing {
    data: Vec<u8>,
    /// Bytes ever written; the newest is at `written - 1`.
    written: u64,
    /// Bytes out (or skipped because overwritten).
    sent: u64,
    /// Bytes overwritten before they went out.
    lost: u64,
}

impl ByteRing {
    /// A ring of `bytes` (at least 1); None when the memory is not there.
    pub fn new(bytes: usize) -> Option<ByteRing> {
        let bytes = bytes.max(1);
        let mut data = Vec::new();
        data.try_reserve_exact(bytes).ok()?;
        data.resize(bytes, 0);
        Some(ByteRing { data, written: 0, sent: 0, lost: 0 })
    }

    /// Empty again, positions from 0 (tests that reuse one ring).
    pub fn clear(&mut self) {
        self.written = 0;
        self.sent = 0;
        self.lost = 0;
    }

    pub fn capacity(&self) -> usize {
        self.data.len()
    }
    pub fn written(&self) -> u64 {
        self.written
    }
    pub fn sent(&self) -> u64 {
        self.sent
    }
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// The oldest position the ring still holds.
    pub fn oldest(&self) -> u64 {
        self.written.saturating_sub(self.data.len() as u64)
    }

    pub fn push(&mut self, b: u8) {
        let i = (self.written % self.data.len() as u64) as usize;
        self.data[i] = b;
        self.written += 1;
    }

    pub fn push_all(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.push(b);
        }
    }

    /// The byte at `pos` if the ring still holds it.
    pub fn at(&self, pos: u64) -> Option<u8> {
        (pos >= self.oldest() && pos < self.written).then(|| self.data[(pos % self.data.len() as u64) as usize])
    }

    /// Where a new listener's log begins: 0, or once the ring wrapped the first whole line it holds (V1
    /// `mirror_log_start`).
    pub fn line_start(&self) -> u64 {
        let oldest = self.oldest();
        if oldest == 0 {
            return 0;
        }
        (oldest..self.written).find(|&p| p > oldest && self.at(p - 1) == Some(b'\n')).unwrap_or(self.written)
    }

    /// Skips what the ring overwrote; returns the bytes lost by that (also added to `lost`).
    pub fn skip_overwritten(&mut self) -> u64 {
        let oldest = self.oldest();
        if self.sent >= oldest {
            return 0;
        }
        let n = oldest - self.sent;
        self.sent = oldest;
        self.lost += n;
        n
    }

    /// Moves `sent` to `pos` without counting anything lost (the start of a new listener: V1 sends the log
    /// from `line_start`, what lies before it was never meant for this listener).
    pub fn start_at(&mut self, pos: u64) {
        self.sent = pos.min(self.written);
    }

    /// Copies the bytes from `from` (at most `out.len()`) into `out`. When more text follows and the piece has
    /// a line end, it is cut after the last one, so a lost datagram never splits a line on the listener's side
    /// without a mark. Returns the count (0: nothing at `from`, or `from` is no longer held).
    pub fn copy(&self, from: u64, out: &mut [u8]) -> usize {
        if from < self.oldest() || from >= self.written {
            return 0;
        }
        let n = ((self.written - from) as usize).min(out.len());
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = self.data[((from + i as u64) % self.data.len() as u64) as usize];
        }
        if from + (n as u64) < self.written
            && let Some(last) = out[..n].iter().rposition(|&b| b == b'\n')
        {
            return last + 1;
        }
        n
    }

    /// `sent` moves to `upto` (never back, never past `written`).
    pub fn mark_sent(&mut self, upto: u64) {
        self.sent = self.sent.max(upto.min(self.written));
    }

    /// Bytes waiting to go out (after skipping overwritten ones).
    pub fn pending(&self) -> u64 {
        self.written - self.sent.max(self.oldest())
    }
}

/// The mirror ring for a machine with `total_ram` bytes: RAM / 8192, rounded up to 64 KiB, 256 KiB .. 8 MiB
/// (QEMU's 2 GB: 256 KiB, as V1's log ring; the Mini-PC's ~24 GB: 3 MiB). The boot before the network is up
/// fits many times over.
pub fn bytes_for_memory(total_ram: u64) -> usize {
    let b = (total_ram / 8192).clamp(256 << 10, 8 << 20) as usize;
    (b + 0xFFFF) & !0xFFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_every_byte_and_wrap() {
        let mut r = ByteRing::new(8).unwrap();
        r.push_all(b"abc\n");
        assert_eq!((r.written(), r.oldest(), r.pending()), (4, 0, 4));
        let mut out = [0u8; 16];
        assert_eq!(r.copy(0, &mut out), 4);
        assert_eq!(&out[..4], b"abc\n");
        r.mark_sent(4);
        r.push_all(b"defgh\nij");
        assert_eq!((r.written(), r.oldest()), (12, 4));
        assert_eq!(r.copy(4, &mut out), 8);
        assert_eq!(&out[..8], b"defgh\nij");
        r.push_all(b"kl");
        assert_eq!(r.oldest(), 6);
        assert_eq!(r.copy(4, &mut out), 0, "4 is overwritten");
        assert_eq!(r.skip_overwritten(), 2);
        assert_eq!((r.sent(), r.lost(), r.pending()), (6, 2, 8));
        assert_eq!(r.at(5), None);
        assert_eq!(r.at(6), Some(b'f'));
        assert_eq!(r.at(14), None);
    }

    #[test]
    fn pieces_end_at_a_line_end_when_more_follows() {
        let mut r = ByteRing::new(64).unwrap();
        r.push_all(b"one\ntwo\nthree");
        let mut out = [0u8; 10];
        assert_eq!(r.copy(0, &mut out), 8, "cut after two\\n");
        assert_eq!(r.copy(8, &mut out), 5, "the rest, no line end needed");
        let mut big = [0u8; 64];
        assert_eq!(r.copy(0, &mut big), 13, "everything fits: not cut");
        let mut r = ByteRing::new(64).unwrap();
        r.push_all(&[b'x'; 30]);
        assert_eq!(r.copy(0, &mut out), 10, "no line end: a whole piece");
        assert_eq!(r.copy(30, &mut out), 0);
        assert_eq!(r.copy(99, &mut out), 0);
    }

    #[test]
    fn a_new_listener_starts_at_a_whole_line() {
        let mut r = ByteRing::new(10).unwrap();
        r.push_all(b"ab\ncd\n");
        assert_eq!(r.line_start(), 0);
        r.push_all(b"efgh\nij");
        // written 13, oldest 3: "cd\nefgh\nij" -> "cd" starts right after the \n at 2, which is gone.
        assert_eq!(r.line_start(), 6);
        r.start_at(r.line_start());
        assert_eq!((r.sent(), r.lost()), (6, 0));
        let mut r = ByteRing::new(4).unwrap();
        r.push_all(b"abcdefgh");
        assert_eq!(r.line_start(), 8, "no whole line: nothing old");
        r.mark_sent(100);
        assert_eq!(r.sent(), 8, "never past written");
        r.mark_sent(3);
        assert_eq!(r.sent(), 8, "never back");
    }

    #[test]
    fn sized_from_memory() {
        assert_eq!(bytes_for_memory(0), 256 << 10);
        assert_eq!(bytes_for_memory(2 << 30), 256 << 10);
        assert_eq!(bytes_for_memory(24 << 30), 3 << 20);
        assert_eq!(bytes_for_memory(1 << 40), 8 << 20);
        assert_eq!(bytes_for_memory(3 << 30) % 0x10000, 0);
        assert!(ByteRing::new(0).is_some_and(|r| r.capacity() == 1));
    }
}

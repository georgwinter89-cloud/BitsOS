//! A byte queue made of page-sized chunks (one direction of a loopback connection).
//!
//! The queue used to be one `VecDeque<u8>` that grew to `PIPE_LIMIT` under the pipe's spin lock, where the
//! heap can neither reclaim nor fall back to single frames: a large block allocated there needs a free
//! contiguous run and brings the kernel down when there is none (Mini-PC, 11.11.2026; see `sock::unix`'s
//! `fill`). Chunks are single pages, made by the writer before it takes the lock (`spare`), and a connection
//! holds only the chunks its unread bytes need.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

pub(crate) const CHUNK: usize = 4096;

#[derive(Default)]
pub(crate) struct ByteRing {
    /// Every chunk has capacity `CHUNK`; all but the last are full.
    chunks: VecDeque<Vec<u8>>,
    /// Bytes of the first chunk already read.
    head: usize,
    len: usize,
}

/// Empty chunks for a `push`; made with the ring's lock dropped.
pub(crate) fn spare(have: &mut Vec<Vec<u8>>, want: usize) {
    have.reserve(want.saturating_sub(have.len()));
    while have.len() < want {
        have.push(Vec::with_capacity(CHUNK));
    }
}

impl ByteRing {
    pub const fn new() -> Self {
        ByteRing { chunks: VecDeque::new(), head: 0, len: 0 }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Chunks a `push` of `n` bytes takes from its spares.
    pub fn chunks_for(&self, n: usize) -> usize {
        let tail = self.chunks.back().map_or(0, |c| CHUNK - c.len());
        n.saturating_sub(tail).div_ceil(CHUNK)
    }

    /// Appends `src`; `spare` holds at least `chunks_for(src.len())` chunks.
    pub fn push(&mut self, mut src: &[u8], spare: &mut Vec<Vec<u8>>) {
        self.len += src.len();
        while !src.is_empty() {
            let room = self.chunks.back().map_or(0, |c| CHUNK - c.len());
            if room == 0 {
                let Some(c) = spare.pop() else { break };
                self.chunks.push_back(c);
                continue;
            }
            let (now, rest) = src.split_at(room.min(src.len()));
            if let Some(c) = self.chunks.back_mut() {
                c.extend_from_slice(now);
            }
            src = rest;
        }
        debug_assert!(src.is_empty(), "push without enough spare chunks");
        self.len -= src.len();
    }

    /// Copies the first bytes into `buf` without taking them.
    pub fn peek(&self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.len);
        let (mut done, mut skip) = (0, self.head);
        for c in &self.chunks {
            if done == n {
                break;
            }
            let part = &c[skip..];
            let k = part.len().min(n - done);
            buf[done..done + k].copy_from_slice(&part[..k]);
            done += k;
            skip = 0;
        }
        n
    }

    /// Takes the first bytes into `buf`.
    pub fn pop(&mut self, buf: &mut [u8]) -> usize {
        let n = self.peek(buf);
        self.skip(n)
    }

    /// Drops the first `n` bytes (fewer if fewer are queued); the count dropped.
    pub fn skip(&mut self, n: usize) -> usize {
        let n = n.min(self.len);
        let mut left = n;
        while left > 0 {
            let only = self.chunks.len() == 1;
            let Some(c) = self.chunks.front_mut() else { break };
            let k = (c.len() - self.head).min(left);
            self.head += k;
            left -= k;
            if self.head == c.len() {
                self.head = 0;
                if only {
                    // The last chunk stays for the next bytes: request and answer in turn allocate nothing.
                    c.clear();
                } else {
                    self.chunks.pop_front();
                }
            }
        }
        self.len -= n;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(r: &mut ByteRing, src: &[u8]) {
        let mut s = Vec::new();
        spare(&mut s, r.chunks_for(src.len()));
        r.push(src, &mut s);
        assert!(s.is_empty(), "chunks_for asked for exactly what push used");
    }

    #[test]
    fn bytes_come_out_in_order_across_chunks() {
        let mut r = ByteRing::new();
        let data: Vec<u8> = (0..3 * CHUNK + 123).map(|i| (i % 251) as u8).collect();
        put(&mut r, &data[..10]);
        put(&mut r, &data[10..CHUNK + 5]);
        put(&mut r, &data[CHUNK + 5..]);
        assert_eq!(r.len(), data.len());
        let mut peeked = alloc::vec![0u8; CHUNK + 100];
        assert_eq!(r.peek(&mut peeked), CHUNK + 100);
        assert_eq!(peeked[..], data[..CHUNK + 100]);
        let mut out = Vec::new();
        let mut b = [0u8; 1000];
        loop {
            let n = r.pop(&mut b);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&b[..n]);
        }
        assert_eq!(out, data);
        assert!(r.is_empty());
    }

    #[test]
    fn emptied_ring_keeps_one_chunk_and_reuses_it() {
        let mut r = ByteRing::new();
        put(&mut r, &[1u8; 2 * CHUNK + 1]);
        assert_eq!(r.skip(usize::MAX), 2 * CHUNK + 1);
        assert_eq!((r.chunks.len(), r.head, r.len()), (1, 0, 0));
        assert_eq!(r.chunks_for(CHUNK), 0, "the kept chunk is empty again");
        put(&mut r, &[2u8; 7]);
        let mut b = [0u8; 16];
        assert_eq!(r.pop(&mut b), 7);
        assert_eq!(b[..7], [2u8; 7]);
    }

    /// A writer that counted chunks, dropped the lock to make them and found the tail used up by another
    /// writer sees it in `chunks_for` (the caller's loop asks again instead of pushing).
    #[test]
    fn chunks_for_follows_the_tail() {
        let mut r = ByteRing::new();
        put(&mut r, &[0u8; 100]);
        assert_eq!(r.chunks_for(CHUNK - 100), 0);
        put(&mut r, &[0u8; 50]);
        assert_eq!(r.chunks_for(CHUNK - 100), 1);
        let mut b = [0u8; 150];
        assert_eq!(r.pop(&mut b), 150);
        assert_eq!(r.chunks_for(CHUNK + 1), 1, "an emptied tail counts in full");
    }
}

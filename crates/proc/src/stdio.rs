//! Standard input and output of a process started by the kernel (the `run` MCP tool, ktests): a captured
//! output stream and a fixed input. Both look like pipe ends to the program (S_IFIFO, not a terminal), as a
//! program started by a shell with redirections sees them on Linux.
//!
//! `Capture` keeps at most `cap` bytes (a bound against a program that prints without end; the MCP answer is
//! bounded too) and counts what it had to drop; the program never blocks on it. FS-3's pipes replace it once
//! programs read each other's output.

use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::{Spare, SpinLock, WaitQueue};
use uapi::errno::*;
use vfs::{Errno, Events, File};

const S_IFIFO: u32 = 0o010000;

struct CapInner {
    data: Vec<u8>,
    dropped: u64,
}

/// An output stream that keeps what the program writes (up to `cap` bytes).
pub struct Capture {
    inner: SpinLock<CapInner>,
    cap: usize,
    /// Woken after every write (the `run` tool streams progress).
    pub wq: WaitQueue,
}

impl Capture {
    pub fn new(cap: usize) -> Arc<Capture> {
        Arc::new(Capture { inner: SpinLock::named("proc.capture", CapInner { data: Vec::new(), dropped: 0 }), cap, wq: WaitQueue::new() })
    }
    /// Everything kept so far.
    pub fn contents(&self) -> Vec<u8> {
        self.since(0).0
    }
    /// Bytes kept from `from` on (for progress), and the new end. The copy's memory is made with the lock
    /// dropped (the kept output is up to `cap` bytes, and under a spin lock the heap may not reclaim for it);
    /// output that arrived meanwhile asks again.
    pub fn since(&self, from: usize) -> (Vec<u8>, usize) {
        let mut out = Vec::new();
        loop {
            let want = {
                let g = self.inner.lock();
                let from = from.min(g.data.len());
                let want = g.data.len() - from;
                if want <= out.capacity() {
                    out.extend_from_slice(&g.data[from..]);
                    return (out, g.data.len());
                }
                want
            };
            out.reserve_exact(want);
        }
    }
    pub fn len(&self) -> usize {
        self.inner.lock().data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Bytes the program wrote after `cap` was reached (or when the heap had no room).
    pub fn dropped(&self) -> u64 {
        self.inner.lock().dropped
    }
}

impl File for Capture {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        // Before the guard: the buffer it replaces is freed after the unlock.
        let mut spare = Spare::new();
        loop {
            let mut g = self.inner.lock();
            let room = self.cap.saturating_sub(g.data.len()).min(buf.len());
            if spare.fits(&mut g.data, room) {
                g.data.extend_from_slice(&buf[..room]);
                g.dropped += (buf.len() - room) as u64;
                break;
            }
            // The larger buffer is made with the lock dropped; another writer's bytes that came meanwhile are
            // kept (the room is worked out again), each write stays whole.
            drop(g);
            if spare.grow().is_err() {
                self.inner.lock().dropped += buf.len() as u64;
                break;
            }
        }
        self.wq.wake_all();
        Ok(buf.len())
    }
    fn mode(&self) -> u32 {
        S_IFIFO | 0o600
    }
    fn events(&self) -> Events {
        Events(Events::OUT)
    }
}

/// A fixed input: the bytes, then end of file.
pub struct Input {
    data: Vec<u8>,
    pos: SpinLock<usize>,
}

impl Input {
    pub fn new(data: Vec<u8>) -> Arc<Input> {
        Arc::new(Input { data, pos: SpinLock::named("proc.input", 0) })
    }
}

impl File for Input {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut p = self.pos.lock();
        let n = (self.data.len() - *p).min(buf.len());
        buf[..n].copy_from_slice(&self.data[*p..*p + n]);
        *p += n;
        Ok(n)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn mode(&self) -> u32 {
        S_IFIFO | 0o600
    }
    fn events(&self) -> Events {
        // A pipe whose writer wrote everything and closed (what a shell's `<<<` or the host's pipe gives):
        // readable while bytes are left, hung up always (Linux `pipe_poll`: EPOLLHUP without writers).
        let left = self.data.len() > *self.pos.lock();
        Events(Events::HUP | if left { Events::IN } else { 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_keeps_up_to_cap_and_counts_the_rest() {
        let c = Capture::new(10);
        assert_eq!(c.write_at(0, b"hello ").unwrap(), 6);
        assert_eq!(c.write_at(0, b"world!!").unwrap(), 7, "the program never sees a short write");
        assert_eq!(c.contents(), b"hello worl");
        assert_eq!(c.dropped(), 3);
        assert_eq!(c.since(6), (b"worl".to_vec(), 10));
        assert_eq!(c.since(99), (Vec::new(), 10));
        assert_eq!(c.read_at(0, &mut [0u8; 4]), Err(EBADF));
    }

    /// The kept output grows past many pages through the spare only (never under the lock), in order, with
    /// writers from several threads; copies taken meanwhile are prefixes of the end result.
    #[test]
    fn capture_grows_outside_its_lock_and_keeps_every_write_whole() {
        extern crate std;
        const WRITERS: u8 = 4;
        const EACH: usize = 400;
        const LINE: usize = 64;
        let cap = WRITERS as usize * EACH * LINE - 1000;
        let c = Capture::new(cap);
        let hs: std::vec::Vec<_> = (0..WRITERS)
            .map(|w| {
                let c = c.clone();
                std::thread::spawn(move || {
                    for i in 0..EACH {
                        let mut line = [b'a' + w; LINE];
                        line[0] = (i % 251) as u8;
                        assert_eq!(c.write_at(0, &line).unwrap(), LINE);
                    }
                })
            })
            .collect();
        let mut seen = Vec::new();
        while hs.iter().any(|h| !h.is_finished()) {
            let (more, end) = c.since(seen.len());
            seen.extend_from_slice(&more);
            assert_eq!(seen.len(), end);
        }
        for h in hs {
            h.join().unwrap();
        }
        let all = c.contents();
        assert_eq!(all.len(), cap, "kept up to the bound");
        assert_eq!(c.dropped(), 1000);
        assert_eq!(&all[..seen.len()], &seen[..], "an earlier copy is not a prefix of the kept output");
        // Whole writes: every 64-byte line is one writer's, and each writer's lines come in its order.
        let mut next = [0usize; WRITERS as usize];
        for line in all.chunks(LINE) {
            let w = (line[line.len().min(2) - 1] - b'a') as usize;
            assert!(line[1..].iter().all(|&b| b == b'a' + w as u8), "two writes mixed in one line");
            if line.len() == LINE {
                assert_eq!(line[0], (next[w] % 251) as u8, "writer {w}: a line out of order or lost before the bound");
                next[w] += 1;
            }
        }
    }

    #[test]
    fn input_then_eof() {
        let i = Input::new(b"abc".to_vec());
        let mut b = [0u8; 2];
        assert_eq!(i.read_at(0, &mut b).unwrap(), 2);
        assert_eq!(&b, b"ab");
        assert_eq!(i.read_at(0, &mut b).unwrap(), 1);
        assert_eq!(i.read_at(0, &mut b).unwrap(), 0);
        assert_eq!(i.write_at(0, b"x"), Err(EBADF));
    }
}

//! Standard input and output of a process started by the kernel (the `run` MCP tool, ktests): a captured
//! output stream and a fixed input. Both look like pipe ends to the program (S_IFIFO, not a terminal), as a
//! program started by a shell with redirections sees them on Linux.
//!
//! `Capture` keeps at most `cap` bytes (a bound against a program that prints without end; the MCP answer is
//! bounded too) and counts what it had to drop; the program never blocks on it. FS-3's pipes replace it once
//! programs read each other's output.

use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::{SpinLock, WaitQueue};
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
        self.inner.lock().data.clone()
    }
    /// Bytes kept from `from` on (for progress), and the new end.
    pub fn since(&self, from: usize) -> (Vec<u8>, usize) {
        let g = self.inner.lock();
        let from = from.min(g.data.len());
        (g.data[from..].to_vec(), g.data.len())
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
        {
            let mut g = self.inner.lock();
            let room = self.cap.saturating_sub(g.data.len()).min(buf.len());
            if room > 0 && g.data.try_reserve(room).is_ok() {
                g.data.extend_from_slice(&buf[..room]);
                g.dropped += (buf.len() - room) as u64;
            } else {
                g.dropped += buf.len() as u64;
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

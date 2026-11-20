//! Lines from a client, bounded before they are kept. The bound of a line is asked when its first byte is
//! there, so the caller can make it depend on who sends (a stranger gets a few bytes, never the memory
//! budget). A read that times out loses nothing: the next call goes on where this one stopped. One call
//! reads at most once, so a sender of bytes without a line end cannot keep the caller in here: it gets
//! [`Line::More`] and looks at its clock.

use std::io::Read;

const CHUNK: usize = 16 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Text(String),
    /// Longer than its bound: nothing of it was kept.
    TooLong,
    /// Bytes came, the line is not complete yet.
    More,
    End,
}

pub struct Lines<R> {
    r: R,
    buf: Vec<u8>,
    /// Bytes of `buf` already searched for a line end.
    seen: usize,
    /// Inside a line that is too long: its bytes are read away up to its end.
    skipping: bool,
    max: usize,
    /// No byte of the next line was looked at yet.
    fresh: bool,
}

impl<R: Read> Lines<R> {
    pub fn new(r: R) -> Lines<R> {
        Lines { r, buf: Vec::new(), seen: 0, skipping: false, max: 0, fresh: true }
    }

    /// The next line without its end. An error of the reader (a timeout among them) is handed on.
    pub fn next(&mut self, bound: &mut dyn FnMut() -> usize) -> std::io::Result<Line> {
        let mut read = false;
        loop {
            if self.fresh && !self.buf.is_empty() {
                self.max = bound();
                self.fresh = false;
            }
            if let Some(i) = self.buf[self.seen..].iter().position(|b| *b == b'\n') {
                let end = self.seen + i;
                let line: Vec<u8> = self.buf.drain(..=end).take(end).collect();
                self.seen = 0;
                self.fresh = true;
                if std::mem::take(&mut self.skipping) || line.len() > self.max {
                    return Ok(Line::TooLong);
                }
                return Ok(Line::Text(String::from_utf8_lossy(&line).into_owned()));
            }
            if self.buf.len() > self.max {
                self.skipping = true;
                self.buf.clear();
            }
            self.seen = self.buf.len();
            if read {
                return Ok(Line::More);
            }
            let mut chunk = [0u8; CHUNK];
            let n = match self.r.read(&mut chunk) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if n == 0 {
                return Ok(Line::End);
            }
            self.buf.extend_from_slice(&chunk[..n]);
            read = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hands out its pieces one read at a time, a timeout between two of them.
    struct Pieces(Vec<Option<&'static [u8]>>);

    impl Read for Pieces {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            match self.0.remove(0) {
                Some(p) => {
                    out[..p.len()].copy_from_slice(p);
                    Ok(p.len())
                }
                None => Err(std::io::ErrorKind::WouldBlock.into()),
            }
        }
    }

    #[test]
    fn lines_survive_timeouts_and_a_long_one_is_dropped_whole() {
        let mut l = Lines::new(Pieces(vec![Some(b"{\"a\""), None, Some(b":1}\nsecond\nway too long for"), Some(b" the bound\nok\n")]));
        let mut asked = 0;
        let mut bound = || {
            asked += 1;
            12
        };
        assert_eq!(l.next(&mut bound).unwrap(), Line::More);
        assert_eq!(l.next(&mut bound).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(l.next(&mut bound).unwrap(), Line::Text("{\"a\":1}".into()));
        assert_eq!(l.next(&mut bound).unwrap(), Line::Text("second".into()));
        assert_eq!(l.next(&mut bound).unwrap(), Line::TooLong);
        assert_eq!(l.next(&mut bound).unwrap(), Line::Text("ok".into()));
        assert_eq!(l.next(&mut bound).unwrap(), Line::End);
        assert_eq!(asked, 4, "the bound is asked once per line");
    }

    #[test]
    fn bytes_without_a_line_end_hand_the_caller_back_after_every_read() {
        let mut l = Lines::new(Pieces(vec![Some(b"a" as &[u8]); 40]));
        let mut bound = || 8;
        for _ in 0..40 {
            assert_eq!(l.next(&mut bound).unwrap(), Line::More, "never a second read in one call");
        }
        assert_eq!(l.next(&mut bound).unwrap(), Line::End);
        assert!(l.buf.len() <= 9, "and what is over the bound is not kept");
    }
}

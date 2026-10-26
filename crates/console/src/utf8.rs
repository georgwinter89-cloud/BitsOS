//! UTF-8 decoding across writes: a program that writes "ä" as two `write(2)` calls (unbuffered byte output) must
//! see "ä" on the screen, not two replacement characters. `Carry` keeps an incomplete sequence at the end of one
//! write (at most 3 bytes) and puts it in front of the next; invalid bytes become U+FFFD as `from_utf8_lossy` does.

use alloc::string::String;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Carry {
    buf: [u8; 3],
    len: u8,
}

impl Carry {
    pub const fn new() -> Carry {
        Carry { buf: [0; 3], len: 0 }
    }

    /// Bytes waiting for the rest of their character.
    pub fn pending(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    /// Decodes `bytes` after what was carried; an incomplete character at the end is kept for the next call.
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        let mut out = String::new();
        let mut rest = bytes;
        // Finish the carried character first (byte by byte: it needs at most 3 more).
        while self.len > 0 {
            let Some((&b, tail)) = rest.split_first() else { return out };
            let mut seq = [0u8; 4];
            let n = self.len as usize;
            seq[..n].copy_from_slice(self.pending());
            seq[n] = b;
            match core::str::from_utf8(&seq[..=n]) {
                Ok(s) => {
                    out.push_str(s);
                    self.len = 0;
                    rest = tail;
                }
                Err(e) if e.error_len().is_none() => {
                    // Still incomplete (a 3- or 4-byte character): keep collecting.
                    if n + 1 > self.buf.len() {
                        out.push(char::REPLACEMENT_CHARACTER);
                        self.len = 0;
                    } else {
                        self.buf[n] = b;
                        self.len += 1;
                    }
                    rest = tail;
                }
                Err(_) => {
                    // The carried bytes were not the start of a character: one U+FFFD, then `b` is read anew.
                    out.push(char::REPLACEMENT_CHARACTER);
                    self.len = 0;
                }
            }
        }
        loop {
            match core::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    return out;
                }
                Err(e) => {
                    let (good, after) = rest.split_at(e.valid_up_to());
                    // `good` is valid UTF-8 by `valid_up_to`; lossy only to stay panic-free.
                    out.push_str(&String::from_utf8_lossy(good));
                    match e.error_len() {
                        Some(bad) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[bad..];
                        }
                        None => {
                            // An incomplete character at the end (at most 3 bytes): wait for the next write.
                            let n = after.len().min(self.buf.len());
                            self.buf[..n].copy_from_slice(&after[..n]);
                            self.len = n as u8;
                            return out;
                        }
                    }
                }
            }
        }
    }
}

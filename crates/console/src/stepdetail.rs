//! IN-9: one step's detail text, kept inline. klog's watch feeds the step list from the emitter's context (any
//! lock may be held there, an IRQ handler, the boot before `sti`), so the model must never allocate: with the
//! detail inline, `Steps::feed` takes nothing but its own lock and that lock is a leaf (no heap lock under it,
//! no lock-order edge to the allocator). The size is the one the list already had: 80 printable characters
//! (the row on screen cuts earlier), now counted in bytes for 4-byte characters.

use core::fmt;

/// The most characters a detail keeps (the screen row and `/proc/bitsos/boot` show one short line).
pub const CHARS: usize = 80;
const BYTES: usize = CHARS * 4;

/// A detail: printable (control characters dropped), at most `CHARS` characters, no heap.
#[derive(Clone, Copy)]
pub struct Detail {
    buf: [u8; BYTES],
    len: u16,
    chars: u8,
}

impl Detail {
    pub const fn new() -> Detail {
        Detail { buf: [0; BYTES], len: 0, chars: 0 }
    }

    /// The text of `args`, made printable and cut like any detail.
    pub fn of(args: fmt::Arguments<'_>) -> Detail {
        let mut d = Detail::new();
        // `write_str` below never fails: a full detail drops the rest.
        let _ = fmt::write(&mut d, args);
        d
    }

    pub fn as_str(&self) -> &str {
        // Only whole characters are ever copied in (`write_str`), so the bytes are UTF-8.
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

impl Default for Detail {
    fn default() -> Self {
        Detail::new()
    }
}

impl fmt::Write for Detail {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars().filter(|c| !c.is_control()) {
            if self.chars as usize >= CHARS {
                break;
            }
            let at = self.len as usize;
            let n = c.encode_utf8(&mut self.buf[at..at + 4]).len();
            self.len += n as u16;
            self.chars += 1;
        }
        Ok(())
    }
}

impl From<&str> for Detail {
    fn from(s: &str) -> Detail {
        Detail::of(format_args!("{s}"))
    }
}

impl core::ops::Deref for Detail {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for Detail {
    fn eq(&self, o: &Detail) -> bool {
        self.as_str() == o.as_str()
    }
}
impl Eq for Detail {}

impl PartialEq<&str> for Detail {
    fn eq(&self, o: &&str) -> bool {
        self.as_str() == *o
    }
}

impl PartialEq<str> for Detail {
    fn eq(&self, o: &str) -> bool {
        self.as_str() == o
    }
}

impl fmt::Display for Detail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Detail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

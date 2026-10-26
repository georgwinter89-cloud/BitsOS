//! A text buffer on the stack (the panic path formats without the heap).

use core::fmt;

pub struct Buf<const N: usize> {
    b: [u8; N],
    n: usize,
    cut: bool,
}

impl<const N: usize> Buf<N> {
    pub const fn new() -> Self {
        Buf { b: [0; N], n: 0, cut: false }
    }
    pub fn as_str(&self) -> &str {
        // Only whole chars are copied in (`write_str`).
        core::str::from_utf8(&self.b[..self.n]).unwrap_or("")
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.b[..self.n]
    }
    pub fn len(&self) -> usize {
        self.n
    }
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    pub fn was_cut(&self) -> bool {
        self.cut
    }
    pub fn clear(&mut self) {
        self.n = 0;
        self.cut = false;
    }
}

impl<const N: usize> Default for Buf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Write for Buf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut k = s.len().min(N - self.n);
        while !s.is_char_boundary(k) {
            k -= 1;
        }
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        if k < s.len() {
            self.cut = true;
        }
        Ok(())
    }
}

/// Writes through with line breaks and other control characters replaced by a space, so a panic message
/// stays one line in the record and in an event field.
pub struct OneLine<'a, W: fmt::Write>(pub &'a mut W);

impl<W: fmt::Write> fmt::Write for OneLine<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut rest = s;
        while let Some(i) = rest.find(char::is_control) {
            self.0.write_str(&rest[..i])?;
            self.0.write_str(" ")?;
            let c = rest[i..].chars().next().map_or(1, char::len_utf8);
            rest = &rest[i + c..];
        }
        self.0.write_str(rest)
    }
}

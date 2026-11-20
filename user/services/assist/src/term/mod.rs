//! The console side of the chat: raw keyboard input, key decoding, the line editor, wrapping and the screen
//! picture. Only `sys` touches the terminal; the rest is plain data in, text out, and tested on the host.

pub mod edit;
pub mod keys;
pub mod sys;
pub mod view;
pub mod wrap;

/// The terminal the chat runs on: the console ([`sys::Console`]), or a scripted one in tests.
pub trait Term {
    /// Columns and rows.
    fn size(&self) -> (usize, usize);
    /// True when something typed is waiting within `ms` milliseconds (0: just look, negative: until it comes).
    fn wait_input(&mut self, ms: i32) -> bool;
    /// Bytes typed; 0 at the end of the input.
    fn read_input(&mut self, buf: &mut [u8]) -> usize;
    /// Forgets everything typed and not read yet.
    fn drop_input(&mut self);
    fn write(&mut self, bytes: &[u8]);
}

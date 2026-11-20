//! Bytes from the keyboard to keys: UTF-8 characters (umlauts type directly on the German layout) and the
//! escape sequences of the Linux console and of xterm-like terminals for the keys the chat uses.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Esc,
    /// Ctrl with a letter, lower case.
    Ctrl(char),
}

#[derive(Default)]
pub struct Decoder {
    /// Bytes of a character or sequence that is not complete yet.
    pending: Vec<u8>,
}

impl Decoder {
    /// A lone Esc looks like the start of a sequence until nothing follows: the reader calls this when the
    /// keyboard stayed quiet for a moment.
    pub fn idle(&mut self) -> Vec<Key> {
        let had = !self.pending.is_empty();
        let esc = self.pending.first() == Some(&0x1b);
        self.pending.clear();
        if had && esc { vec![Key::Esc] } else { Vec::new() }
    }

    pub fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let mut out = Vec::new();
        for &b in bytes {
            self.pending.push(b);
            self.drain(&mut out);
        }
        out
    }

    fn drain(&mut self, out: &mut Vec<Key>) {
        let p = &self.pending;
        let key = match p[0] {
            0x1b => match p.get(1) {
                None => return,
                Some(b'[') | Some(b'O') => {
                    let Some(&last) = p.get(2..).and_then(|rest| rest.last()) else { return };
                    // Parameters and intermediates go on until a final byte; `[[` is the Linux console's
                    // function-key prefix and takes one more byte.
                    if (0x20..0x40).contains(&last) || (p.len() == 3 && last == b'[' && p[1] == b'[') {
                        return;
                    }
                    let params = String::from_utf8_lossy(&p[2..p.len() - 1]).into_owned();
                    let first = params.split(';').next().unwrap_or("");
                    match (last, first) {
                        (_, "[") => None,
                        (b'A', _) => Some(Key::Up),
                        (b'B', _) => Some(Key::Down),
                        (b'C', _) => Some(Key::Right),
                        (b'D', _) => Some(Key::Left),
                        (b'H', _) | (b'~', "1" | "7") => Some(Key::Home),
                        (b'F', _) | (b'~', "4" | "8") => Some(Key::End),
                        (b'~', "3") => Some(Key::Delete),
                        (b'~', "5") => Some(Key::PageUp),
                        (b'~', "6") => Some(Key::PageDown),
                        _ => None,
                    }
                }
                // Esc typed twice: the first one counts, the second is read anew.
                Some(0x1b) => {
                    out.push(Key::Esc);
                    self.pending.truncate(1);
                    return;
                }
                // Alt with a key arrives as Esc and the key in one go: not a key of the chat, and above all
                // not the Esc that leaves it.
                Some(_) => None,
            },
            b'\r' | b'\n' => Some(Key::Enter),
            b'\t' => Some(Key::Tab),
            0x7f | 0x08 => Some(Key::Backspace),
            b @ 0x01..=0x1a => Some(Key::Ctrl((b'a' + b - 1) as char)),
            0x00..=0x1f => None,
            b if b < 0x80 => Some(Key::Char(b as char)),
            b => {
                let need = if b >= 0xf0 {
                    4
                } else if b >= 0xe0 {
                    3
                } else if b >= 0xc0 {
                    2
                } else {
                    1
                };
                if p.len() < need {
                    return;
                }
                std::str::from_utf8(p).ok().and_then(|s| s.chars().next()).map(Key::Char)
            }
        };
        self.pending.clear();
        out.extend(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn umlauts_and_the_keys_of_the_chat() {
        let mut d = Decoder::default();
        assert_eq!(d.feed("Grüße€".as_bytes()), "Grüße€".chars().map(Key::Char).collect::<Vec<_>>());
        assert_eq!(d.feed(b"\r\x7f\x04\x15\t"), vec![Key::Enter, Key::Backspace, Key::Ctrl('d'), Key::Ctrl('u'), Key::Tab]);
        assert_eq!(d.feed(b"\x1b[A\x1b[B\x1b[C\x1b[D"), vec![Key::Up, Key::Down, Key::Right, Key::Left]);
        assert_eq!(d.feed(b"\x1b[1~\x1b[4~\x1b[H\x1b[F\x1bOH"), vec![Key::Home, Key::End, Key::Home, Key::End, Key::Home]);
        assert_eq!(d.feed(b"\x1b[3~\x1b[5~\x1b[6~\x1b[1;5C"), vec![Key::Delete, Key::PageUp, Key::PageDown, Key::Right]);
        // A function key of the Linux console is swallowed whole.
        assert_eq!(d.feed(b"\x1b[[Ax"), vec![Key::Char('x')]);
    }

    #[test]
    fn a_split_character_and_a_lone_escape() {
        let mut d = Decoder::default();
        let bytes = "ä".as_bytes();
        assert!(d.feed(&bytes[..1]).is_empty());
        assert_eq!(d.feed(&bytes[1..]), vec![Key::Char('ä')]);
        assert!(d.feed(b"\x1b").is_empty());
        assert!(d.waiting());
        assert_eq!(d.idle(), vec![Key::Esc]);
        assert!(d.idle().is_empty());
        assert_eq!(d.feed(b"\x1b\x1b"), vec![Key::Esc]);
        assert_eq!(d.idle(), vec![Key::Esc]);
        // Alt-x is not Esc followed by x.
        assert_eq!(d.feed(b"\x1bxy"), vec![Key::Char('y')]);
    }
}

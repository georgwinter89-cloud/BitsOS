//! The input line: a cursor in a line of characters, the usual editing keys, and the earlier questions with
//! arrow up and down.

use super::keys::Key;

#[derive(Default)]
pub struct LineEdit {
    chars: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    /// Which earlier question is shown; None while a new one is typed.
    recall: Option<usize>,
    draft: String,
}

impl LineEdit {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }
    pub fn chars(&self) -> &[char] {
        &self.chars
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    fn set(&mut self, s: &str) {
        self.chars = s.chars().collect();
        self.cursor = self.chars.len();
    }

    /// The line, remembered for arrow up, and an empty line to type the next one.
    pub fn take(&mut self) -> String {
        let s = self.text();
        if !s.trim().is_empty() && self.history.last() != Some(&s) {
            self.history.push(s.clone());
        }
        self.set("");
        self.recall = None;
        s
    }

    /// True when the key changed the line or the cursor.
    pub fn key(&mut self, k: Key) -> bool {
        match k {
            Key::Char(c) => {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
            Key::Tab => return self.key(Key::Char(' ')),
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            Key::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            Key::Left if self.cursor > 0 => self.cursor -= 1,
            Key::Right if self.cursor < self.chars.len() => self.cursor += 1,
            Key::Home | Key::Ctrl('a') => self.cursor = 0,
            Key::End | Key::Ctrl('e') => self.cursor = self.chars.len(),
            Key::Ctrl('u') => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::Ctrl('k') => self.chars.truncate(self.cursor),
            Key::Ctrl('w') => {
                let mut start = self.cursor;
                while start > 0 && self.chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && self.chars[start - 1] != ' ' {
                    start -= 1;
                }
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            Key::Up if !self.history.is_empty() => {
                let i = match self.recall {
                    None => {
                        self.draft = self.text();
                        self.history.len() - 1
                    }
                    Some(i) => i.saturating_sub(1),
                };
                self.recall = Some(i);
                let s = self.history[i].clone();
                self.set(&s);
            }
            Key::Down => match self.recall {
                Some(i) if i + 1 < self.history.len() => {
                    self.recall = Some(i + 1);
                    let s = self.history[i + 1].clone();
                    self.set(&s);
                }
                Some(_) => {
                    self.recall = None;
                    let s = std::mem::take(&mut self.draft);
                    self.set(&s);
                }
                None => return false,
            },
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(e: &mut LineEdit, s: &str) {
        for c in s.chars() {
            e.key(Key::Char(c));
        }
    }

    #[test]
    fn editing_in_the_middle_of_a_line_with_umlauts() {
        let mut e = LineEdit::default();
        typed(&mut e, "Größe ändern");
        for _ in 0..8 {
            e.key(Key::Left);
        }
        e.key(Key::Backspace);
        typed(&mut e, "ss");
        assert_eq!(e.text(), "Grösse ändern");
        e.key(Key::Home);
        e.key(Key::Delete);
        e.key(Key::End);
        e.key(Key::Ctrl('w'));
        assert_eq!(e.text(), "rösse ");
        e.key(Key::Ctrl('u'));
        assert!(e.is_empty());
        assert!(!e.key(Key::Backspace) && !e.key(Key::Left) && !e.key(Key::PageUp));
    }

    #[test]
    fn earlier_questions_come_back_and_the_draft_survives() {
        let mut e = LineEdit::default();
        typed(&mut e, "eins");
        assert_eq!(e.take(), "eins");
        typed(&mut e, "zwei");
        e.take();
        typed(&mut e, "neu");
        e.key(Key::Up);
        assert_eq!(e.text(), "zwei");
        e.key(Key::Up);
        e.key(Key::Up);
        assert_eq!(e.text(), "eins");
        e.key(Key::Down);
        e.key(Key::Down);
        assert_eq!(e.text(), "neu");
    }
}

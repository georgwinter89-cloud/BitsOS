//! The picture of the chat: what was said (wrapped to the screen's width, the newest at the bottom, scrollable),
//! a status line and the input line. `frame` returns the whole screen as one string of plain ANSI sequences
//! (cursor home, erase to end of line, the eight basic colours, reverse video): what the Linux console and
//! every terminal understand.

use super::wrap::{text_width, width, wrap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    User,
    Assistant,
    /// A line from assist itself: a tool at work, a failure explained.
    Note,
    /// A yes/no question to Georg.
    Question,
    /// A technical line (the provider's raw status).
    Tech,
}

struct Entry {
    who: Who,
    text: String,
}

/// An entry's screen lines, and the length of its text when they were made.
struct Laid {
    len: usize,
    lines: Vec<String>,
}

/// What was said. Text is only ever added (a new entry, more of the running answer), so the screen lines of
/// an entry are kept and made anew only when its text grew or the screen changed: a redraw while an answer
/// streams costs the length of that answer, not of the whole conversation.
#[derive(Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    laid: Vec<Laid>,
    /// The width and the assistant's name `laid` was made for.
    shape: (usize, String),
}

fn lay(e: &Entry, cols: usize, name: &str) -> Vec<String> {
    let label = match e.who {
        Who::User => "Du: ".to_string(),
        Who::Assistant => format!("{name}: "),
        Who::Note | Who::Question => "· ".to_string(),
        Who::Tech => "  ".to_string(),
    };
    let indent = text_width(&label).min(cols / 2);
    let mut out = Vec::new();
    for (i, l) in wrap(&e.text, cols.saturating_sub(indent)).into_iter().enumerate() {
        let lead = if i == 0 { label.chars().take(indent).collect::<String>() } else { " ".repeat(indent) };
        out.push(format!("{lead}{l}"));
    }
    if matches!(e.who, Who::User | Who::Assistant) {
        out.push(String::new());
    }
    out
}

impl Transcript {
    pub fn push(&mut self, who: Who, text: &str) {
        self.entries.push(Entry { who, text: text.to_string() });
    }

    /// Adds to the assistant's running answer, or starts one.
    pub fn stream(&mut self, delta: &str) {
        match self.entries.last_mut() {
            Some(e) if e.who == Who::Assistant => e.text.push_str(delta),
            _ => self.push(Who::Assistant, delta),
        }
    }

    /// Adds to the newest entry, whatever it is (the answer behind a question).
    pub fn append(&mut self, more: &str) {
        if let Some(e) = self.entries.last_mut() {
            e.text.push_str(more);
        }
    }

    /// Brings the screen lines up to date for this width; their number.
    fn layout(&mut self, cols: usize, name: &str) -> usize {
        if self.shape.0 != cols || self.shape.1 != name {
            self.laid.clear();
            self.shape = (cols, name.to_string());
        }
        for (i, e) in self.entries.iter().enumerate() {
            if self.laid.get(i).is_some_and(|l| l.len == e.text.len()) {
                continue;
            }
            let l = Laid { len: e.text.len(), lines: lay(e, cols, name) };
            match self.laid.get_mut(i) {
                Some(old) => *old = l,
                None => self.laid.push(l),
            }
        }
        self.laid.iter().map(|l| l.lines.len()).sum()
    }

    /// Every screen line of the last `layout` with the colour of its entry.
    fn rows(&self) -> impl Iterator<Item = (Who, &str)> {
        self.entries.iter().zip(&self.laid).flat_map(|(e, l)| l.lines.iter().map(move |s| (e.who, s.as_str())))
    }
}

fn colour(who: Who) -> &'static str {
    match who {
        Who::User => "\x1b[1m",
        Who::Assistant => "\x1b[0m",
        Who::Note => "\x1b[36m",
        Who::Question => "\x1b[1;33m",
        Who::Tech => "\x1b[33m",
    }
}

/// `s` cut or padded to exactly `cols` columns.
fn fit(s: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        if used + width(c) > cols {
            break;
        }
        out.push(c);
        used += width(c);
    }
    out.push_str(&" ".repeat(cols - used));
    out
}

pub struct View<'a> {
    pub name: &'a str,
    pub status: &'a str,
    pub input: &'a [char],
    pub cursor: usize,
    /// Lines scrolled up from the newest.
    pub scroll: usize,
    pub busy: bool,
}

/// The smallest screen the picture is made for.
fn dims(cols: usize, rows: usize) -> (usize, usize) {
    (cols.max(20), rows.max(4))
}

/// The most `scroll` can be for this picture.
pub fn max_scroll(t: &mut Transcript, name: &str, cols: usize, rows: usize) -> usize {
    let (cols, rows) = dims(cols, rows);
    t.layout(cols, name).saturating_sub(rows - 2)
}

pub fn frame(t: &mut Transcript, v: &View, cols: usize, rows: usize) -> String {
    let (cols, rows) = dims(cols, rows);
    let body = rows - 2;
    let total = t.layout(cols, v.name);
    let scroll = v.scroll.min(total.saturating_sub(body));
    let end = total - scroll;
    let start = end.saturating_sub(body);
    let mut shown = t.rows().skip(start).take(end - start);
    let mut out = String::from("\x1b[H");
    for _ in 0..body {
        match shown.next() {
            Some((who, text)) => out.push_str(&format!("{}{}\x1b[0m\x1b[K\r\n", colour(who), text)),
            None => out.push_str("\x1b[K\r\n"),
        }
    }
    let more = if scroll > 0 { format!(" · {scroll} Zeilen weiter unten") } else { String::new() };
    out.push_str(&format!("\x1b[7m{}\x1b[0m\r\n", fit(&format!(" {}{more}", v.status), cols)));
    // The input line shows the part around the cursor when the text is longer than the screen is wide.
    let prompt = if v.busy { "… " } else { "> " };
    let room = cols - 3;
    let mut from = 0;
    let before = |from: usize| v.input[from..v.cursor].iter().map(|c| width(*c)).sum::<usize>();
    while before(from) > room {
        from += 1;
    }
    let shown: String = v.input[from..].iter().collect();
    out.push_str(&format!("{prompt}{}\x1b[K", fit(&shown, room).trim_end()));
    out.push_str(&format!("\x1b[{};{}H", rows, 3 + before(from)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(frame: &str) -> Vec<String> {
        // Strip the sequences: what is left is the text of each row.
        let mut plain = String::new();
        let mut chars = frame.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                plain.push(c);
            }
        }
        plain.split("\r\n").map(str::to_string).collect()
    }

    #[test]
    fn newest_at_the_bottom_and_scrolling_back() {
        let mut t = Transcript::default();
        for i in 1..=10 {
            t.push(Who::Note, &format!("Zeile {i}"));
        }
        let view = |scroll| View { name: "Claude", status: "Status", input: &[], cursor: 0, scroll, busy: false };
        let rows = rows_of(&frame(&mut t, &view(0), 40, 6));
        assert_eq!(rows.len(), 6);
        assert_eq!(&rows[..4], ["· Zeile 7", "· Zeile 8", "· Zeile 9", "· Zeile 10"]);
        assert!(rows[4].starts_with(" Status"));
        assert_eq!(rows[5], "> ");
        let rows = rows_of(&frame(&mut t, &view(3), 40, 6));
        assert_eq!(&rows[..4], ["· Zeile 4", "· Zeile 5", "· Zeile 6", "· Zeile 7"]);
        assert!(rows[4].contains("3 Zeilen weiter unten"));
        assert_eq!(max_scroll(&mut t, "Claude", 40, 6), 6);
        // Scrolled past the top: the oldest lines, never an empty screen.
        assert_eq!(rows_of(&frame(&mut t, &view(99), 40, 6))[0], "· Zeile 1");
    }

    #[test]
    fn a_streamed_answer_wraps_under_its_label() {
        let mut t = Transcript::default();
        t.push(Who::User, "Mach die Schrift größer");
        t.stream("Die Schrift ist jetzt ");
        t.stream("größer als vorher.");
        let lines = |t: &mut Transcript, cols| {
            t.layout(cols, "Claude");
            t.rows().map(|l| l.1.to_string()).collect::<Vec<_>>()
        };
        assert_eq!(lines(&mut t, 24), ["Du: Mach die Schrift", "    größer", "", "Claude: Die Schrift ist", "        jetzt größer als", "        vorher.", ""]);
        // Kept lines follow the text and the screen: more of the answer, an added word, another width.
        t.stream(" Gut so?");
        assert_eq!(lines(&mut t, 24)[5..], ["        vorher. Gut so?", ""]);
        t.push(Who::Question, "Erlauben?");
        t.append(" ja");
        assert_eq!(lines(&mut t, 24).last().unwrap(), "· Erlauben? ja");
        assert_eq!(lines(&mut t, 60)[..2], ["Du: Mach die Schrift größer", ""]);
    }

    #[test]
    fn a_long_input_line_follows_the_cursor() {
        let mut t = Transcript::default();
        let input: Vec<char> = "abcdefghijklmnopqrstuvwxyzäöü".chars().collect();
        let v = View { name: "A", status: "", input: &input, cursor: input.len(), scroll: 0, busy: false };
        let f = frame(&mut t, &v, 20, 4);
        let last = rows_of(&f).pop().unwrap();
        assert!(last.ends_with("xyzäöü"), "{last:?}");
        assert!(f.ends_with("\x1b[4;20H"), "{f:?}");
    }
}

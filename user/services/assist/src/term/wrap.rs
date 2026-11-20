//! Text to screen lines. Everything shown passes through here, so this is also where text from a provider or a
//! tool loses its control characters: an escape sequence in an answer must not reach the terminal.

/// Columns a character takes: 0 for combining marks, 2 for the wide East Asian and emoji blocks, else 1.
pub fn width(c: char) -> usize {
    let u = c as u32;
    if (0x0300..=0x036f).contains(&u) || (0x200b..=0x200f).contains(&u) || (0xfe00..=0xfe0f).contains(&u) || u == 0x00ad {
        0
    } else if (0x1100..=0x115f).contains(&u)
        || (0x2e80..=0xa4cf).contains(&u)
        || (0xac00..=0xd7a3).contains(&u)
        || (0xf900..=0xfaff).contains(&u)
        || (0xfe30..=0xfe6f).contains(&u)
        || (0xff00..=0xff60).contains(&u)
        || (0xffe0..=0xffe6).contains(&u)
        || (0x1f300..=0x1faff).contains(&u)
        || (0x20000..=0x3fffd).contains(&u)
    {
        2
    } else {
        1
    }
}

pub fn text_width(s: &str) -> usize {
    s.chars().map(width).sum()
}

/// `s` without control characters; line breaks stay, a tab becomes spaces.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// `s` as one bounded line for a place Georg decides by (the question before a tool call): a line break, a
/// control character or an invisible one (zero width, text direction) is written out as its escape, so text
/// from the model can neither add lines to the question nor reorder it, and anything beyond `max` characters
/// is cut and said so with the full length.
pub fn inline(s: &str, max: usize) -> String {
    let total = s.chars().count();
    let mut out = String::new();
    for c in s.chars().take(max) {
        let u = c as u32;
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || u == 0xad || (0x200b..=0x200f).contains(&u) || (0x2028..=0x202e).contains(&u) || (0x2060..=0x206f).contains(&u) || u == 0xfeff => {
                out.push_str(&format!("\\u{{{u:x}}}"));
            }
            c => out.push(c),
        }
    }
    if total > max {
        out.push_str(&format!(" … (gekürzt, {total} Zeichen)"));
    }
    out
}

/// `text` as lines of at most `cols` columns: broken at spaces, a word longer than a line is cut.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut lines = Vec::new();
    for para in clean(text).split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in para.split(' ') {
            let w = text_width(word);
            if used > 0 && used + 1 + w > cols {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            for c in word.chars() {
                let cw = width(c);
                if used + cw > cols && used > 0 {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += cw;
            }
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_spaces_and_cuts_long_words() {
        assert_eq!(wrap("Größe der Schrift ändern", 10), ["Größe der", "Schrift", "ändern"]);
        assert_eq!(wrap("abcdefghijkl mn", 5), ["abcde", "fghij", "kl mn"]);
        assert_eq!(wrap("eins\n\nzwei", 20), ["eins", "", "zwei"]);
        assert_eq!(wrap("", 20), [""]);
        assert!(wrap("日本語のテキスト", 6).iter().all(|l| text_width(l) <= 6));
    }

    #[test]
    fn one_bounded_line_for_a_question() {
        assert_eq!(inline("größer", 200), "größer");
        assert_eq!(inline("x\nErlauben? (j = ja, n = nein) ja\r\x1b[2J", 200), "x\\nErlauben? (j = ja, n = nein) ja\\r\\u{1b}[2J");
        assert_eq!(inline("a\u{202e}b\u{200b}c", 200), "a\\u{202e}b\\u{200b}c");
        let long = "ä".repeat(10_000);
        let shown = inline(&long, 200);
        assert_eq!(shown, format!("{} … (gekürzt, 10000 Zeichen)", "ä".repeat(200)));
        assert!(!shown.contains('\n'));
    }

    #[test]
    fn control_characters_never_reach_the_screen() {
        let hostile = "ok\x1b[2J\x1b]0;title\x07 weiter\r\x08\u{9b}31m";
        let shown = wrap(hostile, 80).join("\n");
        assert!(!shown.chars().any(|c| c.is_control()), "{shown:?}");
        assert!(shown.starts_with("ok[2J"));
    }
}

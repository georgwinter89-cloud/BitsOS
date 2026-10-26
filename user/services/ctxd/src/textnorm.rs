//! Normalisation (DESIGN §5): Unicode NFC, no control characters, collapsed spaces, paragraph breaks kept.

use unicode_normalization::UnicodeNormalization;

pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    let mut newlines = 0usize;
    for c in s.nfc() {
        match c {
            '\r' => {}
            '\n' => {
                newlines += 1;
                pending_space = false;
            }
            c if c.is_whitespace() => pending_space = true,
            c if c.is_control() => pending_space = true,
            '\u{00ad}' | '\u{200b}' | '\u{feff}' => {}
            c => {
                if !out.is_empty() {
                    if newlines >= 2 {
                        out.push_str("\n\n");
                    } else if newlines == 1 {
                        out.push('\n');
                    } else if pending_space {
                        out.push(' ');
                    }
                }
                newlines = 0;
                pending_space = false;
                out.push(c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_and_keeps_paragraphs() {
        assert_eq!(normalize("  a \t b\r\n\r\n\n\nc\u{0007}d  \n e"), "a b\n\nc d\ne");
        // "a" + combining diaeresis → NFC "ä".
        assert_eq!(normalize("Ma\u{0308}rz"), "März");
        assert_eq!(normalize("Soft\u{00ad}hyphen\u{feff}"), "Softhyphen");
    }
}

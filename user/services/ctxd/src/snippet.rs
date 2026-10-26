//! Snippets: "about two sentences" (Georg 15.10.), at most `max_chars` characters (DESIGN §9.3).
//! The sentence with the most query words is chosen, plus the one after it.

use crate::tokenize;

fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut s = 0;
    let b = text.as_bytes();
    for i in 0..b.len() {
        let end = b[i] == b'\n' || (matches!(b[i], b'.' | b'!' | b'?') && b.get(i + 1).is_none_or(|c| *c == b' '));
        if end {
            let t = text[s..=i].trim();
            if !t.is_empty() {
                out.push(t);
            }
            s = i + 1;
        }
    }
    let t = text[s..].trim();
    if !t.is_empty() {
        out.push(t);
    }
    out
}

/// Cut to `max` characters at a word boundary, "…" included in the count.
pub fn cut_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let head: String = s.chars().take(max - 1).collect();
    let cut = match head.rfind(' ') {
        Some(p) if p > head.len() / 2 => &head[..p],
        _ => &head[..],
    };
    format!("{}…", cut.trim_end())
}

pub fn make(text: &str, query: &str, max_chars: usize) -> Option<String> {
    if max_chars == 0 {
        return None;
    }
    let sents = sentences(text);
    if sents.is_empty() {
        return None;
    }
    let qw = tokenize::words(query);
    let mut best = (0usize, 0usize);
    for (i, s) in sents.iter().enumerate() {
        let sw = tokenize::words(s);
        let hits = qw
            .iter()
            .filter(|q| sw.iter().any(|w| w.starts_with(q.as_str()) || q.starts_with(w.as_str()) && w.len() >= 4))
            .count();
        if hits > best.1 {
            best = (i, hits);
        }
    }
    let mut out = sents[best.0].to_string();
    if let Some(next) = sents.get(best.0 + 1) {
        out.push(' ');
        out.push_str(next);
    }
    Some(cut_chars(&out, max_chars))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_sentences_at_most_and_limit_kept() {
        let t = "Erster Satz ohne Bezug. Die Lohnsteuerbescheinigung 2025 liegt bei. Dritter Satz. Vierter.";
        let s = make(t, "Lohnsteuerbescheinigung", 200).unwrap();
        assert_eq!(s, "Die Lohnsteuerbescheinigung 2025 liegt bei. Dritter Satz.");
        for max in [1, 10, 37, 200] {
            let s = make(&t.repeat(20), "Satz", max).unwrap();
            assert!(s.chars().count() <= max, "{max}: {s}");
        }
        assert_eq!(make(t, "x", 0), None);
    }

    #[test]
    fn long_single_sentence_is_cut() {
        let t = "wort ".repeat(200);
        let s = make(&t, "wort", 50).unwrap();
        assert!(s.chars().count() <= 50 && s.ends_with('…'));
    }
}

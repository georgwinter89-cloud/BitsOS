//! Keyword tokenizer (DESIGN §5, evidence in `host/cx-eval/baseline.py`): lower case, German folding so
//! umlauts match both ways (ä/ae, ß/ss), words kept whole, plus character 4-grams of words with 5+ letters
//! so a compound meets its parts ("Stromvertrag" ↔ "Stromliefervertrag").

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

pub const TOKENIZER_NAME: &str = "de4";

/// Common German and English function words; they carry no topic.
const STOP: &[&str] = &[
    "aber", "alle", "als", "also", "am", "an", "auch", "auf", "aus", "bei", "bin", "bis", "da", "damit", "dann", "das",
    "dass", "dein", "dem", "den", "der", "des", "dich", "die", "dies", "diese", "dieser", "dieses", "dir", "doch",
    "du", "durch", "ein", "eine", "einem", "einen", "einer", "eines", "er", "es", "euch", "fuer", "hat", "hatte",
    "ich", "ihr", "im", "in", "ist", "ja", "jahr", "kann", "mein", "meine", "meinen", "meiner", "mich", "mir", "mit",
    "nach", "nicht", "noch", "nun", "nur", "ob", "oder", "sein", "sich", "sie", "sind", "so", "ueber", "um", "und",
    "uns", "unser", "unter", "viel", "vom", "von", "vor", "war", "was", "weil", "wenn", "wer", "werden", "wie", "wir",
    "wird", "wo", "zu", "zum", "zur", "the", "and", "for", "of", "to", "is",
];

fn is_stop(w: &str) -> bool {
    static SET: std::sync::OnceLock<std::collections::HashSet<&'static str>> = std::sync::OnceLock::new();
    SET.get_or_init(|| STOP.iter().copied().collect()).contains(w)
}

/// Lower case + German folding + accents dropped (é → e).
pub fn fold(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 2);
    for c in word.chars().flat_map(char::to_lowercase) {
        match c {
            'ä' => out.push_str("ae"),
            'ö' => out.push_str("oe"),
            'ü' => out.push_str("ue"),
            'ß' => out.push_str("ss"),
            'à' | 'á' | 'â' | 'ã' | 'å' => out.push('a'),
            'è' | 'é' | 'ê' | 'ë' => out.push('e'),
            'ì' | 'í' | 'î' | 'ï' => out.push('i'),
            'ò' | 'ó' | 'ô' | 'õ' => out.push('o'),
            'ù' | 'ú' | 'û' => out.push('u'),
            'ç' => out.push('c'),
            'ñ' => out.push('n'),
            c => out.push(c),
        }
    }
    out
}

/// Every term of `text` with its byte offsets: folded whole words, then their 4-grams.
pub fn terms(text: &str, mut f: impl FnMut(String, usize, usize)) {
    let mut start = None;
    let flush = |s: usize, e: usize, f: &mut dyn FnMut(String, usize, usize)| {
        let w = fold(&text[s..e]);
        if w.chars().count() < 2 || is_stop(&w) {
            return;
        }
        let chars: Vec<char> = w.chars().collect();
        if chars.len() >= 5 {
            for i in 0..=chars.len() - 4 {
                f(chars[i..i + 4].iter().collect(), s, e);
            }
        }
        f(w, s, e);
    };
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            flush(s, i, &mut f);
        }
    }
    if let Some(s) = start {
        flush(s, text.len(), &mut f);
    }
}

pub fn term_list(text: &str) -> Vec<String> {
    let mut v = Vec::new();
    terms(text, |t, _, _| v.push(t));
    v
}

/// Whole folded words only (for snippets and "why").
pub fn words(text: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        if c.is_alphanumeric() && i < text.len() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            let w = fold(&text[s..i]);
            if w.chars().count() >= 2 && !is_stop(&w) {
                v.push(w);
            }
        }
    }
    v
}

#[derive(Clone, Default)]
pub struct GermanTokenizer {
    tokens: Vec<Token>,
}

pub struct VecStream<'a> {
    tokens: &'a mut Vec<Token>,
    idx: usize,
}

impl Tokenizer for GermanTokenizer {
    type TokenStream<'a> = VecStream<'a>;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecStream<'a> {
        self.tokens.clear();
        let mut pos = 0;
        let toks = &mut self.tokens;
        terms(text, |t, s, e| {
            toks.push(Token { offset_from: s, offset_to: e, position: pos, text: t, position_length: 1 });
            pos += 1;
        });
        VecStream { tokens: &mut self.tokens, idx: 0 }
    }
}

impl TokenStream for VecStream<'_> {
    fn advance(&mut self) -> bool {
        self.idx += 1;
        self.idx <= self.tokens.len()
    }
    fn token(&self) -> &Token {
        &self.tokens[self.idx - 1]
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.idx - 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_words_are_folded() {
        for w in STOP {
            assert_eq!(fold(w), *w, "stop words must be in folded form");
        }
    }

    #[test]
    fn folding_and_grams() {
        assert_eq!(fold("Größe"), "groesse");
        let t = term_list("Stromvertrag für 2025");
        assert!(t.contains(&"stromvertrag".to_string()));
        assert!(t.contains(&"stro".to_string()) && t.contains(&"trag".to_string()));
        assert!(t.contains(&"2025".to_string()));
        assert!(!t.contains(&"fuer".to_string()));
        // A compound meets its parts.
        let a = term_list("Stromliefervertrag");
        assert!(a.contains(&"stro".to_string()) && a.contains(&"vert".to_string()));
        // ä and ae meet.
        assert_eq!(term_list("Ärzte"), term_list("Aerzte"));
    }

    #[test]
    fn tantivy_stream() {
        let mut tk = GermanTokenizer::default();
        let mut s = tk.token_stream("Kfz-Versicherung");
        let mut n = 0;
        while s.advance() {
            n += 1;
        }
        // "kfz" + 9 grams of "versicherung" + the word.
        assert_eq!(n, 1 + 9 + 1);
    }
}

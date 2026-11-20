//! A provider key in memory. It leaves this type in exactly one place (the request header); everything shown or
//! logged goes through [`Secret::scrub`] first, because a provider may echo the key in an error text.

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// The value itself, for the request header only.
    pub fn expose(&self) -> &str {
        &self.0
    }
    /// `text` with every occurrence of the key replaced.
    pub fn scrub(&self, text: &str) -> String {
        if self.0.is_empty() { text.to_string() } else { text.replace(&self.0, "***") }
    }
}

/// Scrubs a text that arrives in pieces (the answer as the provider streams it): a key cut in two by the end
/// of a piece is still found. Only the end of the text that could be the start of the key is held back, so
/// an ordinary answer is shown as it arrives.
pub struct Pieces<'a> {
    key: &'a Secret,
    held: String,
}

impl<'a> Pieces<'a> {
    pub fn new(key: &'a Secret) -> Pieces<'a> {
        Pieces { key, held: String::new() }
    }

    /// What may be shown now that `piece` arrived.
    pub fn push(&mut self, piece: &str) -> String {
        let key = self.key.0.as_str();
        if key.is_empty() {
            return piece.to_string();
        }
        self.held.push_str(piece);
        let mut out = self.key.scrub(&self.held);
        let keep = key.char_indices().rev().map(|(i, _)| i).find(|&i| i > 0 && out.ends_with(&key[..i])).unwrap_or(0);
        self.held = out.split_off(out.len() - keep);
        out
    }

    /// The answer is over: what was held back is no key.
    pub fn end(&mut self) -> String {
        std::mem::take(&mut self.held)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() { "Secret(empty)" } else { "Secret(***)" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_cut_by_a_piece_boundary_is_still_scrubbed() {
        let key = Secret::new("sk-äb-12");
        for cut in 0..="x sk-äb-12 y s".len() {
            let text = "x sk-äb-12 y s";
            if !text.is_char_boundary(cut) {
                continue;
            }
            let mut p = Pieces::new(&key);
            let shown = [p.push(&text[..cut]), p.push(&text[cut..]), p.end()].concat();
            assert_eq!(shown, "x *** y s", "cut at {cut}");
        }
        let mut p = Pieces::new(&key);
        assert_eq!(p.push("das wars"), "das war", "only what could start the key waits");
        assert_eq!((p.push("!"), p.end()), ("s!".to_string(), String::new()));
        let none = Secret::default();
        let mut p = Pieces::new(&none);
        assert_eq!((p.push("sk-"), p.end()), ("sk-".to_string(), String::new()), "no key, nothing held");
    }
}

//! Fake embedder (DESIGN §6.1): hashed character 3-grams of the folded words into a fixed dimension. No
//! meaning, but deterministic and fast, so the pipeline, the formula and the crash tests run without a model.
//! `CTXD_TEST_EMBED_DELAY_MS` slows every call (the kill -9 test uses it to hit the middle of indexing).

use super::{EmbedError, EmbedKind, Embedder, Health, normalize};
use std::time::Duration;

pub struct Fake {
    dim: usize,
    delay: Duration,
    id: String,
}

impl Fake {
    pub fn new(dim: usize) -> Fake {
        let delay = std::env::var("CTXD_TEST_EMBED_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        Fake { dim, delay: Duration::from_millis(delay), id: format!("fake/ngram3/{dim}") }
    }

    fn one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for w in crate::tokenize::words(text) {
            let padded: Vec<char> = format!(" {w} ").chars().collect();
            for g in padded.windows(3) {
                let h = blake3::hash(g.iter().collect::<String>().as_bytes());
                let b = h.as_bytes();
                let idx = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize % self.dim;
                v[idx] += if b[4] & 1 == 0 { 1.0 } else { -1.0 };
            }
        }
        normalize(&mut v);
        v
    }
}

impl Embedder for Fake {
    fn id(&self) -> &str {
        &self.id
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn max_tokens(&self) -> usize {
        usize::MAX
    }
    fn batch(&self) -> usize {
        32
    }
    fn embed(&self, _kind: EmbedKind, texts: &[&str], _timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        Ok(texts.iter().map(|t| self.one(t)).collect())
    }
    fn health(&self) -> Health {
        Health::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::dot;

    #[test]
    fn similar_texts_are_closer() {
        let f = Fake { dim: 256, delay: Duration::ZERO, id: String::new() };
        let a = f.one("Steuererklärung Lohnsteuer");
        let b = f.one("Lohnsteuerbescheinigung");
        let c = f.one("Apfelkuchen Rezept");
        assert!(dot(&a, &b) > dot(&a, &c));
        assert!((dot(&a, &a) - 1.0).abs() < 1e-5);
    }
}

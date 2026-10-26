//! The embedder trait (DESIGN §6.1) and its back ends: `llama` (llama-server child process on the CPU),
//! `fake` (character n-gram hashing, for tests). The trait takes a timeout per call so a search can keep
//! its deadline (a small addition to the design's signature).

pub mod fake;
pub mod http;
pub mod llama;

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
    Query,
    Passage,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EmbedError {
    Unavailable(String),
    TooLong,
    Timeout,
    Bad(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Unavailable(r) => write!(f, "unavailable: {r}"),
            EmbedError::TooLong => write!(f, "too_long"),
            EmbedError::Timeout => write!(f, "timeout"),
            EmbedError::Bad(r) => write!(f, "bad: {r}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Health {
    Ready,
    Loading,
    Unavailable(String),
}

pub trait Embedder: Send + Sync {
    /// Identity stored with every vector set (model, revision, dimension, prompt scheme); vectors of
    /// different identities are never compared.
    fn id(&self) -> &str;
    fn dim(&self) -> usize;
    fn max_tokens(&self) -> usize;
    fn batch(&self) -> usize;
    /// Unit-length vectors, one per text.
    fn embed(&self, kind: EmbedKind, texts: &[&str], timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError>;
    fn health(&self) -> Health;
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Directory-safe form of an embedder id.
pub fn slug(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect()
}

/// The passage text given to the model: title and file name first (DESIGN §5 "Titel: ... Text: ...").
pub fn passage_text(title: &str, chunk: &str) -> String {
    format!("Titel: {}\nText: {}", crate::util::cut_bytes(title, 200), chunk)
}

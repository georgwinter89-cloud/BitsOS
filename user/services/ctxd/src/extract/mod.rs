//! Extraction (DESIGN §5): sniff by magic bytes, then extension; one extractor per kind. Runs inside a
//! child process (`pool`), so a hostile file ends a worker, never the service.

pub mod mail;
pub mod office;
pub mod pdf;
pub mod plain;
pub mod pool;

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bumped when an extractor changes its output, so cached text is re-extracted.
pub const EXTRACTOR_VERSION: &str = "cx1.2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Markdown,
    Html,
    Mail,
    Pdf,
    Docx,
    Odt,
    Other,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Markdown => "md",
            Kind::Html => "html",
            Kind::Mail => "mail",
            Kind::Pdf => "pdf",
            Kind::Docx => "docx",
            Kind::Odt => "odt",
            Kind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Extracted {
    pub kind: String,
    pub title: Option<String>,
    pub text: String,
    #[serde(default)]
    pub meta: Vec<(String, String)>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Limits against hostile input (documented bounds on a single file, not counts of things).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Upper bound for text taken out of one file (decompressed XML, PDF text, mail parts).
    pub max_text_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_text_bytes: 64 << 20 }
    }
}

pub fn sniff(path: &Path, head: &[u8]) -> Kind {
    if head.starts_with(b"%PDF") {
        return Kind::Pdf;
    }
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    if head.starts_with(b"PK\x03\x04") {
        return match ext.as_str() {
            "docx" => Kind::Docx,
            "odt" => Kind::Odt,
            _ => office::sniff_zip(head).unwrap_or(Kind::Other),
        };
    }
    match ext.as_str() {
        "txt" | "text" | "log" | "csv" => Kind::Text,
        "md" | "markdown" => Kind::Markdown,
        "html" | "htm" | "xhtml" => Kind::Html,
        "eml" => Kind::Mail,
        "pdf" => Kind::Pdf,
        "docx" => Kind::Docx,
        "odt" => Kind::Odt,
        _ => Kind::Other,
    }
}

/// Extract one file's text. Errors are short reasons (`failed:<reason>`), never panics on purpose;
/// the worker also catches panics of the crates underneath.
pub fn extract_bytes(path: &Path, bytes: &[u8], limits: Limits) -> Result<Extracted, String> {
    let kind = sniff(path, &bytes[..bytes.len().min(4096)]);
    let mut ex = match kind {
        Kind::Text => plain::text(bytes),
        Kind::Markdown => plain::markdown(bytes),
        Kind::Html => plain::html(bytes),
        Kind::Mail => mail::eml(bytes, limits)?,
        Kind::Pdf => pdf::pdf(bytes, limits)?,
        Kind::Docx => office::docx(bytes, limits)?,
        Kind::Odt => office::odt(bytes, limits)?,
        Kind::Other => Extracted::default(),
    };
    ex.kind = kind.name().to_string();
    if ex.text.len() > limits.max_text_bytes {
        ex.text = crate::util::cut_bytes(&ex.text, limits.max_text_bytes).to_string();
        ex.warnings.push("text_truncated".into());
    }
    ex.text = crate::textnorm::normalize(&ex.text);
    ex.title = ex.title.map(|t| crate::textnorm::normalize(&t).replace('\n', " ")).filter(|t| !t.is_empty());
    Ok(ex)
}

/// The title shown and indexed: document title, else the file name without extension.
pub fn display_title(ex_title: Option<&str>, path: &Path) -> String {
    match ex_title {
        Some(t) if !t.trim().is_empty() => crate::snippet::cut_chars(t.trim(), 160),
        _ => path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing() {
        assert_eq!(sniff(Path::new("a.bin"), b"%PDF-1.4"), Kind::Pdf);
        assert_eq!(sniff(Path::new("a.TXT"), b"hi"), Kind::Text);
        assert_eq!(sniff(Path::new("a.eml"), b"From: x"), Kind::Mail);
        assert_eq!(sniff(Path::new("a.jpg"), b"\xff\xd8"), Kind::Other);
        assert_eq!(sniff(Path::new("a.docx"), b"PK\x03\x04"), Kind::Docx);
    }
}

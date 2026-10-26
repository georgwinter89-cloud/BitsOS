//! PDF text layer with the pure-Rust `pdf-extract` crate (V2 must not depend on `pdftotext`; the CX-1 notes
//! compare both). Scans without a text layer give empty text (`empty`), OCR is CX-7.

use super::{Extracted, Limits};

pub fn pdf(bytes: &[u8], limits: Limits) -> Result<Extracted, String> {
    let pages = pdf_extract::extract_text_from_mem_by_pages(bytes).map_err(|e| {
        let s = e.to_string();
        format!("pdf: {}", crate::util::cut_bytes(&s, 120))
    })?;
    let mut text = String::new();
    let mut warnings = Vec::new();
    for p in &pages {
        if text.len() + p.len() > limits.max_text_bytes {
            warnings.push("text_truncated".into());
            break;
        }
        text.push_str(p);
        text.push_str("\n\n");
    }
    let title =
        text.lines().map(str::trim).find(|l| !l.is_empty()).filter(|l| l.chars().count() <= 100).map(String::from);
    let meta = vec![("pages".to_string(), pages.len().to_string())];
    Ok(Extracted { title, text, meta, warnings, ..Default::default() })
}

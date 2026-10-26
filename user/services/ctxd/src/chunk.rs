//! Chunking (DESIGN §5): windows of about `target` bytes with `overlap`, built from units (lines and
//! sentences) so an address line or a sentence is not split when avoidable. A short text is one chunk; the
//! number of chunks per document is unbounded.

/// Byte ranges of the units: a unit ends after a line break or after ". ", "! ", "? ".
fn units(text: &str, target: usize) -> Vec<(usize, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut s = 0;
    let mut i = 0;
    while i < b.len() {
        let end_here = b[i] == b'\n' || (matches!(b[i], b'.' | b'!' | b'?') && b.get(i + 1) == Some(&b' '));
        if end_here {
            push_unit(text, s, i + 1, target, &mut out);
            s = i + 1;
        }
        i += 1;
    }
    if s < b.len() {
        push_unit(text, s, b.len(), target, &mut out);
    }
    out
}

/// A unit longer than the target (a run-on line) is split at spaces, or hard at a char boundary.
fn push_unit(text: &str, mut s: usize, e: usize, target: usize, out: &mut Vec<(usize, usize)>) {
    while e - s > target {
        let mut cut = s + target;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(sp) = text[s..cut].rfind(' ').filter(|&p| p > target / 2) {
            cut = s + sp + 1;
        }
        out.push((s, cut));
        s = cut;
    }
    if e > s {
        out.push((s, e));
    }
}

/// Chunk byte ranges over `text`. Ranges are trimmed of surrounding whitespace and never empty.
pub fn chunk(text: &str, target: usize, overlap: usize) -> Vec<(usize, usize)> {
    let u = units(text, target);
    let mut out = Vec::new();
    let mut i = 0;
    while i < u.len() {
        let start = u[i].0;
        let mut j = i;
        while j + 1 < u.len() && u[j + 1].1 - start <= target {
            j += 1;
        }
        if let Some(r) = trim(text, start, u[j].1) {
            out.push(r);
        }
        if j + 1 >= u.len() {
            break;
        }
        // Next window starts at the earliest unit that still fits into the overlap, always moving forward.
        let end = u[j].1;
        let mut k = j + 1;
        while k - 1 > i && end - u[k - 1].0 <= overlap {
            k -= 1;
        }
        i = k;
    }
    out
}

fn trim(text: &str, s: usize, e: usize) -> Option<(usize, usize)> {
    let t = &text[s..e];
    let lead = t.len() - t.trim_start().len();
    let tail = t.len() - t.trim_end().len();
    (lead + tail < t.len()).then(|| (s + lead, e - tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk("Hallo Welt.", 100, 20), vec![(0, 11)]);
        assert!(chunk("   \n ", 100, 20).is_empty());
    }

    #[test]
    fn windows_overlap_and_cover() {
        let text: String = (0..60).map(|i| format!("Satz Nummer {i} steht hier. ")).collect();
        let ch = chunk(&text, 200, 60);
        assert!(ch.len() > 5);
        for w in ch.windows(2) {
            assert!(w[1].0 > w[0].0, "progress");
            assert!(w[1].0 < w[0].1, "overlap");
        }
        for (s, e) in &ch {
            assert!(e - s <= 200);
        }
        assert_eq!(ch.last().unwrap().1, text.trim_end().len());
    }

    #[test]
    fn long_line_and_multibyte() {
        let text = "ä".repeat(500);
        let ch = chunk(&text, 101, 20);
        for (s, e) in &ch {
            assert!(text.is_char_boundary(*s) && text.is_char_boundary(*e));
        }
        assert_eq!(ch.last().unwrap().1, text.len());
    }
}

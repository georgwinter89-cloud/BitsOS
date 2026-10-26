//! Plain text, Markdown, HTML. Charset: UTF-8 first, then Windows-1252 (a superset of ISO-8859-1 for the
//! printable range) for old German files.

use super::Extracted;

pub fn decode(bytes: &[u8]) -> (String, Option<&'static str>) {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), None),
        Err(_) => {
            let (s, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
            (s.into_owned(), Some("charset_windows_1252"))
        }
    }
}

fn with_warning(mut e: Extracted, w: Option<&str>) -> Extracted {
    if let Some(w) = w {
        e.warnings.push(w.to_string());
    }
    e
}

/// The first line counts as a heading when it is short (letters and notes often start with their subject).
pub fn text(bytes: &[u8]) -> Extracted {
    let (s, w) = decode(bytes);
    let title = s.lines().map(str::trim).find(|l| !l.is_empty()).filter(|l| l.chars().count() <= 100).map(String::from);
    with_warning(Extracted { title, text: s, ..Default::default() }, w)
}

pub fn markdown(bytes: &[u8]) -> Extracted {
    let (s, w) = decode(bytes);
    let title = s
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .or_else(|| s.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from));
    // Markup characters carry no words; keep the text otherwise as it is.
    let text: String = s
        .lines()
        .map(|l| l.trim_start_matches(['#', '>', '*', '-', ' ']).replace("**", "").replace('`', ""))
        .collect::<Vec<_>>()
        .join("\n");
    with_warning(Extracted { title, text, ..Default::default() }, w)
}

const BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "br",
    "li",
    "tr",
    "td",
    "th",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "table",
    "ul",
    "ol",
    "section",
    "article",
    "header",
    "footer",
    "title",
    "blockquote",
    "pre",
    "hr",
];

/// Visible text and `<title>`; script, style and comments are dropped.
pub fn html(bytes: &[u8]) -> Extracted {
    let (s, w) = decode(bytes);
    let mut out = String::with_capacity(s.len());
    let mut title = None;
    let lower = s.to_ascii_lowercase();
    let mut i = 0;
    while i < s.len() {
        if lower[i..].starts_with("<!--") {
            i = lower[i..].find("-->").map(|p| i + p + 3).unwrap_or(s.len());
        } else if lower[i..].starts_with("<script") || lower[i..].starts_with("<style") {
            let close = if lower[i..].starts_with("<script") { "</script" } else { "</style" };
            i = lower[i..].find(close).map(|p| i + p + close.len()).unwrap_or(s.len());
            i = lower[i..].find('>').map(|p| i + p + 1).unwrap_or(s.len());
        } else if s.as_bytes()[i] == b'<' {
            let end = lower[i..].find('>').map(|p| i + p + 1).unwrap_or(s.len());
            let inner = &lower[(i + 1).min(end)..end];
            let tag = inner
                .strip_suffix('>')
                .unwrap_or(inner)
                .trim_start_matches('/')
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap_or("")
                .to_string();
            if tag == "title"
                && !lower[i + 1..].starts_with('/')
                && title.is_none()
                && let Some(p) = lower[end..].find("</title")
            {
                title = Some(entities(&s[end..end + p]).trim().to_string());
            }
            if BLOCK_TAGS.contains(&tag.as_str()) {
                out.push('\n');
            } else {
                out.push(' ');
            }
            i = end;
        } else {
            let next = s[i..].find('<').map(|p| i + p).unwrap_or(s.len());
            out.push_str(&entities(&s[i..next]));
            i = next;
        }
    }
    with_warning(Extracted { title, text: out, ..Default::default() }, w)
}

pub fn entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let end = rest[1..].find(';').map(|e| e + 1).filter(|&e| e <= 10);
        let decoded = end.and_then(|e| entity(&rest[1..e]).map(|c| (c, e)));
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(n) = name.strip_prefix('#') {
        let v = match n.strip_prefix(['x', 'X']) {
            Some(h) => u32::from_str_radix(h, 16).ok()?,
            None => n.parse().ok()?,
        };
        return char::from_u32(v);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "Auml" => 'Ä',
        "Ouml" => 'Ö',
        "Uuml" => 'Ü',
        "szlig" => 'ß',
        "euro" => '€',
        "eacute" => 'é',
        "ndash" => '–',
        "mdash" => '—',
        "bdquo" => '„',
        "ldquo" => '“',
        "rdquo" => '”',
        "sect" => '§',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_visible_text_only() {
        let h = b"<html><head><title>Reise &amp; Plan</title><style>p{x:1}</style></head>\
                  <body><script>var a='<p>';</script><p>Rom im M&auml;rz</p><!-- hidden --><p>Ende&#8364;</p></body></html>";
        let e = html(h);
        assert_eq!(e.title.as_deref(), Some("Reise & Plan"));
        assert!(e.text.contains("Rom im März"));
        assert!(e.text.contains("Ende€"));
        assert!(!e.text.contains("var a") && !e.text.contains("hidden") && !e.text.contains("x:1"));
    }

    #[test]
    fn windows_1252_fallback() {
        let e = text(b"Gr\xfc\xdfe aus K\xf6ln");
        assert_eq!(e.text, "Grüße aus Köln");
        assert_eq!(e.warnings, vec!["charset_windows_1252"]);
    }

    #[test]
    fn hostile_html_does_not_panic() {
        for h in [&b"<"[..], b"<title>", b"<script", b"&#xFFFFFFFF;", b"&;&&#;<!--", b"<a <b <c>>"] {
            let _ = html(h);
        }
    }

    #[test]
    fn markdown_title() {
        let e = markdown("Vorspann\n# Urlaub in Italien\n- **Rom**".as_bytes());
        assert_eq!(e.title.as_deref(), Some("Urlaub in Italien"));
        assert!(e.text.contains("Rom"));
    }
}

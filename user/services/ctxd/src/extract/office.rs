//! DOCX and ODT: zip + XML, paragraphs and table cells. Every read from the archive is bounded, so a zip
//! bomb ends as `text_truncated` or `failed:too_large`, never as an exhausted machine.

use super::{Extracted, Kind, Limits};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::io::{Cursor, Read};

/// From the first bytes only: the ODF `mimetype` entry is stored first and uncompressed.
pub fn sniff_zip(head: &[u8]) -> Option<Kind> {
    let h = String::from_utf8_lossy(head);
    if h.contains("application/vnd.oasis.opendocument.text") {
        Some(Kind::Odt)
    } else if h.contains("word/") || h.contains("[Content_Types].xml") {
        Some(Kind::Docx)
    } else {
        None
    }
}

fn read_entry(bytes: &[u8], name: &str, limit: usize) -> Result<(Vec<u8>, bool), String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("zip: {e}"))?;
    let entry = zip.by_name(name).map_err(|_| format!("zip: no {name}"))?;
    let mut out = Vec::new();
    entry.take(limit as u64 + 1).read_to_end(&mut out).map_err(|e| format!("zip read: {e}"))?;
    let truncated = out.len() > limit;
    out.truncate(limit);
    Ok((out, truncated))
}

/// Collect text of `text_tag` elements; `para_tags` end a paragraph, `cell_tags` end a cell, `tab_tags`
/// and `break_tags` become a space/newline.
fn xml_text(xml: &[u8], text_tags: &[&str], para_tags: &[&str], cell_tags: &[&str], limit: usize) -> String {
    let mut r = Reader::from_reader(xml);
    let mut out = String::new();
    let mut depth_in_text = 0usize;
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let n = e.local_name();
                let n: &str = n.as_ref();
                if text_tags.contains(&n) {
                    depth_in_text += 1;
                }
            }
            Ok(Event::End(e)) => {
                let n = e.local_name();
                let n: &str = n.as_ref();
                if text_tags.contains(&n) {
                    depth_in_text = depth_in_text.saturating_sub(1);
                }
                if para_tags.contains(&n) {
                    out.push('\n');
                } else if cell_tags.contains(&n) {
                    out.push('\t');
                }
            }
            Ok(Event::Empty(e)) => {
                let n = e.local_name();
                match AsRef::<str>::as_ref(&n) {
                    "tab" | "s" => out.push(' '),
                    "br" | "line-break" | "cr" => out.push('\n'),
                    _ => {}
                }
            }
            Ok(Event::Text(t)) if depth_in_text > 0 => {
                let raw: &str = t.as_ref();
                out.push_str(&super::plain::entities(raw));
            }
            Ok(Event::GeneralRef(g)) if depth_in_text > 0 => {
                let raw: &str = &g;
                out.push_str(&super::plain::entities(&format!("&{raw};")));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        if out.len() > limit {
            break;
        }
        buf.clear();
    }
    out
}

pub fn docx(bytes: &[u8], limits: Limits) -> Result<Extracted, String> {
    let (xml, truncated) = read_entry(bytes, "word/document.xml", limits.max_text_bytes)?;
    let text = xml_text(&xml, &["t"], &["p"], &["tc"], limits.max_text_bytes);
    let title = core_title(bytes, "docProps/core.xml");
    Ok(finish(text, title, truncated))
}

pub fn odt(bytes: &[u8], limits: Limits) -> Result<Extracted, String> {
    let (xml, truncated) = read_entry(bytes, "content.xml", limits.max_text_bytes)?;
    // In ODF the text sits directly in text:p / text:h (with spans inside).
    let text = xml_text(&xml, &["p", "h"], &["p", "h"], &["table-cell"], limits.max_text_bytes);
    let title = core_title(bytes, "meta.xml");
    Ok(finish(text, title, truncated))
}

fn core_title(bytes: &[u8], entry: &str) -> Option<String> {
    let (xml, _) = read_entry(bytes, entry, 1 << 20).ok()?;
    let t = xml_text(&xml, &["title"], &[], &[], 4096);
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn finish(text: String, title: Option<String>, truncated: bool) -> Extracted {
    let title = title.or_else(|| {
        text.lines().map(str::trim).find(|l| !l.is_empty()).filter(|l| l.chars().count() <= 100).map(String::from)
    });
    let warnings = if truncated { vec!["text_truncated".to_string()] } else { vec![] };
    Extracted { title, text, warnings, ..Default::default() }
}

#[cfg(test)]
pub mod testdata {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    pub fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in files {
            w.start_file(*name, SimpleFileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::testdata::zip_of;
    use super::*;

    #[test]
    fn docx_paragraphs_and_cells() {
        let doc = br#"<w:document xmlns:w="x"><w:body><w:p><w:r><w:t>Mietvertrag</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>Kaltmiete</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t xml:space="preserve">850 &amp; mehr</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
        let z = zip_of(&[("[Content_Types].xml", b"<x/>"), ("word/document.xml", doc)]);
        let e = docx(&z, Limits::default()).unwrap();
        assert!(
            e.text.contains("Mietvertrag") && e.text.contains("Kaltmiete") && e.text.contains("850 & mehr"),
            "{}",
            e.text
        );
        assert_eq!(e.title.as_deref(), Some("Mietvertrag"));
    }

    #[test]
    fn odt_text() {
        let c = br#"<office:document-content xmlns:text="t" xmlns:office="o"><office:body><office:text>
            <text:h>Gartenplan</text:h><text:p>Tomaten <text:span>und</text:span><text:s/>Gurken</text:p></office:text></office:body></office:document-content>"#;
        let z = zip_of(&[("mimetype", b"application/vnd.oasis.opendocument.text"), ("content.xml", c)]);
        assert_eq!(super::super::sniff(std::path::Path::new("a.odt"), &z), Kind::Odt);
        assert_eq!(sniff_zip(b"PK\x03\x04....mimetypeapplication/vnd.oasis.opendocument.text"), Some(Kind::Odt));
        let e = odt(&z, Limits::default()).unwrap();
        assert!(e.text.contains("Gartenplan") && e.text.contains("Tomaten und Gurken"), "{}", e.text);
    }

    #[test]
    fn zip_bomb_is_bounded() {
        let body = vec![b'a'; 8 << 20];
        let mut doc = b"<w:document><w:p><w:t>".to_vec();
        doc.extend_from_slice(&body);
        let z = zip_of(&[("word/document.xml", &doc)]);
        assert!(z.len() < 200_000, "compresses well: {}", z.len());
        let e = docx(&z, Limits { max_text_bytes: 1 << 20 }).unwrap();
        assert!(e.text.len() <= (1 << 20) + 64);
        assert!(e.warnings.contains(&"text_truncated".to_string()));
    }

    #[test]
    fn broken_zip_fails_cleanly() {
        assert!(docx(b"PK\x03\x04garbage", Limits::default()).is_err());
        assert!(odt(b"", Limits::default()).is_err());
    }
}

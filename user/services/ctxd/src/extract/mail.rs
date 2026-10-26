//! `.eml`: headers (RFC 2047 words), MIME parts, quoted-printable and base64, charsets (mail-parser).
//! Attachments are listed by name; their content is not indexed in CX-1 (`context.index.attachments`
//! comes later, DESIGN §5).

use super::{Extracted, Limits};
use mail_parser::{Address, MessageParser, MimeHeaders};

fn addr(a: Option<&Address>) -> Option<String> {
    let a = a?;
    let list: Vec<String> = a
        .iter()
        .map(|x| match (x.name(), x.address()) {
            (Some(n), Some(e)) => format!("{n} <{e}>"),
            (Some(n), None) => n.to_string(),
            (None, Some(e)) => e.to_string(),
            (None, None) => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then(|| list.join(", "))
}

pub fn eml(bytes: &[u8], limits: Limits) -> Result<Extracted, String> {
    let msg = MessageParser::default().parse(bytes).ok_or("mail_unparsable")?;
    let mut ex = Extracted::default();
    let mut head = String::new();
    let subject = msg.subject().map(str::to_string);
    if let Some(s) = &subject {
        head.push_str(&format!("Betreff: {s}\n"));
    }
    if let Some(f) = addr(msg.from()) {
        head.push_str(&format!("Von: {f}\n"));
        ex.meta.push(("from".into(), f));
    }
    if let Some(t) = addr(msg.to()) {
        head.push_str(&format!("An: {t}\n"));
        ex.meta.push(("to".into(), t));
    }
    if let Some(d) = msg.date() {
        ex.meta.push(("date".into(), d.to_rfc3339()));
        head.push_str(&format!("Datum: {}\n", d.to_rfc3339().get(..10).unwrap_or("")));
    }
    let mut text = head;
    text.push('\n');
    for i in 0..msg.text_body_count() {
        if let Some(b) = msg.body_text(i) {
            if text.len() + b.len() > limits.max_text_bytes {
                ex.warnings.push("text_truncated".into());
                break;
            }
            text.push_str(&b);
            text.push_str("\n\n");
        }
    }
    let names: Vec<String> = msg.attachments().filter_map(|a| a.attachment_name().map(String::from)).collect();
    if !names.is_empty() {
        text.push_str(&format!("Anhänge: {}\n", names.join(", ")));
        ex.meta.push(("attachments".into(), names.join(", ")));
    }
    ex.title = subject;
    ex.text = text;
    Ok(ex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_words_and_qp() {
        let m = b"From: =?utf-8?q?J=C3=BCrgen?= <j@example.com>\r\nTo: a@example.com\r\n\
                  Subject: =?iso-8859-1?q?Gr=FC=DFe_aus_K=F6ln?=\r\nDate: Mon, 3 Mar 2025 10:00:00 +0100\r\n\
                  MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\
                  Content-Transfer-Encoding: quoted-printable\r\n\r\nSch=C3=B6ne Gr=C3=BC=C3=9Fe\r\n";
        let e = eml(m, Limits::default()).unwrap();
        assert_eq!(e.title.as_deref(), Some("Grüße aus Köln"));
        assert!(e.text.contains("Schöne Grüße"), "{}", e.text);
        assert!(e.text.contains("Jürgen"));
        assert!(e.meta.iter().any(|(k, v)| k == "date" && v.starts_with("2025-03-03")));
    }

    #[test]
    fn many_headers_do_not_hurt() {
        let mut m = String::new();
        for i in 0..10_000 {
            m.push_str(&format!("X-H{i}: v\r\n"));
        }
        m.push_str("Subject: s\r\n\r\nbody\r\n");
        let e = eml(m.as_bytes(), Limits::default()).unwrap();
        assert!(e.text.contains("body"));
    }
}

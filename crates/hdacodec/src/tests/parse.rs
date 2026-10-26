//! The dump text format.

use super::ALC269;
use crate::dump::{Dump, Line, ParseError, ParseErrorKind};
use crate::walk::check;

fn err(text: &str) -> ParseError {
    Dump::parse(text).unwrap_err()
}

#[test]
fn header_and_lines() {
    let d = Dump::parse("# format: hdacodec-dump 1\n# synthetic: no\n#plain comment\n\n0 00 F0000 10EC0269\n2 1f 0xf0009 -\n").unwrap();
    assert_eq!(d.header_value("format"), Some("hdacodec-dump 1"));
    assert!(!d.is_synthetic());
    assert_eq!(d.get(0, 0, 0xF0000), Some(Some(0x10EC_0269)));
    assert_eq!(d.get(2, 0x1F, 0xF0009), Some(None));
    assert_eq!(d.get(2, 0x1F, 0xF000C), None);
    assert_eq!(d.codecs(), [0, 2]);
}

#[test]
fn round_trip() {
    let d = Dump::parse(ALC269).unwrap();
    assert!(d.is_synthetic());
    assert_eq!(d.to_text(), ALC269);
}

#[test]
fn errors_name_the_line() {
    assert_eq!(
        err("0 00 F0000\n"),
        ParseError {
            line: 1,
            kind: ParseErrorKind::FieldCount
        }
    );
    assert_eq!(err("\n0 00 F0000 1 2\n").line, 2);
    assert_eq!(
        err("0 00 F0000 123456789\n").kind,
        ParseErrorKind::BadNumber
    );
    assert_eq!(err("10 00 F0000 0\n").kind, ParseErrorKind::BadNumber);
    assert_eq!(err("0 80 F0000 0\n").kind, ParseErrorKind::NidOutOfRange);
    assert_eq!(err("0 00 G0000 0\n").kind, ParseErrorKind::BadNumber);
    assert_eq!(err("0 01 70500 0\n").kind, ParseErrorKind::NotReadOnly);
    assert_eq!(err("0 01 3B000 0\n").kind, ParseErrorKind::NotReadOnly);
    assert_eq!(
        err("0 00 F0000 1\n0 00 F0000 2\n"),
        ParseError {
            line: 2,
            kind: ParseErrorKind::Conflict
        }
    );
    assert!(Dump::parse("0 00 F0000 1\n0 00 F0000 1\n").is_ok());
}

#[test]
fn check_finds_missing_and_extra() {
    let mut text: std::string::String = ALC269
        .lines()
        .filter(|l| !l.starts_with("0 15 F1C00"))
        .collect::<std::vec::Vec<_>>()
        .join("\n");
    text.push_str("\n0 15 F0F00 00000000\n");
    let d = Dump::parse(&text).unwrap();
    let c = check(&d, 0);
    assert_eq!(c.missing, [(0x15, 0xF1C00)]);
    assert_eq!(c.extra, [(0x15, 0xF0F00)]);
}

#[test]
fn push_checks_like_parse() {
    let mut d = Dump::new();
    assert_eq!(
        d.push(Line {
            cad: 16,
            nid: 0,
            verb: 0xF0000,
            answer: None
        }),
        Err(ParseErrorKind::CadOutOfRange)
    );
    assert_eq!(
        d.push(Line {
            cad: 0,
            nid: 0,
            verb: 0x7FFFFF,
            answer: None
        }),
        Err(ParseErrorKind::NotReadOnly)
    );
}

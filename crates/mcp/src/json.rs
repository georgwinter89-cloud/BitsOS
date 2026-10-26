//! A small JSON value with parser and serializer (no_std + alloc), strict RFC 8259 like V1's `json.asm`:
//! exact number grammar, no lone surrogates, control characters refused, nesting bounded. Equal keys: the last
//! one wins (`get`). `parse_with_members` also gives the raw text of the root object's members, so a
//! JSON-RPC id goes back byte for byte as it came.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    /// Keeps key order (MCP answers are easier to read that way).
    Obj(Vec<(String, Json)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub at: usize,
}

/// A member of the root object: its key and the byte range of its value in the parsed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub key: String,
    pub start: usize,
    pub end: usize,
}

/// Containers nested deeper than this (the root counts) are refused: a bound against hostile input, the same
/// depth V1 accepted, far beyond any real MCP request.
pub const MAX_DEPTH: usize = 64;

impl Json {
    pub fn parse(s: &str) -> Result<Json, ParseError> {
        Self::parse_inner(s, None)
    }

    pub fn parse_with_members(s: &str) -> Result<(Json, Vec<Member>), ParseError> {
        let mut m = Vec::new();
        let v = Self::parse_inner(s, Some(&mut m))?;
        Ok((v, m))
    }

    fn parse_inner(s: &str, members: Option<&mut Vec<Member>>) -> Result<Json, ParseError> {
        let mut p = P { b: s.as_bytes(), i: 0, members };
        let v = p.value(0)?;
        p.ws();
        if p.i != p.b.len() {
            return Err(ParseError { at: p.i });
        }
        Ok(v)
    }

    /// The member `key` of an object; with equal keys the last one.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(v) => v.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        if let Json::Str(s) = self { Some(s) } else { None }
    }
    pub fn as_f64(&self) -> Option<f64> {
        if let Json::Num(n) = self { Some(*n) } else { None }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) if *n >= 0.0 && *n <= 9.007_199_254_740_992e15 && (*n as u64) as f64 == *n => Some(*n as u64),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(n) if n.abs() <= 9.007_199_254_740_992e15 && (*n as i64) as f64 == *n => Some(*n as i64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Json::Bool(b) = self { Some(*b) } else { None }
    }
    pub fn as_array(&self) -> Option<&[Json]> {
        if let Json::Arr(a) = self { Some(a) } else { None }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (String::from(k), v)).collect())
    }
    pub fn str(s: &str) -> Json {
        Json::Str(String::from(s))
    }
    /// G0 API: callers in no_std crates use it without importing `ToString`.
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut s = String::new();
        self.write_to(&mut s);
        s
    }
    pub fn write_to(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => {
                if *n > -1e15 && *n < 1e15 && (*n as i64) as f64 == *n {
                    let _ = write!(out, "{}", *n as i64);
                } else if n.is_finite() {
                    let _ = write!(out, "{n}");
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => escape_into(s, out),
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_to(out);
                }
                out.push(']');
            }
            Json::Obj(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    escape_into(k, out);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }
}

/// `s` as a JSON string literal (quotes included) appended to `out`.
pub fn escape_into(s: &str, out: &mut String) {
    out.reserve(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct P<'a, 'm> {
    b: &'a [u8],
    i: usize,
    members: Option<&'m mut Vec<Member>>,
}

impl P<'_, '_> {
    fn err<T>(&self) -> Result<T, ParseError> {
        Err(ParseError { at: self.i })
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }
    fn lit(&mut self, s: &[u8], v: Json) -> Result<Json, ParseError> {
        if self.b[self.i..].starts_with(s) {
            self.i += s.len();
            Ok(v)
        } else {
            self.err()
        }
    }
    fn value(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.ws();
        match self.peek() {
            Some(b'n') => self.lit(b"null", Json::Null),
            Some(b't') => self.lit(b"true", Json::Bool(true)),
            Some(b'f') => self.lit(b"false", Json::Bool(false)),
            Some(b'"') => self.string().map(Json::Str),
            Some(b'[') if depth < MAX_DEPTH => self.array(depth),
            Some(b'{') if depth < MAX_DEPTH => self.object(depth),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => self.err(),
        }
    }
    fn array(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.i += 1;
        let mut v = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(v));
        }
        loop {
            v.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(v));
                }
                _ => return self.err(),
            }
        }
    }
    fn object(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.i += 1;
        let mut v = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(v));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return self.err();
            }
            let k = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return self.err();
            }
            self.i += 1;
            self.ws();
            let start = self.i;
            let val = self.value(depth + 1)?;
            if depth == 0
                && let Some(m) = self.members.as_deref_mut()
            {
                m.push(Member { key: k.clone(), start, end: self.i });
            }
            v.push((k, val));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(v));
                }
                _ => return self.err(),
            }
        }
    }
    fn digits(&mut self) -> usize {
        let s = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - s
    }
    /// RFC 8259: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return self.err(),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if self.digits() == 0 {
                return self.err();
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.digits() == 0 {
                return self.err();
            }
        }
        let s = core::str::from_utf8(&self.b[start..self.i]).map_err(|_| ParseError { at: start })?;
        s.parse::<f64>().map(Json::Num).map_err(|_| ParseError { at: start })
    }
    fn hex4(&mut self) -> Result<u32, ParseError> {
        let h = self.b.get(self.i..self.i + 4).ok_or(ParseError { at: self.i })?;
        if !h.iter().all(u8::is_ascii_hexdigit) {
            return self.err();
        }
        let s = core::str::from_utf8(h).map_err(|_| ParseError { at: self.i })?;
        let v = u32::from_str_radix(s, 16).map_err(|_| ParseError { at: self.i })?;
        self.i += 4;
        Ok(v)
    }
    fn escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        let Some(e) = self.peek() else { return self.err() };
        self.i += 1;
        let c = match e {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let hi = self.hex4()?;
                let cp = match hi {
                    0xD800..=0xDBFF => {
                        if self.b.get(self.i..self.i + 2) != Some(b"\\u") {
                            return self.err();
                        }
                        self.i += 2;
                        let lo = self.hex4()?;
                        if !(0xDC00..=0xDFFF).contains(&lo) {
                            return self.err();
                        }
                        0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                    }
                    0xDC00..=0xDFFF => return self.err(),
                    c => c,
                };
                char::from_u32(cp).ok_or(ParseError { at: self.i })?
            }
            _ => return self.err(),
        };
        out.push(c);
        Ok(())
    }
    fn string(&mut self) -> Result<String, ParseError> {
        self.i += 1;
        let mut out = String::new();
        loop {
            // Copy the plain run up to the next quote, backslash or control byte in one go.
            let run = self.b[self.i..].iter().position(|&c| c == b'"' || c == b'\\' || c < 0x20);
            let Some(n) = run else { return Err(ParseError { at: self.b.len() }) };
            // The input is a &str and the run ends before an ASCII byte, so it is whole UTF-8.
            out.push_str(core::str::from_utf8(&self.b[self.i..self.i + n]).map_err(|_| ParseError { at: self.i })?);
            self.i += n;
            match self.b[self.i] {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    self.escape(&mut out)?;
                }
                _ => return self.err(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::format;

    #[test]
    fn round_trip_and_hostile() {
        let s = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"log","arguments":{"lines":20,"x":[true,null,-1.5,"äü\n"]}}}"#;
        let v = Json::parse(s).unwrap();
        assert_eq!(v.get("id").and_then(Json::as_u64), Some(7));
        assert_eq!(v.get("params").unwrap().get("name").unwrap().as_str(), Some("log"));
        let again = Json::parse(&v.to_string()).unwrap();
        assert_eq!(v, again);
        assert!(Json::parse("{").is_err());
        assert!(Json::parse("[1,]").is_err());
        assert!(Json::parse("\"\x01\"").is_err());
        let deep = "[".repeat(10_000);
        assert!(Json::parse(&deep).is_err(), "depth limit, no stack overflow");
        assert_eq!(Json::parse(r#""😀""#).unwrap().as_str(), Some("😀"));
    }

    #[test]
    fn numbers_strict() {
        for bad in ["01", "-01", "1.", ".5", "-", "+1", "1e", "1e+", "1.e1", "0x10", "Infinity", "NaN", "1_0", "0.5.1", "--1", "1e1.5"] {
            assert!(Json::parse(bad).is_err(), "{bad}");
        }
        for good in ["0", "-0", "1e999999", "-1.5E-300", "123456789012345678901234567890", "0.000", "1e+5"] {
            assert!(Json::parse(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn escapes_and_surrogates() {
        assert_eq!(Json::parse(r#""😀ä""#).unwrap().as_str(), Some("😀ä"));
        for bad in [r"\x41", r"\u12", r"\u12G4", r"\u 123", r"\uD800", r"\uD800x", r"\uD800A", r"\uDC00", r"\uDBFF\uDBFF", r"\'", r"\0", "\\", r"\uD83D\", r"\uD83D\u"] {
            assert!(Json::parse(&format!("\"{bad}\"")).is_err(), "{bad}");
        }
        for c in [0u8, 1, 9, 10, 13, 0x1F] {
            let s = format!("\"a{}b\"", c as char);
            assert!(Json::parse(&s).is_err());
        }
    }

    #[test]
    fn depth_64_counts_the_root() {
        let ok = format!("{}{}", "[".repeat(64), "]".repeat(64));
        let bad = format!("{}{}", "[".repeat(65), "]".repeat(65));
        assert!(Json::parse(&ok).is_ok());
        assert!(Json::parse(&bad).is_err());
        let objs = format!("{}1{}", "{\"b\":".repeat(64), "}".repeat(64));
        assert!(Json::parse(&objs).is_ok(), "a scalar inside 64 containers is fine");
    }

    #[test]
    fn last_key_wins_and_members() {
        let s = r#" { "id" : 1 , "x":[1], "id": "a\"b" } "#;
        let (v, m) = Json::parse_with_members(s).unwrap();
        assert_eq!(v.get("id").unwrap().as_str(), Some("a\"b"));
        let id = m.iter().rev().find(|m| m.key == "id").unwrap();
        assert_eq!(&s[id.start..id.end], r#""a\"b""#);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn whitespace_only_rfc() {
        for bad in ["", " ", "\u{a0}{}", "\u{feff}{}", "{} x", "{}{}"] {
            assert!(Json::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Json::parse(" \r\n\t{ \"x\" : [ 1 , { } ] } \n").is_ok());
    }
}

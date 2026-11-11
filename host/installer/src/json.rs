//! The little JSON the release manifest needs: objects, arrays, strings, unsigned integers, booleans and null.
//! Strict: anything else (floats, trailing garbage, duplicate keys) is an error, since a manifest that parses two
//! ways could check one asset and install another.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr(Vec<Value>),
    Obj(BTreeMap<String, Value>),
}

impl Value {
    pub fn get(&self, k: &str) -> Option<&Value> {
        match self {
            Value::Obj(m) => m.get(k),
            _ => None,
        }
    }
    pub fn str(&self, k: &str) -> Result<&str, String> {
        match self.get(k) {
            Some(Value::Str(s)) => Ok(s),
            _ => Err(format!("\"{k}\" is not a string")),
        }
    }
    pub fn num(&self, k: &str) -> Result<u64, String> {
        match self.get(k) {
            Some(Value::Num(n)) => Ok(*n),
            _ => Err(format!("\"{k}\" is not a number")),
        }
    }
}

const MAX_DEPTH: usize = 16;

pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser { b: text.as_bytes(), i: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("trailing bytes at offset {}", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("{what} at offset {}", self.i))
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn word(&mut self, w: &str, v: Value) -> Result<Value, String> {
        if self.b[self.i..].starts_with(w.as_bytes()) {
            self.i += w.len();
            Ok(v)
        } else {
            self.err("unknown word")
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return self.err("nested too deep");
        }
        self.ws();
        match self.b.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut m = BTreeMap::new();
                if self.eat(b'}') {
                    return Ok(Value::Obj(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    if !self.eat(b':') {
                        return self.err("expected ':'");
                    }
                    let v = self.value(depth + 1)?;
                    if m.insert(k.clone(), v).is_some() {
                        return Err(format!("duplicate key \"{k}\""));
                    }
                    if self.eat(b'}') {
                        return Ok(Value::Obj(m));
                    }
                    if !self.eat(b',') {
                        return self.err("expected ',' or '}'");
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                if self.eat(b']') {
                    return Ok(Value::Arr(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    if self.eat(b']') {
                        return Ok(Value::Arr(a));
                    }
                    if !self.eat(b',') {
                        return self.err("expected ',' or ']'");
                    }
                }
            }
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.word("true", Value::Bool(true)),
            Some(b'f') => self.word("false", Value::Bool(false)),
            Some(b'n') => self.word("null", Value::Null),
            Some(c) if c.is_ascii_digit() => {
                let start = self.i;
                while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                    self.i += 1;
                }
                let s = std::str::from_utf8(&self.b[start..self.i]).expect("digits");
                if (s.len() > 1 && s.starts_with('0')) || matches!(self.b.get(self.i), Some(b'.' | b'e' | b'E')) {
                    return self.err("only plain unsigned integers are allowed");
                }
                s.parse().map(Value::Num).or_else(|_| self.err("number too large"))
            }
            _ => self.err("expected a value"),
        }
    }
    fn string(&mut self) -> Result<String, String> {
        if self.b.get(self.i) != Some(&b'"') {
            return self.err("expected a string");
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.i) else { return self.err("unterminated string") };
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&e) = self.b.get(self.i) else { return self.err("unterminated escape") };
                    self.i += 1;
                    out.push(match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'u' => {
                            let h = self.b.get(self.i..self.i + 4).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u32::from_str_radix(h, 16).ok());
                            self.i += 4;
                            match h.and_then(char::from_u32) {
                                Some(ch) => ch,
                                None => return self.err("bad \\u escape (surrogates are not allowed)"),
                            }
                        }
                        _ => return self.err("bad escape"),
                    });
                }
                c if c < 0x20 => return self.err("control character in a string"),
                _ => {
                    // Copy the whole UTF-8 sequence at once.
                    let start = self.i - 1;
                    let mut end = self.i;
                    while end < self.b.len() && self.b[end] & 0xC0 == 0x80 {
                        end += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.b[start..end]).map_err(|_| format!("bad UTF-8 at offset {start}"))?);
                    self.i = end;
                }
            }
        }
    }
}

pub fn quote(s: &str) -> String {
    let mut o = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_the_manifest_uses() {
        let v = parse(r#" {"a": [1, "x\"y\u00e4", true, null], "b": {}} "#).unwrap();
        let Some(Value::Arr(a)) = v.get("a") else { panic!() };
        assert_eq!(a[0], Value::Num(1));
        assert_eq!(a[1], Value::Str("x\"y\u{e4}".into()));
        assert_eq!(parse(&quote("a\"b\\c\n")).unwrap(), Value::Str("a\"b\\c\n".into()));
    }

    #[test]
    fn refuses_ambiguous_input() {
        for bad in [r#"{"a":1,"a":2}"#, "1.5", "01", "{} x", r#"{"a" 1}"#, "[1,]", "\"\\ud800\"", "-1"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}

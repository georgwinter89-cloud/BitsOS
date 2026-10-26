//! The TOML subset a manifest uses: comments, `key = value` lines, `[[name]]` array-of-table headers, basic and
//! literal strings, integers, booleans and one-line arrays of those. Anything else (dotted keys, `[table]`,
//! inline tables, floats, dates, multi-line strings) is refused with its line number: a manifest comes from a
//! package, which may be hostile, so the reader is strict and never panics.

/// Bound against hostile input (R1 note in CONTRACTS section 4): one manifest file, not a count of things.
pub const MAX_BYTES: usize = 16 << 20;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Arr(Vec<Value>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub line: usize,
    pub entries: Vec<(String, Value)>,
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Doc {
    pub root: Table,
    /// `[[name]]` tables in file order.
    pub arrays: Vec<(String, Table)>,
}

pub fn parse(text: &str) -> Result<Doc, String> {
    if text.len() > MAX_BYTES {
        return Err(format!("manifest is {} bytes, more than {MAX_BYTES}", text.len()));
    }
    let mut doc = Doc { root: Table { line: 1, entries: Vec::new() }, arrays: Vec::new() };
    for (i, raw) in text.split('\n').enumerate() {
        let n = i + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let line = if n == 1 { line.trim_start_matches('\u{feff}') } else { line };
        line_into(&mut doc, line, n).map_err(|e| format!("line {n}: {e}"))?;
    }
    Ok(doc)
}

fn line_into(doc: &mut Doc, line: &str, n: usize) -> Result<(), String> {
    let t = line.trim_matches([' ', '\t']);
    if t.is_empty() || t.starts_with('#') {
        return Ok(());
    }
    if let Some(h) = t.strip_prefix("[[") {
        let (name, rest) = h.split_once("]]").ok_or("header without ]]")?;
        let name = name.trim_matches([' ', '\t']);
        if !bare_key(name) || !end_ok(rest) {
            return Err(format!("bad header [[{name}]]"));
        }
        doc.arrays.push((name.to_string(), Table { line: n, entries: Vec::new() }));
        return Ok(());
    }
    if t.starts_with('[') {
        return Err("only [[name]] headers are allowed".into());
    }
    let (k, v) = t.split_once('=').ok_or("want key = value")?;
    let key = k.trim_matches([' ', '\t']);
    if !bare_key(key) {
        return Err(format!("bad key {key:?} (bare keys only)"));
    }
    let mut c = Cursor { s: v.as_bytes(), i: 0, text: v };
    c.ws();
    let val = c.value(true)?;
    c.ws();
    if !end_ok(&v[c.i..]) {
        return Err("text after the value".into());
    }
    let table = match doc.arrays.last_mut() {
        Some((_, t)) => t,
        None => &mut doc.root,
    };
    if table.get(key).is_some() {
        return Err(format!("key {key} given twice"));
    }
    table.entries.push((key.to_string(), val));
    Ok(())
}

fn bare_key(k: &str) -> bool {
    !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn end_ok(rest: &str) -> bool {
    let r = rest.trim_matches([' ', '\t']);
    r.is_empty() || r.starts_with('#')
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
}

impl Cursor<'_> {
    fn ws(&mut self) {
        while matches!(self.s.get(self.i), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }

    fn value(&mut self, top: bool) -> Result<Value, String> {
        match self.s.get(self.i) {
            Some(b'"') => self.basic().map(Value::Str),
            Some(b'\'') => self.literal().map(Value::Str),
            Some(b'[') if top => self.array(),
            Some(b'[') => Err("nested arrays are not allowed".into()),
            Some(b't') if self.text[self.i..].starts_with("true") => {
                self.i += 4;
                Ok(Value::Bool(true))
            }
            Some(b'f') if self.text[self.i..].starts_with("false") => {
                self.i += 5;
                Ok(Value::Bool(false))
            }
            Some(b'+' | b'-' | b'0'..=b'9') => self.int(),
            _ => Err("want a string, integer, boolean or array".into()),
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            self.ws();
            if self.s.get(self.i) == Some(&b']') {
                self.i += 1;
                return Ok(Value::Arr(out));
            }
            out.push(self.value(false)?);
            self.ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {}
                _ => return Err("array: want , or ] (arrays stay on one line)".into()),
            }
        }
    }

    fn int(&mut self) -> Result<Value, String> {
        let start = self.i;
        self.i += 1;
        while matches!(self.s.get(self.i), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        let t = &self.text[start..self.i];
        let digits = t.trim_start_matches(['+', '-']);
        if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
            return Err(format!("bad integer {t:?}"));
        }
        t.parse::<i64>().map(Value::Int).map_err(|_| format!("integer {t:?} out of range"))
    }

    fn literal(&mut self) -> Result<String, String> {
        let rest = &self.text[self.i + 1..];
        let end = rest.find('\'').ok_or("literal string without closing '")?;
        let s = &rest[..end];
        if s.chars().any(|c| c.is_control() && c != '\t') {
            return Err("control character in a string".into());
        }
        self.i += end + 2;
        Ok(s.to_string())
    }

    fn basic(&mut self) -> Result<String, String> {
        let mut out = String::new();
        let mut it = self.text[self.i + 1..].char_indices();
        while let Some((at, c)) = it.next() {
            match c {
                '"' => {
                    self.i += 1 + at + 1;
                    return Ok(out);
                }
                '\\' => {
                    let (_, e) = it.next().ok_or("string ends in \\")?;
                    let ch = match e {
                        'b' => '\u{8}',
                        't' => '\t',
                        'n' => '\n',
                        'f' => '\u{c}',
                        'r' => '\r',
                        '"' => '"',
                        '\\' => '\\',
                        'u' | 'U' => {
                            let len = if e == 'u' { 4 } else { 8 };
                            let mut v = 0u32;
                            for _ in 0..len {
                                let (_, h) = it.next().ok_or("short \\u escape")?;
                                v = v * 16 + h.to_digit(16).ok_or("bad \\u escape")?;
                            }
                            char::from_u32(v).ok_or("\\u escape is not a character")?
                        }
                        _ => return Err(format!("unknown escape \\{e}")),
                    };
                    out.push(ch);
                }
                c if c.is_control() && c != '\t' => return Err("control character in a string".into()),
                c => out.push(c),
            }
        }
        Err("string without closing \"".into())
    }
}

/// A TOML basic string for `s`.
pub fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            c if c.is_control() => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

pub fn write_value(v: &Value) -> String {
    match v {
        Value::Str(s) => quote(s),
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Arr(a) => {
            let parts: Vec<String> = a.iter().map(write_value).collect();
            format!("[{}]", parts.join(", "))
        }
    }
}

pub fn write(doc: &Doc) -> String {
    let mut o = String::new();
    for (k, v) in &doc.root.entries {
        o.push_str(&format!("{k} = {}\n", write_value(v)));
    }
    for (name, t) in &doc.arrays {
        o.push_str(&format!("\n[[{name}]]\n"));
        for (k, v) in &t.entries {
            o.push_str(&format!("{k} = {}\n", write_value(v)));
        }
    }
    o
}

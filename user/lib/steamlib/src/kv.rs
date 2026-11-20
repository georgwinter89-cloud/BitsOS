//! Valve's KeyValues text ("key" "value", nested `{ }`, `//` comments, `\"` and `\\` escapes). Pure and bounded: the input
//! is a byte slice (lossy UTF-8), nesting deeper than [`MAX_DEPTH`] is an error, nothing panics on any input.

use std::fmt;

/// Blocks nested deeper than this are refused (sofa-mode.md 3.3: Steam's own files nest four or five levels).
pub const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Block(Vec<(String, Value)>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            Value::Block(_) => None,
        }
    }
    pub fn as_block(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Block(b) => Some(b),
            Value::Str(_) => None,
        }
    }
}

/// The first value under `key`, compared as Steam does: ASCII case does not count.
pub fn get<'a>(pairs: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    pairs.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub what: &'static str,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.what)
    }
}

enum Tok {
    Text(String),
    Open,
    Close,
}

struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
    line: usize,
}

impl Lexer<'_> {
    fn err(&self, what: &'static str) -> Error {
        Error { line: self.line, what }
    }

    /// The next token; `[$WIN32]`-style conditions are skipped (they only matter to Valve's own tools).
    fn next(&mut self) -> Result<Option<Tok>, Error> {
        loop {
            while let Some(&c) = self.b.get(self.i) {
                if c == b'\n' {
                    self.line += 1;
                } else if !c.is_ascii_whitespace() {
                    break;
                }
                self.i += 1;
            }
            let Some(&c) = self.b.get(self.i) else { return Ok(None) };
            match c {
                b'/' if self.b.get(self.i + 1) == Some(&b'/') => {
                    while self.b.get(self.i).is_some_and(|&c| c != b'\n') {
                        self.i += 1;
                    }
                }
                b'[' => {
                    while self.b.get(self.i).is_some_and(|&c| c != b']' && c != b'\n') {
                        self.i += 1;
                    }
                    if self.b.get(self.i) != Some(&b']') {
                        return Err(self.err("condition without ]"));
                    }
                    self.i += 1;
                }
                b'{' => {
                    self.i += 1;
                    return Ok(Some(Tok::Open));
                }
                b'}' => {
                    self.i += 1;
                    return Ok(Some(Tok::Close));
                }
                b'"' => return self.quoted().map(|s| Some(Tok::Text(s))),
                _ => return Ok(Some(Tok::Text(self.bare()))),
            }
        }
    }

    fn quoted(&mut self) -> Result<String, Error> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let Some(&c) = self.b.get(self.i) else { return Err(self.err("string without closing quote")) };
            self.i += 1;
            match c {
                b'"' => return Ok(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => match self.b.get(self.i) {
                    Some(&e @ (b'"' | b'\\')) => {
                        out.push(e);
                        self.i += 1;
                    }
                    Some(b'n') => {
                        out.push(b'\n');
                        self.i += 1;
                    }
                    Some(b't') => {
                        out.push(b'\t');
                        self.i += 1;
                    }
                    _ => out.push(b'\\'),
                },
                b'\n' => {
                    self.line += 1;
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }

    fn bare(&mut self) -> String {
        let start = self.i;
        while self.b.get(self.i).is_some_and(|&c| !c.is_ascii_whitespace() && !matches!(c, b'"' | b'{' | b'}')) {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.b[start..self.i]).into_owned()
    }
}

/// Parses a whole file into its top-level pairs. Iterative: the depth bound is the only stack there is.
pub fn parse(text: &[u8]) -> Result<Vec<(String, Value)>, Error> {
    let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
    let mut lx = Lexer { b: text, i: 0, line: 1 };
    // Each open block: its key and the pairs so far; the bottom one is the file itself.
    let mut stack: Vec<(String, Vec<(String, Value)>)> = vec![(String::new(), Vec::new())];
    loop {
        let key = match lx.next()? {
            None if stack.len() == 1 => return Ok(stack.pop().map(|(_, p)| p).unwrap_or_default()),
            None => return Err(lx.err("block without closing }")),
            Some(Tok::Close) if stack.len() > 1 => {
                let Some((k, pairs)) = stack.pop() else { return Err(lx.err("internal")) };
                if let Some(top) = stack.last_mut() {
                    top.1.push((k, Value::Block(pairs)));
                }
                continue;
            }
            Some(Tok::Close) => return Err(lx.err("} without a block")),
            Some(Tok::Open) => return Err(lx.err("{ without a key")),
            Some(Tok::Text(k)) => k,
        };
        match lx.next()? {
            Some(Tok::Text(v)) => {
                if let Some(top) = stack.last_mut() {
                    top.1.push((key, Value::Str(v)));
                }
            }
            Some(Tok::Open) => {
                if stack.len() > MAX_DEPTH {
                    return Err(lx.err("nested too deep"));
                }
                stack.push((key, Vec::new()));
            }
            Some(Tok::Close) | None => return Err(lx.err("key without a value")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::Str(v.into())
    }

    #[test]
    fn pairs_blocks_comments_escapes() {
        let t = b"// head\n\"A\" { \"k\" \"v\" // tail\n \"p\" \"C:\\\\Steam \\\"x\\\"\" sub { b 1 } }\n";
        let p = parse(t).unwrap();
        let a = get(&p, "a").unwrap().as_block().unwrap();
        assert_eq!(get(a, "K"), Some(&s("v")));
        assert_eq!(get(a, "p"), Some(&s("C:\\Steam \"x\"")));
        assert_eq!(get(get(a, "sub").unwrap().as_block().unwrap(), "b"), Some(&s("1")));
    }

    #[test]
    fn conditions_bom_and_unknown_escape() {
        let p = parse(b"\xEF\xBB\xBF\"k\" \"a\\qb\" [$WIN32]\n").unwrap();
        assert_eq!(p, vec![("k".into(), s("a\\qb"))]);
    }

    #[test]
    fn broken_input_is_an_error_with_a_line() {
        for t in [&b"\"a\" {"[..], b"\"a\" \"b", b"}", b"{", b"\"a\"", b"\"a\" }", b"a [x"] {
            assert!(parse(t).is_err(), "{:?}", String::from_utf8_lossy(t));
        }
        assert_eq!(parse(b"\n\n\"a\"").unwrap_err().line, 3);
    }

    #[test]
    fn depth_bound() {
        let ok = format!("{}{}", "k {".repeat(MAX_DEPTH), "}".repeat(MAX_DEPTH));
        assert!(parse(ok.as_bytes()).is_ok());
        let deep = format!("{}{}", "k {".repeat(MAX_DEPTH + 1), "}".repeat(MAX_DEPTH + 1));
        assert_eq!(parse(deep.as_bytes()).unwrap_err().what, "nested too deep");
        let huge = "k {".repeat(100_000);
        assert!(parse(huge.as_bytes()).is_err());
    }
}

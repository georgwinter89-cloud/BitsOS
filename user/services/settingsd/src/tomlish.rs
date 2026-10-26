//! The settings files `\BITSOS2\settings\<group>.toml`: flat `key = value` lines Georg can read and edit. The
//! reader takes the TOML subset such files need (comments, `[table]` headers, bare/dotted/quoted keys, basic and
//! literal strings, integers, booleans) and reports a line it cannot read instead of failing the whole file;
//! the writer emits valid TOML (dotted bare keys, so a key never doubles as a table: see `registry::conflicts`).

use crate::value::Value;

pub struct Parsed {
    pub entries: Vec<(String, Value)>,
    pub bad_lines: Vec<(usize, String)>,
}

pub fn read(text: &str) -> Parsed {
    let mut p = Parsed { entries: Vec::new(), bad_lines: Vec::new() };
    let mut table = String::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let r = if let Some(h) = line.strip_prefix('[') {
            header(h).map(|t| table = t)
        } else {
            entry(line).map(|(k, v)| {
                let name = if table.is_empty() { k } else { format!("{table}.{k}") };
                p.entries.push((name, v));
            })
        };
        if let Err(e) = r {
            p.bad_lines.push((n + 1, e));
        }
    }
    p
}

fn header(h: &str) -> Result<String, String> {
    let h = strip_comment(h).trim_end();
    let inner = h.strip_suffix(']').ok_or("table header without ']'")?;
    let (k, rest) = key(inner.trim())?;
    if !rest.trim().is_empty() {
        return Err("junk after table name".into());
    }
    Ok(k)
}

fn entry(line: &str) -> Result<(String, Value), String> {
    let (k, rest) = key(line)?;
    let rest = rest.trim_start().strip_prefix('=').ok_or("want key = value")?.trim_start();
    let (v, tail) = value(rest)?;
    let tail = tail.trim();
    if !(tail.is_empty() || tail.starts_with('#')) {
        return Err("junk after value".into());
    }
    Ok((k, v))
}

/// A dotted key of bare or quoted parts; returns the joined name and the text after it.
fn key(s: &str) -> Result<(String, &str), String> {
    let mut parts = Vec::new();
    let mut rest = s;
    loop {
        rest = rest.trim_start();
        let (part, after) = if rest.starts_with('"') || rest.starts_with('\'') {
            let (v, after) = value(rest)?;
            let Value::Text(t) = v else { return Err("bad quoted key".into()) };
            (t, after)
        } else {
            let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(rest.len());
            if end == 0 {
                return Err("want a key".into());
            }
            (rest[..end].to_string(), &rest[end..])
        };
        parts.push(part);
        let t = after.trim_start();
        if let Some(r) = t.strip_prefix('.') {
            rest = r;
        } else {
            return Ok((parts.join("."), after));
        }
    }
}

fn strip_comment(s: &str) -> &str {
    s.split('#').next().unwrap_or("")
}

/// One value at the start of `s`; returns it and the rest.
fn value(s: &str) -> Result<(Value, &str), String> {
    if let Some(r) = s.strip_prefix('\'') {
        let end = r.find('\'').ok_or("unterminated literal string")?;
        return Ok((Value::Text(r[..end].to_string()), &r[end + 1..]));
    }
    if let Some(r) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut it = r.char_indices();
        while let Some((i, c)) = it.next() {
            match c {
                '"' => return Ok((Value::Text(out), &r[i + 1..])),
                '\\' => {
                    let (_, e) = it.next().ok_or("unterminated escape")?;
                    match e {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' | 'U' => {
                            let n = if e == 'u' { 4 } else { 8 };
                            let mut h = String::new();
                            for _ in 0..n {
                                h.push(it.next().ok_or("short unicode escape")?.1);
                            }
                            let cp = u32::from_str_radix(&h, 16).map_err(|_| "bad unicode escape")?;
                            out.push(char::from_u32(cp).ok_or("bad unicode escape")?);
                        }
                        _ => return Err("bad escape".into()),
                    }
                }
                c => out.push(c),
            }
        }
        return Err("unterminated string".into());
    }
    let end = s.find(|c: char| c.is_whitespace() || c == '#').unwrap_or(s.len());
    let (word, rest) = (&s[..end], &s[end..]);
    match word {
        "true" => Ok((Value::Bool(true), rest)),
        "false" => Ok((Value::Bool(false), rest)),
        w => {
            let digits: String = w.chars().filter(|&c| c != '_').collect();
            digits.parse::<i64>().map(|n| (Value::Int(n), rest)).map_err(|_| format!("cannot read value {w:?}"))
        }
    }
}

pub fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A group file: a header comment, then one sorted line per changed key (defaults are not written, so a new
/// default reaches every key Georg never changed).
pub fn write(group: &str, entries: &[(String, Value)]) -> String {
    let mut out = format!(
        "# BitsOS settings, group {group}. Written by settingsd; you may edit it (one `key = value` per line).\n\
         # Only changed keys are listed; a removed line means the default.\n"
    );
    let mut e: Vec<&(String, Value)> = entries.iter().collect();
    e.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in e {
        let val = match v {
            Value::Bool(b) => b.to_string(),
            Value::Int(n) => n.to_string(),
            Value::Text(s) => quote(s),
        };
        out.push_str(&format!("{k} = {val}\n"));
    }
    out
}

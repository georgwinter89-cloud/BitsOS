//! Lexer and a small C preprocessor: comments, line splices, #if/#ifdef/#else/#endif, #define, and
//! #include of the other input headers (system headers are known facts, see `parse::builtin`).

use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Id(String),
    Num(String),
    Chr(i64),
    Str(String),
    P(String),
}

impl Tok {
    pub fn is_p(&self, p: &str) -> bool {
        matches!(self, Tok::P(s) if s == p)
    }
    pub fn id(&self) -> Option<&str> {
        match self {
            Tok::Id(s) => Some(s),
            _ => None,
        }
    }
}

pub struct Macro {
    pub name: String,
    pub params: Option<Vec<String>>,
    pub body: Vec<Tok>,
    pub file: usize,
}

pub struct Input<'a> {
    pub label: &'a str, // e.g. "libdrm/drm.h"
    pub text: &'a str,
}

#[derive(Default)]
pub struct Pp {
    pub macros: Vec<Macro>,
    pub index: HashMap<String, usize>,
    pub code: Vec<(Tok, usize)>, // declaration tokens with the input they came from
    pub notices: Vec<String>,    // leading comment blocks per input (licence notices)
}

/// Removes line splices and comments; returns the cleaned text and the comment blocks that come before the
/// first code character.
fn clean(src: &str) -> (String, String) {
    let s = src.replace("\\\r\n", "").replace("\\\n", "");
    let b: Vec<char> = s.chars().collect();
    let (mut out, mut lead) = (String::new(), String::new());
    let (mut i, mut code_seen) = (0, false);
    while i < b.len() {
        let c = b[i];
        if c == '/' && b.get(i + 1) == Some(&'*') {
            let end = (i + 2..b.len().saturating_sub(1)).find(|&j| b[j] == '*' && b[j + 1] == '/').map_or(b.len(), |j| j + 2);
            if !code_seen {
                lead.extend(&b[i..end]);
                lead.push('\n');
            }
            out.extend(b[i..end].iter().map(|&c| if c == '\n' { '\n' } else { ' ' }));
            i = end;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '"' || c == '\'' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != c {
                i += if b[i] == '\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            out.extend(&b[start..i]);
            code_seen = true;
        } else {
            if !c.is_whitespace() {
                code_seen = true;
            }
            out.push(c);
            i += 1;
        }
    }
    (out, lead)
}

const PUNCT2: [&str; 10] = ["<<", ">>", "->", "##", "&&", "||", "==", "!=", "<=", ">="];

pub fn lex(line: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = line.chars().collect();
    let mut v = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            v.push(Tok::Id(b[s..i].iter().collect()));
        } else if c.is_ascii_digit() {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '.') {
                i += 1;
            }
            v.push(Tok::Num(b[s..i].iter().collect()));
        } else if c == '\'' {
            let (val, n) = match (b.get(i + 1), b.get(i + 2), b.get(i + 3)) {
                (Some('\\'), Some('0'), Some('\'')) => (0, 4),
                (Some('\\'), Some(&e), Some('\'')) => (e as i64, 4),
                (Some(&ch), Some('\''), _) => (ch as i64, 3),
                _ => return Err(format!("bad char literal in `{line}`")),
            };
            v.push(Tok::Chr(val));
            i += n;
        } else if c == '"' {
            let s = i + 1;
            i += 1;
            while i < b.len() && b[i] != '"' {
                i += if b[i] == '\\' { 2 } else { 1 };
            }
            v.push(Tok::Str(b[s..i.min(b.len())].iter().collect()));
            i += 1;
        } else {
            let two: String = b[i..(i + 2).min(b.len())].iter().collect();
            if PUNCT2.contains(&two.as_str()) {
                v.push(Tok::P(two));
                i += 2;
            } else {
                v.push(Tok::P(c.to_string()));
                i += 1;
            }
        }
    }
    Ok(v)
}

/// Macros that exist on Linux x86_64 before any input is read (compiler and system headers).
const PREDEFINED: [&str; 2] = ["__linux__", "__x86_64__"];

impl Pp {
    pub fn run(inputs: &[Input]) -> Result<Pp, String> {
        let mut pp = Pp::default();
        pp.notices = vec![String::new(); inputs.len()];
        for (i, _) in inputs.iter().enumerate() {
            pp.file(inputs, i, 0)?;
        }
        Ok(pp)
    }

    pub fn get(&self, name: &str) -> Option<&Macro> {
        self.index.get(name).map(|&i| &self.macros[i])
    }

    fn defined(&self, name: &str) -> bool {
        PREDEFINED.contains(&name) || self.index.contains_key(name)
    }

    fn file(&mut self, inputs: &[Input], f: usize, depth: usize) -> Result<(), String> {
        if depth > 8 {
            return Err("include nesting too deep".into());
        }
        let (text, lead) = clean(inputs[f].text);
        if self.notices[f].is_empty() {
            self.notices[f] = lead;
        }
        // Stack of (this branch active, some branch of this #if already taken).
        let mut stack: Vec<(bool, bool)> = Vec::new();
        let label = inputs[f].label;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            let active = stack.iter().all(|s| s.0);
            let err = |e: String| format!("{label}:{}: {e}", n + 1);
            let Some(dir) = line.strip_prefix('#') else {
                if active {
                    self.code.extend(lex(line).map_err(err)?.into_iter().map(|t| (t, f)));
                }
                continue;
            };
            let toks = lex(dir).map_err(err)?;
            let word = toks.first().and_then(|t| t.id()).unwrap_or("");
            let rest = &toks[toks.len().min(1)..];
            match word {
                "if" | "ifdef" | "ifndef" => {
                    let c = active
                        && match word {
                            "ifdef" => rest.first().and_then(|t| t.id()).is_some_and(|m| self.defined(m)),
                            "ifndef" => !rest.first().and_then(|t| t.id()).is_some_and(|m| self.defined(m)),
                            _ => self.cond(rest).map_err(err)?,
                        };
                    stack.push((c, c));
                }
                "elif" => {
                    let outer = stack.len() < 2 || stack[..stack.len() - 1].iter().all(|s| s.0);
                    let c = self.cond(rest).map_err(err)?;
                    let top = stack.last_mut().ok_or_else(|| err("#elif without #if".into()))?;
                    top.0 = outer && !top.1 && c;
                    top.1 |= top.0;
                }
                "else" => {
                    let outer = stack.len() < 2 || stack[..stack.len() - 1].iter().all(|s| s.0);
                    let top = stack.last_mut().ok_or_else(|| err("#else without #if".into()))?;
                    top.0 = outer && !top.1;
                    top.1 = true;
                }
                "endif" => {
                    stack.pop().ok_or_else(|| err("#endif without #if".into()))?;
                }
                _ if !active => {}
                "define" => self.define(rest, f, line).map_err(err)?,
                "undef" => {
                    if let Some(m) = rest.first().and_then(|t| t.id()) {
                        self.index.remove(m);
                    }
                }
                "include" => {
                    let target = include_name(dir);
                    if let Some(j) = inputs.iter().position(|i| i.label.rsplit('/').next() == Some(target.as_str()) || i.label == target) {
                        if j != f {
                            self.file(inputs, j, depth + 1)?;
                        }
                    }
                }
                _ => return Err(err(format!("unsupported directive #{word}"))),
            }
        }
        if !stack.is_empty() {
            return Err(format!("{label}: unterminated #if"));
        }
        Ok(())
    }

    fn define(&mut self, rest: &[Tok], f: usize, line: &str) -> Result<(), String> {
        let name = rest.first().and_then(|t| t.id()).ok_or("#define without a name")?.to_string();
        // Function-like only when '(' follows the name without a space.
        let after = line.trim_start_matches('#').trim_start().trim_start_matches("define").trim_start();
        let func = after[name.len()..].starts_with('(');
        let (params, body) = if func {
            let close = rest.iter().position(|t| t.is_p(")")).ok_or("unclosed macro parameter list")?;
            let ps = rest[2..close].iter().filter_map(|t| t.id().map(str::to_string)).collect();
            (Some(ps), rest[close + 1..].to_vec())
        } else {
            (None, rest[1..].to_vec())
        };
        self.index.insert(name.clone(), self.macros.len());
        self.macros.push(Macro { name, params, body, file: f });
        Ok(())
    }

    fn cond(&self, toks: &[Tok]) -> Result<bool, String> {
        // Replace defined(X) / defined X by 1 or 0, then evaluate; unknown names are 0 as in C.
        let mut v = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            if toks[i].id() == Some("defined") {
                let (name, n) = if toks.get(i + 1).is_some_and(|t| t.is_p("(")) { (toks.get(i + 2), 4) } else { (toks.get(i + 1), 2) };
                let d = name.and_then(|t| t.id()).is_some_and(|m| self.defined(m));
                v.push(Tok::Num(if d { "1" } else { "0" }.into()));
                i += n;
            } else {
                v.push(toks[i].clone());
                i += 1;
            }
        }
        crate::eval::cond(self, &v)
    }
}

fn include_name(dir: &str) -> String {
    let s = dir.trim_start().trim_start_matches("include").trim();
    s.trim_matches(|c| c == '"' || c == '<' || c == '>').to_string()
}

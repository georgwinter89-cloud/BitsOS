//! Types and values of settings keys. Every value that reaches the registry went through `Type::check`, so a file,
//! a CLI line or a JSON request all end in the same normalised form (a colour is always `#rrggbb`, a choice is
//! spelled as declared).

use crate::json::Json;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Text(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    Bool,
    /// Inclusive range; `unit` is shown to people only ("%", "px"). `word`: a word that may stand instead of a
    /// number (`all` for kernel.preload.max_mb, FS-13b), any case, kept as `Value::Text(word)`.
    Int {
        min: i64,
        max: i64,
        unit: String,
        word: Option<String>,
    },
    Choice(Vec<String>),
    /// `#rrggbb`.
    Color,
    /// Free text up to `max` bytes (a per-key bound the owner declares, so one key cannot fill the stick).
    Text {
        max: usize,
    },
    /// The kernel's `loglevel=` grammar (crates/klog/src/levels.rs `parse`): `info` or `warn,net=debug`.
    LogSpec,
}

const LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

impl Type {
    pub fn name(&self) -> &'static str {
        match self {
            Type::Bool => "bool",
            Type::Int { .. } => "int",
            Type::Choice(_) => "choice",
            Type::Color => "color",
            Type::Text { .. } => "text",
            Type::LogSpec => "logspec",
        }
    }

    /// Parses a value given as text (CLI, a settings file, CONFIG.TXT) into its normal form.
    pub fn parse_text(&self, s: &str) -> Result<Value, String> {
        let t = s.trim();
        match self {
            Type::Bool => match t.to_ascii_lowercase().as_str() {
                "true" | "on" | "yes" | "1" => Ok(Value::Bool(true)),
                "false" | "off" | "no" | "0" => Ok(Value::Bool(false)),
                _ => Err(format!("want on or off, got {t:?}")),
            },
            Type::Int { word: Some(w), .. } if t.eq_ignore_ascii_case(w) => Ok(Value::Text(w.clone())),
            Type::Int { unit, .. } => {
                let num = t.strip_suffix(unit.as_str()).filter(|_| !unit.is_empty()).unwrap_or(t).trim();
                let n = num.parse::<i64>().map_err(|_| match self {
                    Type::Int { word: Some(w), .. } => format!("want a whole number or {w}, got {t:?}"),
                    _ => format!("want a whole number, got {t:?}"),
                })?;
                self.check(Value::Int(n))
            }
            Type::Text { .. } => self.check(Value::Text(s.to_string())),
            _ => self.check(Value::Text(t.to_string())),
        }
    }

    pub fn parse_json(&self, j: &Json) -> Result<Value, String> {
        match (self, j) {
            (Type::Bool, Json::Bool(b)) => Ok(Value::Bool(*b)),
            (Type::Int { .. }, Json::Int(_) | Json::Float(_)) => {
                let n = j.as_i64().ok_or("want a whole number")?;
                self.check(Value::Int(n))
            }
            (_, Json::Str(s)) => self.parse_text(s),
            _ => Err(format!("want a {} value", self.name())),
        }
    }

    /// Checks a value against the type and returns its normal form.
    pub fn check(&self, v: Value) -> Result<Value, String> {
        match (self, v) {
            (Type::Bool, v @ Value::Bool(_)) => Ok(v),
            (Type::Int { word: Some(w), .. }, Value::Text(s)) if s.trim().eq_ignore_ascii_case(w) => Ok(Value::Text(w.clone())),
            (Type::Int { min, max, unit, .. }, Value::Int(n)) => {
                if n < *min || n > *max {
                    Err(format!("want {min}..{max}{unit}, got {n}{unit}"))
                } else {
                    Ok(Value::Int(n))
                }
            }
            (Type::Choice(c), Value::Text(s)) => c
                .iter()
                .find(|x| x.eq_ignore_ascii_case(s.trim()))
                .map(|x| Value::Text(x.clone()))
                .ok_or_else(|| format!("want one of {}, got {s:?}", c.join("|"))),
            (Type::Color, Value::Text(s)) => color(&s).map(Value::Text),
            (Type::Text { max }, Value::Text(s)) => {
                if s.len() > *max {
                    Err(format!("at most {max} bytes, got {}", s.len()))
                } else if s.chars().any(|c| c.is_control()) {
                    Err("no control characters (line breaks, tabs)".to_string())
                } else {
                    Ok(Value::Text(s))
                }
            }
            (Type::LogSpec, Value::Text(s)) => logspec(&s).map(Value::Text),
            (t, v) => Err(format!("want a {} value, got {}", t.name(), v.show())),
        }
    }

    pub fn describe(&self) -> Json {
        let j = Json::obj().with("type", self.name());
        match self {
            Type::Int { min, max, unit, word } => {
                let j = j.with("min", *min).with("max", *max).with("unit", unit.as_str());
                match word {
                    Some(w) => j.with("word", w.as_str()),
                    None => j,
                }
            }
            Type::Choice(c) => j.with("choices", c.iter().map(|x| Json::from(x.as_str())).collect::<Vec<_>>()),
            Type::Text { max } => j.with("max_bytes", *max),
            Type::LogSpec => j.with("levels", LEVELS.iter().map(|x| Json::from(*x)).collect::<Vec<_>>()),
            _ => j,
        }
    }

    /// Reads a type from a registration request (`{"type":"int","min":0,"max":9,"unit":"px"}`).
    pub fn from_json(j: &Json) -> Result<Type, String> {
        Ok(match j.str("type").ok_or("type missing")? {
            "bool" => Type::Bool,
            "int" => {
                let min = j.get("min").and_then(Json::as_i64).ok_or("int needs min")?;
                let max = j.get("max").and_then(Json::as_i64).ok_or("int needs max")?;
                if min > max {
                    return Err("min > max".into());
                }
                let word = match j.get("word") {
                    None => None,
                    Some(w) => match w.as_str() {
                        Some(w) if !w.is_empty() && w.len() <= 16 && w.chars().all(|c| c.is_ascii_alphabetic()) => Some(w.to_string()),
                        _ => return Err("int word must be 1..16 letters".into()),
                    },
                };
                Type::Int { min, max, unit: j.str("unit").unwrap_or("").to_string(), word }
            }
            "choice" => {
                let Some(Json::Arr(a)) = j.get("choices") else { return Err("choice needs choices".into()) };
                let c: Vec<String> = a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                if c.is_empty()
                    || c.len() != a.len()
                    || c.iter().any(|x| x.is_empty() || x.chars().any(char::is_control))
                {
                    return Err("choices must be non-empty strings".into());
                }
                Type::Choice(c)
            }
            "color" => Type::Color,
            "text" => {
                let max = j.get("max_bytes").and_then(Json::as_i64).ok_or("text needs max_bytes")?;
                Type::Text { max: usize::try_from(max).map_err(|_| "bad max_bytes")? }
            }
            "logspec" => Type::LogSpec,
            other => return Err(format!("unknown type {other:?}")),
        })
    }
}

impl Value {
    /// The text form: CONFIG.TXT lines, the CLI, /sys/bitsos files.
    pub fn show(&self) -> String {
        match self {
            Value::Bool(b) => (if *b { "on" } else { "off" }).to_string(),
            Value::Int(n) => n.to_string(),
            Value::Text(s) => s.clone(),
        }
    }
    pub fn to_json(&self) -> Json {
        match self {
            Value::Bool(b) => Json::Bool(*b),
            Value::Int(n) => Json::Int(*n),
            Value::Text(s) => Json::Str(s.clone()),
        }
    }
    /// Values in the journal keep their kind (a choice "1" stays text, an int 1 stays a number).
    pub fn from_json(j: &Json) -> Option<Value> {
        match j {
            Json::Bool(b) => Some(Value::Bool(*b)),
            Json::Int(n) => Some(Value::Int(*n)),
            Json::Str(s) => Some(Value::Text(s.clone())),
            _ => None,
        }
    }
}

fn color(s: &str) -> Result<String, String> {
    let h = s.trim().trim_start_matches('#');
    if h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(format!("#{}", h.to_ascii_lowercase()))
    } else {
        Err(format!("want a colour #rrggbb, got {s:?}"))
    }
}

/// Same grammar and errors as the kernel's `levels::parse`, so a value accepted here is never refused at boot.
fn logspec(s: &str) -> Result<String, String> {
    let mut parts = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, l) = match part.split_once('=') {
            Some((k, l)) => (Some(k.trim()), l.trim()),
            None => (None, part),
        };
        if !LEVELS.contains(&l) {
            return Err(format!("bad part {part:?}: want a level of {}", LEVELS.join("|")));
        }
        match k {
            Some(k) if k.is_empty() || k.contains(char::is_whitespace) || k.contains('#') => {
                return Err(format!("bad part {part:?}: want subsystem=level"));
            }
            Some(k) => parts.push(format!("{k}={l}")),
            None => parts.push(l.to_string()),
        }
    }
    if parts.is_empty() {
        return Err("want a level, e.g. info or warn,net=debug".into());
    }
    Ok(parts.join(","))
}

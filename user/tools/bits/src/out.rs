//! What `bits` prints and its exit status (BITS.md "Antworten und Rückgabewerte"). With `--json` every call prints
//! exactly one JSON object: `{"ok":true, ...}` or `{"ok":false,"error":<code>,"message":<German>}`, the envelope
//! settingsd and bitspkgd answer with. Without it, plain German lines for people.

use serde_json::{Map, Value, json};

static JSON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// `--json` was given (set once by main).
pub fn set_json(on: bool) {
    let _ = JSON.set(on);
}

pub fn json_mode() -> bool {
    JSON.get().copied().unwrap_or(false)
}

/// Why a call did not do what it should; `code` picks the exit status.
#[derive(Debug, Clone, PartialEq)]
pub struct Fail {
    pub code: &'static str,
    pub message: String,
}

pub type R<T = Done> = Result<T, Fail>;

/// Exit codes, one per kind of failure (BITS.md).
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_UNREACHABLE: i32 = 3;
pub const EXIT_TIMEOUT: i32 = 4;
pub const EXIT_REFUSED: i32 = 5;

impl Fail {
    pub fn new(code: &'static str, message: impl Into<String>) -> Fail {
        Fail { code, message: message.into() }
    }
    /// The call itself was wrong (unknown verb, missing argument).
    pub fn usage(message: impl Into<String>) -> Fail {
        Fail::new("usage", message)
    }
    pub fn failed(message: impl Into<String>) -> Fail {
        Fail::new("failed", message)
    }
    /// A level, a fixed rule or Georg said no (or did not answer).
    pub fn refused(message: impl Into<String>) -> Fail {
        Fail::new("refused", message)
    }
    pub fn unreachable(message: impl Into<String>) -> Fail {
        Fail::new("unreachable", message)
    }
    pub fn timeout(message: impl Into<String>) -> Fail {
        Fail::new("timeout", message)
    }
    pub fn exit(&self) -> i32 {
        match self.code {
            "usage" => EXIT_USAGE,
            "unreachable" => EXIT_UNREACHABLE,
            "timeout" => EXIT_TIMEOUT,
            "refused" => EXIT_REFUSED,
            _ => EXIT_FAILED,
        }
    }
    pub fn json(&self) -> Value {
        json!({"ok": false, "error": self.code, "message": self.message})
    }
}

/// A finished call: the JSON fields beside `"ok":true`, and the text people see.
#[derive(Debug, Clone, PartialEq)]
pub struct Done {
    pub fields: Map<String, Value>,
    pub text: String,
}

impl Done {
    pub fn new(text: impl Into<String>) -> Done {
        Done { fields: Map::new(), text: text.into() }
    }
    pub fn with(mut self, key: &str, v: impl Into<Value>) -> Done {
        self.fields.insert(key.to_string(), v.into());
        self
    }
    pub fn json(&self) -> Value {
        let mut m = Map::new();
        m.insert("ok".into(), Value::Bool(true));
        m.extend(self.fields.clone());
        Value::Object(m)
    }
}

/// Prints the result and gives the exit status.
pub fn show(r: &R, as_json: bool) -> i32 {
    match r {
        Ok(d) => {
            if as_json {
                println!("{}", d.json());
            } else if !d.text.is_empty() {
                println!("{}", d.text.trim_end_matches('\n'));
            }
            0
        }
        Err(f) => {
            if as_json {
                println!("{}", f.json());
            } else {
                eprintln!("bits: {}", f.message);
            }
            f.exit()
        }
    }
}

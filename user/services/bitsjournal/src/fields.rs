//! Field filters on a record's `key=value ... | message` text, the same terms as the kernel's MCP `events`
//! (crates/klog/src/fields.rs): space-separated terms that must all hold, each `key=value`, `key!=value`,
//! `key>n`, `key<n`, `key>=n`, `key<=n` (numbers decimal or 0x hex) or a bare `key` (the field exists).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Has,
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Term {
    key: String,
    op: Op,
    value: String,
    num: Option<i128>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldFilter {
    terms: Vec<Term>,
}

fn number(s: &str) -> Option<i128> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let v = match s.strip_prefix("0x") {
        Some(h) => i128::from_str_radix(h, 16).ok()?,
        None => s.parse::<i128>().ok()?,
    };
    Some(if neg { -v } else { v })
}

impl Term {
    fn equals(&self, v: &str) -> bool {
        let unquoted = v.strip_prefix('"').and_then(|v| v.strip_suffix('"'));
        v == self.value || unquoted == Some(self.value.as_str()) || (self.num.is_some() && number(v) == self.num)
    }
}

impl FieldFilter {
    /// Parses the terms; the error names the bad term.
    pub fn parse(spec: &str) -> Result<FieldFilter, String> {
        let mut terms = Vec::new();
        for t in spec.split_ascii_whitespace() {
            let (key, rest) = match t.find(['=', '!', '<', '>']) {
                Some(i) => t.split_at(i),
                None => (t, ""),
            };
            let (op, value) = if rest.is_empty() {
                (Op::Has, "")
            } else if let Some(v) = rest.strip_prefix("!=") {
                (Op::Ne, v)
            } else if let Some(v) = rest.strip_prefix(">=") {
                (Op::Ge, v)
            } else if let Some(v) = rest.strip_prefix("<=") {
                (Op::Le, v)
            } else if let Some(v) = rest.strip_prefix('=') {
                (Op::Eq, v)
            } else if let Some(v) = rest.strip_prefix('>') {
                (Op::Gt, v)
            } else if let Some(v) = rest.strip_prefix('<') {
                (Op::Lt, v)
            } else {
                return Err(t.into());
            };
            let valid_key = !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
            let num = number(value);
            if !valid_key || (op != Op::Has && value.is_empty()) || (matches!(op, Op::Gt | Op::Lt | Op::Ge | Op::Le) && num.is_none()) {
                return Err(t.into());
            }
            terms.push(Term { key: key.into(), op, value: value.into(), num });
        }
        Ok(FieldFilter { terms })
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn matches(&self, text: &str) -> bool {
        self.terms.iter().all(|t| {
            let v = value_of(text, &t.key);
            match (t.op, v) {
                (Op::Has, v) => v.is_some(),
                (Op::Ne, None) => true,
                (_, None) => false,
                (Op::Eq, Some(v)) => t.equals(v),
                (Op::Ne, Some(v)) => !t.equals(v),
                (op, Some(v)) => match (number(v), t.num) {
                    (Some(a), Some(b)) => match op {
                        Op::Gt => a > b,
                        Op::Lt => a < b,
                        Op::Ge => a >= b,
                        _ => a <= b,
                    },
                    _ => false,
                },
            }
        })
    }
}

/// The value of field `key` in `k=v k2="text with spaces" | message`; fields end at the message separator.
pub fn value_of<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        if i >= b.len() || b[i..].starts_with(b"| ") {
            return None;
        }
        let k0 = i;
        while i < b.len() && b[i] != b'=' && b[i] != b' ' {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            return None;
        }
        let k = &text[k0..i];
        i += 1;
        let v0 = i;
        if i < b.len() && b[i] == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
        } else {
            while i < b.len() && b[i] != b' ' {
                i += 1;
            }
        }
        if k == key {
            return text.get(v0..i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_as_the_kernel_reads_them() {
        let t = r#"peer=10.0.2.2:443 bytes=0x10 name="a b" | msg x=1"#;
        assert!(FieldFilter::parse("peer=10.0.2.2:443 bytes>=16 bytes<17 name=a\\ b").is_ok());
        assert!(FieldFilter::parse("peer=10.0.2.2:443 bytes>=16").unwrap().matches(t));
        assert!(FieldFilter::parse("name=\"a").unwrap().matches(t) == false);
        assert!(FieldFilter::parse("x").unwrap().matches(t) == false, "x is in the message, not a field");
        assert!(FieldFilter::parse("missing!=3").unwrap().matches(t));
        assert!(FieldFilter::parse("bytes>z").is_err());
        assert!(FieldFilter::parse("=3").is_err());
        assert!(FieldFilter::parse("").unwrap().is_empty());
    }
}

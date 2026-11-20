//! B6 event lines: `bitsbench <event> k=v ...` to /dev/kmsg when it may be written (root), always to stderr. As
//! uid 1000 under bitscomp, stderr is bitscomp's, which reaches the journal.

use std::io::Write;

/// A value as one word: quoted when it holds a space, a quote or an equals sign.
pub fn q(v: &str) -> String {
    if !v.is_empty() && !v.contains([' ', '"', '=', '\n']) {
        return v.to_string();
    }
    format!("{:?}", v.replace('\n', " "))
}

pub fn event(name: &str, fields: &str) {
    let line = if fields.is_empty() { format!("bitsbench {name}\n") } else { format!("bitsbench {name} {fields}\n") };
    let _ = std::fs::OpenOptions::new().write(true).open("/dev/kmsg").and_then(|mut f| f.write_all(line.as_bytes()));
    eprint!("{line}");
}

#[cfg(test)]
mod tests {
    #[test]
    fn values_with_spaces_are_quoted() {
        assert_eq!(super::q("abc"), "abc");
        assert_eq!(super::q("a b"), "\"a b\"");
        assert_eq!(super::q(""), "\"\"");
        assert_eq!(super::q("x\ny"), "\"x y\"");
    }
}

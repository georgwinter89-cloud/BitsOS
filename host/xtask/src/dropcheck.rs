//! `drop-bounds` gate (desk 23.10.2026, Georg): rustc lets a struct's bounds be bypassed by assigning a field after
//! construction (rust-lang/rust#164250, open since 2017), so a `Drop` impl that relies on its type's bounds (e.g.
//! `T: 'static`) can be handed a borrowed value and keep it past its lifetime, in safe Rust. Every `Drop` impl whose
//! generics carry a bound (anything but `?Sized`) or a where clause is red until the desk has read it and listed it
//! in `ALLOWED`. Remove this gate once rustc rejects the pattern.
use std::path::Path;

/// (file relative to the repo, type name) the desk has checked: its `drop` does not rely on the bounds.
const ALLOWED: &[(&str, &str)] = &[];

/// Trees scanned for Rust sources (all of them BitsOS's own code).
const TREES: &[&str] = &["crates", "kernel", "user", "host"];

/// One `impl ... Drop for ...` header that carries bounds: (1-based line, the type name, the header text).
#[derive(Debug, PartialEq)]
pub struct Risky {
    pub line: usize,
    pub ty: String,
    pub header: String,
}

/// Removes `//` line comments and `/* */` block comments, keeping newlines (so line numbers stay right). String
/// literals are not parsed: a `//` inside one only shortens what is scanned on that line.
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                if b[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 2;
        } else {
            let c = src[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// True when generics text (the part after `impl`) states a bound other than `?Sized`; a const generic's type
/// (`const N: usize`) is not a bound.
fn has_bound(generics: &str) -> bool {
    let g = generics.replace("::", "").replace("?Sized", "");
    g.split(',').any(|p| {
        let p = p.trim_start().trim_start_matches('<').trim_start();
        match p.split_once(':') {
            Some(_) if p.starts_with("const ") => false,
            Some((_, b)) => !b.trim_matches(|c: char| c.is_whitespace() || c == '+' || c == '>').is_empty(),
            None => false,
        }
    })
}

/// Every `Drop` impl in `src` whose impl generics carry a bound or which has a where clause.
pub fn risky_drop_impls(src: &str) -> Vec<Risky> {
    let s = strip_comments(src);
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = s[from..].find("Drop for ") {
        let pos = from + at;
        from = pos + 9;
        // The header runs from the `impl` before `Drop for` to the `{` after it.
        let Some(start) = s[..pos].rfind("impl") else { continue };
        if s[start..pos].contains(['{', '}', ';']) {
            continue;
        }
        let Some(end_rel) = s[pos..].find('{') else { continue };
        let generics = &s[start + 4..pos];
        let tail = &s[pos + 9..pos + end_rel];
        let ty: String = tail.trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        let risky = has_bound(generics) || tail.contains("where");
        if risky {
            let header = s[start..pos + end_rel].split_whitespace().collect::<Vec<_>>().join(" ");
            out.push(Risky { line: s[..start].matches('\n').count() + 1, ty, header });
        }
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if p.is_dir() {
            rust_files(&p, out);
        } else if name.ends_with(".rs") {
            out.push(p);
        }
    }
}

pub fn drop_bounds_gate(root: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    for t in TREES {
        rust_files(&root.join(t), &mut files);
    }
    files.sort();
    let mut bad = Vec::new();
    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else { continue };
        if !src.contains("Drop for ") {
            continue;
        }
        let rel = f.strip_prefix(root).unwrap_or(f).to_string_lossy().to_string();
        if rel == "host/xtask/src/dropcheck.rs" {
            continue; // its own test inputs
        }
        for r in risky_drop_impls(&src) {
            if !ALLOWED.iter().any(|(file, ty)| *file == rel && *ty == r.ty) {
                bad.push(format!("{rel}:{} `{}`", r.line, r.header));
            }
        }
    }
    if bad.is_empty() {
        println!("drop-bounds gate: ok ({} files)", files.len());
        Ok(())
    } else {
        for b in &bad {
            println!("drop-bounds gate: OFFENDER {b} (a Drop impl with bounds: rust-lang/rust#164250; the desk reads it and lists it in host/xtask/src/dropcheck.rs ALLOWED)");
        }
        Err(format!("drop-bounds gate: {} offender(s)", bad.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tys(src: &str) -> Vec<String> {
        risky_drop_impls(src).into_iter().map(|r| r.ty).collect()
    }

    #[test]
    fn the_issue_patterns_are_found() {
        assert_eq!(tys("struct Q<T: 'static + Send>(Option<T>);\nimpl<T: 'static + Send> Drop for Q<T> {\n fn drop(&mut self) {}\n}\n"), ["Q"]);
        assert_eq!(tys("impl<'a: 'static> Drop for Q<'a> { fn drop(&mut self) {} }"), ["Q"]);
        assert_eq!(tys("impl<T: Copy, F: FnMut(T)> Drop for Q<T, F> { fn drop(&mut self) {} }"), ["Q"]);
        assert_eq!(tys("impl<T> Drop for W<T>\nwhere\n    T: Send,\n{\n fn drop(&mut self) {}\n}"), ["W"]);
        assert_eq!(tys("impl<T: core::fmt::Debug> Drop for D<T> { fn drop(&mut self) {} }"), ["D"]);
    }

    #[test]
    fn plain_impls_pass() {
        assert!(tys("impl Drop for Plain { fn drop(&mut self) {} }").is_empty());
        assert!(tys("impl<T> Drop for PerCpu<T> { fn drop(&mut self) {} }").is_empty());
        assert!(tys("impl<T: ?Sized> Drop for MutexGuard<'_, T> { fn drop(&mut self) {} }").is_empty());
        assert_eq!(tys("impl<T: ?Sized + Send> Drop for G<T> { fn drop(&mut self) {} }"), ["G"]);
        assert!(tys("impl<const N: usize> Drop for Secret<N> { fn drop(&mut self) {} }").is_empty());
        assert_eq!(tys("impl<const N: usize, T: Send> Drop for S<N, T> { fn drop(&mut self) {} }"), ["S"]);
        assert!(tys("// impl<T: 'static> Drop for Q<T> {}\n/* impl<T: Send> Drop for R<T> {} */\nfn f() {}").is_empty());
    }

    #[test]
    fn line_numbers_survive_block_comments() {
        let r = risky_drop_impls("/* a\nb\n*/\nimpl<T: Send>\n Drop for Z<T> {}");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].line, 4);
    }
}

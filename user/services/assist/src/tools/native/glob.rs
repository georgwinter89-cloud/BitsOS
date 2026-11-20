//! Glob patterns for `dateien_finden` and `text_suchen`: `*` and `?` within one name, `**` across folders,
//! `[abc]` and `[a-z]` (with `!` or `^` for "none of"). Matched against the path relative to the search folder.

fn class(p: &[char], c: char) -> Option<(bool, usize)> {
    // p starts after '['; returns (matched, length up to and with ']').
    let mut i = 0;
    let negate = matches!(p.first(), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == ']' && !first {
            return Some((hit != negate, i + 1));
        }
        first = false;
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            hit |= p[i] <= c && c <= p[i + 2];
            i += 3;
        } else {
            hit |= p[i] == c;
            i += 1;
        }
    }
    None
}

/// One name against a pattern without `/`. Each (pattern place, name place) is looked at once: a pattern of
/// many `*` would otherwise take time that grows without bound with the name's length.
fn name_matches(p: &[char], s: &[char]) -> bool {
    let mut seen = vec![None; (p.len() + 1) * (s.len() + 1)];
    name_at(p, s, 0, 0, &mut seen)
}

fn name_at(p: &[char], s: &[char], i: usize, j: usize, seen: &mut [Option<bool>]) -> bool {
    let k = i * (s.len() + 1) + j;
    if let Some(v) = seen[k] {
        return v;
    }
    let v = match (p.get(i), s.get(j)) {
        (None, None) => true,
        (Some('*'), _) => name_at(p, s, i + 1, j, seen) || (j < s.len() && name_at(p, s, i, j + 1, seen)),
        (Some('?'), Some(_)) => name_at(p, s, i + 1, j + 1, seen),
        (Some('['), Some(c)) => match class(&p[i + 1..], *c) {
            Some((true, n)) => name_at(p, s, i + 1 + n, j + 1, seen),
            Some((false, _)) => false,
            None => *c == '[' && name_at(p, s, i + 1, j + 1, seen),
        },
        (Some(a), Some(b)) => a == b && name_at(p, s, i + 1, j + 1, seen),
        _ => false,
    };
    seen[k] = Some(v);
    v
}

/// The same once per (pattern part, path part), for the `**` that may stand for any number of folders.
fn parts_match(p: &[Vec<char>], s: &[Vec<char>]) -> bool {
    let mut seen = vec![None; (p.len() + 1) * (s.len() + 1)];
    parts_at(p, s, 0, 0, &mut seen)
}

fn parts_at(p: &[Vec<char>], s: &[Vec<char>], i: usize, j: usize, seen: &mut [Option<bool>]) -> bool {
    let k = i * (s.len() + 1) + j;
    if let Some(v) = seen[k] {
        return v;
    }
    let v = match p.get(i) {
        None => j == s.len(),
        Some(first) if first.iter().collect::<String>() == "**" => (j..=s.len()).any(|m| parts_at(p, s, i + 1, m, seen)),
        Some(first) => j < s.len() && name_matches(first, &s[j]) && parts_at(p, s, i + 1, j + 1, seen),
    };
    seen[k] = Some(v);
    v
}

fn split(s: &str) -> Vec<Vec<char>> {
    s.split('/').filter(|x| !x.is_empty()).map(|x| x.chars().collect()).collect()
}

/// The longest pattern taken: the matching looks at each place of it once per name, and its depth of calls
/// follows its length.
pub const PATTERN_MAX: usize = 1000;

/// Does `rel` (a path below the search folder, `/` between its names) fit `pattern`? A pattern without `/`
/// is matched against the file's name alone, at any depth (as `**/pattern`).
pub fn matches(pattern: &str, rel: &str) -> bool {
    let p = if pattern.contains('/') { split(pattern) } else { split(&format!("**/{pattern}")) };
    parts_match(&p, &split(rel))
}

/// The fixed folder a pattern starts with (`src/**/*.rs` starts in `src`), and the rest.
pub fn base(pattern: &str) -> (String, String) {
    let parts: Vec<&str> = pattern.split('/').filter(|x| !x.is_empty()).collect();
    let fixed = parts.iter().take_while(|p| !p.contains(['*', '?', '['])).count().min(parts.len().saturating_sub(1));
    (parts[..fixed].join("/"), parts[fixed..].join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        for (p, s, want) in [
            ("*.rs", "main.rs", true),
            ("*.rs", "src/a/main.rs", true),
            ("*.rs", "main.rsx", false),
            ("src/*.rs", "src/a.rs", true),
            ("src/*.rs", "src/x/a.rs", false),
            ("src/**/*.rs", "src/x/y/a.rs", true),
            ("src/**/*.rs", "src/a.rs", true),
            ("**", "a/b/c", true),
            ("a?c.txt", "abc.txt", true),
            ("[ab]*.pdf", "b1.pdf", true),
            ("[!ab]*.pdf", "b1.pdf", false),
            ("[a-c]x", "bx", true),
            ("Rechnung*", "Downloads/Rechnung (1).pdf", true),
            ("*", "", false),
        ] {
            assert_eq!(matches(p, s), want, "{p} {s}");
        }
        assert_eq!(base("src/**/*.rs"), ("src".into(), "**/*.rs".into()));
        assert_eq!(base("*.pdf"), (String::new(), "*.pdf".into()));
        assert_eq!(base("a/b/c.txt"), ("a/b".into(), "c.txt".into()));
    }

    #[test]
    fn many_stars_take_no_long_time() {
        let t = std::time::Instant::now();
        let name = "a".repeat(200);
        assert!(!matches("*a*a*a*a*a*a*a*a*a*a*a*a*b", &name));
        assert!(matches("*a*a*a*a*a*a*a*a*a*a*a*a", &name));
        let deep = vec!["d"; 60].join("/");
        assert!(!matches("**/**/**/**/**/**/**/**/x", &deep));
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}", t.elapsed());
    }
}

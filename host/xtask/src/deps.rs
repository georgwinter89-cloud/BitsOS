//! `xtask deps`: parse the item tables of plan/V2-LANES.md and check the "Builds on" graph (V2-LANES 3.2).
use crate::util::Args;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Default)]
pub struct Graph {
    /// item -> items it builds on (after expansion), in file order
    pub deps: BTreeMap<String, Vec<String>>,
    pub warnings: Vec<String>,
}

/// Splits a table row on '|' (a backslash-escaped bar stays in the cell); drops the empty outer cells.
fn cells(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut esc = false;
    for c in line.trim().chars() {
        if esc {
            out.last_mut().unwrap().push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '|' {
            out.push(String::new());
        } else {
            out.last_mut().unwrap().push(c);
        }
    }
    out.remove(0);
    if out.last().map(|s| s.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.into_iter().map(|s| s.trim().to_string()).collect()
}

/// `BT-1 ★` -> `BT-1`; None when the cell is not an item id.
pub fn item_id(cell: &str) -> Option<String> {
    let t = cell.trim().trim_end_matches('★').trim();
    let (p, n) = t.split_once('-')?;
    let ok_p = (2..=3).contains(&p.len()) && p.chars().all(|c| c.is_ascii_uppercase());
    let digits = n.chars().take_while(|c| c.is_ascii_digit()).count();
    let tail = &n[digits..];
    if ok_p && digits > 0 && tail.len() <= 1 && tail.chars().all(|c| c.is_ascii_lowercase()) {
        Some(t.to_string())
    } else {
        None
    }
}

fn strip_parens(s: &str) -> String {
    let mut depth = 0;
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Item references in a "Builds on" cell: `AB-9`, `MM-5a`, ranges `AB-5..8`. Words like G0, A1, none, `C: X` are not items.
pub fn refs(cell: &str) -> Vec<String> {
    let s: Vec<char> = strip_parens(cell).replace('`', " ").chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let start = i;
        let prev_ok = start == 0 || !(s[start - 1].is_ascii_alphanumeric());
        let mut j = i;
        while j < s.len() && s[j].is_ascii_uppercase() {
            j += 1;
        }
        let letters = j - i;
        if prev_ok && (2..=3).contains(&letters) && j < s.len() && s[j] == '-' {
            let mut k = j + 1;
            let ds = k;
            while k < s.len() && s[k].is_ascii_digit() {
                k += 1;
            }
            if k > ds {
                let prefix: String = s[i..j].iter().collect();
                let num: String = s[ds..k].iter().collect();
                // optional single lowercase letter suffix, not followed by another letter
                let mut suffix = String::new();
                if k < s.len() && s[k].is_ascii_lowercase() && !s.get(k + 1).map(|c| c.is_ascii_alphanumeric()).unwrap_or(false) {
                    suffix.push(s[k]);
                    k += 1;
                }
                // range `AB-5..8`
                if suffix.is_empty() && s.get(k) == Some(&'.') && s.get(k + 1) == Some(&'.') {
                    let mut m = k + 2;
                    let ms = m;
                    while m < s.len() && s[m].is_ascii_digit() {
                        m += 1;
                    }
                    if m > ms {
                        let end: String = s[ms..m].iter().collect();
                        if let (Ok(a), Ok(b)) = (num.parse::<u32>(), end.parse::<u32>()) {
                            for n in a..=b {
                                out.push(format!("{prefix}-{n}"));
                            }
                            i = m;
                            continue;
                        }
                    }
                }
                out.push(format!("{prefix}-{num}{suffix}"));
                i = k;
                continue;
            }
        }
        i = if j > i { j } else { i + 1 };
    }
    out
}

pub fn parse(text: &str) -> Graph {
    let mut g = Graph::default();
    let mut raw: Vec<(String, Vec<String>)> = Vec::new();
    let mut builds_col: Option<usize> = None;
    let mut in_table = false;
    for line in text.lines() {
        if !line.trim_start().starts_with('|') {
            in_table = false;
            builds_col = None;
            continue;
        }
        let c = cells(line);
        if !in_table {
            in_table = true;
            builds_col = c.iter().position(|h| h.trim_matches('*').trim().eq_ignore_ascii_case("builds on"));
            continue;
        }
        if c.iter().all(|x| x.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')) {
            continue;
        }
        let (Some(col), Some(first)) = (builds_col, c.first()) else { continue };
        let Some(id) = item_id(first) else { continue };
        let r = c.get(col).map(|x| refs(x)).unwrap_or_default();
        if raw.iter().any(|(i, _)| *i == id) {
            g.warnings.push(format!("{id} is defined twice"));
        }
        raw.push((id, r));
    }
    let known: BTreeSet<&str> = raw.iter().map(|(i, _)| i.as_str()).collect();
    let prefixes: BTreeSet<&str> = raw.iter().filter_map(|(i, _)| i.split('-').next()).collect();
    for (id, rs) in &raw {
        let mut deps = Vec::new();
        for r in rs {
            let pre = r.split('-').next().unwrap_or("");
            if known.contains(r.as_str()) {
                deps.push(r.clone());
            } else if let Some(fam) = family(&known, r) {
                deps.extend(fam);
            } else if prefixes.contains(pre) {
                g.warnings.push(format!("{id}: builds on unknown item {r}"));
            }
            // an unknown prefix (UTF-8, R8 ...) is not an item reference
        }
        g.deps.insert(id.clone(), deps);
    }
    g
}

/// `MM-5` stands for the family `MM-5a`, `MM-5b` when no plain `MM-5` exists.
fn family(known: &BTreeSet<&str>, r: &str) -> Option<Vec<String>> {
    let v: Vec<String> = known
        .iter()
        .filter(|k| k.starts_with(r) && k.len() == r.len() + 1 && k[r.len()..].chars().all(|c| c.is_ascii_lowercase()))
        .map(|k| k.to_string())
        .collect();
    if v.is_empty() { None } else { Some(v) }
}

/// A cycle as a path `A -> B -> A`, if there is one.
pub fn find_cycle(g: &Graph) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum C { White, Grey, Black }
    fn visit(n: &str, g: &Graph, col: &mut BTreeMap<String, C>, stack: &mut Vec<String>) -> Option<Vec<String>> {
        col.insert(n.to_string(), C::Grey);
        stack.push(n.to_string());
        for d in g.deps.get(n).map(|v| v.as_slice()).unwrap_or(&[]) {
            match col.get(d.as_str()).copied().unwrap_or(C::White) {
                C::Grey => {
                    let at = stack.iter().position(|x| x == d).unwrap();
                    let mut cyc = stack[at..].to_vec();
                    cyc.push(d.clone());
                    return Some(cyc);
                }
                C::White => {
                    if let Some(c) = visit(d, g, col, stack) {
                        return Some(c);
                    }
                }
                C::Black => {}
            }
        }
        stack.pop();
        col.insert(n.to_string(), C::Black);
        None
    }
    let mut col = BTreeMap::new();
    for n in g.deps.keys() {
        if col.get(n.as_str()).copied().unwrap_or(C::White) == C::White {
            if let Some(c) = visit(n, g, &mut col, &mut Vec::new()) {
                return Some(c);
            }
        }
    }
    None
}

pub fn run_deps(root: &Path) -> Result<(), String> {
    let p = root.join("plan/V2-LANES.md");
    // The public tree ships without plan/ (DECISIONS 24.10.2026), so there is no lane table to check there.
    if !p.exists() {
        println!("deps: skipped, no {}", p.display());
        return Ok(());
    }
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let g = parse(&text);
    let edges: usize = g.deps.values().map(|v| v.len()).sum();
    println!("deps: {} items, {} dependencies", g.deps.len(), edges);
    for w in &g.warnings {
        println!("deps: warning: {w}");
    }
    if let Some(c) = find_cycle(&g) {
        return Err(format!("deps: CYCLE: {}", c.join(" -> ")));
    }
    println!("deps: no cycle");
    Ok(())
}

pub fn cmd_deps(root: &Path, a: Args) -> Result<(), String> {
    a.positionals()?;
    run_deps(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Intro text with BT-9 in it.

| Item | Content | Size | Builds on | Integrates with |
|---|---|---|---|---|
| BT-1 ★ | loader | M | G0 (BootInfo v1) | – |
| BT-2 ★ | markers | S | BT-1 | – |
| MM-5a | pages | M | BT-2, MM-9 (not this FS-1), C: Pollable | none |
| MM-5b | more | S | MM-5a, A1 | |
| AB-8 | calls | M | BT-1..2, MM-5, ZZ-3, UTF-8 | - |

| Other | Table | Builds on |
|---|---|---|
| XX-1 | a | AB-8 |

| Item | Content |
|---|---|
| QQ-1 | no builds column |
";

    #[test]
    fn item_ids_and_refs() {
        assert_eq!(item_id("BT-1 ★ ").as_deref(), Some("BT-1"));
        assert_eq!(item_id("MM-5a").as_deref(), Some("MM-5a"));
        assert_eq!(item_id("AB-11").as_deref(), Some("AB-11"));
        assert_eq!(item_id("G0"), None);
        assert_eq!(item_id("Item"), None);
        assert_eq!(refs("G0, CP-1"), vec!["CP-1"]);
        assert_eq!(refs("YT-4a, AB-5..8, NW-6"), vec!["YT-4a", "AB-5", "AB-6", "AB-7", "AB-8", "NW-6"]);
        assert_eq!(refs("PC-1, G0 (`mcp_tool!`)"), vec!["PC-1"]);
        assert_eq!(refs("US-3, C: BlockDevice"), vec!["US-3"]);
        assert!(refs("none").is_empty() && refs("WL-4, A1") == vec!["WL-4"]);
        assert_eq!(refs("G0 (see FS-1)"), Vec::<String>::new());
    }

    #[test]
    fn parses_tables_by_column_name() {
        let g = parse(SAMPLE);
        assert_eq!(g.deps.keys().cloned().collect::<Vec<_>>(), vec!["AB-8", "BT-1", "BT-2", "MM-5a", "MM-5b", "XX-1"]);
        assert!(g.deps["BT-1"].is_empty());
        assert_eq!(g.deps["BT-2"], vec!["BT-1"]);
        assert_eq!(g.deps["MM-5a"], vec!["BT-2"]);
        // BT-1..2 is a range; MM-5 stands for the family MM-5a/MM-5b
        assert!(g.deps["AB-8"].contains(&"MM-5a".to_string()) && g.deps["AB-8"].contains(&"MM-5b".to_string()));
        assert_eq!(g.deps["XX-1"], vec!["AB-8"]);
        assert!(!g.deps.contains_key("QQ-1"));
        assert!(g.warnings.iter().any(|w| w.contains("MM-9")), "{:?}", g.warnings);
        assert!(!g.warnings.iter().any(|w| w.contains("UTF") || w.contains("ZZ")));
    }

    #[test]
    fn cycle_is_found_and_dag_is_fine() {
        assert!(find_cycle(&parse(SAMPLE)).is_none());
        let cyc = "| Item | Builds on |\n|---|---|\n| AA-1 | AA-3 |\n| AA-2 | AA-1 |\n| AA-3 | AA-2 |\n| AA-4 | AA-4 |\n";
        let g = parse(cyc);
        let c = find_cycle(&g).unwrap();
        assert_eq!(c.first(), c.last());
        assert!(c.len() >= 2);
    }

    #[test]
    fn duplicate_definition_warns() {
        let g = parse("| Item | Builds on |\n|---|---|\n| AA-1 | none |\n| AA-1 | none |\n");
        assert!(g.warnings.iter().any(|w| w.contains("twice")));
    }
}

//! `bitspkg search QUERY` and `bitspkg info --repo [REPO/]NAME` over the indexes, in source order. A name found
//! in an earlier repository hides the same name in a later one, as pacman installs from the first
//! (pacman/lib/libalpm/sync.c resolvedep, plan 1.5); search still lists the hidden one, marked with the source
//! that hides it, as `pacman -Ss` lists every repository's. Ranking: exact name, name prefix, name part, description;
//! among equal matches Rust programs first (DECISIONS 24.10.), then source order, then the name.

use super::index::Index;
use super::refresh;
use super::sources::Config;
use super::syncdb::Pkg;
use std::path::Path;

pub struct Hit {
    pub repo: String,
    pub pkg: Pkg,
    pub rank: u8,
    /// The earlier source whose package of the same name is the one installed.
    pub shadowed_by: Option<String>,
}

/// The loaded indexes in source order; a source not loaded yet is named in `missing`.
pub fn indexes(root: &Path, cfg: &Config) -> (Vec<(String, Index)>, Vec<String>) {
    let sync = refresh::sync_dir(root);
    let mut v = Vec::new();
    let mut missing = Vec::new();
    for s in &cfg.sources {
        match refresh::load(&sync, &s.name) {
            Ok(i) => v.push((s.name.clone(), i)),
            Err(e) => missing.push(e),
        }
    }
    (v, missing)
}

fn rank(p: &Pkg, q: &str) -> Option<u8> {
    let n = p.name.to_lowercase();
    if n == q {
        Some(0)
    } else if n.starts_with(q) {
        Some(1)
    } else if n.contains(q) || p.provides.iter().any(|x| x.to_lowercase().split(['=', '<', '>']).next() == Some(q)) {
        Some(2)
    } else if p.desc.to_lowercase().contains(q) {
        Some(3)
    } else {
        None
    }
}

pub fn search(idx: &[(String, Index)], query: &str) -> Vec<Hit> {
    let q = query.to_lowercase();
    let mut first: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    let mut hits = Vec::new();
    for (order, (repo, i)) in idx.iter().enumerate() {
        for p in i.pkgs.values() {
            let winner = *first.entry(p.name.as_str()).or_insert(repo.as_str());
            let shadowed_by = (winner != repo.as_str()).then(|| winner.to_string());
            if let Some(r) = rank(p, &q) {
                hits.push((r, !p.rust(), order, Hit { repo: repo.clone(), pkg: p.clone(), rank: r, shadowed_by }));
            }
        }
    }
    hits.sort_by(|a, b| (a.0, a.1, a.2, &a.3.pkg.name).cmp(&(b.0, b.1, b.2, &b.3.pkg.name)));
    hits.into_iter().map(|h| h.3).collect()
}

/// `NAME` (the first source that has it) or `REPO/NAME`.
pub fn find<'a>(idx: &'a [(String, Index)], what: &str) -> Option<(&'a str, &'a Pkg)> {
    let (repo, name) = match what.split_once('/') {
        Some((r, n)) => (Some(r), n),
        None => (None, what),
    };
    idx.iter().filter(|(r, _)| repo.is_none_or(|x| x == r)).find_map(|(r, i)| i.pkgs.get(name).map(|p| (r.as_str(), p)))
}

pub fn date(t: i64) -> String {
    // Civil date from Unix days (H. Hinnant's algorithm), UTC.
    let z = t.div_euclid(86400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn info_lines(repo: &str, p: &Pkg) -> Vec<String> {
    let mut v = vec![format!("{repo}/{} {}", p.name, p.version)];
    let mut f = |k: &str, val: String| {
        if !val.is_empty() {
            v.push(format!("  {k}: {val}"));
        }
    };
    f("beschreibung", p.desc.clone());
    f("url", p.url.clone());
    f("lizenz", p.licenses.join(" "));
    f("bietet", p.provides.join(" "));
    f("braucht", p.depends.join(" "));
    f("zusätze", p.optdepends.join("; "));
    f("konflikte", p.conflicts.join(" "));
    f("ersetzt", p.replaces.join(" "));
    f("rust", if p.rust() { "ja".into() } else { String::new() });
    f("download", format!("{} Bytes ({})", p.csize, p.filename));
    f("installiert", format!("{} Bytes", p.isize));
    f("gebaut", date(p.builddate));
    f("sha256", p.sha256.clone());
    f("signatur", if p.pgpsig.is_empty() { "keine in der Datenbank (.sig vom Spiegel)".into() } else { format!("{} Zeichen base64 in der Datenbank", p.pgpsig.len()) });
    v
}

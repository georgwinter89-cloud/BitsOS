//! `host/tests/stable.txt`: the tests that were never red in any kept log and green in many (`xtask stable-list`).
//! `xtask test --interim` leaves them to the integrator's full run; the full gate (`xtask test`) never reads the list.
use std::collections::BTreeSet;
use std::path::Path;

pub const FILE: &str = "host/tests/stable.txt";

/// Gate steps that are never left out: builds, static checks, the ledger, and the steps whose single tests are left
/// out one by one instead (cargo tests by `--skip`, ktest boots by the filter, scenario boots by script).
const NEVER_LEFT_OUT: &[&str] = &[
    "host tests", "user host tests", "reaper (musl)", "bitscomp tests", "forbid-unsafe", "drop-bounds", "home paths",
    "sysroot", "ledger start", "scenario boots", "ledger", "measure build", "warnings",
];

pub fn step_can_be_left_out(name: &str) -> bool {
    !NEVER_LEFT_OUT.contains(&name) && !name.starts_with("ktest")
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stable {
    pub ktest: BTreeSet<String>,
    /// `<crate>::<target source>::<test path>`, e.g. `hidparse::tests/fuzz.rs::large_hostile_shapes_stay_linear`.
    pub host: BTreeSet<String>,
    /// `<folder>/<script>`.
    pub scenario: BTreeSet<String>,
    pub step: BTreeSet<String>,
}

/// A ktest name as the kernel registers them: dotted lower-case words.
pub fn is_ktest_name(s: &str) -> bool {
    s.contains('.') && s.split('.').all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
}

/// `crate::target::path` -> its three parts. Crate names hold no `::`, target sources no `::` and always a `/`.
pub fn host_key_parts(key: &str) -> Option<(&str, &str, &str)> {
    let (krate, rest) = key.split_once("::")?;
    let (target, path) = rest.split_once("::")?;
    (!krate.is_empty() && target.contains('/') && !path.is_empty()).then_some((krate, target, path))
}

/// A wrong line is an error, not a line that does nothing: a typo must not let a test run that someone meant to leave out,
/// nor leave out a step that must always run.
pub fn parse(text: &str) -> Result<Stable, String> {
    let mut s = Stable::default();
    for (i, l) in text.lines().enumerate() {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let bad = |why: &str| format!("{FILE}:{}: {why}: {l}", i + 1);
        let (kind, name) = l.split_once(':').ok_or_else(|| bad("no kind prefix"))?;
        match kind {
            "ktest" if is_ktest_name(name) => s.ktest.insert(name.to_string()),
            "host" if host_key_parts(name).is_some() => s.host.insert(name.to_string()),
            "scenario" if name.split_once('/').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty()) => s.scenario.insert(name.to_string()),
            "step" if step_can_be_left_out(name) => s.step.insert(name.to_string()),
            "step" => return Err(bad("this step always runs")),
            "ktest" | "host" | "scenario" => return Err(bad("malformed name")),
            _ => return Err(bad("unknown kind")),
        };
    }
    Ok(s)
}

pub fn load(root: &Path) -> Result<Stable, String> {
    let p = root.join(FILE);
    parse(&std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?)
}

/// The test binary a cargo `Running` line names: (crate, binary, target source). The crate is `None` in the old
/// layout (`.../deps/<bin>-<hash>`), whose path names only the binary.
/// `Running tests/fuzz.rs (target/debug/build/hidparse/<h>/out/fuzz-<h>)` -> (Some("hidparse"), "fuzz", "tests/fuzz.rs").
pub fn running_binary(line: &str) -> Option<(Option<String>, String, String)> {
    let r = line.trim_start().strip_prefix("Running ")?;
    let r = r.strip_prefix("unittests ").unwrap_or(r);
    let (target, path) = r.split_once(" (")?;
    let path = path.trim_end().strip_suffix(')')?;
    let comps: Vec<&str> = path.split('/').collect();
    let file = *comps.last()?;
    let bin = file.rsplit_once('-').map_or(file, |(b, _)| b).to_string();
    let krate = (0..comps.len().saturating_sub(4)).rev().find(|&i| comps[i] == "build" && comps[i + 3] == "out").map(|i| comps[i + 1].to_string());
    Some((krate, bin, target.trim().to_string()))
}

/// The libtest arguments that leave out exactly the listed tests of one cargo run, from that run's `-- --list` output,
/// and how many tests they leave out. `--skip` reaches every binary of the run, so a test path is skipped only when
/// every binary that has it lists it as stable (a binary of unknown crate never does).
pub fn host_skips(list_out: &str, st: &Stable) -> (Vec<String>, usize) {
    let mut bin: Option<Option<String>> = None;
    let mut seen: std::collections::BTreeMap<String, (usize, bool)> = Default::default();
    for l in list_out.lines() {
        if let Some((krate, _, target)) = running_binary(l) {
            bin = Some(krate.map(|k| format!("{k}::{target}")));
            continue;
        }
        if l.trim_start().starts_with("Doc-tests ") {
            bin = Some(None);
            continue;
        }
        let Some(path) = l.strip_suffix(": test") else { continue };
        let stable = matches!(&bin, Some(Some(b)) if st.host.contains(&format!("{b}::{path}")));
        let e = seen.entry(path.to_string()).or_insert((0, true));
        e.0 += 1;
        e.1 &= stable;
    }
    let mut args = Vec::new();
    let mut n = 0;
    for (path, (count, all)) in seen {
        if all {
            n += count;
            args.extend(["--skip".to_string(), path]);
        }
    }
    if !args.is_empty() {
        args.insert(0, "--exact".into());
    }
    (args, n)
}

/// Every string literal of ktest-name shape in the kernel's sources: the names a boot may have, new tests included.
pub fn source_names(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut todo = vec![root.join("crates"), root.join("kernel")];
    while let Some(d) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if p.extension().is_some_and(|x| x == "rs")
                && let Ok(text) = std::fs::read_to_string(&p)
            {
                out.extend(text.split('"').filter(|s| is_ktest_name(s)).map(String::from));
            }
        }
    }
    out
}

/// The listed ktest names a boot can leave out. The kernel's `-name` takes out every test whose name starts with it,
/// so a name that is the prefix of a known test not in the list stays in (it runs; the other one too).
pub fn ktest_exclude(st: &Stable, known: &BTreeSet<String>) -> Vec<String> {
    st.ktest
        .iter()
        .filter(|n| !known.range(n.to_string()..).take_while(|m| m.starts_with(n.as_str())).any(|m| m != *n && !st.ktest.contains(m)))
        .cloned()
        .collect()
}

/// What an interim boot leaves out of this tree: `ktest_exclude` against the tree's sources.
pub fn boot_exclude(root: &Path, st: &Stable) -> Vec<String> {
    let mut known = source_names(root);
    known.extend(st.ktest.iter().cloned());
    ktest_exclude(st, &known)
}

/// `filter` with the names taken out (the kernel's `-prefix` terms).
pub fn with_exclude(filter: &str, exclude: &[String]) -> String {
    let mut s = filter.to_string();
    for e in exclude {
        if !s.is_empty() {
            s.push(',');
        }
        s.push('-');
        s.push_str(e);
    }
    s
}

/// True when every test the filter selects is taken out: each wanted prefix starts with an excluded name.
pub fn runs_nothing(filter: &str, exclude: &[String]) -> bool {
    let wanted: Vec<&str> = filter.split(',').map(str::trim).filter(|p| !p.is_empty() && !p.starts_with('-')).collect();
    !wanted.is_empty() && wanted.iter().all(|p| exclude.iter().any(|e| p.starts_with(e.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "# made by hand\n\nktest:mm.alloc\nktest:mm.alloc.big\nhost:hidparse::tests/fuzz.rs::a\n\
        host:bitsos-install::src/main.rs::tests::b\nscenario:rb/20-reload\nstep:wy gles\n";

    #[test]
    fn the_list_is_read_by_kind() {
        let s = parse(LIST).unwrap();
        assert_eq!(s.ktest.len(), 2);
        assert!(s.host.contains("bitsos-install::src/main.rs::tests::b"));
        assert!(s.scenario.contains("rb/20-reload") && s.step.contains("wy gles"));
    }

    #[test]
    fn a_wrong_line_is_an_error() {
        for bad in ["mm.alloc", "kest:mm.alloc", "ktest:Mm.Alloc", "host:hidparse::a", "scenario:reload", "step:warnings", "step:ktest release", "step:host tests"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(host_key_parts("a::src/lib.rs::m::t") == Some(("a", "src/lib.rs", "m::t")));
    }

    #[test]
    fn running_lines_name_crate_binary_and_target() {
        let l = "     Running tests/fuzz.rs (target/debug/build/hidparse/6ded2c805d736914/out/fuzz-6ded2c805d736914)";
        assert_eq!(running_binary(l), Some((Some("hidparse".into()), "fuzz".into(), "tests/fuzz.rs".into())));
        let l = "     Running unittests src/main.rs (target/x86_64-unknown-linux-musl/debug/build/bitsos-install/9f1e/out/bitsos_install-9f1e)";
        assert_eq!(running_binary(l), Some((Some("bitsos-install".into()), "bitsos_install".into(), "src/main.rs".into())));
        let l = "     Running unittests src/lib.rs (/var/tmp/x/target/debug/deps/bitsmusik-44426455d4112e30)";
        assert_eq!(running_binary(l), Some((None, "bitsmusik".into(), "src/lib.rs".into())));
        assert_eq!(running_binary("test a ... ok"), None);
    }

    #[test]
    fn only_tests_stable_in_every_binary_are_skipped_exactly() {
        let st = parse("host:a::src/lib.rs::tests::x\nhost:a::src/lib.rs::tests::y\nhost:b::tests/t.rs::tests::y\nhost:a::src/lib.rs::z\n").unwrap();
        let out = "   Compiling a\n     Running unittests src/lib.rs (target/debug/build/a/1/out/a-1)\ntests::x: test\ntests::y: test\ntests::xx: test\nz: test\n\n4 tests, 0 benchmarks\n\
            \x20    Running tests/t.rs (target/debug/build/b/2/out/t-2)\ntests::y: test\n\
            \x20    Running tests/u.rs (target/debug/deps/u-3)\nz: test\n   Doc-tests a\nsrc/lib.rs - f (line 3): test\n";
        let (args, n) = host_skips(out, &st);
        assert_eq!(args, ["--exact", "--skip", "tests::x", "--skip", "tests::y"]);
        assert_eq!(n, 3);
        assert_eq!(host_skips(out, &Stable::default()), (vec![], 0));
    }

    #[test]
    fn a_ktest_name_that_is_a_prefix_of_another_test_stays_in() {
        let st = parse("ktest:mm.alloc\nktest:mm.free\nktest:mm.map\nktest:mm.map.huge\n").unwrap();
        let known: BTreeSet<String> = ["mm.alloc", "mm.alloc_big", "mm.free", "mm.free.x", "mm.map", "mm.map.huge"].map(String::from).into();
        let mut known_free = known.clone();
        known_free.remove("mm.free.x");
        assert_eq!(ktest_exclude(&st, &known), ["mm.map", "mm.map.huge"]);
        assert_eq!(ktest_exclude(&st, &known_free), ["mm.free", "mm.map", "mm.map.huge"]);
    }

    #[test]
    fn the_exclude_joins_the_filter() {
        let ex = vec!["a.b".to_string(), "c.d".to_string()];
        assert_eq!(with_exclude("", &ex), "-a.b,-c.d");
        assert_eq!(with_exclude("mm.", &ex), "mm.,-a.b,-c.d");
        assert_eq!(with_exclude("mm.", &[]), "mm.");
        assert!(runs_nothing("a.b", &ex) && runs_nothing("a.b.x,c.d", &ex));
        assert!(!runs_nothing("", &ex) && !runs_nothing("a.", &ex) && !runs_nothing("a.b,e.f", &ex));
    }

    /// The kernel's filter rule (crates/ktest) against what `with_exclude` builds: only the excluded names go.
    #[test]
    fn the_kernel_takes_out_exactly_the_excluded_names() {
        let ex = vec!["mm.map".to_string()];
        let f = with_exclude("", &ex);
        assert!(!::ktest::matches(&f, "mm.map") && ::ktest::matches(&f, "mm.alloc"));
        // A prefix rule: why `ktest_exclude` keeps names that start another test's name.
        assert!(!::ktest::matches(&f, "mm.mapper.x"));
        let f = with_exclude("mm.", &ex);
        assert!(::ktest::matches(&f, "mm.alloc") && !::ktest::matches(&f, "fs.x"));
    }
}

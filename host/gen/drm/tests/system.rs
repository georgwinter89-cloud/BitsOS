//! The checked-in files against this host's headers. Three tests, so one skip does not hide another check:
//! the pinned hashes against the generated files (always runs), a fresh run against the checked-in files, and the
//! C probe against tests/data/c-layout.txt. The last two skip with a loud SKIPPED line when the headers are absent
//! or another version (the line names both hashes), or when no C compiler exists.

use std::path::{Path, PathBuf};

use drm_gen::{C_LAYOUT, INPUTS, PROBE_C, SET, fnv1a64, generate, run_probe};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// The installed inputs if all of them are the reviewed versions; otherwise prints why the caller skips.
fn reviewed_headers(what: &str) -> Option<Vec<Vec<u8>>> {
    let include = Path::new("/usr/include");
    let mut texts = Vec::new();
    for (k, (rel, _)) in INPUTS.iter().enumerate() {
        let Ok(t) = std::fs::read(include.join(rel)) else {
            eprintln!("SKIPPED {what}: {rel} is not installed");
            return None;
        };
        let h = fnv1a64(&t);
        if SET[k] != 0 && h != SET[k] {
            eprintln!("SKIPPED {what}: {rel} has FNV-1a 64 {h:016x}, the reviewed one {:016x} (host/gen/drm/src/lib.rs SET)", SET[k]);
            return None;
        }
        texts.push(t);
    }
    Some(texts)
}

#[test]
fn pinned_hashes_are_the_ones_in_the_generated_files() {
    for (k, (rel, module)) in INPUTS.iter().enumerate() {
        let path = repo().join(format!("crates/drm/src/uapi/{module}.rs"));
        let text = std::fs::read_to_string(&path).unwrap();
        let line = format!("// input {rel}: ");
        let got = text.lines().find(|l| l.starts_with(&line)).unwrap_or_else(|| panic!("{module}.rs has no '{line}' line"));
        assert!(got.ends_with(&format!("FNV-1a 64 {:016x}", SET[k])), "{module}.rs was generated from another {rel}: {got}");
    }
}

#[test]
fn checked_in_files_are_fresh() {
    let Some(texts) = reviewed_headers("drm-gen freshness check") else { return };
    let o = generate(&texts).unwrap();
    for (rel, bytes) in &o.files {
        let disk = std::fs::read(repo().join(rel)).unwrap_or_default();
        assert!(disk == *bytes, "{rel} is out of date: cargo run --offline -p drm-gen");
    }
}

#[test]
fn c_probe_matches_c_layout() {
    if reviewed_headers("C-PROBE").is_none() {
        return;
    }
    let probe_c = std::fs::read_to_string(repo().join(PROBE_C)).unwrap();
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("drm-gen-probe");
    match run_probe(&probe_c, Path::new("/usr/include"), &work).unwrap() {
        None => eprintln!("SKIPPED C-PROBE: no C compiler, {C_LAYOUT} not compared with GCC"),
        Some(txt) => assert!(std::fs::read_to_string(repo().join(C_LAYOUT)).unwrap() == txt, "{C_LAYOUT} differs from the probe"),
    }
}

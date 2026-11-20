//! The checked-in files against this host's headers. Three tests, so one red does not hide another check:
//! the pinned hashes against the generated files, a fresh run against the checked-in files, and the C probe
//! against tests/data/c-layout.txt. The last two fail when a header is absent or another version (the message
//! names both hashes: regenerate, review, update SET) or when no C compiler exists; they never skip.

use std::path::{Path, PathBuf};

use drm_gen::{C_LAYOUT, INPUTS, PROBE_C, SET, fnv1a64, generate, run_probe};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// The installed inputs; each must be the reviewed version.
fn reviewed_headers(what: &str) -> Vec<Vec<u8>> {
    let include = Path::new("/usr/include");
    let mut texts = Vec::new();
    for (k, (rel, _)) in INPUTS.iter().enumerate() {
        let t = std::fs::read(include.join(rel)).unwrap_or_else(|e| panic!("{what}: /usr/include/{rel}: {e} (linux-api-headers, glibc)"));
        let h = fnv1a64(&t);
        assert!(
            SET[k] == 0 || h == SET[k],
            "{what}: {rel} has FNV-1a 64 {h:016x}, the reviewed one {:016x}: the host headers changed; run `cargo run --offline -p drm-gen`, review the diff, update SET in host/gen/drm/src/lib.rs",
            SET[k]
        );
        texts.push(t);
    }
    texts
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
    let texts = reviewed_headers("drm-gen freshness check");
    let o = generate(&texts).unwrap();
    for (rel, bytes) in &o.files {
        let disk = std::fs::read(repo().join(rel)).unwrap_or_default();
        assert!(disk == *bytes, "{rel} is out of date: cargo run --offline -p drm-gen");
    }
}

#[test]
fn c_probe_matches_c_layout() {
    reviewed_headers("C-PROBE");
    let probe_c = std::fs::read_to_string(repo().join(PROBE_C)).unwrap();
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("drm-gen-probe");
    match run_probe(&probe_c, Path::new("/usr/include"), &work).unwrap() {
        None => panic!("C-PROBE: no C compiler (cc), {C_LAYOUT} cannot be compared with GCC (install gcc)"),
        Some(txt) => assert!(std::fs::read_to_string(repo().join(C_LAYOUT)).unwrap() == txt, "{C_LAYOUT} differs from the probe"),
    }
}

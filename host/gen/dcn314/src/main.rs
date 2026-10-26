//! dcn314-gen: writes crates/dcn314-regs/src/generated/ and tests/data/p1-decoded.txt.
//!
//!   cargo run --offline -- [--root <BitsOS dir>] [--out <crate dir>] [--check]
//!
//! --root defaults to $BITSOS_ROOT or /home/georg/BitsOS (inputs are read-only there);
//! --out defaults to crates/dcn314-regs of the tree this generator lives in.
//! --check writes nothing and fails if the files on disk differ from a fresh run.

#![forbid(unsafe_code)]

mod emit;
mod parse;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const DCN_DIR: &str = "werkzeuge/plattform/gpu/linux-6.12/drivers/gpu/drm/amd/include/asic_reg/dcn";
const P1: &str = "werkzeuge/plattform/gpu/messung/p1-decoded.txt";

pub struct Output {
    pub files: Vec<(String, Vec<u8>)>, // path relative to the crate dir
    pub excluded: usize,
    pub registers: usize,
    pub fields: usize,
}

pub fn generate(offset_h: &[u8], sh_mask_h: &[u8], p1: &[u8]) -> Result<Output, String> {
    let off = std::str::from_utf8(offset_h).map_err(|e| format!("offset.h: {e}"))?;
    let sh = std::str::from_utf8(sh_mask_h).map_err(|e| format!("sh_mask.h: {e}"))?;
    let p1s = std::str::from_utf8(p1).map_err(|e| format!("p1: {e}"))?;
    let raw = parse::offsets(off)?;
    let known: BTreeMap<String, usize> = raw.iter().enumerate().map(|(i, r)| (r.name.clone(), i)).collect();
    let (fields, orphans) = parse::fields(sh, &known)?;
    if !orphans.is_empty() {
        // A field without its register would silently vanish from the crate.
        return Err(format!("sh_mask.h: {} defines without a register, e.g. {}", orphans.len(), orphans[0]));
    }
    let snap = parse::snapshot(p1s)?;
    let (regs, excl) = emit::classify(raw, &fields)?;
    let inputs = [
        emit::Input { label: "dcn_3_1_4_offset.h", bytes: offset_h },
        emit::Input { label: "dcn_3_1_4_sh_mask.h", bytes: sh_mask_h },
        emit::Input { label: "p1-decoded.txt", bytes: p1 },
    ];
    let mut files: Vec<(String, Vec<u8>)> = emit::render(&regs, &excl, &snap, &inputs)
        .into_iter()
        .map(|(p, s)| (format!("src/generated/{p}"), s.into_bytes()))
        .collect();
    files.push(("tests/data/p1-decoded.txt".to_string(), p1.to_vec()));
    Ok(Output {
        files,
        excluded: excl.len(),
        fields: regs.iter().map(|r| r.fields.len()).sum(),
        registers: regs.len(),
    })
}

fn read(p: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn run() -> Result<(), String> {
    let mut root = std::env::var_os("BITSOS_ROOT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/home/georg/BitsOS"));
    let mut out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../crates/dcn314-regs");
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => root = args.next().ok_or("--root needs a path")?.into(),
            "--out" => out = args.next().ok_or("--out needs a path")?.into(),
            "--check" => check = true,
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    let dcn = root.join(DCN_DIR);
    let o = generate(&read(&dcn.join("dcn_3_1_4_offset.h"))?, &read(&dcn.join("dcn_3_1_4_sh_mask.h"))?, &read(&root.join(P1))?)?;
    eprintln!("dcn314-gen: {} registers, {} fields, {} excluded", o.registers, o.fields, o.excluded);
    let r = sync(&out, &o.files, check)?;
    if check {
        if r.differ.is_empty() && r.stale.is_empty() {
            eprintln!("dcn314-gen: up to date ({} files)", o.files.len());
            return Ok(());
        }
        return Err(format!("out of date: {} differ, {} stale: {:?} {:?}", r.differ.len(), r.stale.len(), r.differ, r.stale));
    }
    eprintln!(
        "dcn314-gen: {} files written, {} removed, {} unchanged",
        r.differ.len(),
        r.stale.len(),
        o.files.len() - r.differ.len()
    );
    Ok(())
}

struct Sync {
    differ: Vec<String>,
    stale: Vec<PathBuf>,
}

/// Compares `files` with the crate dir `out`; unless `check`, writes what differs and removes stale
/// files under src/generated.
fn sync(out: &Path, files: &[(String, Vec<u8>)], check: bool) -> Result<Sync, String> {
    let gen_dir = out.join("src/generated");
    let mut stale = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&gen_dir) {
        for e in rd.flatten() {
            let rel = format!("src/generated/{}", e.file_name().to_string_lossy());
            if !files.iter().any(|(p, _)| *p == rel) {
                stale.push(e.path());
            }
        }
    }
    stale.sort();
    let mut differ = Vec::new();
    for (rel, bytes) in files {
        let p = out.join(rel);
        if std::fs::read(&p).ok().as_deref() != Some(bytes.as_slice()) {
            differ.push(rel.clone());
            if !check {
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
                }
                std::fs::write(&p, bytes).map_err(|e| format!("{}: {e}", p.display()))?;
            }
        }
    }
    if !check {
        for p in &stale {
            std::fs::remove_file(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    Ok(Sync { differ, stale })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dcn314-gen: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINI: [&[u8]; 3] = [
        include_bytes!("../tests/data/mini_offset.h"),
        include_bytes!("../tests/data/mini_sh_mask.h"),
        include_bytes!("../tests/data/mini_p1.txt"),
    ];

    /// The real Linux headers; missing inputs fail unless DCN314_ALLOW_SKIP=1 (machine without werkzeuge).
    fn real_inputs() -> Option<[Vec<u8>; 3]> {
        let root = std::env::var_os("BITSOS_ROOT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/home/georg/BitsOS"));
        let dcn = root.join(DCN_DIR);
        let r = (|| Ok::<_, String>([
            read(&dcn.join("dcn_3_1_4_offset.h"))?,
            read(&dcn.join("dcn_3_1_4_sh_mask.h"))?,
            read(&root.join(P1))?,
        ]))();
        match r {
            Ok(x) => Some(x),
            Err(e) if std::env::var_os("DCN314_ALLOW_SKIP").is_some_and(|v| v == "1") => {
                eprintln!("real inputs not found ({e}), skipped by DCN314_ALLOW_SKIP=1");
                None
            }
            Err(e) => panic!("real inputs not found: {e} (set BITSOS_ROOT, or DCN314_ALLOW_SKIP=1)"),
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dcn314-gen-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn mini_two_runs_are_identical() {
        let x = generate(MINI[0], MINI[1], MINI[2]).unwrap();
        let y = generate(MINI[0], MINI[1], MINI[2]).unwrap();
        assert_eq!(x.files, y.files);
        assert_eq!((x.registers, x.excluded, x.fields), (3, 1, 4));
    }

    #[test]
    fn real_two_runs_are_identical() {
        let Some([a, b, c]) = real_inputs() else { return };
        let x = generate(&a, &b, &c).unwrap();
        let y = generate(&a, &b, &c).unwrap();
        assert_eq!(x.files, y.files);
    }

    #[test]
    fn check_mode_detects_drift() {
        let d = scratch("check");
        let o = generate(MINI[0], MINI[1], MINI[2]).unwrap();
        let s = sync(&d, &o.files, true).unwrap();
        assert_eq!(s.differ.len(), o.files.len(), "empty dir: everything differs");
        assert!(std::fs::read_dir(&d).is_err(), "check mode wrote something");
        sync(&d, &o.files, false).unwrap();
        let clean = |d: &Path| {
            let s = sync(d, &o.files, true).unwrap();
            s.differ.is_empty() && s.stale.is_empty()
        };
        assert!(clean(&d));

        let (rel, _) = o.files.iter().find(|(p, _)| p.starts_with("src/generated/block_")).unwrap();
        let p = d.join(rel);
        let mut edited = std::fs::read(&p).unwrap();
        edited.extend_from_slice(b"// hand edit\n");
        std::fs::write(&p, edited).unwrap();
        let s = sync(&d, &o.files, true).unwrap();
        assert_eq!(s.differ, std::slice::from_ref(rel));

        std::fs::write(d.join("src/generated/block_gone.rs"), b"").unwrap();
        let s = sync(&d, &o.files, true).unwrap();
        assert_eq!(s.stale, [d.join("src/generated/block_gone.rs")]);
        assert!(!clean(&d));

        sync(&d, &o.files, false).unwrap();
        assert!(clean(&d));
        assert!(!d.join("src/generated/block_gone.rs").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        type Case = (&'static [u8], &'static [u8], &'static [u8], &'static str);
        let cases: &[Case] = &[
            (b"#define\n", b"", b"", "unexpected define"),
            (b"#define regX zz\n", b"", b"", "bad number"),
            (b"#define what 1\n", b"", b"", "unexpected define"),
            (b"#define regX 1\n#define regX 2\n", b"", b"", "twice"),
            (b"#define regX_BASE_IDX 2\n", b"", b"", "without its register"),
            (b"", b"#define\n", b"", "unexpected define"),
            (b"", b"#define A__B_MASK 0x1L\n", b"", "without a register"),
            (b"#define regA 1\n", b"#define A__B_BITS 0x1\n", b"", "neither SHIFT nor MASK"),
            (b"#define regA 1\n", b"#define A__B__SHIFT 0x0\n", b"", "missing"),
            (b"#define regA 1\n", b"#define A__B__SHIFT 0x0\n#define A__B_MASK 0x5L\n", b"", "not contiguous"),
            (b"", b"", b"0x10\n", "short line"),
            (&[0xFF, 0xFE], b"", b"", "offset.h"),
            (b"", &[0xC3, 0x28], b"", "sh_mask.h"),
            (b"", b"", &[0x80], "p1"),
        ];
        for (a, b, c, want) in cases {
            match generate(a, b, c) {
                Err(e) => assert!(e.contains(want), "{e:?} lacks {want:?}"),
                Ok(_) => panic!("accepted {:?} {:?} {:?}", a, b, c),
            }
        }
        // Truncated real-shaped input: every prefix either parses or errors, never panics.
        for n in 0..MINI[0].len() {
            let _ = generate(&MINI[0][..n], MINI[1], MINI[2]);
        }
        for n in 0..MINI[1].len() {
            let _ = generate(MINI[0], &MINI[1][..n], MINI[2]);
        }
        assert!(generate(b"", b"", b"").is_ok());
    }
}

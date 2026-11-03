//! drm-gen: writes crates/drm/src/uapi/*.rs, crates/drm/tests/data/{layout_table.rs,c-layout.txt} and
//! host/gen/drm/tests/probe.c.
//!
//!   cargo run --offline -p drm-gen -- [--include <dir>] [--out <repo root>] [--check] [--no-probe]
//!
//! --include defaults to /usr/include (the headers libdrm and linux-api-headers install).
//! --out defaults to the tree this generator lives in.
//! --check writes nothing and fails if the files on disk differ from a fresh run.
//! --no-probe skips compiling the C probe (c-layout.txt is then neither written nor checked).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use drm_gen::{C_LAYOUT, INPUTS, generate, run_probe};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("drm-gen: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut include = PathBuf::from("/usr/include");
    let mut out = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.."));
    let (mut check, mut probe) = (false, true);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--include" => include = args.next().ok_or("--include needs a directory")?.into(),
            "--out" => out = args.next().ok_or("--out needs a directory")?.into(),
            "--check" => check = true,
            "--no-probe" => probe = false,
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    let mut texts = Vec::new();
    for (rel, _) in INPUTS {
        let p = include.join(rel);
        texts.push(std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?);
    }
    let o = generate(&texts)?;
    let mut files = o.files;
    if probe {
        let work = std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| out.join("target"), PathBuf::from).join("drm-gen");
        match run_probe(&o.probe_c, &include, &work)? {
            Some(txt) => files.push((C_LAYOUT.to_string(), txt.into_bytes())),
            None if check => eprintln!("drm-gen: no C compiler, {C_LAYOUT} not checked"),
            None => return Err("no C compiler (set CC, or pass --no-probe to keep the existing c-layout.txt)".into()),
        }
    }
    let mut stale = Vec::new();
    for (rel, bytes) in &files {
        let p = out.join(rel);
        if check {
            if std::fs::read(&p).ok().as_deref() != Some(bytes.as_slice()) {
                stale.push(rel.clone());
            }
        } else {
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            }
            std::fs::write(&p, bytes).map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    if !stale.is_empty() {
        return Err(format!("out of date (run drm-gen without --check): {}", stale.join(", ")));
    }
    println!("drm-gen: {} files {}, {} types, {} constants ({} ioctl numbers)", files.len(), if check { "checked" } else { "written" }, o.types, o.consts, o.ioctls);
    Ok(())
}

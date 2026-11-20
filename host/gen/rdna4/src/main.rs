//! rdna4-gen: writes crates/rdna4-regs/src/generated/ from Linux 7.3.1's `asic_reg` headers.
//!
//!   cargo run -p rdna4-gen -- [--linux <kernel tree>] [--out <crate dir>] [--check]
//!
//! --linux defaults to $LINUX_REF or /var/tmp/claude-v2/ref/linux-7.3.1 (read-only input);
//! --out defaults to crates/rdna4-regs of the tree this generator lives in;
//! --check writes nothing and fails if the files on disk differ from a fresh run.

#![forbid(unsafe_code)]

mod emit;
mod parse;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const ASIC_REG: &str = "drivers/gpu/drm/amd/include/asic_reg";

/// One block's file from its two headers.
pub fn generate(b: &emit::Block, offset_h: &str, sh_mask_h: &str) -> Result<(String, emit::Counts), String> {
    let mut regs = parse::offsets(offset_h, b.headerless, b.stray).map_err(|e| format!("{}: {e}", b.name))?;
    let (orphans, halves) = parse::fields(sh_mask_h, &mut regs, b.wide).map_err(|e| format!("{}: {e}", b.name))?;
    emit::block(b, &regs, orphans.len(), &halves)
}

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn run() -> Result<(), String> {
    let mut linux = PathBuf::from(std::env::var_os("LINUX_REF").unwrap_or_else(|| "/var/tmp/claude-v2/ref/linux-7.3.1".into()));
    let mut out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../crates/rdna4-regs");
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--linux" => linux = args.next().ok_or("--linux needs a path")?.into(),
            "--out" => out = args.next().ok_or("--out needs a path")?.into(),
            "--check" => check = true,
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    let mut files: Vec<(String, String)> = Vec::new();
    for b in &emit::BLOCKS {
        let dir = linux.join(ASIC_REG).join(b.dir);
        let (text, c) = generate(b, &read(&dir.join(format!("{}_offset.h", b.name)))?, &read(&dir.join(format!("{}_sh_mask.h", b.name)))?)?;
        eprintln!("rdna4-gen: {}: {} registers, {} indirect, {} config, {} fields", b.name, c.mmio, c.indirect, c.config, c.fields);
        files.push((format!("{}.rs", b.name), text));
    }
    files.push(("mod.rs".to_string(), emit::index(&emit::BLOCKS.iter().map(|b| b.name).collect::<Vec<_>>())));
    let gen_dir = out.join("src/generated");
    let differ: Vec<&str> = files.iter().filter(|(p, s)| std::fs::read_to_string(gen_dir.join(p)).ok().as_deref() != Some(s.as_str())).map(|(p, _)| p.as_str()).collect();
    let stale: Vec<PathBuf> = std::fs::read_dir(&gen_dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| !files.iter().any(|(n, _)| p.file_name().is_some_and(|f| f == n.as_str()))).collect())
        .unwrap_or_default();
    if check {
        return if differ.is_empty() && stale.is_empty() { Ok(()) } else { Err(format!("out of date: differ {differ:?}, stale {stale:?}")) };
    }
    std::fs::create_dir_all(&gen_dir).map_err(|e| format!("{}: {e}", gen_dir.display()))?;
    for (p, s) in &files {
        if differ.contains(&p.as_str()) {
            std::fs::write(gen_dir.join(p), s).map_err(|e| format!("{p}: {e}"))?;
        }
    }
    for p in &stale {
        std::fs::remove_file(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    eprintln!("rdna4-gen: {} files written, {} removed, {} unchanged", differ.len(), stale.len(), files.len() - differ.len());
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rdna4-gen: {e}");
            ExitCode::FAILURE
        }
    }
}

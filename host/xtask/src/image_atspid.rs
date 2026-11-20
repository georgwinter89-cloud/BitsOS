//! DB5a: atspid (the accessibility bus and Durchblick's bridge to foreign programs) and its test program
//! atspid-fake. A cargo workspace of its own like ctxd (its crates.io set, zbus and busd, stays out of
//! user/Cargo.lock): built static musl, `--locked --offline`, with user/'s stable toolchain, then stripped.

use crate::util::{run_status, xdir};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PROGRAMS: &[&str] = &["atspid", "atspid-fake"];

pub fn build(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let tdir = root.join("target/atspid");
    let mut c = Command::new("cargo");
    c.current_dir(root.join("user/services/atspid"))
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTUP_TOOLCHAIN")
        .args(["build", "--release", "--locked", "--offline", "--target", "x86_64-unknown-linux-musl", "--target-dir"])
        .arg(&tdir);
    run_status(&mut c, "building atspid (user/services/atspid)")?;
    let mut out = Vec::new();
    for name in PROGRAMS {
        let built = tdir.join("x86_64-unknown-linux-musl/release").join(name);
        let dst = xdir(root).join(name);
        let tmp = xdir(root).join(format!("{name}.tmp{}", std::process::id()));
        let st = Command::new("llvm-strip").arg("-o").arg(&tmp).arg(&built).status().map_err(|e| format!("llvm-strip: {e}"))?;
        if !st.success() {
            return Err(format!("llvm-strip {} failed ({st})", built.display()));
        }
        std::fs::rename(&tmp, &dst).map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), dst.display()))?;
        out.push((name.to_string(), dst));
    }
    Ok(out)
}

//! Builds the vDSO (AB-14) from `vdso/vdso.rs` with the toolchain's own rustc and rust-lld: the code for
//! x86_64-unknown-none (soft-float, so no SSE/AVX instruction), position-independent, linked as a shared object
//! with `vdso/vdso.lds` and the LINUX_2.6 version script. `src/vdso.rs` embeds `$OUT_DIR/vdso.so`.
//!
//! The image is checked here, because a loader that does not like it says nothing (glibc and musl then
//! silently make the syscalls): ELF64 ET_DYN, one PT_LOAD at address 0 / offset 0 that is not writable, a
//! PT_DYNAMIC with DT_HASH, DT_GNU_HASH, DT_VERSYM and DT_VERDEF, and no relocation or DT_NEEDED at all.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join("vdso");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    for f in ["vdso.rs", "vdso.lds", "vdso.ver", "../src/vdso/layout.rs"] {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let obj = out.join("vdso.o");
    let so = out.join("vdso.so");
    run(Command::new(&rustc)
        .args(["--edition", "2024", "--crate-type", "lib", "--crate-name", "bitsos_vdso", "--target", "x86_64-unknown-none"])
        .args(["-C", "opt-level=2", "-C", "panic=abort", "-C", "relocation-model=pic", "-C", "code-model=small"])
        .args(["-C", "debuginfo=0", "-C", "overflow-checks=off", "-C", "codegen-units=1", "-C", "force-unwind-tables=yes"])
        .arg(format!("--emit=obj={}", obj.display()))
        .arg(dir.join("vdso.rs")));
    run(Command::new(lld(&rustc))
        .args(["-flavor", "gnu", "-shared", "--hash-style=both", "--build-id=none", "-soname", "linux-vdso.so.1"])
        .args(["-z", "max-page-size=4096", "-z", "noexecstack", "--no-undefined", "-Bsymbolic", "--eh-frame-hdr"])
        .arg("--version-script")
        .arg(dir.join("vdso.ver"))
        .arg("-T")
        .arg(dir.join("vdso.lds"))
        .arg("-o")
        .arg(&so)
        .arg(&obj));
    let img = std::fs::read(&so).unwrap_or_else(|e| panic!("{}: {e}", so.display()));
    if let Err(e) = check(&img) {
        panic!("the vDSO image {} is not usable: {e}", so.display());
    }
}

fn run(c: &mut Command) {
    match c.status() {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("{c:?} failed: {s}"),
        Err(e) => panic!("{c:?}: {e}"),
    }
}

/// rust-lld of the toolchain that runs this build (`<sysroot>/lib/rustlib/<host>/bin`).
fn lld(rustc: &str) -> PathBuf {
    let o = Command::new(rustc).args(["--print", "sysroot"]).output().unwrap_or_else(|e| panic!("{rustc} --print sysroot: {e}"));
    let sysroot = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let host = env::var("HOST").expect("HOST");
    let p = Path::new(&sysroot).join("lib/rustlib").join(host).join("bin/rust-lld");
    if !p.exists() {
        panic!("no rust-lld at {} (the toolchain ships it)", p.display());
    }
    p
}

fn u16_at(b: &[u8], o: usize) -> Result<u16, String> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| format!("short at {o:#x}"))
}
fn u32_at(b: &[u8], o: usize) -> Result<u32, String> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| format!("short at {o:#x}"))
}
fn u64_at(b: &[u8], o: usize) -> Result<u64, String> {
    b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap_or([0; 8]))).ok_or_else(|| format!("short at {o:#x}"))
}

fn check(b: &[u8]) -> Result<(), String> {
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const PF_W: u32 = 2;
    if b.get(0..4) != Some(b"\x7fELF") || b.get(4) != Some(&2) || u16_at(b, 16)? != 3 {
        return Err("not an ELF64 shared object".into());
    }
    let (phoff, phentsize, phnum) = (u64_at(b, 32)? as usize, u16_at(b, 54)? as usize, u16_at(b, 56)? as usize);
    let mut loads = 0;
    let mut dynamic = None;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        let (ty, flags, off, vaddr, filesz, memsz) = (u32_at(b, p)?, u32_at(b, p + 4)?, u64_at(b, p + 8)?, u64_at(b, p + 16)?, u64_at(b, p + 32)?, u64_at(b, p + 40)?);
        match ty {
            PT_LOAD => {
                loads += 1;
                if off != 0 || vaddr != 0 || flags & PF_W != 0 || memsz != filesz || filesz as usize > b.len() {
                    return Err(format!("PT_LOAD off {off:#x} vaddr {vaddr:#x} flags {flags:#x} filesz {filesz:#x} memsz {memsz:#x}"));
                }
            }
            PT_DYNAMIC => dynamic = Some((off as usize, filesz as usize)),
            _ => {}
        }
    }
    if loads != 1 {
        return Err(format!("{loads} PT_LOAD headers (want exactly one)"));
    }
    let (doff, dsz) = dynamic.ok_or("no PT_DYNAMIC")?;
    let mut seen = Vec::new();
    for i in 0..dsz / 16 {
        let tag = u64_at(b, doff + i * 16)?;
        if tag == 0 {
            break;
        }
        seen.push(tag);
    }
    // DT_NEEDED, DT_RELA, DT_REL, DT_JMPREL, DT_TEXTREL, DT_RELR
    for bad in [1u64, 7, 17, 23, 22, 36] {
        if seen.contains(&bad) {
            return Err(format!("dynamic tag {bad} present (the vDSO must need nothing and have no relocations)"));
        }
    }
    // DT_HASH (musl needs it), DT_GNU_HASH, DT_SYMTAB, DT_STRTAB, DT_VERSYM, DT_VERDEF
    for need in [4u64, 0x6fff_fef5, 6, 5, 0x6fff_fff0, 0x6fff_fffc] {
        if !seen.contains(&need) {
            return Err(format!("dynamic tag {need:#x} missing"));
        }
    }
    Ok(())
}

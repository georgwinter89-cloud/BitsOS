//! AB-8: the dynamic programs of the ABI diff. `user/abi-tests/dyn/*.c` are built with the host's gcc against
//! CachyOS's glibc (`dyn-<stem>`, PIE, PT_INTERP `/lib64/ld-linux-x86-64.so.2`; `*-static.c` as static-PIE), CachyOS's own `ls` and `cat`
//! are copied unchanged (`dyn-ls`, `dyn-cat`), and the libraries `ldd` lists for them (the loader, libc.so.6,
//! libcap.so.2) go into the image's `/bin` under their sonames. On V2 they belong to `/lib64` and `/usr/lib`:
//! until the image puts them there (UL-5, desk change request in plan/notes/ab-AB-8.md) the harness writes
//! them there over MCP (`install_libs`) and the ktests link them (`proc::ktests_ab8`). Without gcc or ldd the
//! dynamic programs are left out with a warning (their ktests then skip).

use super::{Json, Mcp, jstr, run_status};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Copies by a rename: a reader in a parallel xtask sees the old file or the new one, whole.
fn copy_whole(from: &Path, to: &Path) -> Result<(), String> {
    let tmp = to.with_file_name(format!(".copy.{}", std::process::id()));
    std::fs::copy(from, &tmp).map_err(|e| format!("{}: {e}", from.display()))?;
    std::fs::rename(&tmp, to).map_err(|e| format!("{}: {e}", to.display()))
}

/// CachyOS programs copied as they are (their name in the image, the file).
const HOST_PROGRAMS: &[(&str, &str)] = &[("dyn-ls", "/usr/bin/ls"), ("dyn-cat", "/usr/bin/cat")];

/// A library of the set (a file name with `.so`), not a program to run.
pub fn is_lib(name: &str) -> bool {
    name.contains(".so")
}

/// Where a library of the set lives on Linux (and on V2 once installed).
pub fn lib_path(name: &str) -> String {
    if name.starts_with("ld-linux") { format!("/lib64/{name}") } else { format!("/usr/lib/{name}") }
}

/// The dynamic programs and their library set, built into `out`; empty (with a warning) when the host cannot.
pub fn build(root: &Path, out: &Path) -> Vec<(String, PathBuf)> {
    match try_build(root, out) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("xtask: warning: no dynamic ABI programs in the image: {e}");
            Vec::new()
        }
    }
}

fn try_build(root: &Path, out: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let dir = root.join("user/abi-tests/dyn");
    let mut srcs: Vec<PathBuf> = std::fs::read_dir(&dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "c")).collect()).unwrap_or_default();
    srcs.sort();
    let mut progs = Vec::new();
    for s in srcs {
        let name = format!("dyn-{}", s.file_stem().unwrap_or_default().to_string_lossy());
        let exe = out.join(&name);
        let tmp = out.join(format!(".{name}.{}", std::process::id()));
        let mut c = Command::new("gcc");
        // AB-14: `<stem>-static.c` is a static-PIE glibc program (glibc's own vDSO set-up without ld.so).
        let link: &[&str] = if name.ends_with("-static") { &["-static-pie"] } else { &["-fPIE", "-pie"] };
        c.args(["-O2", "-Wall", "-Werror", "-std=gnu11", "-pthread"]).args(link).arg(format!("-I{}", root.join("user/abi-tests/c").display())).arg("-o").arg(&tmp).arg(&s);
        // DR-2 CR (INT-W23): a first line `// abi-dyn-link: -ldrm -ldrm_amdgpu` adds libraries (only -l flags).
        c.args(link_flags(&std::fs::read_to_string(&s).unwrap_or_default())?);
        run_status(&mut c, &format!("building {name} (host gcc, glibc)"))?;
        // By a rename: another xtask of a parallel scenario may be reading the last build.
        std::fs::rename(&tmp, &exe).map_err(|e| format!("{}: {e}", exe.display()))?;
        progs.push((name, exe));
    }
    for (name, from) in HOST_PROGRAMS {
        let exe = out.join(name);
        copy_whole(Path::new(from), &exe)?;
        progs.push((name.to_string(), exe));
    }
    let mut libs: Vec<(String, PathBuf)> = Vec::new();
    for (_, exe) in &progs {
        for (soname, file) in ldd(exe)? {
            if !libs.iter().any(|(n, _)| *n == soname) {
                let dst = out.join(&soname);
                copy_whole(Path::new(&file), &dst)?;
                libs.push((soname, dst));
            }
        }
    }
    if !libs.iter().any(|(n, _)| n.starts_with("ld-linux")) {
        return Err("ldd named no ld-linux loader".into());
    }
    progs.extend(libs);
    Ok(progs)
}

/// The extra libraries a dyn program names on its first line (`// abi-dyn-link: -lfoo -lbar`); anything but a
/// `-l<name>` flag is refused, so a source file cannot change how the others are built.
fn link_flags(src: &str) -> Result<Vec<String>, String> {
    let Some(rest) = src.lines().next().and_then(|l| l.strip_prefix("// abi-dyn-link:")) else { return Ok(Vec::new()) };
    rest.split_whitespace()
        .map(|f| {
            let ok = f.len() > 2 && f.starts_with("-l") && f[2..].chars().all(|c| c.is_ascii_alphanumeric() || "_+-.".contains(c));
            if ok { Ok(f.to_string()) } else { Err(format!("abi-dyn-link: {f:?} is not a -l<library> flag")) }
        })
        .collect()
}

/// `ldd exe`: (soname, the real file) for every library and the loader; the vDSO has no file.
fn ldd(exe: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let o = Command::new("ldd").arg(exe).output().map_err(|e| format!("ldd: {e}"))?;
    if !o.status.success() {
        return Err(format!("ldd {}: {}", exe.display(), String::from_utf8_lossy(&o.stderr)));
    }
    let mut v = Vec::new();
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        let l = l.trim();
        // "libc.so.6 => /usr/lib/libc.so.6 (0x...)" or "/lib64/ld-linux-x86-64.so.2 (0x...)"
        let (name, file) = match l.split_once(" => ") {
            Some((n, rest)) => (n.trim(), rest.split(" (").next().unwrap_or("").trim()),
            None => (l.split(" (").next().unwrap_or("").trim(), l.split(" (").next().unwrap_or("").trim()),
        };
        if !file.starts_with('/') {
            continue;
        }
        let soname = Path::new(name).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let real = std::fs::canonicalize(file).map_err(|e| format!("{file}: {e}"))?;
        v.push((soname, real));
    }
    Ok(v)
}

/// FNV-1a 64 (what MCP's `stat` with `hash:true` shows).
fn fnv1a64(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x0100_0000_01b3))
}

fn b64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

/// Writes the library set to its Linux paths on the V2 behind `m` (unless the same file is there already),
/// in pieces that fit MCP's 256 KiB request limit.
pub fn install_libs(m: &mut Mcp, progs: &[(String, PathBuf)]) -> Result<(), String> {
    for (name, file) in progs.iter().filter(|(n, _)| is_lib(n)) {
        let data = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let dest = lib_path(name);
        let want = format!("{:#018x}", fnv1a64(&data));
        if let Ok((false, t)) = m.tool("stat", &format!(r#"{{"path":{},"hash":true}}"#, jstr(&dest)))
            && Json::parse(&t).ok().and_then(|j| j.get("fnv1a64").and_then(Json::as_str).map(|h| h == want)) == Some(true)
        {
            continue;
        }
        for (i, piece) in data.chunks(150 * 1024).enumerate() {
            let how = if i == 0 { r#""mode":"0755","parents":true"# } else { r#""append":true"# };
            let (err, t) = m.tool("write_file", &format!(r#"{{"path":{},"data":"{}",{how}}}"#, jstr(&dest), b64(piece)))?;
            if err {
                return Err(format!("write_file {dest}: {t}"));
            }
        }
        println!("abi-diff: {dest} written over MCP ({} bytes)", data.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_fnv() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn library_paths() {
        assert!(is_lib("libc.so.6") && is_lib("ld-linux-x86-64.so.2") && !is_lib("dyn-ls"));
        assert_eq!(lib_path("ld-linux-x86-64.so.2"), "/lib64/ld-linux-x86-64.so.2");
        assert_eq!(lib_path("libcap.so.2"), "/usr/lib/libcap.so.2");
    }

    #[test]
    fn dyn_link_line() {
        assert!(link_flags("#include <stdio.h>\n").unwrap().is_empty());
        assert_eq!(link_flags("// abi-dyn-link: -ldrm -ldrm_amdgpu\nint x;").unwrap(), ["-ldrm", "-ldrm_amdgpu"]);
        assert!(link_flags("// abi-dyn-link: -ldrm -Wl,-rpath,/x\n").is_err());
        assert!(link_flags("// abi-dyn-link: -l\n").is_err());
    }
}

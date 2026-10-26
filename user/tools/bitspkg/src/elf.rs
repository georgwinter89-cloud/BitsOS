//! PK-4: just enough ELF to tell a dynamic program (PT_INTERP) and a shared library (ET_DYN with PT_DYNAMIC, no
//! PT_INTERP) from other files, and `ldd`'s answer (from the loader itself) as (name, file) pairs. The same checks as xtask's
//! package.rs (is_dynamic, is_shared_lib, ldd_in); bitspkg runs on the build host, so it has its own copy.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const ET_DYN: u16 = 3;

pub enum Kind {
    Program,
    Library,
    Other,
}

/// The first 64 KiB: the ELF and program headers (never the whole of a 200 MB library).
fn head(f: &Path) -> Option<Vec<u8>> {
    let mut b = Vec::new();
    std::fs::File::open(f).ok()?.take(64 << 10).read_to_end(&mut b).ok()?;
    Some(b)
}

pub fn kind(f: &Path) -> Kind {
    let Some(e) = head(f) else { return Kind::Other };
    if e.len() < 64 || !e.starts_with(b"\x7fELF") || e[4] != 2 || e[5] != 1 {
        return Kind::Other;
    }
    let u16_at = |o: usize| u16::from_le_bytes([e[o], e[o + 1]]);
    let phoff = u64::from_le_bytes(e[0x20..0x28].try_into().unwrap_or([0; 8])) as usize;
    let (ent, num) = (u16_at(0x36) as usize, u16_at(0x38) as usize);
    let has = |t: u32| {
        (0..num).any(|i| {
            let o = phoff.saturating_add(i.saturating_mul(ent));
            e.get(o..o.saturating_add(4)).is_some_and(|b| b == t.to_le_bytes())
        })
    };
    if has(PT_INTERP) {
        Kind::Program
    } else if u16_at(0x10) == ET_DYN && has(PT_DYNAMIC) {
        Kind::Library
    } else {
        Kind::Other
    }
}

/// One line of `ldd`: a library (`libx.so.1 => /usr/lib/libx.so.1 (0x..)`), the loader
/// (`/lib64/ld-linux-x86-64.so.2 => /usr/lib64/ld-linux-x86-64.so.2 (0x..)`), or a missing one (`file` None).
pub struct Needed {
    pub name: String,
    pub file: Option<PathBuf>,
    pub loader: bool,
}

/// The build host's loader. `ld.so --list` is what glibc's ldd script runs, without the script's fallback that
/// executes a file no loader accepts with LD_TRACE_LOADED_OBJECTS set: in trace mode the loader maps the
/// libraries, prints them and exits before any relocation or constructor, so no code of the foreign tree runs.
const HOST_LOADERS: [&str; 3] = ["/usr/lib/ld-linux-x86-64.so.2", "/usr/lib64/ld-linux-x86-64.so.2", "/lib64/ld-linux-x86-64.so.2"];

/// `ld.so --list file` with LD_LIBRARY_PATH = `lib_path` (the package's own library folder, found first as the
/// launcher's LD_LIBRARY_PATH finds it on V2) and nothing else of this environment. The vDSO has no file and is
/// left out.
pub fn ldd(file: &Path, lib_path: &Path) -> Result<Vec<Needed>, String> {
    let ld = HOST_LOADERS.iter().find(|p| Path::new(p).is_file()).ok_or("no ld-linux-x86-64.so.2 on this host (import runs on the build host, CachyOS)")?;
    let o = Command::new(ld)
        .arg("--list")
        .arg(file)
        .env_clear()
        .env("LC_ALL", "C")
        .env("LD_LIBRARY_PATH", lib_path)
        .output()
        .map_err(|e| format!("{ld} --list: {e}"))?;
    if !o.status.success() {
        return Err(format!("ldd {}: {}", file.display(), String::from_utf8_lossy(&o.stderr).trim()));
    }
    let mut v = Vec::new();
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        let l = l.trim();
        let (name, rest) = match l.split_once(" => ") {
            Some((n, r)) => (n.trim(), r.trim()),
            None => (l.split(" (").next().unwrap_or("").trim(), l),
        };
        if name.is_empty() || name.starts_with("linux-vdso") || name.starts_with("statically") {
            continue;
        }
        let file = if rest.starts_with("not found") {
            None
        } else {
            let f = rest.split(" (").next().unwrap_or("").trim();
            (f.starts_with('/')).then(|| PathBuf::from(f))
        };
        // The loader line names PT_INTERP; any other absolute name is a DT_NEEDED path, which import refuses.
        let loader = name.starts_with('/') && name.rsplit('/').next() == Some("ld-linux-x86-64.so.2");
        v.push(Needed { name: name.to_string(), file, loader });
    }
    Ok(v)
}

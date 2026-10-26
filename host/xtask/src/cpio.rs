//! cpio "newc" (070701) writer and a small reader for the round-trip test.
//! Reproducible on purpose: mtime 0, uid/gid 0, inode numbers by order.

pub struct Entry {
    /// Path without a leading slash, e.g. `bin/hello`.
    pub name: String,
    pub mode: u32,
    pub data: Vec<u8>,
}

pub const S_DIR: u32 = 0o040000;
pub const S_REG: u32 = 0o100000;
pub const S_LNK: u32 = 0o120000;

pub fn dir(name: &str) -> Entry {
    Entry { name: name.to_string(), mode: S_DIR | 0o755, data: Vec::new() }
}

pub fn file(name: &str, perm: u32, data: Vec<u8>) -> Entry {
    Entry { name: name.to_string(), mode: S_REG | perm, data }
}

/// A symbolic link; newc stores the target as the file data (the kernel's reader: crates/vfs/src/cpio.rs).
pub fn symlink(name: &str, target: &str) -> Entry {
    Entry { name: name.to_string(), mode: S_LNK | 0o777, data: target.as_bytes().to_vec() }
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

fn header(v: &mut Vec<u8>, ino: u32, mode: u32, nlink: u32, size: u32, name: &str) {
    let f = |x: u32| format!("{x:08X}");
    v.extend_from_slice(b"070701");
    for field in [ino, mode, 0, 0, nlink, 0, size, 0, 0, 0, 0, name.len() as u32 + 1, 0] {
        v.extend_from_slice(f(field).as_bytes());
    }
    v.extend_from_slice(name.as_bytes());
    v.push(0);
    pad4(v);
}

/// Parent directories of every entry are added automatically (sorted, once each).
pub fn write(entries: &[Entry]) -> Vec<u8> {
    let mut dirs = std::collections::BTreeSet::new();
    for e in entries {
        let mut p = e.name.as_str();
        while let Some((par, _)) = p.rsplit_once('/') {
            dirs.insert(par.to_string());
            p = par;
        }
    }
    for e in entries {
        if e.mode & S_DIR != 0 {
            dirs.remove(&e.name);
        }
    }
    let mut out = Vec::new();
    let mut ino = 1;
    let mut all: Vec<Entry> = dirs.into_iter().map(|d| dir(&d)).collect();
    all.extend(entries.iter().map(|e| Entry { name: e.name.clone(), mode: e.mode, data: e.data.clone() }));
    all.sort_by(|a, b| a.name.cmp(&b.name));
    for e in &all {
        let is_dir = e.mode & S_DIR != 0;
        header(&mut out, ino, e.mode, if is_dir { 2 } else { 1 }, e.data.len() as u32, &e.name);
        out.extend_from_slice(&e.data);
        pad4(&mut out);
        ino += 1;
    }
    header(&mut out, 0, 0, 1, 0, "TRAILER!!!");
    out
}

/// Parses an archive written by `write` (or any newc archive without hard links).
#[cfg(test)]
pub fn read(buf: &[u8]) -> Result<Vec<Entry>, String> {
    let mut pos = 0;
    let mut out = Vec::new();
    let num = |b: &[u8], i: usize| -> Result<u32, String> {
        let s = std::str::from_utf8(&b[6 + i * 8..14 + i * 8]).map_err(|e| e.to_string())?;
        u32::from_str_radix(s, 16).map_err(|e| e.to_string())
    };
    loop {
        if buf.len() < pos + 110 || &buf[pos..pos + 6] != b"070701" {
            return Err(format!("bad header at {pos}"));
        }
        let h = &buf[pos..pos + 110];
        let mode = num(h, 1)?;
        let size = num(h, 6)? as usize;
        let nsize = num(h, 11)? as usize;
        let name_end = pos + 110 + nsize;
        let name = String::from_utf8_lossy(&buf[pos + 110..name_end - 1]).to_string();
        let data_start = (name_end + 3) & !3;
        let data_end = data_start + size;
        if name == "TRAILER!!!" {
            return Ok(out);
        }
        if data_end > buf.len() {
            return Err("truncated".into());
        }
        out.push(Entry { name, mode, data: buf[data_start..data_end].to_vec() });
        pos = (data_end + 3) & !3;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Entry> {
        vec![
            file("bin/hello", 0o755, b"ELF-ish".to_vec()),
            file("bin/odd", 0o755, vec![1, 2, 3, 4, 5]),
            file("etc/motd", 0o644, b"hi\n".to_vec()),
            file("empty", 0o644, vec![]),
            symlink("bin/sh", "odd"),
        ]
    }

    #[test]
    fn round_trip_with_own_reader() {
        let a = write(&sample());
        assert_eq!(a.len() % 4, 0);
        let back = read(&a).unwrap();
        let names: Vec<_> = back.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["bin", "bin/hello", "bin/odd", "bin/sh", "empty", "etc", "etc/motd"]);
        let sh = back.iter().find(|e| e.name == "bin/sh").unwrap();
        assert_eq!((sh.mode, sh.data.as_slice()), (S_LNK | 0o777, &b"odd"[..]));
        let odd = back.iter().find(|e| e.name == "bin/odd").unwrap();
        assert_eq!(odd.data, vec![1, 2, 3, 4, 5]);
        assert_eq!(odd.mode, S_REG | 0o755);
        assert!(back.iter().find(|e| e.name == "bin").unwrap().mode & S_DIR != 0);
    }

    #[test]
    fn round_trip_through_system_cpio() {
        if !crate::util::have_tool("cpio") {
            eprintln!("cpio not installed, skipped");
            return;
        }
        let base = std::env::temp_dir().join(format!("xt-cpio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.cpio"), write(&sample())).unwrap();
        let st = std::process::Command::new("cpio")
            .args(["-id", "--quiet", "-F", "a.cpio"])
            .current_dir(&base)
            .status()
            .unwrap();
        assert!(st.success());
        assert_eq!(std::fs::read(base.join("bin/odd")).unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(std::fs::read(base.join("etc/motd")).unwrap(), b"hi\n");
        assert_eq!(std::fs::read_link(base.join("bin/sh")).unwrap(), std::path::PathBuf::from("odd"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn trailer_and_alignment() {
        let a = write(&[]);
        assert!(a.starts_with(b"070701"));
        assert!(String::from_utf8_lossy(&a).contains("TRAILER!!!"));
        assert!(read(&a).unwrap().is_empty());
    }
}

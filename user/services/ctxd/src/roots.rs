//! Roots: the only places ctxd reads (DESIGN §3). Refusal of the Windows disk (R8a) and the walk.
//! The walk never follows symlinks and never crosses a mount point, so a link or a mount inside a root
//! cannot lead it out.

use crate::config::RootCfg;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// File systems Windows uses (FAT for the EFI partition and Windows USB disks); a root on one of them (or
/// containing one) is refused.
const WINDOWS_FS: &[&str] =
    &["ntfs", "ntfs3", "ntfs-3g", "fuseblk", "fuse.ntfs", "fuse.ntfs-3g", "exfat", "vfat", "msdos", "fat"];
/// Places where removable and foreign disks, and the EFI partition, get mounted on CachyOS.
const FORBIDDEN_PREFIXES: &[&str] = &["/run/media", "/media", "/boot", "/efi"];
/// Partition types of Windows and EFI partitions (GPT type GUIDs and MBR ids as udev reports them).
const WINDOWS_PARTS: &[&str] = &[
    "c12a7328-f81f-11d2-ba4b-00a0c93ec93b", // EFI System
    "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7", // Microsoft basic data
    "e3c9e316-0b5c-4db8-817d-f92df00215ae", // Microsoft reserved
    "de94bba4-06d1-4d40-a16a-bfd50179d6ac", // Windows recovery
    "5808c8aa-7e8f-42e0-85d2-e1e90434cfb3", // LDM metadata
    "af9b60a0-1431-4f62-bc68-3311714a69ad", // LDM data
    "0x7",
    "0xb",
    "0xc",
    "0xe",
    "0x27",
    "0xef",
];

#[derive(Debug, Clone)]
pub struct Root {
    pub path: PathBuf,
    pub private: bool,
    pub dev: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Denied {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Mount {
    pub point: PathBuf,
    pub fstype: String,
    /// Partition type of the block device behind the mount (None: not a partition, or unknown).
    pub part_type: Option<String>,
}

impl Mount {
    fn windows(&self) -> Option<String> {
        if WINDOWS_FS.contains(&self.fstype.as_str()) {
            return Some(format!("fs {}", self.fstype));
        }
        let t = self.part_type.as_deref()?.to_ascii_lowercase();
        WINDOWS_PARTS.contains(&t.as_str()).then(|| format!("partition {t}"))
    }
}

/// `/proc/self/mountinfo`: field 5 is the mount point (octal escapes), after " - " come the fs type and the
/// source.
pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines().filter_map(|l| parse_mount_line(l).map(|(m, _)| m)).collect()
}

fn parse_mount_line(line: &str) -> Option<(Mount, Option<String>)> {
    let (left, right) = line.split_once(" - ")?;
    let point = left.split(' ').nth(4)?;
    let mut r = right.split(' ');
    let fstype = r.next()?;
    let source = r.next().map(unescape_octal);
    Some((Mount { point: PathBuf::from(unescape_octal(point)), fstype: fstype.to_string(), part_type: None }, source))
}

fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decide whether a configured root may be read. `mounts` comes from mountinfo (tests pass their own).
pub fn check_root(path: &Path, forbidden: &[PathBuf], mounts: &[Mount]) -> Result<PathBuf, String> {
    let canon = fs::canonicalize(path).map_err(|e| format!("not_found: {e}"))?;
    if !canon.is_dir() {
        return Err("not_a_directory".into());
    }
    for p in FORBIDDEN_PREFIXES {
        if canon.starts_with(p) {
            return Err(format!("forbidden_prefix {p}"));
        }
    }
    if canon.starts_with("/mnt")
        && canon.components().nth(2).is_some_and(|c| c.as_os_str().to_string_lossy().starts_with("windows"))
    {
        return Err("forbidden_prefix /mnt/windows*".into());
    }
    for f in forbidden {
        let f = fs::canonicalize(f).unwrap_or_else(|_| f.clone());
        if canon.starts_with(&f) {
            return Err(format!("forbidden_root {}", f.display()));
        }
    }
    // The mount that holds the root: the longest mount point that is a prefix.
    let holder = mounts.iter().filter(|m| canon.starts_with(&m.point)).max_by_key(|m| m.point.as_os_str().len());
    if let Some(why) = holder.and_then(Mount::windows) {
        return Err(format!("windows_fs {why}"));
    }
    if let Some(m) = mounts.iter().find(|m| m.point.starts_with(&canon) && m.windows().is_some()) {
        return Err(format!("contains_windows_mount {}", m.point.display()));
    }
    Ok(canon)
}

pub fn system_mounts() -> Vec<Mount> {
    let text = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    text.lines()
        .filter_map(parse_mount_line)
        .map(|(mut m, src)| {
            m.part_type = src.as_deref().and_then(part_type);
            m
        })
        .collect()
}

/// The partition type udev recorded for a `/dev/...` source, found through sysfs (major:minor). Nothing on
/// the device itself is opened.
fn part_type(source: &str) -> Option<String> {
    let name = source.strip_prefix("/dev/")?;
    let name = Path::new(name).file_name()?.to_str()?;
    let devno = fs::read_to_string(Path::new("/sys/class/block").join(name).join("dev")).ok()?;
    let data = fs::read_to_string(format!("/run/udev/data/b{}", devno.trim())).ok()?;
    data.lines().find_map(|l| l.strip_prefix("E:ID_PART_ENTRY_TYPE=")).map(str::to_string)
}

/// Resolve configured roots; refused ones are reported, never read.
pub fn resolve(cfg: &[RootCfg], forbidden: &[PathBuf]) -> (Vec<Root>, Vec<Denied>) {
    let mounts = system_mounts();
    let mut ok = Vec::new();
    let mut denied = Vec::new();
    for r in cfg {
        match check_root(&r.path, forbidden, &mounts).and_then(|p| {
            let dev = fs::metadata(&p).map_err(|e| e.to_string())?.dev();
            Ok((p, dev))
        }) {
            Ok((path, dev)) => {
                // A root inside another root would index files twice under two privacy settings: the
                // inner root wins for its files (the walk of the outer one skips it).
                ok.push(Root { path, private: r.private, dev });
            }
            Err(reason) => {
                crate::event!("warn", "policy.denied", access = "root", reason = reason.clone());
                denied.push(Denied { path: r.path.display().to_string(), reason });
            }
        }
    }
    (ok, denied)
}

/// `*` and `?` wildcards over a single path component.
pub fn glob_match(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn excluded(name: &str, excludes: &[String]) -> bool {
    excludes.iter().any(|p| glob_match(p, name))
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub root: usize,
    pub size: u64,
    pub mtime: i64,
    /// Full stamps in nanoseconds: an edit within the same second (same size, same inode) still differs here.
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub dev: u64,
    pub ino: u64,
}

/// Every directory the walk of `scan` would enter from `dir` (same device, no symlinks, not excluded), minus
/// `skip` and what lies below it.
pub fn walk_dirs(dir: &Path, dev: u64, excludes: &[String], skip: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if d.starts_with(skip) {
            continue;
        }
        let Ok(rd) = fs::read_dir(&d) else { continue };
        out.push(d);
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if excluded(name, excludes) {
                continue;
            }
            let path = e.path();
            if let Ok(md) = fs::symlink_metadata(&path)
                && md.file_type().is_dir()
                && md.dev() == dev
            {
                stack.push(path);
            }
        }
    }
    out
}

pub fn stamp_ns(secs: i64, nsec: i64) -> i64 {
    secs.saturating_mul(1_000_000_000).saturating_add(nsec)
}

#[derive(Debug, Default)]
pub struct Scan {
    pub files: Vec<FileInfo>,
    pub excluded: u64,
    pub too_large: u64,
    pub symlinks: u64,
    pub other_fs: u64,
    pub bad_names: u64,
    pub errors: u64,
}

/// Walk every root (explicit stack, no recursion). Files above `max_bytes` are counted, not listed.
pub fn scan(roots: &[Root], excludes: &[String], max_bytes: u64) -> Scan {
    let mut s = Scan::default();
    for (ri, root) in roots.iter().enumerate() {
        let mut stack = vec![root.path.clone()];
        while let Some(dir) = stack.pop() {
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => {
                    s.errors += 1;
                    continue;
                }
            };
            for e in rd {
                let Ok(e) = e else {
                    s.errors += 1;
                    continue;
                };
                let path = e.path();
                let name = e.file_name();
                let Some(name) = name.to_str() else {
                    // Non-UTF-8 names cannot be shown or passed over JSON faithfully.
                    s.bad_names += 1;
                    continue;
                };
                if excluded(name, excludes) {
                    s.excluded += 1;
                    continue;
                }
                let Ok(md) = fs::symlink_metadata(&path) else {
                    s.errors += 1;
                    continue;
                };
                let ft = md.file_type();
                if ft.is_symlink() {
                    s.symlinks += 1;
                } else if md.dev() != root.dev {
                    s.other_fs += 1;
                } else if ft.is_dir() {
                    // A nested root is walked as itself (its own privacy flag).
                    if !roots.iter().enumerate().any(|(j, r)| j != ri && r.path == path) {
                        stack.push(path);
                    }
                } else if ft.is_file() {
                    if md.len() > max_bytes {
                        s.too_large += 1;
                        crate::event!("info", "doc.excluded", reason = "too_large", size = md.len());
                        continue;
                    }
                    s.files.push(FileInfo {
                        path,
                        root: ri,
                        size: md.len(),
                        mtime: md.mtime(),
                        mtime_ns: stamp_ns(md.mtime(), md.mtime_nsec()),
                        ctime_ns: stamp_ns(md.ctime(), md.ctime_nsec()),
                        dev: md.dev(),
                        ino: md.ino(),
                    });
                }
            }
        }
    }
    s.files.sort_by(|a, b| a.path.cmp(&b.path));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_parses_escapes_and_types() {
        let t = "36 35 98:0 / /mnt/my\\040disk rw,noatime master:1 - ntfs3 /dev/nvme0n1p3 rw\n\
                 37 35 98:1 / / rw - ext4 /dev/root rw\n";
        let m = parse_mountinfo(t);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].point, PathBuf::from("/mnt/my disk"));
        assert_eq!(m[0].fstype, "ntfs3");
    }

    #[test]
    fn windows_mounts_and_prefixes_are_refused() {
        let tmp = std::env::temp_dir().join(format!("ctxd-roots-{}", std::process::id()));
        let inner = tmp.join("inner");
        fs::create_dir_all(&inner).unwrap();
        let canon = fs::canonicalize(&tmp).unwrap();
        let mnt = |p: &Path, fs: &str, part: Option<&str>| Mount {
            point: p.to_path_buf(),
            fstype: fs.into(),
            part_type: part.map(str::to_string),
        };
        let linux = Some("0fc63daf-8483-4772-8e79-3d69d8477de4");
        let none: Vec<Mount> = vec![mnt(Path::new("/"), "ext4", linux)];
        assert!(check_root(&tmp, &[], &none).is_ok());
        // The root lies on an ntfs3 mount, or on a bind mount of an ntfs directory (mountinfo keeps the type).
        for fs in ["ntfs3", "fuse.ntfs-3g", "vfat", "msdos", "exfat"] {
            let on = vec![none[0].clone(), mnt(&canon, fs, None)];
            assert!(check_root(&tmp, &[], &on).unwrap_err().starts_with("windows_fs"), "{fs}");
        }
        // A Linux file system on a partition typed EFI or Microsoft basic data.
        for part in ["c12a7328-f81f-11d2-ba4b-00a0c93ec93b", "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7", "0x7"] {
            let on = vec![none[0].clone(), mnt(&canon, "ext4", Some(part))];
            assert!(check_root(&tmp, &[], &on).unwrap_err().starts_with("windows_fs partition"), "{part}");
        }
        // A Windows mount below the root.
        let below = vec![none[0].clone(), mnt(&canon.join("inner"), "fuseblk", None)];
        assert!(check_root(&tmp, &[], &below).unwrap_err().starts_with("contains_windows_mount"));
        let below = vec![none[0].clone(), mnt(&canon.join("inner"), "vfat", None)];
        assert!(check_root(&tmp, &[], &below).unwrap_err().starts_with("contains_windows_mount"));
        // A forbidden root named in the settings, reached through a symlink.
        let link = std::env::temp_dir().join(format!("ctxd-roots-link-{}", std::process::id()));
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&inner, &link).unwrap();
        assert!(check_root(&link, std::slice::from_ref(&inner), &none).unwrap_err().starts_with("forbidden_root"));
        assert!(check_root(Path::new("/run/media"), &[], &none).is_err());
        if Path::new("/boot").is_dir() {
            assert!(check_root(Path::new("/boot"), &[], &none).unwrap_err().starts_with("forbidden_prefix"));
        }
        let _ = fs::remove_file(&link);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.kdbx", "pass.kdbx"));
        assert!(glob_match(".ssh", ".ssh"));
        assert!(!glob_match("*.key", "keynote.txt"));
        assert!(glob_match(".Trash-*", ".Trash-1000"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
    }
}

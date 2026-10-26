//! Host-test helpers: images from mkfs.ext4 (optionally filled from a directory with `-d`), e2fsck checks,
//! a volume on a memory device with a test clock.

use crate::dev::MemDev;
use crate::vol::{Options, Volume};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::{String, ToString};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::vec::Vec;
use std::{format, vec};
use vfs::abi::Time;

static N: AtomicU32 = AtomicU32::new(0);
static CLOCK: AtomicU64 = AtomicU64::new(0);

pub fn now() -> Time {
    let n = CLOCK.fetch_add(1, Ordering::Relaxed);
    Time { sec: 1_700_000_100 + (n / 1000) as i64, nsec: ((n % 1000) * 1_000_000) as u32 }
}

pub fn opts() -> Options {
    Options { writable: true, now }
}

/// A scratch directory below the target directory, removed with the value.
pub struct Scratch(pub PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Scratch {
    pub fn new() -> Scratch {
        let base = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| {
            let exe = std::env::current_exe().unwrap();
            exe.ancestors().find(|a| a.file_name().is_some_and(|n| n == "target")).map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir)
        });
        let d = base.join("fs-ext4-tests").join(format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Scratch(d)
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

/// mkfs.ext4 of `size` with extra options (e.g. `-b 1024`, `-O ^has_journal`), filled from `from`.
pub fn mkfs(s: &Scratch, size: &str, extra: &[&str], from: Option<&Path>) -> Vec<u8> {
    let img = s.path("mk.img");
    let _ = std::fs::remove_file(&img);
    let t = Command::new("truncate").args(["-s", size]).arg(&img).status().unwrap();
    assert!(t.success());
    let mut c = Command::new("mkfs.ext4");
    c.env("E2FSPROGS_FAKE_TIME", "1700000000").args(["-q", "-F", "-E", "lazy_itable_init=1,nodiscard"]).args(extra);
    if let Some(d) = from {
        c.arg("-d").arg(d);
    }
    let o = c.arg(&img).output().unwrap();
    assert!(o.status.success(), "mkfs.ext4 failed: {}", String::from_utf8_lossy(&o.stderr));
    std::fs::read(&img).unwrap()
}

/// `e2fsck -fn` of an image: (exit code, the problem lines).
pub fn fsck(s: &Scratch, img: &[u8]) -> (i32, String) {
    let p = s.path("check.img");
    std::fs::write(&p, img).unwrap();
    let o = Command::new("e2fsck").env("LC_ALL", "C").args(["-fn"]).arg(&p).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    let lines: Vec<&str> = out.lines().filter(|l| !l.starts_with("Pass ") && !l.is_empty() && !l.starts_with("e2fsck ")).collect();
    (o.status.code().unwrap_or(-1), lines.join("\n"))
}

/// Asserts e2fsck finds nothing (exit 0).
pub fn assert_clean(s: &Scratch, img: &[u8], what: &str) {
    let (c, out) = fsck(s, img);
    assert_eq!(c, 0, "e2fsck after {what}:\n{out}");
}

/// e2fsck -fy repairs a copy; returns (exit code of the repair, exit code of a following -fn).
pub fn repair(s: &Scratch, img: &[u8]) -> (i32, i32, String) {
    let p = s.path("repair.img");
    std::fs::write(&p, img).unwrap();
    let o = Command::new("e2fsck").env("LC_ALL", "C").args(["-fy"]).arg(&p).output().unwrap();
    let first = o.status.code().unwrap_or(-1);
    let o2 = Command::new("e2fsck").env("LC_ALL", "C").args(["-fn"]).arg(&p).output().unwrap();
    (first, o2.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).lines().take(20).collect::<Vec<_>>().join("\n"))
}

pub fn mount(img: Vec<u8>) -> (Arc<MemDev>, Arc<Volume>) {
    let dev = Arc::new(MemDev::new(img));
    let v = Volume::mount(dev.clone(), opts()).expect("mount");
    (dev, v)
}

/// debugfs -R of an image (read-only queries).
pub fn debugfs(s: &Scratch, img: &[u8], req: &str) -> String {
    let p = s.path("dbg.img");
    std::fs::write(&p, img).unwrap();
    let o = Command::new("debugfs").env("LC_ALL", "C").args(["-R", req]).arg(&p).output().unwrap();
    String::from_utf8_lossy(&o.stdout).to_string()
}

/// A directory tree on the host for `mkfs.ext4 -d`.
pub fn tree(s: &Scratch) -> PathBuf {
    let d = s.path("tree");
    std::fs::create_dir_all(d.join("sub/deeper")).unwrap();
    std::fs::write(d.join("hello.txt"), b"hello ext4\n").unwrap();
    std::fs::write(d.join("sub/deeper/data.bin"), (0..300_000u32).map(|i| (i * 31 % 251) as u8).collect::<Vec<u8>>()).unwrap();
    std::os::unix::fs::symlink("hello.txt", d.join("fast-link")).unwrap();
    std::os::unix::fs::symlink("x/".repeat(60), d.join("slow-link")).unwrap();
    std::fs::hard_link(d.join("hello.txt"), d.join("sub/hard")).unwrap();
    std::fs::create_dir_all(d.join("many")).unwrap();
    for i in 0..600 {
        std::fs::write(d.join(format!("many/entry-with-a-long-name-{i:04}")), format!("{i}")).unwrap();
    }
    let f = std::fs::File::create(d.join("sparse")).unwrap();
    f.set_len(50 << 20).unwrap();
    use std::os::unix::fs::FileExt;
    f.write_at(b"end", (50 << 20) - 3).unwrap();
    let _ = vec![0u8; 0];
    d
}

/// Every name below `dir` (recursively), as "path" strings, from the volume.
pub fn walk(v: &Volume, dir: u32, prefix: &str, out: &mut Vec<String>) {
    let mut l = v.list(dir).unwrap();
    l.sort();
    for (name, ino, ft) in l {
        let p = format!("{prefix}/{}", String::from_utf8_lossy(&name));
        out.push(p.clone());
        if ft == crate::dir::FT_DIR {
            walk(v, ino, &p, out);
        }
    }
}

/// The inode of `path` (from the root), by listing.
pub fn lookup(v: &Volume, path: &str) -> u32 {
    let mut cur = crate::inode::ROOT;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        cur = v.list(cur).unwrap().into_iter().find(|(n, _, _)| n == part.as_bytes()).unwrap_or_else(|| panic!("{path}: no {part}")).1;
    }
    cur
}

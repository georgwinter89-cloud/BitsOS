//! Host-test helpers: FAT32 images made by mkfs.fat and filled by mtools, checked by fsck.fat (dosfstools,
//! installed on CachyOS), a volume mounted into a fresh VFS view.

use crate::dev::MemDev;
use crate::vol::{Clock, Options, Volume};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::string::{String, ToString};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::vec::Vec;
use std::{format, vec};

static N: AtomicU32 = AtomicU32::new(0);

/// Tests run in parallel in one process, and the process-wide write-back pass of
/// `background_write_back_makes_the_stick_durable` holds every registered file of every test for a moment: a
/// file unlinked meanwhile keeps its clusters until a later change (correct in the kernel, the next pass frees
/// them; but a test's exact cluster count or fsck would see them). Every test holds this shared from its
/// `scratch` on (per thread, until the test's thread ends); that test takes it exclusively.
static PASS: std::sync::RwLock<()> = std::sync::RwLock::new(());

std::thread_local! {
    static SHARED: std::cell::RefCell<Option<std::sync::RwLockReadGuard<'static, ()>>> = const { std::cell::RefCell::new(None) };
}

/// The process-wide write-back pass may run (no other test is between `scratch` and its end).
pub fn exclusive() -> std::sync::RwLockWriteGuard<'static, ()> {
    SHARED.with(|s| s.borrow_mut().take());
    PASS.write().unwrap_or_else(|e| e.into_inner())
}

/// The scratch directories of the calling test's thread; removed when the thread (the test) ends, so a gate
/// run does not leave its images behind (FS-4: unremoved images had filled /tmp, a tmpfs, with 23 GB).
struct ScratchDirs(Vec<PathBuf>);

impl Drop for ScratchDirs {
    fn drop(&mut self) {
        for d in &self.0 {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

std::thread_local! {
    static DIRS: std::cell::RefCell<ScratchDirs> = const { std::cell::RefCell::new(ScratchDirs(Vec::new())) };
}

/// Where scratch directories go: `$CARGO_TARGET_DIR`, else the target directory the test binary runs from (on
/// disk, not the RAM-backed /tmp), else the temp directory. Directories of test processes that are gone (a
/// killed run) are removed the first time.
fn scratch_base() -> PathBuf {
    static BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).or_else(|| {
            let exe = std::env::current_exe().ok()?;
            exe.ancestors().find(|a| a.file_name().is_some_and(|n| n == "target")).map(Path::to_path_buf)
        });
        let base = target.unwrap_or_else(std::env::temp_dir).join("fs-fat-tests");
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let pid = name.rsplit('-').nth(1).and_then(|p| p.parse::<u32>().ok());
                if pid.is_some_and(|p| p != std::process::id() && !Path::new(&format!("/proc/{p}")).exists()) {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        base
    })
    .clone()
}

/// A fresh scratch directory for one test (removed when the test's thread ends).
pub fn scratch(tag: &str) -> PathBuf {
    SHARED.with(|s| {
        s.borrow_mut().take();
        *s.borrow_mut() = Some(PASS.read().unwrap_or_else(|e| e.into_inner()));
    });
    let d = scratch_base().join(format!("{}-{}-{}", tag, std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    DIRS.with(|l| l.borrow_mut().0.push(d.clone()));
    d
}

fn run(cmd: &mut Command) -> (i32, String) {
    let o = cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    (o.status.code().unwrap_or(-1), s)
}

/// `mkfs.fat -F 32` image of `mib` MiB with `spc` sectors per cluster and label `label`.
pub fn mkfs(dir: &Path, mib: u32, spc: u32, label: &str) -> PathBuf {
    let img = dir.join("vol.img");
    let (c, out) = run(Command::new("mkfs.fat").args(["-F", "32", "-s", &spc.to_string(), "-n", label, "-C"]).arg(&img).arg((mib * 1024).to_string()));
    assert_eq!(c, 0, "mkfs.fat: {out}");
    img
}

/// Copies host file `src` into the image at `to` (mtools; directories made with `mmd` first).
pub fn put(img: &Path, src: &Path, to: &str) {
    let (c, out) = run(Command::new("mcopy").arg("-o").arg("-i").arg(img).arg(src).arg(format!("::{to}")));
    assert_eq!(c, 0, "mcopy {to}: {out}");
}

pub fn put_bytes(img: &Path, dir: &Path, to: &str, data: &[u8]) {
    let tmp = dir.join("put.tmp");
    std::fs::write(&tmp, data).unwrap();
    put(img, &tmp, to);
}

pub fn mkdir(img: &Path, path: &str) {
    let (c, out) = run(Command::new("mmd").arg("-i").arg(img).arg(format!("::{path}")));
    assert_eq!(c, 0, "mmd {path}: {out}");
}

/// A file of the image as mtools reads it (None: missing).
pub fn get(img: &Path, path: &str) -> Option<Vec<u8>> {
    let o = Command::new("mtype").arg("-i").arg(img).arg(format!("::{path}")).output().unwrap();
    o.status.success().then_some(o.stdout)
}

/// The names in a directory of the image as mtools lists them (long names).
pub fn ls(img: &Path, path: &str) -> Vec<String> {
    let o = Command::new("mdir").args(["-b", "-i"]).arg(img).arg(format!("::{path}")).output().unwrap();
    String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()).filter(|s| !s.is_empty()).collect()
}

/// `fsck.fat -n` of image bytes: (exit code, output).
pub fn fsck(dir: &Path, bytes: &[u8]) -> (i32, String) {
    let p = dir.join("fsck.img");
    std::fs::write(&p, bytes).unwrap();
    run(Command::new("fsck.fat").arg("-n").arg(&p))
}

/// `fsck.fat -a` (repair) of image bytes: the repaired image.
pub fn repair(dir: &Path, bytes: &[u8]) -> Vec<u8> {
    let p = dir.join("repair.img");
    std::fs::write(&p, bytes).unwrap();
    let (c, out) = run(Command::new("fsck.fat").args(["-a", "-w"]).arg(&p));
    assert!(c == 0 || c == 1, "fsck.fat -a: {out}");
    std::fs::read(&p).unwrap()
}

pub fn assert_clean(dir: &Path, bytes: &[u8]) {
    let (c, out) = fsck(dir, bytes);
    assert_eq!(c, 0, "fsck.fat -n is not clean:\n{out}");
    assert!(!out.contains("Free cluster summary"), "FSInfo left unknown or wrong:\n{out}");
}

pub fn load(img: &Path) -> Arc<MemDev> {
    Arc::new(MemDev::new(std::fs::read(img).unwrap(), 512))
}

/// Mounts a plain test volume (no stick rules) read-write.
pub fn mount_plain(dev: &Arc<MemDev>) -> Arc<Volume> {
    Volume::mount(dev.clone(), Options { writable: true, stick: false }, Clock::UTC_EPOCH).unwrap()
}

/// Mounts with the stick rules, read-write.
pub fn mount_stick(dev: &Arc<MemDev>) -> Arc<Volume> {
    Volume::mount(dev.clone(), Options { writable: true, stick: true }, Clock::UTC_EPOCH).unwrap()
}

/// A fresh view with `vol` at /boot.
pub fn view(vol: &Arc<Volume>) -> vfs::MountNs {
    let ns = vfs::MountNs::new_empty();
    crate::glue::mount(vol, &ns, "/boot", false).unwrap();
    ns
}

/// Pseudo-random bytes (deterministic).
pub fn pattern(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    let mut v = vec![0u8; n];
    for b in v.iter_mut() {
        s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
        *b = (s >> 16) as u8;
    }
    v
}

/// Writes the image back to a file (for mtools after our changes).
pub fn save(dir: &Path, dev: &MemDev) -> PathBuf {
    let p = dir.join("after.img");
    std::fs::write(&p, dev.image()).unwrap();
    p
}

//! FS-13 ktests (mount flags enforced):
//! - `fs.mountflags.kernel`: a nosuid,nodev,noexec tmpfs made in the kernel: device nodes do not open (O_PATH
//!   does), access(X_OK) and mmap PROT_EXEC of its files are refused, VM_MAYEXEC goes; a plain tmpfs answers
//!   as before;
//! - `fs.mountflags.programs`: `/bin/fs-mountflags at DIR` (host/tests/fs/progs/fs-mountflags.c) on such a
//!   mount, line by line against the tmpfs block CachyOS printed (host/tests/fs/mountflags-linux.txt);
//! - `fs.mountflags.boot`: the boot view's mounts keep programs runnable where they live: /, /boot (busybox
//!   from /boot/BITSOS2/bin) and /tmp are not noexec; /boot shows its device as the source.
//! Safe and rerunnable: each run mounts under a fresh name and unmounts and removes it at the end.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;
use vfs::mount_ns::{MNT_NODEV, MNT_NOEXEC, MNT_NOSUID, MNT_RELATIME};
use vfs::ops::Ctx;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const PROG: &str = "/bin/fs-mountflags";
const LINUX: &str = include_str!("../../../host/tests/fs/mountflags-linux.txt");
static RUN: AtomicU64 = AtomicU64::new(0);

/// A tmpfs a test mounts at `path` for one run; unmounted (its files go with it) and the mount point removed
/// when dropped, so an early `?` return leaves nothing behind either (Safe tests run several times per boot).
pub(crate) struct ScratchMount {
    pub(crate) path: String,
}

impl ScratchMount {
    pub(crate) fn new(path: String, perm: u32, size_pages: Option<u64>, flags: Option<u32>) -> Result<ScratchMount, String> {
        let m = vfs::tmpfs::mount_new(&vfs::root_ns(), &path, perm, size_pages).map_err(|e| format!("mount {path}: {e}"))?;
        if let Some(f) = flags {
            m.set_flags(f);
        }
        Ok(ScratchMount { path })
    }
}

impl Drop for ScratchMount {
    fn drop(&mut self) {
        let ns = vfs::root_ns();
        let r = ns.lookup_path(self.path.as_bytes(), true).and_then(|p| ns.umount(&p)).and_then(|_| Ctx::kernel(&ns).rmdir(None, self.path.as_bytes()));
        if let Err(e) = r {
            klog::event!(Warn, "fs", "ktest.scratch_mount_cleanup", path = &*self.path, errno = e);
        }
    }
}

/// A nosuid,nodev,noexec tmpfs at a fresh /tmp/fs13-<n> with /dev/null's node `null` (1:3).
struct NoexecMount {
    path: String,
    _m: ScratchMount,
}

impl NoexecMount {
    fn new() -> Result<NoexecMount, String> {
        let path = format!("/tmp/fs13-{}-{}", time::now_ns() / MS, RUN.fetch_add(1, Ordering::Relaxed));
        let m = ScratchMount::new(path.clone(), 0o777, Some(256), Some(MNT_NOSUID | MNT_NODEV | MNT_NOEXEC | MNT_RELATIME))?;
        let ns = vfs::root_ns();
        Ctx::kernel(&ns).mknod(None, format!("{path}/null").as_bytes(), uapi::S_IFCHR | 0o666, (1 << 8) | 3).map_err(|e| format!("mknod: {e}"))?;
        Ok(NoexecMount { path, _m: m })
    }
}

fn kernel_checks() -> TestResult {
    let m = NoexecMount::new()?;
    let ns = vfs::root_ns();
    let k = Ctx::kernel(&ns);
    let p = |n: &str| format!("{}/{n}", m.path).into_bytes();
    let rd = vfs::abi::O_RDONLY;
    kassert!(k.open(None, &p("null"), rd, 0).err() == Some(EACCES), "nodev: the device node opened");
    kassert!(k.open(None, &p("null"), vfs::abi::O_PATH, 0).is_ok(), "nodev: O_PATH refused");
    kassert!(k.open(None, b"/dev/null", rd, 0).is_ok(), "/dev/null itself does not open");
    let f = k.open(None, &p("f"), vfs::abi::O_CREAT | vfs::abi::O_RDWR, 0o755).map_err(|e| format!("create: {e}"))?;
    f.write(&[0x90; 64]).map_err(|e| format!("write: {e}"))?;
    kassert!(k.access(None, &p("f"), vfs::abi::X_OK, true).err() == Some(EACCES), "noexec: access(X_OK) allowed");
    kassert!(k.access(None, m.path.as_bytes(), vfs::abi::X_OK, true).is_ok(), "noexec: the directory is not searchable");
    use vfs::mmap::{MAP_PRIVATE, MAP_SHARED, PROT_EXEC, check};
    kassert!(check(&f, 1 | PROT_EXEC, MAP_PRIVATE, 0, 4096).err() == Some(EPERM), "noexec: private PROT_EXEC mapping allowed");
    kassert!(check(&f, 1 | PROT_EXEC, MAP_SHARED, 0, 4096).err() == Some(EPERM), "noexec: shared PROT_EXEC mapping allowed");
    let ok = check(&f, 1, MAP_PRIVATE, 0, 4096).map_err(|e| format!("read mapping: {e}"))?;
    kassert!(!ok.may_exec, "noexec: VM_MAYEXEC kept");
    drop(ok);
    drop(f);
    let _ = k.unlink(None, &p("f"));
    Ok(())
}
ktest!("fs.mountflags.kernel", Safe, kernel_checks);

/// The tmpfs block of mountflags-linux.txt without its setup line: (end line, stdout lines).
fn linux_block() -> Option<(&'static str, Vec<&'static str>)> {
    let mut inside = false;
    let mut end = None;
    let mut v = Vec::new();
    for l in LINUX.lines().filter(|l| !l.starts_with('#')) {
        if let Some(m) = l.strip_prefix("=== ") {
            if inside {
                break;
            }
            inside = m == "tmpfs";
        } else if inside && end.is_none() {
            end = Some(l);
        } else if inside && !l.starts_with("setup:") {
            v.push(l);
        }
    }
    end.map(|e| (e, v))
}

/// Lines another lane's change request decides: (prefix, what V2 says until then, why). Strict both ways: V2 must
/// say exactly the pending answer, and once it says Linux's answer the test fails until the entry is removed
/// here, so a merged change request cannot stay hidden behind this list.
/// Empty since INT-W4 merged FS-13 CR1 (exec) and CR2 (mprotect); `pending_rules` keeps the rules tested.
const PENDING: &[(&str, &str, &str)] = &[];

/// Compares the program's lines with Linux's under PENDING; Err names the first line that is wrong.
fn compare(got: &[&str], lines: &[&str]) -> Result<(), String> {
    compare_with(got, lines, PENDING)
}

pub(crate) fn compare_with(got: &[&str], lines: &[&str], pending_list: &[(&str, &str, &str)]) -> Result<(), String> {
    if got.len() != lines.len() {
        return Err(format!("{} lines vs Linux {}", got.len(), lines.len()));
    }
    for (pre, until, _) in pending_list {
        let Some(l) = lines.iter().find(|l| l.starts_with(pre)) else {
            return Err(format!("PENDING {pre:?} names no line of Linux's output: remove it"));
        };
        if *l == *until {
            return Err(format!("PENDING {pre:?}: V2's pending answer is Linux's own ({l:?})"));
        }
    }
    for (i, (g, l)) in got.iter().zip(lines.iter()).enumerate() {
        let pending = pending_list.iter().find(|(pre, _, _)| l.starts_with(pre));
        match pending {
            None if g == l => {}
            Some((_, until, why)) if g == until => {
                klog::event!(Info, "fs", "mountflags.pending", line = i, v2 = *g, linux = *l, owner = *why);
            }
            Some((pre, _, why)) if g == l => {
                return Err(format!("line {i}: V2 now answers {g:?} as Linux does ({why} is in): remove {pre:?} from PENDING"));
            }
            _ => {
                klog::event!(Warn, "fs", "mountflags.diff", line = i, v2 = *g, linux = *l);
                return Err(format!("line {i}: {g:?} vs Linux {l:?}"));
            }
        }
    }
    Ok(())
}

fn programs() -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::skip("/bin/fs-mountflags is not in this image (xtask builds host/tests/fs/progs into every image)");
    }
    let (end, lines) = linux_block().ok_or("mountflags-linux.txt has no tmpfs block")?;
    let m = NoexecMount::new()?;
    let out = proc::stdio::Capture::new(1 << 16);
    let argv = ["fs-mountflags", "at", m.path.as_str()];
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::node::File>);
    let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn: {e}"))?;
    p.wait(Some(60 * SEC));
    let got_end = match p.status() {
        Some(proc::ExitStatus::Exited(c)) => format!("end: exit {c}"),
        Some(proc::ExitStatus::Signaled(s)) => format!("end: signal {s}"),
        None => {
            p.kill(9);
            p.wait(Some(5 * SEC));
            return Err(String::from("fs-mountflags still ran after 60 s"));
        }
    };
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    let got: Vec<&str> = text.lines().collect();
    kassert!(got_end == end, "{got_end} vs Linux {end}; it said {text:?}");
    compare(&got, &lines).map_err(|e| format!("{e}; it said {text:?}"))
}
ktest!("fs.mountflags.programs", Safe, programs);

fn boot_mounts() -> TestResult {
    let ns = vfs::root_ns();
    for (path, must_exec) in [("/", true), ("/tmp", true), ("/proc", false)] {
        let Ok(p) = ns.lookup_path(path.as_bytes(), true) else { continue };
        kassert!(p.noexec() != must_exec, "{path}: noexec is {}", p.noexec());
    }
    let t = ns.lookup_path(b"/tmp", true).map_err(|e| format!("/tmp: {e}"))?;
    kassert!(t.nodev() && t.nosuid(), "/tmp is not nosuid,nodev");
    let Ok(b) = ns.lookup_path(b"/boot", true) else { return ktest::skip("no /boot (no boot stick mounted)") };
    if !b.is_mount_root() || b.mnt.sb.fstype != "vfat" {
        return ktest::skip("/boot is no vfat mount here (no boot stick)");
    }
    kassert!(!b.noexec() && !b.nodev(), "/boot: flags {:#x}: busybox in /boot/BITSOS2/bin could not run", b.mnt.flags());
    let src = b.mnt.sb.source();
    kassert!(src.starts_with("/dev/") && src.len() > 5, "/boot source {src:?}");
    Ok(())
}
ktest!("fs.mountflags.boot", Safe, boot_mounts);

/// The PENDING rules themselves: a pending line passes only with its pending answer, and Linux's answer there
/// fails (the entry must go once the change request is merged).
fn pending_rules() -> TestResult {
    // The entries FS-13 had until INT-W4 merged its change requests.
    const LIST: &[(&str, &str, &str)] = &[("exec noexec:", "exec noexec: ran", "CR1"), ("mprotect RX:", "mprotect RX: 0", "CR2")];
    let linux = ["exec noexec: EACCES", "mprotect RX: EACCES", "end"];
    let pending = ["exec noexec: ran", "mprotect RX: 0", "end"];
    kassert!(compare_with(&pending, &linux, LIST).is_ok(), "the pending answers fail");
    kassert!(compare(&linux, &linux).is_ok(), "Linux's own answers fail with the live list");
    let mut fixed = pending;
    fixed[0] = linux[0];
    kassert!(compare_with(&fixed, &linux, LIST).err().is_some_and(|e| e.contains("remove")), "a merged change request goes unnoticed");
    let mut other = pending;
    other[1] = "mprotect RX: EPERM";
    kassert!(compare_with(&other, &linux, LIST).is_err(), "a third answer passes");
    kassert!(compare_with(&pending[..2], &linux, LIST).is_err(), "a missing line passes");
    Ok(())
}
ktest!("fs.mountflags.pending_rules", Safe, pending_rules);

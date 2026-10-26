//! FS-14 ktests, tmpfs and memfd pages charged when allocated (`/bin/fs-memfd-lazy`,
//! host/tests/fs/progs/fs-memfd-lazy.c):
//! - `fs.memfd.lazy`: modes `pools` (foot's 4 x 512 MiB buffer pools) and `blocks` (st_blocks counts allocated
//!   pages only) compared line by line with what CachyOS printed (host/tests/fs/memfd-lazy-linux.txt);
//! - `fs.memfd.lazy.limit`: mode `at DIR` on a 64-page tmpfs made in the kernel, compared with the limit block
//!   (a short write then ENOSPC, fallocate all or nothing, a fault past the size is SIGBUS, a punch gives back);
//! - `fs.memfd.lazy.kernel`: the same rules below the syscalls, and the kernel's shared anonymous objects still
//!   charged by size (a fault inside them never meets an empty pool).
//! Safe and rerunnable: the mount gets a fresh name and goes at the end; programs still running are killed.

use crate::fs13_ktests::ScratchMount;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
const P: u64 = 4096;
const PROG: &str = "/bin/fs-memfd-lazy";
const LINUX: &str = include_str!("../../../host/tests/fs/memfd-lazy-linux.txt");
static RUN: AtomicU64 = AtomicU64::new(0);

/// The block of `mode` in memfd-lazy-linux.txt without setup lines: (end line, stdout lines).
fn block(mode: &str) -> Option<(&'static str, Vec<&'static str>)> {
    let mut inside = false;
    let mut end = None;
    let mut v = Vec::new();
    for l in LINUX.lines().filter(|l| !l.starts_with('#')) {
        if let Some(m) = l.strip_prefix("=== ") {
            if inside {
                break;
            }
            inside = m == mode;
        } else if inside && end.is_none() {
            end = Some(l);
        } else if inside && !l.starts_with("setup:") {
            v.push(l);
        }
    }
    end.map(|e| (e, v))
}

/// Runs the program with `args`, kills it after 60 s; (end line, stdout).
fn run(args: &[&str]) -> Result<(String, String), String> {
    let mut argv = Vec::from(["fs-memfd-lazy"]);
    argv.extend_from_slice(args);
    let env = ["HOME=/", "PATH=/bin"];
    let out = proc::stdio::Capture::new(1 << 16);
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::node::File>);
    let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn: {e}"))?;
    p.wait(Some(60 * SEC));
    let end = match p.status() {
        Some(proc::ExitStatus::Exited(c)) => format!("end: exit {c}"),
        Some(proc::ExitStatus::Signaled(s)) => format!("end: signal {s}"),
        None => {
            p.kill(9);
            p.wait(Some(5 * SEC));
            return Err(format!("{args:?} still ran after 60 s"));
        }
    };
    Ok((end, String::from_utf8_lossy(&out.contents()).into_owned()))
}

fn compare(mode: &str, args: &[&str], pending: &[(&str, &str, &str)]) -> TestResult {
    let (end, lines) = block(mode).ok_or_else(|| format!("memfd-lazy-linux.txt has no {mode} block"))?;
    let (got_end, text) = run(args)?;
    let got: Vec<&str> = text.lines().collect();
    kassert!(got_end == end, "{mode}: {got_end} vs Linux {end}; it said {text:?}");
    crate::fs13_ktests::compare_with(&got, &lines, pending).map_err(|e| {
        klog::event!(Warn, "fs", "memfd_lazy.diff", mode = mode);
        format!("{mode}: {e}; it said {text:?}")
    })
}

fn have_prog() -> Option<TestResult> {
    vfs::root_ns().lookup(PROG).is_err().then(|| ktest::skip("/bin/fs-memfd-lazy is not in this image (xtask builds host/tests/fs/progs into every image)"))
}

fn programs() -> TestResult {
    if let Some(skip) = have_prog() {
        return skip;
    }
    compare("pools", &["pools"], &[])?;
    compare("blocks", &["blocks"], &[])
}
ktest!("fs.memfd.lazy", Safe, programs);

fn limit() -> TestResult {
    if let Some(skip) = have_prog() {
        return skip;
    }
    let path = format!("/tmp/fs14-{}-{}", time::now_ns() / MS, RUN.fetch_add(1, Ordering::Relaxed));
    let _m = ScratchMount::new(path.clone(), 0o777, Some(64), None)?;
    compare("limit", &["at", &path], &[])
}
ktest!("fs.memfd.lazy.limit", Safe, limit);

/// Below the syscalls: a 4-page tmpfs; growth free, pages charged as made, a refused fault is SIGBUS (`Bus`),
/// the last reference gives everything back; the kernel's shared anonymous object is charged by its size.
fn kernel() -> TestResult {
    use vfs::abi::{O_CREAT, O_DIRECTORY, O_EXCL, O_RDWR, O_TMPFILE_BIT};
    use vfs::mmap::MapBacking;
    use vfs::pagecache::PageFault;
    let path = format!("/tmp/fs14k-{}-{}", time::now_ns() / MS, RUN.fetch_add(1, Ordering::Relaxed));
    let _m = ScratchMount::new(path.clone(), 0o777, Some(4), None)?;
    let ns = vfs::root_ns();
    let f = ns.open_flags(format!("{path}/f").as_bytes(), O_CREAT | O_RDWR, 0o600).map_err(|e| format!("open: {e}"))?;
    let i = f.inode().ok_or("no inode")?.clone();
    let cache = i.page_cache().ok_or("no cache")?;
    kassert!(cache.lazy(), "a tmpfs file is charged per page");
    i.truncate(1 << 30).map_err(|e| format!("grow: {e}"))?;
    kassert!(cache.charged() == 0, "growth charged {}", cache.charged());
    let m = vfs::mmap::check(&f, 3, vfs::mmap::MAP_SHARED, 0, 8 * P).map_err(|e| format!("map: {e}"))?;
    let mut pins = Vec::new();
    for pg in 0..4 {
        pins.push(m.backing.get_page(pg, true).map_err(|e| format!("fault {pg}: {e:?}"))?);
    }
    kassert!(cache.charged() == 4, "four faults, {} charged", cache.charged());
    let refused = m.backing.get_page(4, true);
    kassert!(refused == Err(PageFault::Bus), "a fault past the mount's size: {refused:?}");
    kassert!(f.pwrite(5 * P, b"x") == Err(ENOSPC), "a write past the mount's size");
    for p in pins {
        kassert!(m.backing.put_page(p), "put");
    }
    drop(m);
    i.truncate(0).map_err(|e| format!("shrink: {e}"))?;
    kassert!(cache.charged() == 0, "after truncate {} charged", cache.charged());
    drop(f);
    drop(i);
    let _ = vfs::Ctx::kernel(&ns).unlink(None, format!("{path}/f").as_bytes());
    // MM's shared anonymous memory: a kernel O_TMPFILE in /dev/shm, charged when its size grows.
    if ns.lookup("/dev/shm").is_ok() {
        let o = ns.open_flags(b"/dev/shm", O_TMPFILE_BIT | O_DIRECTORY | O_RDWR | O_EXCL, 0o600).map_err(|e| format!("tmpfile: {e}"))?;
        let oi = o.inode().ok_or("no inode")?.clone();
        oi.truncate(3 * P).map_err(|e| format!("object size: {e}"))?;
        let oc = oi.page_cache().ok_or("no cache")?;
        kassert!(!oc.lazy() && oc.charged() == 3, "the kernel's nameless object: lazy {} charged {}", oc.lazy(), oc.charged());
    }
    Ok(())
}
ktest!("fs.memfd.lazy.kernel", Safe, kernel);

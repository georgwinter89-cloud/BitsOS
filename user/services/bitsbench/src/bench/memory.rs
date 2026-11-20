//! Memory under load, the desk's load round in small (round-at3.sh/load2.sh, bounded to 10 s a run): readers
//! stream the Steam games' files (else a file of the runner's), a writer fills a file in /tmp (tmpfs, so memory)
//! and frees it again, a third walks /sys. Meanwhile 256 MiB are taken and every page written; the value is how
//! long that took, in ms. Everything is bounded: the tmpfs file by a quarter of the free memory (at most 1.5 GB),
//! the readers by 512 MiB per run, the walk by 20 000 entries.

use super::{Ctx, Fail, Values, ms, repeat};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const TOUCH: usize = 256 << 20;
const LOAD_FOR: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_secs(2);
const READ_MAX: u64 = 512 << 20;
const FILL_MAX: u64 = 1536 << 20;
const READERS: usize = 4;
const LIST_MAX: usize = 10_000;

/// MemAvailable of /proc/meminfo in bytes.
pub fn available(meminfo: &str) -> Option<u64> {
    let l = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
    Some(l.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
}

fn files_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo: Vec<(PathBuf, u32)> = dirs.iter().map(|d| (d.clone(), 0)).collect();
    while let Some((d, depth)) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if out.len() >= LIST_MAX {
                return out;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() && depth < 8 => todo.push((e.path(), depth + 1)),
                Ok(t) if t.is_file() => out.push(e.path()),
                _ => {}
            }
        }
    }
    out
}

fn touch() -> f64 {
    let t0 = Instant::now();
    let mut v: Vec<u8> = Vec::with_capacity(TOUCH);
    v.resize(TOUCH, 0);
    for i in (0..TOUCH).step_by(4096) {
        v[i] = (i >> 12) as u8;
    }
    std::hint::black_box(&v);
    ms(t0.elapsed())
}

fn walk(p: &Path, left: &mut usize, stop: &AtomicBool) {
    let Ok(rd) = std::fs::read_dir(p) else { return };
    for e in rd.flatten() {
        if *left == 0 || stop.load(Ordering::Relaxed) {
            return;
        }
        *left -= 1;
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&e.path(), left, stop);
        }
    }
}

fn one(files: &Arc<Vec<PathBuf>>, fill: &Path, fill_max: u64, quit: &dyn Fn() -> bool) -> Option<f64> {
    let stop = Arc::new(AtomicBool::new(false));
    let read = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for k in 0..READERS {
        let (stop, read, files) = (stop.clone(), read.clone(), files.clone());
        hs.push(std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 20];
            for p in files.iter().skip(k).step_by(READERS) {
                let Ok(mut f) = std::fs::File::open(p) else { continue };
                while let Ok(n) = f.read(&mut buf) {
                    if n == 0 || stop.load(Ordering::Relaxed) || read.fetch_add(n as u64, Ordering::Relaxed) > READ_MAX {
                        break;
                    }
                }
                if stop.load(Ordering::Relaxed) || read.load(Ordering::Relaxed) > READ_MAX {
                    return;
                }
            }
        }));
    }
    let (s2, f2) = (stop.clone(), fill.to_path_buf());
    hs.push(std::thread::spawn(move || {
        let chunk = vec![0x5au8; 1 << 20];
        while !s2.load(Ordering::Relaxed) {
            if let Ok(mut f) = std::fs::File::create(&f2) {
                let mut put = 0;
                while put < fill_max && !s2.load(Ordering::Relaxed) && f.write_all(&chunk).is_ok() {
                    put += chunk.len() as u64;
                }
            }
            let _ = std::fs::remove_file(&f2);
            std::thread::sleep(Duration::from_millis(200));
        }
    }));
    let s3 = stop.clone();
    hs.push(std::thread::spawn(move || {
        while !s3.load(Ordering::Relaxed) {
            let mut left = 20_000;
            walk(Path::new("/sys/bus"), &mut left, &s3);
            std::thread::sleep(Duration::from_millis(100));
        }
    }));
    let t0 = Instant::now();
    let wait = |until: Duration| {
        while t0.elapsed() < until {
            if quit() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        true
    };
    let v = if wait(SETTLE) { Some(touch()) } else { None };
    let v = v.filter(|_| wait(LOAD_FOR));
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        let _ = h.join();
    }
    let _ = std::fs::remove_file(fill);
    v
}

pub fn run(ctx: &Ctx) -> Values {
    let free = std::fs::read_to_string("/proc/meminfo").ok().as_deref().and_then(available).unwrap_or(0);
    let fill_max = (free / 4).min(FILL_MAX);
    if free < (TOUCH as u64) * 4 {
        return Err(Fail::Missing("zu wenig freier Speicher für die Messung".into()));
    }
    let mut files = files_in(&ctx.cfg.load_dirs);
    let own = ctx.cfg.tmp().join("last");
    if files.is_empty() {
        std::fs::create_dir_all(&own).map_err(|e| Fail::Missing(format!("Testordner: {e}")))?;
        let p = own.join("lesen");
        let chunk = vec![0x33u8; 1 << 20];
        let mut f = std::fs::File::create(&p).map_err(|e| Fail::Missing(format!("Testordner: {e}")))?;
        for _ in 0..128 {
            f.write_all(&chunk).map_err(|e| Fail::Missing(format!("Testordner: {e}")))?;
        }
        files.push(p);
    }
    let files = Arc::new(files);
    let fill = PathBuf::from(format!("/tmp/{FILL_PREFIX}{}", std::process::id()));
    let r = repeat(ctx, |_| {
        let v = one(&files, &fill, fill_max, ctx.stop).ok_or(Fail::Stopped)?;
        ctx.sleep(Duration::from_secs(1))?;
        Ok(v)
    });
    let _ = std::fs::remove_dir_all(&own);
    r
}

/// The fill's name in /tmp before its runner's pid.
pub const FILL_PREFIX: &str = "bitsbench-last-";

/// Removes the fills in `dir` whose runner is gone (a crash or a power cut mid-run; /tmp is memory). Returns how
/// many went.
pub fn tidy_fills(dir: &std::path::Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let me = std::process::id();
    rd.flatten()
        .filter(|e| {
            let name = e.file_name();
            let pid = name.to_str().and_then(|n| n.strip_prefix(FILL_PREFIX)).and_then(|p| p.parse::<u32>().ok());
            pid.is_some_and(|p| p != me && !crate::serve::alive(p))
        })
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count()
}

//! File read and write in a test folder under /data/leistung/tmp (removed after).
//! - `file_read`: abi-pfread's read path (PF-1, Linux 4c0f369's test shape) in its file-per-thread form: 8 threads,
//!   each preads 64 KiB pieces of its own 16 MiB file for 2 s; MB/s over all of them.
//! - `file_mix`: a real-file mix: 400 files of 4 to 64 KiB and one of 64 MiB written (synced), read back whole,
//!   listed and removed; the time in ms.

use super::{Ctx, Fail, Values, ms, repeat};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const THREADS: usize = 8;
const FILE: usize = 16 << 20;
const PIECE: usize = 64 << 10;
const READ_FOR: Duration = Duration::from_secs(2);
const SMALL: usize = 400;
const BIG: usize = 64 << 20;

fn pattern(id: usize, len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i >> 4) * 29 + (i & 15) * 7 + id * 101 + (i >> 12)) as u8).collect()
}

fn miss(e: std::io::Error) -> Fail {
    Fail::Missing(format!("Testordner: {e}"))
}

struct Gone(PathBuf);

impl Drop for Gone {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_round(files: &[PathBuf]) -> std::io::Result<f64> {
    let stop = Arc::new(AtomicBool::new(false));
    let bytes = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();
    let mut hs = Vec::new();
    for p in files {
        let f = std::fs::File::open(p)?;
        let (stop, bytes) = (stop.clone(), bytes.clone());
        hs.push(std::thread::spawn(move || {
            let mut buf = vec![0u8; PIECE];
            let mut off = 0u64;
            while !stop.load(Ordering::Relaxed) {
                match f.read_at(&mut buf, off) {
                    Ok(n) if n > 0 => {
                        bytes.fetch_add(n as u64, Ordering::Relaxed);
                        off = (off + n as u64) % FILE as u64;
                    }
                    _ => return,
                }
            }
        }));
    }
    std::thread::sleep(READ_FOR);
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        let _ = h.join();
    }
    let secs = t0.elapsed().as_secs_f64();
    Ok(bytes.load(Ordering::Relaxed) as f64 / 1e6 / secs)
}

pub fn read(ctx: &Ctx) -> Values {
    let dir = ctx.cfg.tmp().join("lesen");
    std::fs::create_dir_all(&dir).map_err(miss)?;
    let _gone = Gone(dir.clone());
    let mut files = Vec::new();
    for id in 0..THREADS {
        ctx.check()?;
        let p = dir.join(format!("pfread-{id}"));
        std::fs::write(&p, pattern(id, FILE)).map_err(miss)?;
        files.push(p);
    }
    repeat(ctx, |_| read_round(&files).map_err(miss))
}

fn size(i: usize) -> usize {
    4096 + (i * 7919) % (60 << 10)
}

fn mix_round(dir: &Path) -> std::io::Result<f64> {
    std::fs::create_dir_all(dir)?;
    let t0 = Instant::now();
    let chunk = pattern(3, 1 << 20);
    for i in 0..SMALL {
        let mut f = std::fs::File::create(dir.join(format!("f{i:04}")))?;
        f.write_all(&chunk[..size(i)])?;
        if i % 50 == 49 {
            f.sync_all()?;
        }
    }
    let mut big = std::fs::File::create(dir.join("gross"))?;
    for _ in 0..BIG / chunk.len() {
        big.write_all(&chunk)?;
    }
    big.sync_all()?;
    drop(big);
    let mut buf = Vec::new();
    let mut seen = 0;
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        buf.clear();
        std::fs::File::open(&p)?.read_to_end(&mut buf)?;
        seen += 1;
        std::fs::remove_file(&p)?;
    }
    if seen != SMALL + 1 {
        return Err(std::io::Error::other(format!("{seen} files read back, {} written", SMALL + 1)));
    }
    Ok(ms(t0.elapsed()))
}

pub fn mix(ctx: &Ctx) -> Values {
    let base = ctx.cfg.tmp().join("mischung");
    let _gone = Gone(base.clone());
    repeat(ctx, |i| mix_round(&base.join(format!("lauf-{i}"))).map_err(miss))
}

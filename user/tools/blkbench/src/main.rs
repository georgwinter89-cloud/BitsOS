//! blkbench (BL-0, plan/notes/desk-W35-mm-blk.md "BL-0 procedure"): read-path measurements, the same static binary
//! on V2 and on CachyOS. One `key=value` line per measurement (`test=<name> ...`), medians of `--runs` runs (3).
//!
//! - `prepare DIR`: seq.bin (4 GiB), rand.bin (1 GiB), small/ (4000 files, 4 KiB..1 MiB log-uniform, seed 35);
//!   pseudo-random content (not compressible), each file fsynced.
//! - `cold FILE|DIR...`: POSIX_FADV_DONTNEED on every file, then mincore on a read-only mapping of each: prints
//!   `resident_pages=` and fails (exit 1) when any page of a measured file is still resident (the cold proof).
//! - `seq FILE [--bs 1M] [--threads N]`: sequential read (N threads: N contiguous slices): MB/s, wall, own CPU.
//! - `rand FILE [--bs 4K] [--count 20000] [--threads 1|8|32]`: random pread: IOPS, latency p50/p99.
//! - `small DIR`: open + read + close of every file in name order: files/s, MB/s.
//! - `mmap FILE`: one byte per 4 KiB touched in order through a mapping (faults, read-ahead): MB/s.
//! - `all DIR`: seq, rand QD1, rand x32, small, mmap of DIR's prepared files.
//!
//! Every measured run is preceded by the cold step of its files (a run that cannot be made cold fails).
//! CPU is this process's utime/stime from /proc/self/stat (10 ms ticks). Exit: 0 ok, 1 a failed step, 2 usage.

mod bench;
mod sys;

use bench::{f1, line, list, median, percentile, Cmd, Rng};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::Instant;

type R<T> = Result<T, String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match bench::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("blkbench: {e}\n{}", bench::USAGE);
            exit(2);
        }
    };
    if let Err(e) = run(cmd) {
        println!("test=error what={}", e.replace(' ', "_"));
        eprintln!("blkbench: {e}");
        exit(1);
    }
}

fn run(cmd: Cmd) -> R<()> {
    match cmd {
        Cmd::Prepare { dir, seq_mb, rand_mb, small, seed } => prepare(Path::new(&dir), seq_mb, rand_mb, small, seed),
        Cmd::Cold { paths } => cold(&paths.iter().map(PathBuf::from).collect::<Vec<_>>()).map(|_| ()),
        Cmd::Seq { file, bs, threads, runs } => seq(Path::new(&file), bs, threads, runs),
        Cmd::Rand { file, bs, count, threads, runs } => rand(Path::new(&file), bs, count, threads, runs),
        Cmd::Small { dir, runs } => small(Path::new(&dir), runs),
        Cmd::Mmap { file, runs } => mmap(Path::new(&file), runs),
        Cmd::All { dir, runs } => {
            let d = Path::new(&dir);
            seq(&d.join("seq.bin"), 1 << 20, 1, runs)?;
            rand(&d.join("rand.bin"), 4096, 20000, 1, runs)?;
            rand(&d.join("rand.bin"), 4096, 20000, 32, runs)?;
            small(&d.join("small"), runs)?;
            mmap(&d.join("seq.bin"), runs)
        }
    }
}

fn ctx<T, E: std::fmt::Display>(r: Result<T, E>, what: &dyn std::fmt::Display) -> R<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

fn write_file(path: &Path, bytes: u64, rng: &mut Rng) -> R<()> {
    let mut f = ctx(File::create(path), &path.display())?;
    let mut buf = vec![0u8; 1 << 20];
    let mut left = bytes;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        rng.fill(&mut buf[..n]);
        ctx(f.write_all(&buf[..n]), &path.display())?;
        left -= n as u64;
    }
    ctx(f.sync_all(), &path.display())
}

fn prepare(dir: &Path, seq_mb: u64, rand_mb: u64, n: u64, seed: u64) -> R<()> {
    let t = Instant::now();
    let sd = dir.join("small");
    ctx(fs::create_dir_all(&sd), &sd.display())?;
    write_file(&dir.join("seq.bin"), seq_mb << 20, &mut Rng::new(seed))?;
    write_file(&dir.join("rand.bin"), rand_mb << 20, &mut Rng::new(seed + 1))?;
    let sizes = bench::small_sizes(n, seed);
    for (i, &s) in sizes.iter().enumerate() {
        write_file(&sd.join(bench::small_name(i as u64)), s, &mut Rng::new(seed + 2 + i as u64))?;
    }
    for d in [&sd, dir] {
        ctx(File::open(d).and_then(|f| f.sync_all()), &d.display())?;
    }
    let total: u64 = sizes.iter().sum();
    println!("{}", line("prepare", &[("dir", dir.display().to_string()), ("seq_bytes", (seq_mb << 20).to_string()), ("rand_bytes", (rand_mb << 20).to_string()),
        ("small_files", n.to_string()), ("small_bytes", total.to_string()), ("wall_ms", t.elapsed().as_millis().to_string())]));
    Ok(())
}

/// The regular files under `paths` (directories in name order).
fn files(paths: &[PathBuf]) -> R<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in paths {
        let m = ctx(fs::metadata(p), &p.display())?;
        if m.is_dir() {
            let mut names: Vec<PathBuf> = ctx(fs::read_dir(p), &p.display())?.filter_map(|e| e.ok().map(|e| e.path())).collect();
            names.sort();
            out.extend(files(&names)?);
        } else if m.is_file() {
            out.push(p.clone());
        }
    }
    Ok(out)
}

/// DONTNEED on every file, then mincore of a fresh read-only mapping: Err when a page stayed resident.
fn cold(paths: &[PathBuf]) -> R<(u64, u64)> {
    let list = files(paths)?;
    let (mut pages, mut resident, mut hot) = (0u64, 0u64, Vec::new());
    for p in &list {
        let f = ctx(File::open(p), &p.display())?;
        let len = ctx(f.metadata(), &p.display())?.len();
        if let Err(e) = sys::fadvise(&f, 0, 0, sys::POSIX_FADV_DONTNEED) {
            return Err(format!("{}: posix_fadvise DONTNEED: errno {e}", p.display()));
        }
        if len == 0 {
            continue;
        }
        let m = sys::FileMap::new(&f, len as usize).map_err(|e| format!("{}: mmap: errno {e}", p.display()))?;
        let r = m.resident().map_err(|e| format!("{}: mincore: errno {e}", p.display()))? as u64;
        pages += len.div_ceil(sys::PAGE as u64);
        resident += r;
        if r > 0 {
            hot.push((p.clone(), r));
        }
    }
    println!("{}", line("cold", &[("files", list.len().to_string()), ("pages", pages.to_string()), ("resident_pages", resident.to_string()), ("ok", ((resident == 0) as u8).to_string())]));
    for (p, r) in &hot {
        println!("{}", line("cold_resident", &[("file", p.display().to_string()), ("resident_pages", r.to_string())]));
    }
    if resident > 0 {
        return Err(format!("{resident} page(s) stayed resident after POSIX_FADV_DONTNEED"));
    }
    Ok((list.len() as u64, pages))
}

/// One measured run: its wall seconds and this process's CPU ticks (user, system).
struct Run {
    secs: f64,
    user: u64,
    sys: u64,
}

fn measured(paths: &[PathBuf], f: &mut dyn FnMut() -> R<()>) -> R<Run> {
    cold(paths)?;
    let (u0, s0) = sys::cpu_ticks();
    let t = Instant::now();
    f()?;
    let secs = t.elapsed().as_secs_f64().max(1e-9);
    let (u1, s1) = sys::cpu_ticks();
    Ok(Run { secs, user: u1.saturating_sub(u0), sys: s1.saturating_sub(s0) })
}

/// The common fields of a measurement: median wall time and CPU of the runs.
fn common(runs: &[Run]) -> Vec<(&'static str, String)> {
    let ms = |t: u64| (t * 1000 / bench::USER_HZ).to_string();
    let walls: Vec<f64> = runs.iter().map(|r| r.secs * 1000.0).collect();
    let user: Vec<f64> = runs.iter().map(|r| r.user as f64).collect();
    let sys: Vec<f64> = runs.iter().map(|r| r.sys as f64).collect();
    vec![("runs", runs.len().to_string()), ("wall_ms", f1(median(&walls))), ("user_ms", ms(median(&user) as u64)), ("sys_ms", ms(median(&sys) as u64)), ("cold", "fadvise".into())]
}

fn mb_s(bytes: u64, r: &Run) -> f64 {
    bytes as f64 / (1u64 << 20) as f64 / r.secs
}

fn seq(file: &Path, bs: u64, threads: u64, runs: u64) -> R<()> {
    let len = ctx(fs::metadata(file), &file.display())?.len();
    let mut all = Vec::new();
    for _ in 0..runs {
        all.push(measured(&[file.to_path_buf()], &mut || seq_once(file, len, bs, threads))?);
    }
    let rates: Vec<f64> = all.iter().map(|r| mb_s(len, r)).collect();
    let mut kv = vec![("file", file.display().to_string()), ("bytes", len.to_string()), ("bs", bs.to_string()), ("threads", threads.to_string()), ("mb_s", f1(median(&rates))), ("mb_s_runs", list(&rates))];
    kv.extend(common(&all));
    println!("{}", line("seq", &kv));
    Ok(())
}

fn seq_once(file: &Path, len: u64, bs: u64, threads: u64) -> R<()> {
    if threads == 1 {
        let mut f = ctx(File::open(file), &file.display())?;
        let mut buf = vec![0u8; bs as usize];
        let mut got = 0u64;
        loop {
            match ctx(f.read(&mut buf), &file.display())? {
                0 => break,
                n => got += n as u64,
            }
        }
        return if got == len { Ok(()) } else { Err(format!("{}: read {got} of {len} bytes", file.display())) };
    }
    let slice = len.div_ceil(threads).div_ceil(bs) * bs;
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads).map(|t| s.spawn(move || -> R<()> {
            let f = ctx(File::open(file), &file.display())?;
            let mut buf = vec![0u8; bs as usize];
            let (mut at, end) = ((t * slice).min(len), ((t + 1) * slice).min(len));
            while at < end {
                let n = ctx(f.read_at(&mut buf[..(end - at).min(bs) as usize], at), &file.display())?;
                if n == 0 {
                    return Err(format!("{}: early end at {at}", file.display()));
                }
                at += n as u64;
            }
            Ok(())
        })).collect();
        hs.into_iter().try_for_each(|h| h.join().map_err(|_| "a reader thread panicked".to_string())?)
    })
}

fn rand(file: &Path, bs: u64, count: u64, threads: u64, runs: u64) -> R<()> {
    let len = ctx(fs::metadata(file), &file.display())?.len();
    let blocks = len / bs;
    if blocks == 0 {
        return Err(format!("{}: smaller than one {bs}-byte block", file.display()));
    }
    let (mut all, mut iops, mut p50, mut p99) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for run in 0..runs {
        let mut lat: Vec<u64> = Vec::new();
        let r = measured(&[file.to_path_buf()], &mut || {
            lat = rand_once(file, bs, blocks, count, threads, 35 + run)?;
            Ok(())
        })?;
        iops.push(count as f64 / r.secs);
        p50.push(percentile(&mut lat, 50.0) as f64 / 1000.0);
        p99.push(percentile(&mut lat, 99.0) as f64 / 1000.0);
        all.push(r);
    }
    let mut kv = vec![("file", file.display().to_string()), ("bs", bs.to_string()), ("count", count.to_string()), ("threads", threads.to_string()), ("iops", f1(median(&iops))),
        ("iops_runs", list(&iops)), ("lat_p50_us", f1(median(&p50))), ("lat_p99_us", f1(median(&p99)))];
    kv.extend(common(&all));
    println!("{}", line("rand", &kv));
    Ok(())
}

/// `count` random block reads spread over `threads`; every read's latency in ns.
fn rand_once(file: &Path, bs: u64, blocks: u64, count: u64, threads: u64, seed: u64) -> R<Vec<u64>> {
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads).map(|t| s.spawn(move || -> R<Vec<u64>> {
            let f = ctx(File::open(file), &file.display())?;
            let mut buf = vec![0u8; bs as usize];
            let mut rng = Rng::new(seed * 1000 + t);
            let mine = count / threads + (t < count % threads) as u64;
            let mut lat = Vec::with_capacity(mine as usize);
            for _ in 0..mine {
                let at = (rng.next() % blocks) * bs;
                let t0 = Instant::now();
                let n = ctx(f.read_at(&mut buf, at), &file.display())?;
                lat.push(t0.elapsed().as_nanos() as u64);
                if n as u64 != bs {
                    return Err(format!("{}: short read at {at}", file.display()));
                }
            }
            Ok(lat)
        })).collect();
        let mut all = Vec::new();
        for h in hs {
            all.extend(h.join().map_err(|_| "a reader thread panicked".to_string())??);
        }
        Ok(all)
    })
}

fn small(dir: &Path, runs: u64) -> R<()> {
    let list = files(&[dir.to_path_buf()])?;
    let mut bytes = 0u64;
    let mut all = Vec::new();
    for _ in 0..runs {
        bytes = 0;
        all.push(measured(&[dir.to_path_buf()], &mut || {
            let mut buf = vec![0u8; 1 << 20];
            for p in &list {
                let mut f = ctx(File::open(p), &p.display())?;
                loop {
                    match ctx(f.read(&mut buf), &p.display())? {
                        0 => break,
                        n => bytes += n as u64,
                    }
                }
            }
            Ok(())
        })?);
    }
    let fps: Vec<f64> = all.iter().map(|r| list.len() as f64 / r.secs).collect();
    let rates: Vec<f64> = all.iter().map(|r| mb_s(bytes, r)).collect();
    let mut kv = vec![("dir", dir.display().to_string()), ("files", list.len().to_string()), ("bytes", bytes.to_string()), ("files_s", f1(median(&fps))), ("mb_s", f1(median(&rates))), ("files_s_runs", bench::list(&fps))];
    kv.extend(common(&all));
    println!("{}", line("small", &kv));
    Ok(())
}

fn mmap(file: &Path, runs: u64) -> R<()> {
    let len = ctx(fs::metadata(file), &file.display())?.len();
    if len == 0 {
        return Err(format!("{}: empty", file.display()));
    }
    let mut all = Vec::new();
    let mut sum = 0u64;
    for _ in 0..runs {
        all.push(measured(&[file.to_path_buf()], &mut || {
            let f = ctx(File::open(file), &file.display())?;
            let m = sys::FileMap::new(&f, len as usize).map_err(|e| format!("{}: mmap: errno {e}", file.display()))?;
            sum = sum.wrapping_add(m.touch(0, (len as usize).div_ceil(sys::PAGE)));
            Ok(())
        })?);
    }
    let rates: Vec<f64> = all.iter().map(|r| mb_s(len, r)).collect();
    let mut kv = vec![("file", file.display().to_string()), ("bytes", len.to_string()), ("mb_s", f1(median(&rates))), ("mb_s_runs", list(&rates)), ("touched_sum", (sum % 1000).to_string())];
    kv.extend(common(&all));
    println!("{}", line("mmap", &kv));
    Ok(())
}

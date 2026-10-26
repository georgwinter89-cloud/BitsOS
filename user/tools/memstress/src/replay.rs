//! `memstress replay`: libcobalt's memory pattern from Y-NUMBERS 1.1 (werkzeuge/plattform/plaene/youtube/
//! Y-NUMBERS.md:37-64), in counts and sizes; the order is a fixed-seed mix, not a trace.

use crate::{ENOMEM, MADV_DONTNEED, MADV_NOHUGEPAGE, MIB, PAGE, Rng, fail};
use std::os::fd::AsRawFd;
use usys::dev::SharedMap;
use usys::{MAP_NORESERVE, Mapping, PROT_NONE, PROT_READ, PROT_WRITE};

const GIB: usize = 1 << 30;
/// MADV_WIPEONFORK: Y-NUMBERS saw advice 18 once.
const MADV_18: usize = 18;

struct Sizes {
    commit_mib: usize,
    commits: usize,
    dontneeds: usize,
    big_dontneed_mib: usize,
    shm_maps: usize,
}

const SMALL: Sizes = Sizes { commit_mib: 64, commits: 2250, dontneeds: 2000, big_dontneed_mib: 16, shm_maps: 200 };
const FULL: Sizes = Sizes { commit_mib: 770, commits: 2250, dontneeds: 26_400, big_dontneed_mib: 512, shm_maps: 1300 };
/// How many shm files are mapped at once (two views each) before the oldest is unmapped.
const SHM_LIVE: usize = 64;

/// Sleeps so that `done` of `total` steps end at their share of `seconds` (0: no pacing).
fn pace(start: u64, seconds: u64, done: usize, total: usize) {
    if seconds == 0 || total == 0 {
        return;
    }
    // main refuses a `seconds` whose nanoseconds do not fit u64, so the share fits too.
    let due = start.saturating_add((seconds as u128 * 1_000_000_000 * done as u128 / total as u128) as u64);
    let now = usys::mono_ns();
    if due > now {
        std::thread::sleep(std::time::Duration::from_nanos(due - now));
    }
}

struct ShmFile {
    writer: SharedMap,
    reader: SharedMap,
}

/// mkstemp-like create in /dev/shm, a reader fd, unlink, size, both views (Chromium's shared memory, Y-NUMBERS 1.4).
fn shm_file(n: usize, len: usize) -> Result<ShmFile, String> {
    let path = format!("/dev/shm/.memstress.{}.{n}", std::process::id());
    let w = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).map_err(|e| format!("create {path}: {e}"));
    let r = w.as_ref().ok().map(|_| std::fs::File::open(&path));
    let _ = std::fs::remove_file(&path);
    let w = w?;
    let r = r.ok_or("no reader")?.map_err(|e| format!("open {path}: {e}"))?;
    w.set_len(len as u64).map_err(|e| format!("ftruncate {len}: {e}"))?;
    let writer = SharedMap::new(w.as_raw_fd() as usize, len, PROT_READ | PROT_WRITE, 0).map_err(|e| format!("mmap(MAP_SHARED, RW): {}", e.name()))?;
    let reader = SharedMap::new(r.as_raw_fd() as usize, len, PROT_READ, 0).map_err(|e| format!("mmap(MAP_SHARED, RO): {}", e.name()))?;
    // The files close here; the mappings keep them (Linux and V2 alike).
    Ok(ShmFile { writer, reader })
}

pub fn run(full: bool, seconds: u64) -> i32 {
    let s = if full { FULL } else { SMALL };
    let start = usys::mono_ns();
    // Reservations: PROT_NONE, MAP_NORESERVE; they cost no commit.
    let sizes = [32 * GIB, 32 * GIB, 4 * GIB, GIB / 2, GIB / 2, GIB / 2, GIB / 2, GIB / 4, GIB / 8];
    let res: Vec<Mapping> = sizes
        .iter()
        .map(|&len| Mapping::anon(len, PROT_NONE, MAP_NORESERVE).unwrap_or_else(|e| fail(&format!("reserve {} MiB: {}", len / MIB, e.name()))))
        .collect();
    let total: usize = sizes.iter().sum();
    println!("reserve {:.1} GiB in {} ranges (PROT_NONE, MAP_NORESERVE): ok", total as f64 / GIB as f64, sizes.len());

    // Commits: mprotect(RW) pieces one after another in the first pool, each touched. The first ENOMEM ends the
    // replay: the pieces after it stay PROT_NONE, and touching them would be a SIGSEGV, not the errno this shows.
    let pool = &res[0];
    let piece = (s.commit_mib * MIB / s.commits).div_ceil(PAGE) * PAGE;
    let mut enomem = 0;
    let steps = s.commits + s.dontneeds + s.shm_maps;
    let mut done_steps = 0;
    for i in 0..s.commits {
        match pool.protect(i * piece, piece, PROT_READ | PROT_WRITE) {
            Ok(_) => {
                let _ = pool.advise(i * piece, piece, MADV_NOHUGEPAGE);
                pool.set(i * piece, 0x5a);
            }
            Err(e) if e.0 == ENOMEM => {
                println!("commit: ENOMEM (an errno, no signal)");
                eprintln!("memstress: ENOMEM at commit {i} of {}, {} MiB committed", s.commits, i * piece / MIB);
                drop(res);
                println!("replay: ENOMEM seen");
                return 1;
            }
            Err(e) => fail(&format!("mprotect(RW) commit {i}: {}", e.name())),
        }
        done_steps += 1;
        pace(start, seconds, done_steps, steps);
    }
    let committed = s.commits * piece;
    println!("commit {} calls of {} KiB (mprotect RW, each touched): {} MiB", s.commits, piece / 1024, committed / MIB);

    // Every page of the committed area written once, so DONTNEED has something to drop.
    let pages = committed / PAGE;
    for p in 0..pages {
        pool.set(p * PAGE, (p as u8) | 1);
    }
    let mut rng = Rng(0xc0ba17);
    let mut zero_bad = 0;
    for _ in 0..s.dontneeds {
        let n = 1 + rng.below(5); // 4-20 KiB
        let at = rng.below(pages.saturating_sub(n).max(1));
        match pool.advise(at * PAGE, n * PAGE, MADV_DONTNEED) {
            Ok(_) => {
                if pool.get(at * PAGE) != Some(0) {
                    zero_bad += 1;
                }
                pool.set(at * PAGE, (at as u8) | 1);
            }
            Err(e) => fail(&format!("madvise(DONTNEED) {n} pages: {}", e.name())),
        }
        done_steps += 1;
        pace(start, seconds, done_steps, steps);
    }
    if zero_bad > 0 {
        fail(&format!("{zero_bad} DONTNEED pieces did not read zero"));
    }
    println!("madvise(DONTNEED) {} calls of 4-20 KiB: each piece reads zero after", s.dontneeds);
    let big = (s.big_dontneed_mib * MIB).min(committed);
    pool.advise(0, big, MADV_DONTNEED).unwrap_or_else(|e| fail(&format!("madvise(DONTNEED) {} MiB: {}", big / MIB, e.name())));
    if pool.get(0) != Some(0) || pool.get(big - PAGE) != Some(0) {
        fail("the big DONTNEED did not zero its range");
    }
    println!("madvise(DONTNEED) one call of {} MiB: zero", big / MIB);
    match pool.advise(committed - PAGE, PAGE, MADV_18) {
        Ok(_) => println!("madvise(18) once: ok"),
        Err(e) => println!("madvise(18) once: {}", e.name()),
    }

    // Shared memory: files in /dev/shm, two views each, a window of live files.
    let mut live: std::collections::VecDeque<ShmFile> = std::collections::VecDeque::new();
    let (mut maps, mut unmaps, mut files, mut bytes) = (0usize, 0usize, 0usize, 0usize);
    while maps < s.shm_maps {
        let len = (4 + rng.below(61)) * PAGE; // 16-256 KiB
        let f = match shm_file(files, len) {
            Ok(f) => f,
            Err(e) if e.contains("Cannot allocate memory") || e.contains("ENOMEM") => {
                enomem += 1;
                break;
            }
            Err(e) => fail(&format!("shm file {files}: {e}")),
        };
        let stamp = [(files as u8) | 1; 64];
        f.writer.write_at(len - 64, &stamp);
        let mut back = [0u8; 64];
        f.reader.read_at(len - 64, &mut back);
        if back != stamp {
            fail(&format!("shm file {files}: the reader view does not see the writer's bytes"));
        }
        files += 1;
        maps += 2;
        bytes += len;
        live.push_back(f);
        if live.len() > SHM_LIVE {
            drop(live.pop_front());
            unmaps += 2;
        }
        done_steps += 2;
        pace(start, seconds, done_steps, steps);
    }
    unmaps += live.len() * 2;
    drop(live);
    println!("shm: {maps} mmaps (MAP_SHARED) of {files} /dev/shm files, writer and reader views agree, {unmaps} munmaps");
    eprintln!("memstress: shm distinct {} MiB", bytes / MIB);
    drop(res);
    println!("enomem: {enomem}");
    let secs = (usys::mono_ns() - start) as f64 / 1e9;
    eprintln!("memstress: replay {} in {secs:.1} s: {:.0} DONTNEED/s, {:.0} commits/s", if full { "full" } else { "small" }, s.dontneeds as f64 / secs, s.commits as f64 / secs);
    if enomem == 0 {
        println!("replay: ok");
        0
    } else {
        println!("replay: ENOMEM seen");
        1
    }
}

//! memstress: allocation, commit and touch patterns, and the Y-NUMBERS replay (item UL-4b).
//!
//! - `memstress touch [--mb N] [--rounds R]`: N MiB (default 16) written page by page in four orders
//!   (forward, backward, stride, shuffled) R times (default 2), read back; MADV_DONTNEED of the odd pages and
//!   then of all: they read zero, the rest keeps its bytes.
//! - `memstress commit [--mb N] [--step-mb S]`: a PROT_NONE MAP_NORESERVE reserve of N MiB (default 64) made
//!   writable by mprotect in S MiB steps (default 4) until all of it or ENOMEM (V2's strict commit,
//!   linux-6.6/mm/util.c:931 `__vm_enough_memory` mode 2), each step touched; then given back by munmap and
//!   done once more, which must reach the same amount (nothing leaked). ENOMEM comes as an errno, never as a
//!   signal.
//!   `--mmap`: each step is a fresh mmap(MAP_PRIVATE, PROT_READ|PROT_WRITE) instead (V2 charges at mmap too).
//! - `memstress rules`: the errno rules of file and memfd mappings (rules.rs).
//! - `memstress truncate`: a store past the end of a shrunk shared memfd map must be SIGBUS (rules.rs).
//! - `memstress replay [--full] [--seconds S]`: libcobalt's memory pattern (Y-NUMBERS 1.1): 70.4 GB reserved in
//!   9 ranges (2 x 32 GB, 4 GB, 4 x 512 MB, 256 MB, 128 MB), 2,250 mprotect(RW) commits, MADV_DONTNEED of
//!   4-20 KiB pieces plus one big call, madvise(18) once, /dev/shm files mapped MAP_SHARED twice each (writer
//!   and reader view) and unmapped. Default sizes are small (64 MiB committed, 2,000 DONTNEEDs, 200 shm maps)
//!   for QEMU and the CachyOS oracle; `--full` uses Y-NUMBERS' (770 MiB, 26,400 DONTNEEDs, 1,300 shm maps).
//!   `--seconds S` spreads the work over S seconds (`--full --seconds 330` = the 5.5-minute rate).
//! stdout holds no addresses or times (CachyOS's transcript is the expected one); timing goes to stderr.
//! Exit codes: 0 ok, 1 a check failed, 2 usage.
#![forbid(unsafe_code)]

mod replay;
mod rules;

use std::process::exit;
use usys::{MAP_NORESERVE, Mapping, PROT_NONE, PROT_READ, PROT_WRITE};

pub const PAGE: usize = 4096;
pub const MIB: usize = 1 << 20;
pub const MADV_DONTNEED: usize = 4;
/// CachyOS runs transparent huge pages "always": without this a first touch may map 2 MiB (mm5b.c does the
/// same), which changes nothing in the transcript but much in the RSS.
pub const MADV_NOHUGEPAGE: usize = 15;
pub const ENOMEM: i32 = 12;

const USAGE: &str = "usage: memstress touch [--mb N] [--rounds R] | commit [--mb N] [--step-mb S] [--mmap] | rules | truncate | replay [--full] [--seconds S]\n";

fn usage(msg: &str) -> ! {
    eprint!("memstress: {msg}\n{USAGE}");
    exit(2)
}

fn help() -> ! {
    print!(
        "{USAGE}\n  touch    write/read N MiB (default 16) in four page orders, then MADV_DONTNEED checks\n\
         \x20 commit   mprotect(RW) a reserve of N MiB (default 64) in S MiB steps (default 4) until done or ENOMEM,\n\
         \x20          give it back and do it again (must reach the same amount); --mmap: fresh private RW maps\n\
         \x20 rules    errno rules of file and memfd maps (EACCES, seals, mremap, munmap holes)\n\
         \x20 truncate a store past the end of a shrunk shared memfd map: SIGBUS (exit 0) or not (exit 1)\n\
         \x20 replay   libcobalt's pattern (Y-NUMBERS 1.1): 70.4 GB reserved, 2,250 commits, DONTNEED pieces,\n\
         \x20          /dev/shm shared maps; small by default, --full for the real sizes, --seconds S for its rate\n\
         exit codes: 0 ok, 1 a check failed, 2 usage\n"
    );
    exit(0)
}

/// A small fixed-seed generator (xorshift64*) so both systems do the same work.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

pub fn fail(what: &str) -> ! {
    println!("FAIL {what}");
    exit(1)
}

fn mark(i: usize, round: usize) -> u8 {
    (i.wrapping_mul(31) ^ round.wrapping_mul(7)) as u8 | 1
}

fn orders(pages: usize, rng: &mut Rng) -> [(&'static str, Vec<usize>); 4] {
    let mut shuffled: Vec<usize> = (0..pages).collect();
    for i in (1..pages).rev() {
        shuffled.swap(i, rng.below(i + 1));
    }
    // A stride coprime with the page count, so the stride order visits every page once.
    let mut stride = 2_654_435_761usize % pages.max(1) | 1;
    while gcd(stride, pages.max(1)) != 1 {
        stride += 2;
    }
    [
        ("forward", (0..pages).collect()),
        ("backward", (0..pages).rev().collect()),
        ("stride", (0..pages).map(|i| i * stride % pages).collect()),
        ("shuffled", shuffled),
    ]
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn touch(mb: usize, rounds: usize) {
    let len = mb * MIB;
    let pages = len / PAGE;
    let m = Mapping::anon(len, PROT_READ | PROT_WRITE, 0).unwrap_or_else(|e| fail(&format!("mmap {mb} MiB: {}", e.name())));
    let _ = m.advise(0, len, MADV_NOHUGEPAGE);
    let mut rng = Rng(0x5eed);
    for r in 0..rounds {
        for (k, (name, order)) in orders(pages, &mut rng).into_iter().enumerate() {
            let round = r * 4 + k;
            for &i in &order {
                m.set(i * PAGE, mark(i, round));
                m.set(i * PAGE + PAGE - 1, !mark(i, round));
            }
            if let Some(&i) = order.iter().find(|&&i| m.get(i * PAGE) != Some(mark(i, round)) || m.get(i * PAGE + PAGE - 1) != Some(!mark(i, round))) {
                fail(&format!("round {r} {name}: page {i} reads back wrong"));
            }
        }
    }
    println!("touch {mb} MiB ({pages} pages), 4 orders x {rounds} rounds: written and read back");
    let last = rounds * 4 - 1;
    for i in (1..pages).step_by(2) {
        if let Err(e) = m.advise(i * PAGE, PAGE, MADV_DONTNEED) {
            fail(&format!("madvise(DONTNEED) page {i}: {}", e.name()));
        }
    }
    let bad = (0..pages).find(|&i| {
        let want = if i % 2 == 1 { 0 } else { mark(i, last) };
        m.get(i * PAGE) != Some(want)
    });
    match bad {
        None => println!("dontneed odd pages: they read zero, the even ones kept their bytes"),
        Some(i) => fail(&format!("after DONTNEED of the odd pages, page {i} reads {:?}", m.get(i * PAGE))),
    }
    m.advise(0, len, MADV_DONTNEED).unwrap_or_else(|e| fail(&format!("madvise(DONTNEED) all: {}", e.name())));
    match (0..pages).find(|&i| m.get(i * PAGE) != Some(0) || m.get(i * PAGE + PAGE - 1) != Some(0)) {
        None => println!("dontneed all: zero"),
        Some(i) => fail(&format!("page {i} not zero after DONTNEED of all")),
    }
    println!("touch: ok");
}

/// Commits `mb` MiB in `step` MiB pieces of fresh private RW mappings; (MiB committed, stopped by ENOMEM).
fn commit_mmap_once(mb: usize, step: usize) -> (usize, bool) {
    let mut maps = Vec::new();
    let mut done = 0;
    while done < mb {
        let n = step.min(mb - done);
        let m = match Mapping::anon(n * MIB, PROT_READ | PROT_WRITE, 0) {
            Ok(m) => m,
            Err(e) if e.0 == ENOMEM => return (done, true),
            Err(e) => fail(&format!("mmap(MAP_PRIVATE, RW) at {done} MiB: {}", e.name())),
        };
        let _ = m.advise(0, n * MIB, MADV_NOHUGEPAGE);
        m.set(0, 1);
        m.set(n * MIB - 1, 2);
        if m.get(0) != Some(1) || m.get(n * MIB - 1) != Some(2) {
            fail(&format!("the map at {done} MiB does not keep its bytes"));
        }
        maps.push(m);
        done += n;
    }
    (done, false)
}

/// Commits `mb` MiB of a fresh reserve in `step` MiB pieces; (MiB committed, stopped by ENOMEM).
fn commit_once(mb: usize, step: usize, by_mmap: bool) -> (usize, bool) {
    if by_mmap {
        return commit_mmap_once(mb, step);
    }
    let len = mb.div_ceil(step) * step * MIB;
    let m = Mapping::anon(len, PROT_NONE, MAP_NORESERVE).unwrap_or_else(|e| fail(&format!("reserve {mb} MiB: {}", e.name())));
    let mut done = 0;
    while done < mb {
        let n = step.min(mb - done);
        match m.protect(done * MIB, n * MIB, PROT_READ | PROT_WRITE) {
            Ok(_) => {}
            Err(e) if e.0 == ENOMEM => return (done, true),
            Err(e) => fail(&format!("mprotect(RW) at {done} MiB: {}", e.name())),
        }
        let _ = m.advise(done * MIB, n * MIB, MADV_NOHUGEPAGE);
        m.set(done * MIB, 1);
        m.set((done + n) * MIB - 1, 2);
        if m.get(done * MIB) != Some(1) || m.get((done + n) * MIB - 1) != Some(2) {
            fail(&format!("the step at {done} MiB does not keep its bytes"));
        }
        done += n;
    }
    (done, false)
}

fn commit(mb: usize, step: usize, by_mmap: bool) {
    let how = if by_mmap { " (mmap)" } else { "" };
    let (first, enomem) = commit_once(mb, step, by_mmap);
    let stop = if enomem { "ENOMEM" } else { "limit" };
    if enomem {
        println!("commit: asked {mb} MiB in {step} MiB steps{how}: stopped by ENOMEM (an errno, no signal)");
        eprintln!("memstress: committed {first} MiB before ENOMEM");
    } else {
        println!("commit: asked {mb} MiB in {step} MiB steps{how}: committed {first} MiB, stop={stop}");
    }
    let (second, enomem2) = commit_once(mb, step, by_mmap);
    eprintln!("memstress: again {second} MiB (ENOMEM {enomem2})");
    // Other programs may take or give back memory meanwhile: two steps of slack when the limit was ENOMEM.
    if enomem2 == enomem && second + 2 * step >= first && (enomem || second == first) {
        println!("again after munmap: the same amount, nothing leaked");
        println!("commit: ok");
    } else {
        fail(&format!("again after munmap: {second} MiB (ENOMEM {enomem2}), first {first} MiB (ENOMEM {enomem})"));
    }
}

fn main() {
    let mut it = std::env::args().skip(1);
    let mode = it.next().unwrap_or_else(|| usage("which mode?"));
    if mode == "--help" || mode == "-h" {
        help();
    }
    let (mut mb, mut rounds, mut step, mut full, mut seconds, mut by_mmap) = (None, 2usize, 4usize, false, 0u64, false);
    while let Some(a) = it.next() {
        let mut n = |what: &str| -> usize {
            it.next().and_then(|v| v.parse().ok()).filter(|v| *v > 0 || what == "--seconds").unwrap_or_else(|| usage(&format!("{what} wants a number")))
        };
        match a.as_str() {
            "--help" | "-h" => help(),
            "--mb" => mb = Some(n("--mb")),
            "--rounds" => rounds = n("--rounds"),
            "--step-mb" => step = n("--step-mb"),
            "--seconds" => seconds = n("--seconds") as u64,
            "--full" => full = true,
            "--mmap" => by_mmap = true,
            _ => usage(&format!("unknown argument {a}")),
        }
    }
    // Sizes in bytes and nanoseconds must fit (a wrapped size or deadline would test something else silently).
    if mb.is_some_and(|m| m.div_ceil(step).checked_mul(step).and_then(|v| v.checked_mul(MIB)).is_none()) {
        usage("--mb/--step-mb too large");
    }
    if seconds.checked_mul(1_000_000_000).is_none() {
        usage("--seconds too large");
    }
    match mode.as_str() {
        "touch" => touch(mb.unwrap_or(16), rounds),
        "commit" => commit(mb.unwrap_or(64), step, by_mmap),
        "rules" => exit(rules::run()),
        "truncate" => exit(rules::truncate()),
        "truncate-child" => exit(rules::sigbus_child()),
        "replay" => exit(replay::run(full, seconds)),
        m => usage(&format!("unknown mode {m}")),
    }
}

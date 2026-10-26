//! stress: counting threads (item UL-2).
//!
//! `stress [-t THREADS] [-s SECONDS] [-q]`: starts THREADS workers (default: the CPUs in the affinity mask), each
//! counting as fast as it can for SECONDS (default 2) and noting which CPUs it ran on (rdtscp's TSC_AUX, which
//! Linux and BitsOS both fill with the CPU number). Prints one line per worker and a summary:
//!
//! ```text
//! stress: worker 3 count=812345678 cpus=3
//! stress: 16 workers (threads), 2 s, total=12998765432, distinct cpus=16 [0-15]
//! ```
//!
//! Without threads (a kernel that answers clone with ENOSYS: BitsOS until AB-4) the first spawn fails; stress
//! then says so and counts in its one thread, so N copies started side by side (the `ul.stress.procs` ktest)
//! show the same thing per process. Exit 0 when every worker counted, 1 otherwise.
#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct Result {
    count: u64,
    cpus: BTreeSet<u32>,
}

fn work(stop: &AtomicBool, deadline_ns: u64) -> Result {
    let mut count = 0u64;
    let mut cpus = BTreeSet::new();
    loop {
        // A stretch of pure counting between looks at the clock (each look is a syscall without a vDSO).
        for _ in 0..(1u32 << 16) {
            count = std::hint::black_box(count.wrapping_add(1));
        }
        cpus.insert(usys::rdtscp_cpu());
        if stop.load(Ordering::Relaxed) || usys::mono_ns() >= deadline_ns {
            break;
        }
    }
    Result { count, cpus }
}

fn ranges(set: &BTreeSet<u32>) -> String {
    let mut out: Vec<String> = Vec::new();
    let v: Vec<u32> = set.iter().copied().collect();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        out.push(if i == j { v[i].to_string() } else { format!("{}-{}", v[i], v[j]) });
        i = j + 1;
    }
    out.join(",")
}

fn usage() -> ! {
    eprintln!("usage: stress [-t THREADS] [-s SECONDS] [-q]");
    std::process::exit(2)
}

fn main() {
    let mut threads: Option<usize> = None;
    let mut secs: u64 = 2;
    let mut quiet = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-t" => threads = Some(args.next().and_then(|v| v.parse().ok()).filter(|n| *n >= 1).unwrap_or_else(|| usage())),
            "-s" => secs = args.next().and_then(|v| v.parse().ok()).filter(|n| (1..=3600).contains(n)).unwrap_or_else(|| usage()),
            "-q" => quiet = true,
            _ => usage(),
        }
    }
    // Default: the CPUs in the affinity mask; without sched_getaffinity (BitsOS until AB-5) the processor blocks
    // of /proc/cpuinfo.
    let n = threads.unwrap_or_else(|| {
        let mask = usys::sched_getaffinity(0, 512).unwrap_or_default();
        let from_mask: usize = mask.iter().map(|b| b.count_ones() as usize).sum();
        if from_mask > 0 {
            return from_mask;
        }
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        cpuinfo.lines().filter(|l| l.starts_with("processor")).count().max(1)
    });
    let deadline = usys::mono_ns().saturating_add(secs * 1_000_000_000);
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    let mut spawn_error = None;
    for i in 1..n {
        let s = stop.clone();
        match std::thread::Builder::new().name(format!("stress-{i}")).stack_size(64 * 1024).spawn(move || work(&s, deadline)) {
            Ok(h) => handles.push(h),
            Err(e) => {
                spawn_error = Some(e);
                break;
            }
        }
    }
    let mode = match &spawn_error {
        None => "threads",
        Some(e) => {
            println!("stress: thread {} of {n} could not start ({e}); counting in {} thread(s)", handles.len() + 1, handles.len() + 1);
            "threads unavailable"
        }
    };
    let mut results = vec![work(&stop, deadline)];
    stop.store(true, Ordering::Relaxed);
    let mut lost = 0;
    for h in handles {
        match h.join() {
            Ok(r) => results.push(r),
            Err(_) => lost += 1,
        }
    }
    let mut all = BTreeSet::new();
    let mut total = 0u64;
    for (i, r) in results.iter().enumerate() {
        all.extend(r.cpus.iter().copied());
        total = total.wrapping_add(r.count);
        if !quiet {
            println!("stress: worker {i} count={} cpus={}", r.count, ranges(&r.cpus));
        }
    }
    println!(
        "stress: {} workers ({mode}), {secs} s, total={total}, distinct cpus={} [{}]",
        results.len(),
        all.len(),
        ranges(&all)
    );
    let ok = lost == 0 && results.iter().all(|r| r.count > 0);
    std::process::exit(if ok { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_ranges() {
        let s: BTreeSet<u32> = [0, 1, 2, 3, 5, 7, 8].into_iter().collect();
        assert_eq!(ranges(&s), "0-3,5,7-8");
        assert_eq!(ranges(&BTreeSet::new()), "");
    }

    #[test]
    fn counts_until_the_deadline() {
        let stop = AtomicBool::new(false);
        let r = work(&stop, usys::mono_ns() + 20_000_000);
        assert!(r.count > 0 && !r.cpus.is_empty());
    }
}

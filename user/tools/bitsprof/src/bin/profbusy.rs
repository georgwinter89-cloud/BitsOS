//! profbusy: a CPU-bound program with one hot function, `profbusy_hot_loop`, for the profiler's scenario and its
//! overhead measurement. `profbusy SECONDS [THREADS]` spins that long on that many threads and prints the work done
//! (`rounds=<n> seconds=<s> rate=<rounds per second>`), so runs with and without the profiler can be compared.
#![forbid(unsafe_code)]

use std::hint::black_box;
use std::time::{Duration, Instant};

#[inline(never)]
pub fn profbusy_hot_loop(seed: u64) -> u64 {
    let mut x = seed;
    for i in 0..20_000u64 {
        x = black_box(x.wrapping_mul(6364136223846793005).wrapping_add(i) ^ (x >> 29));
    }
    x
}

fn main() {
    let mut a = std::env::args().skip(1);
    let secs: f64 = a.next().and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let threads: usize = a.next().and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
    let until = Duration::from_secs_f64(secs);
    let start = Instant::now();
    let workers: Vec<_> = (0..threads)
        .map(|t| {
            std::thread::spawn(move || {
                let (mut n, mut x) = (0u64, t as u64);
                while start.elapsed() < until {
                    x = profbusy_hot_loop(x);
                    n += 1;
                }
                black_box(x);
                n
            })
        })
        .collect();
    let rounds: u64 = workers.into_iter().map(|w| w.join().unwrap_or(0)).sum();
    let s = start.elapsed().as_secs_f64();
    println!("rounds={rounds} seconds={s:.3} rate={:.1}", rounds as f64 / s);
}

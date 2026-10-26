//! IRQ storm sampler (RB-7a): every `PERIOD_NS` klogd reads each CPU's per-vector interrupt counts (hal counts
//! them anyway, so the interrupt path pays nothing more) and, where a CPU takes more than
//! `trace::Limits::irq_per_s`, writes a trace entry into that CPU's ring. The log gets one line when a storm
//! starts and one when it ends, never one per sample.

use alloc::boxed::Box;
use alloc::vec::Vec;
use hal::vectors::{Vector, count};

pub const PERIOD_NS: u64 = 100_000_000;
/// First vector hal hands out (below are the CPU exceptions).
const FIRST_VECTOR: usize = 0x20;

struct Cpu {
    counts: Box<[u64; 256]>,
    /// Storm running since (ns), and its highest rate.
    storm: Option<(u64, u64)>,
}

#[derive(Default)]
pub struct Sampler {
    cpus: Vec<Cpu>,
    last_ns: u64,
}

/// The vector with the most new interrupts and the total of new interrupts between two snapshots.
pub fn busiest(before: &[u64; 256], now: &[u64; 256]) -> (u8, u64) {
    let mut top = (0u8, 0u64);
    let mut total = 0u64;
    for v in FIRST_VECTOR..256 {
        let d = now[v].saturating_sub(before[v]);
        total += d;
        if d > top.1 {
            top = (v as u8, d);
        }
    }
    (top.0, total)
}

pub fn per_s(delta: u64, dt_ns: u64) -> u64 {
    ((delta as u128 * 1_000_000_000) / dt_ns.max(1) as u128).min(u64::MAX as u128) as u64
}

/// What one look found on one CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// A storm began: trace entry and one Warn line.
    Start { vector: u8, per_s: u64 },
    /// The storm goes on: trace entry only.
    Goes { vector: u8, per_s: u64 },
    /// The storm ended: one Info line.
    End { ms: u64, max_per_s: u64 },
}

impl Sampler {
    /// One look if `PERIOD_NS` passed since the last (klogd's loop calls it every round).
    pub fn step(&mut self, now: u64) {
        let limit = klog::trace::limits().irq_per_s;
        let read = |c: usize, v: u8| count(Vector { cpu: c as u32, vector: v });
        self.step_with(now, hal::percpu::cpu_count(), limit, read, |c, ch| match ch {
            Change::Start { vector, per_s } => {
                klog::trace::irq_storm(c, vector, per_s);
                klog::event!(Warn, "irq", "storm.start", cpu = c, vector = vector, per_s = per_s, limit = limit);
            }
            Change::Goes { vector, per_s } => klog::trace::irq_storm(c, vector, per_s),
            Change::End { ms, max_per_s } => klog::event!(Info, "irq", "storm.end", cpu = c, ms = ms, max_per_s = max_per_s),
        });
    }

    /// `step` over any count source (`read(cpu, vector)`), handing each change to `on` (host tests).
    pub fn step_with(&mut self, now: u64, cpus: usize, limit: u64, read: impl Fn(usize, u8) -> u64, mut on: impl FnMut(usize, Change)) {
        if now.saturating_sub(self.last_ns) < PERIOD_NS {
            return;
        }
        let dt = now - self.last_ns;
        let first = self.last_ns == 0;
        self.last_ns = now;
        for c in 0..cpus {
            let new = c >= self.cpus.len();
            if new {
                self.cpus.push(Cpu { counts: Box::new([0; 256]), storm: None });
            }
            let mut fresh = [0u64; 256];
            for (v, n) in fresh.iter_mut().enumerate().skip(FIRST_VECTOR) {
                *n = read(c, v as u8);
            }
            let st = &mut self.cpus[c];
            let (vector, delta) = busiest(&st.counts, &fresh);
            *st.counts = fresh;
            if first || new {
                continue;
            }
            let rate = per_s(delta, dt);
            match (&mut st.storm, rate >= limit) {
                (None, true) => {
                    st.storm = Some((now, rate));
                    on(c, Change::Start { vector, per_s: rate });
                }
                (Some((_, max)), true) => {
                    *max = (*max).max(rate);
                    on(c, Change::Goes { vector, per_s: rate });
                }
                (Some((since, max)), false) => {
                    on(c, Change::End { ms: (now - *since) / 1_000_000, max_per_s: *max });
                    st.storm = None;
                }
                (None, false) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_busiest_vector_and_the_rate() {
        let mut a = [0u64; 256];
        let mut b = [0u64; 256];
        a[0x41] = 100;
        b[0x41] = 5100;
        b[0x30] = 10;
        b[0x05] = 1_000_000; // exceptions do not count
        assert_eq!(busiest(&a, &b), (0x41, 5010));
        assert_eq!(per_s(5000, 100_000_000), 50_000);
        assert_eq!(per_s(1, 0), 1_000_000_000);
    }

    /// Quiet, a storm of three looks on CPU 1, quiet again: one start, two continuations, one end; CPU 0 that
    /// stays quiet and the first look (no earlier counts) give nothing.
    #[test]
    fn a_storm_starts_goes_on_and_ends_once() {
        use core::cell::Cell;
        let total = Cell::new(0u64);
        let mut s = Sampler::default();
        let mut got = Vec::new();
        let limit = 50_000;
        // CPU 1 vector 0x41 takes `per_look` interrupts per 100 ms look; CPU 0 a few on 0x30.
        let look = |s: &mut Sampler, n: u64, at: u64, got: &mut Vec<(usize, Change)>| {
            total.set(total.get() + n);
            let t = total.get();
            s.step_with(at, 2, limit, |c, v| match (c, v) {
                (1, 0x41) => t,
                (0, 0x30) => at / PERIOD_NS,
                _ => 0,
            }, |c, ch| got.push((c, ch)));
        };
        let p = PERIOD_NS;
        look(&mut s, 100, p, &mut got); // first look: nothing to compare with
        look(&mut s, 100, 2 * p, &mut got); // 1000/s: quiet
        look(&mut s, 10_000, 3 * p, &mut got); // 100 000/s: starts
        look(&mut s, 20_000, 4 * p, &mut got);
        look(&mut s, 6_000, 5 * p, &mut got); // 60 000/s: still above
        look(&mut s, 50, 5 * p + p / 2, &mut got); // too soon: no look
        look(&mut s, 50, 6 * p, &mut got); // quiet again: ends
        look(&mut s, 50, 7 * p, &mut got);
        assert_eq!(
            got,
            [
                (1, Change::Start { vector: 0x41, per_s: 100_000 }),
                (1, Change::Goes { vector: 0x41, per_s: 200_000 }),
                (1, Change::Goes { vector: 0x41, per_s: 60_000 }),
                (1, Change::End { ms: 300, max_per_s: 200_000 }),
            ]
        );
    }
}

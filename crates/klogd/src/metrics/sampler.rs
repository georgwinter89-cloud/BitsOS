//! One sampling round: raw readings (mostly running totals) in, values per period out. Kept apart from the
//! kernel sources (`sources.rs`) so host tests and ktests can feed it made-up readings.

use super::store::{Store, Unit};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Default)]
pub struct CpuRaw {
    pub cpu: usize,
    pub busy_ns: u64,
    pub idle_ns: u64,
    pub user_ns: u64,
}

#[derive(Clone, Debug)]
pub struct ProcRaw {
    pub pid: u32,
    pub name: String,
    pub cpu_ns: u64,
}

#[derive(Clone, Debug)]
pub struct NetRaw {
    pub name: String,
    /// rx_bytes, tx_bytes, rx_frames, tx_frames, rx_dropped, tx_dropped (running totals).
    pub totals: [u64; 6],
}

#[derive(Clone, Debug)]
pub struct DiskRaw {
    pub name: String,
    /// Running totals: requests, dispatches.
    pub requests: u64,
    pub dispatches: u64,
    /// Tallies shown as they are: errors, timeouts, and the level `queued`.
    pub errors: u64,
    pub timeouts: u64,
    pub queued: u64,
    /// Running totals: bytes read and written, time with a request at the device (ns).
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub busy_ns: u64,
}

/// Everything one round read. Missing sources stay empty / `None` (no series then, never a made-up 0).
#[derive(Clone, Debug, Default)]
pub struct Reading {
    pub now_ns: u64,
    pub cpus: Vec<CpuRaw>,
    pub procs: Vec<ProcRaw>,
    /// total, free, then used bytes per frame kind (`mm_core::Kind` order).
    pub mem: Option<(u64, u64, [u64; 8])>,
    pub nets: Vec<NetRaw>,
    pub disks: Vec<DiskRaw>,
    pub counters: Vec<(&'static str, u64)>,
    /// Name (`tctl`, `ccd0`, ...) and thousandths of a degree.
    pub temps: Vec<(String, u64)>,
    /// Package energy status (32-bit running total) and the energy unit exponent (J = raw / 2^esu).
    pub pkg_energy: Option<(u32, u32)>,
    /// What the previous round cost the sampler (ns).
    pub cost_ns: u64,
}

pub const KIND_NAMES: [&str; 8] = ["heap", "pagetable", "user", "dma", "stack", "pagecache", "driver", "other"];

/// Increase of a running total; a total that went down (driver reload, counter reset) counts from 0.
fn grew(now: u64, before: u64) -> u64 {
    if now >= before { now - before } else { now }
}

/// `part` of `whole` in hundredths of a percent.
fn pct(part: u64, whole: u64) -> u64 {
    if whole == 0 { 0 } else { (part as u128 * 10_000 / whole as u128).min(10_000_000) as u64 }
}

/// Thousandths per second (`milli`) or whole units per second.
fn rate(delta: u64, dt_ns: u64, milli: bool) -> u64 {
    let k: u128 = if milli { 1_000_000_000_000 } else { 1_000_000_000 };
    (delta as u128 * k / dt_ns.max(1) as u128).min(u64::MAX as u128 - 1) as u64
}

/// Names from devices and programs inside a series name: no spaces or controls (one token per line).
pub fn token(s: &str) -> String {
    let t: String = s.chars().take(32).map(|c| if c.is_whitespace() || c.is_control() { '_' } else { c }).collect();
    if t.is_empty() { String::from("_") } else { t }
}

#[derive(Default)]
pub struct Sampler {
    prev_ns: Option<u64>,
    cpus: BTreeMap<usize, CpuRaw>,
    procs: BTreeMap<u32, u64>,
    nets: BTreeMap<String, [u64; 6]>,
    /// requests, dispatches, read bytes, written bytes, busy ns.
    disks: BTreeMap<String, [u64; 5]>,
    energy: Option<u32>,
}

impl Sampler {
    /// Writes one round into `store`. The first round only sets the baselines of the rates.
    pub fn step(&mut self, r: &Reading, store: &mut Store) {
        let dt = self.prev_ns.map(|p| r.now_ns.saturating_sub(p)).filter(|&d| d > 0);
        self.prev_ns = Some(r.now_ns);
        store.begin(r.now_ns);
        self.cpu(r, dt, store);
        self.procs(r, dt, store);
        if let Some((total, free, used)) = r.mem {
            store.put("mem.total", Unit::Bytes, total);
            store.put("mem.free", Unit::Bytes, free);
            for (k, v) in KIND_NAMES.iter().zip(used) {
                store.put(&format!("mem.used.{k}"), Unit::Bytes, v);
            }
        }
        self.nets(r, dt, store);
        self.disks(r, dt, store);
        for (name, v) in &r.counters {
            store.put(name, Unit::Count, *v);
        }
        for (name, mc) in &r.temps {
            store.put(&format!("temp.{name}"), Unit::Celsius, *mc);
        }
        if let Some((raw, esu)) = r.pkg_energy {
            if let (Some(before), Some(dt)) = (self.energy, dt) {
                // 32-bit counter: the wrapping difference is right as long as it wraps at most once per period.
                let units = raw.wrapping_sub(before) as u128;
                let mw = units * 1_000 * 1_000_000_000 / ((1u128 << esu.min(31)) * dt as u128);
                store.put("power.pkg", Unit::Watt, mw.min(u64::MAX as u128 - 1) as u64);
            }
            self.energy = Some(raw);
        }
        if let Some(dt) = dt {
            store.put("metrics.cost", Unit::Pct, pct(r.cost_ns, dt));
        }
        store.end();
    }

    fn cpu(&mut self, r: &Reading, dt: Option<u64>, store: &mut Store) {
        let (mut busy, mut user, mut all) = (0u64, 0u64, 0u64);
        for c in &r.cpus {
            // Scheduler times never reset, but a read from another CPU can come out a few µs behind the
            // previous one (sc.cputime.monotonic_read): such a step is 0, never a "reset" (`grew` would take
            // the whole time since boot), and the baseline keeps the larger value so nothing counts twice.
            let mut base = *c;
            if let Some(p) = self.cpus.get(&c.cpu) {
                (base.busy_ns, base.idle_ns, base.user_ns) = (c.busy_ns.max(p.busy_ns), c.idle_ns.max(p.idle_ns), c.user_ns.max(p.user_ns));
            }
            if let (Some(p), Some(_)) = (self.cpus.get(&c.cpu), dt) {
                let b = c.busy_ns.saturating_sub(p.busy_ns);
                let u = c.user_ns.saturating_sub(p.user_ns).min(b);
                let total = b + c.idle_ns.saturating_sub(p.idle_ns);
                store.put(&format!("cpu.{}.busy", c.cpu), Unit::Pct, pct(b, total));
                store.put(&format!("cpu.{}.user", c.cpu), Unit::Pct, pct(u, total));
                store.put(&format!("cpu.{}.system", c.cpu), Unit::Pct, pct(b - u, total));
                (busy, user, all) = (busy + b, user + u, all + total);
            }
            self.cpus.insert(c.cpu, base);
        }
        if all > 0 {
            store.put("cpu.all.busy", Unit::Pct, pct(busy, all));
            store.put("cpu.all.user", Unit::Pct, pct(user, all));
            store.put("cpu.all.system", Unit::Pct, pct(busy - user, all));
        }
    }

    /// Per program (thread group): share of one CPU, like `top`. A series starts with the first round in
    /// which the program used CPU time, so idle programs cost nothing.
    fn procs(&mut self, r: &Reading, dt: Option<u64>, store: &mut Store) {
        let mut seen = BTreeMap::new();
        for p in &r.procs {
            if let (Some(&before), Some(dt)) = (self.procs.get(&p.pid), dt) {
                let used = p.cpu_ns.saturating_sub(before);
                let name = format!("proc.{}.{}.cpu", p.pid, token(&p.name));
                if used > 0 || store.has(&name) {
                    store.put(&name, Unit::Pct, pct(used, dt));
                }
            }
            seen.insert(p.pid, p.cpu_ns);
        }
        self.procs = seen;
    }

    fn nets(&mut self, r: &Reading, dt: Option<u64>, store: &mut Store) {
        const NAMES: [(&str, bool); 6] =
            [("rx_bytes", false), ("tx_bytes", false), ("rx_frames", true), ("tx_frames", true), ("rx_dropped", true), ("tx_dropped", true)];
        let mut seen = BTreeMap::new();
        for n in &r.nets {
            let dev = token(&n.name);
            if let (Some(before), Some(dt)) = (self.nets.get(&dev), dt) {
                for (i, (what, milli)) in NAMES.iter().enumerate() {
                    let unit = if *milli { Unit::PerS } else { Unit::BytesPerS };
                    store.put(&format!("net.{dev}.{what}"), unit, rate(grew(n.totals[i], before[i]), dt, *milli));
                }
            }
            seen.insert(dev, n.totals);
        }
        self.nets = seen;
    }

    fn disks(&mut self, r: &Reading, dt: Option<u64>, store: &mut Store) {
        let mut seen = BTreeMap::new();
        for d in &r.disks {
            let dev = token(&d.name);
            if let (Some(&[req, dis, rb, wb, busy]), Some(dt)) = (self.disks.get(&dev), dt) {
                store.put(&format!("disk.{dev}.requests"), Unit::PerS, rate(grew(d.requests, req), dt, true));
                store.put(&format!("disk.{dev}.dispatches"), Unit::PerS, rate(grew(d.dispatches, dis), dt, true));
                store.put(&format!("disk.{dev}.read_bytes"), Unit::BytesPerS, rate(grew(d.read_bytes, rb), dt, false));
                store.put(&format!("disk.{dev}.write_bytes"), Unit::BytesPerS, rate(grew(d.write_bytes, wb), dt, false));
                store.put(&format!("disk.{dev}.busy"), Unit::Pct, pct(grew(d.busy_ns, busy), dt).min(10_000));
            }
            store.put(&format!("disk.{dev}.queued"), Unit::Count, d.queued);
            store.put(&format!("disk.{dev}.errors"), Unit::Count, d.errors);
            store.put(&format!("disk.{dev}.timeouts"), Unit::Count, d.timeouts);
            seen.insert(dev, [d.requests, d.dispatches, d.read_bytes, d.write_bytes, d.busy_ns]);
        }
        self.disks = seen;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(s: &Store, name: &str) -> Option<u64> {
        s.series().find(|x| x.0 == name).and_then(|x| x.1.latest())
    }

    fn reading(t: u64, busy: u64, user: u64, idle: u64, prog: u64, rx: u64, energy: u32) -> Reading {
        Reading {
            now_ns: t,
            cpus: alloc::vec![CpuRaw { cpu: 0, busy_ns: busy, idle_ns: idle, user_ns: user }],
            procs: alloc::vec![ProcRaw { pid: 7, name: "bits comp".into(), cpu_ns: prog }, ProcRaw { pid: 8, name: "idle".into(), cpu_ns: 5 }],
            mem: Some((1000, 400, [1, 2, 3, 4, 5, 6, 7, 8])),
            nets: alloc::vec![NetRaw { name: "igc0".into(), totals: [rx, 0, 10, 0, 1, 0] }],
            disks: alloc::vec![DiskRaw { name: "usb0".into(), requests: rx / 1000, dispatches: 0, errors: 2, timeouts: 0, queued: 1, read_bytes: rx, write_bytes: 0, busy_ns: t / 2 }],
            counters: alloc::vec![("proc.faulted", 3)],
            temps: alloc::vec![("tctl".into(), 48_125)],
            pkg_energy: Some((energy, 16)),
            cost_ns: 2_000_000,
        }
    }

    #[test]
    fn first_round_sets_baselines_then_rates_follow() {
        let mut s = Store::new(1 << 20);
        let mut m = Sampler::default();
        m.step(&reading(1_000_000_000, 0, 0, 0, 0, 0, u32::MAX - 65535), &mut s);
        assert_eq!(val(&s, "cpu.0.busy"), None, "no rate from one reading");
        assert_eq!((val(&s, "mem.free"), val(&s, "mem.used.dma"), val(&s, "temp.tctl")), (Some(400), Some(4), Some(48_125)));
        assert_eq!((val(&s, "proc.faulted"), val(&s, "disk.usb0.errors")), (Some(3), Some(2)));
        // 2 s later: 0.5 s busy of 2 s, 0.2 s of it in user mode; the program used 1 s; 4000 bytes, energy +2^17 (2 J).
        m.step(&reading(3_000_000_000, 500_000_000, 200_000_000, 1_500_000_000, 1_000_000_000, 4000, 65536), &mut s);
        assert_eq!((val(&s, "cpu.0.busy"), val(&s, "cpu.0.user"), val(&s, "cpu.0.system")), (Some(2500), Some(1000), Some(1500)));
        assert_eq!(val(&s, "cpu.all.busy"), Some(2500));
        assert_eq!(val(&s, "proc.7.bits_comp.cpu"), Some(5000), "half of one CPU; spaces become _");
        assert!(!s.has("proc.8.idle.cpu"), "a program without CPU time gets no series");
        assert_eq!((val(&s, "net.igc0.rx_bytes"), val(&s, "net.igc0.rx_frames"), val(&s, "net.igc0.rx_dropped")), (Some(2000), Some(0), Some(0)));
        assert_eq!(val(&s, "disk.usb0.requests"), Some(2000), "4 requests in 2 s = 2.000/s");
        assert_eq!(val(&s, "power.pkg"), Some(1000), "2 J in 2 s = 1 W across the 32-bit wrap");
        assert_eq!(val(&s, "metrics.cost"), Some(10), "2 ms of 2 s = 0.1 %");
    }

    #[test]
    fn counters_that_go_back_count_from_zero_and_gone_programs_leave() {
        let mut s = Store::new(1 << 20);
        let mut m = Sampler::default();
        m.step(&reading(1_000_000_000, 0, 0, 0, 0, 9000, 0), &mut s);
        m.step(&reading(2_000_000_000, 0, 0, 0, 1, 500, 0), &mut s);
        assert_eq!(val(&s, "net.igc0.rx_bytes"), Some(500), "a reset counter (driver reload) counts from 0");
        let mut r = reading(3_000_000_000, 0, 0, 0, 1, 500, 0);
        r.procs.clear();
        m.step(&r, &mut s);
        assert!(m.procs.is_empty(), "exited programs leave the baselines");
        assert_eq!(val(&s, "proc.7.bits_comp.cpu"), Some(0), "the series stays until its values age out");
    }

    #[test]
    fn scheduler_times_that_step_back_a_little_are_no_reset() {
        let mut s = Store::new(1 << 20);
        let mut m = Sampler::default();
        let t = |busy: u64, idle: u64| reading(0, busy, busy / 2, idle, 0, 0, 0);
        let mut r = t(100_000_000_000, 100_000_000_000);
        r.now_ns = 1_000_000_000;
        m.step(&r, &mut s);
        // 1 s later: 0.25 s busy, 0.75 s idle.
        let mut r = t(100_250_000_000, 100_750_000_000);
        r.now_ns = 2_000_000_000;
        m.step(&r, &mut s);
        assert_eq!(val(&s, "cpu.0.busy"), Some(2500));
        // busy read from another CPU comes out 2 µs behind: 0 busy this second, not 100 s of it.
        let mut r = t(100_249_998_000, 101_750_000_000);
        r.now_ns = 3_000_000_000;
        m.step(&r, &mut s);
        assert_eq!((val(&s, "cpu.0.busy"), val(&s, "cpu.all.busy")), (Some(0), Some(0)));
        // The 2 µs are not counted twice: the baseline kept the larger reading.
        let mut r = t(100_500_000_000, 102_500_000_000);
        r.now_ns = 4_000_000_000;
        m.step(&r, &mut s);
        assert_eq!(val(&s, "cpu.0.busy"), Some(2500));
        // idle stepping back neither gives 100 % busy.
        let mut r = t(100_750_000_000, 102_499_999_000);
        r.now_ns = 5_000_000_000;
        m.step(&r, &mut s);
        assert_eq!(val(&s, "cpu.0.busy"), Some(10_000), "all of the period busy, idle 0");
        assert!(val(&s, "cpu.0.user").unwrap() <= 10_000);
    }
}

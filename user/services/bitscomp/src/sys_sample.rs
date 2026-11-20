//! SYS-A: the sampler of the program "System", run on its worker thread once a second while a System window is
//! open. The system-wide numbers every time (a dozen small files); the process list only when a shown tab needs it
//! (Überblick's "Was gerade arbeitet", Speicher, Prozesse), the GPU memory of the processes (fdinfo, many files)
//! every fifth second and only on the tab Prozesse; the facts (cpuinfo, DMI, USB, block devices) on the first look
//! and every half minute. Rates and CPU shares are differences to the previous look.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use crate::sys_parse as parse;
use crate::sys_snap::{Cpu, Disk, Facts, Kernel, Memory, Mount, Net, Proc, Snapshot};
use crate::sys_sources::{self as src, Root};

/// What the shown tabs need this second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Want {
    pub procs: bool,
    pub gpu_procs: bool,
    /// A window shows 24 h: the journal is asked every 5 min.
    pub day: bool,
}

/// Seconds on one monotonic clock for every sampler (the history's buckets survive a closed program).
pub fn now_s() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() + 1.0
}

type Statfs = fn(&Root, &str) -> Option<(u64, u64)>;

pub struct Sampler {
    pub root: Root,
    statfs: Statfs,
    prev_t: Option<f64>,
    prev_stat: Option<parse::Stat>,
    prev_net: HashMap<String, (u64, u64)>,
    prev_disk: HashMap<String, (u64, u64)>,
    /// pid -> (start, cpu ticks, disk bytes).
    prev_proc: HashMap<u32, (u64, u64, Option<u64>)>,
    prev_calls: Option<(u64, u64)>,
    prev_ctxt: Option<u64>,
    argv: HashMap<(u32, u64), Vec<String>>,
    /// pid -> (start, GPU bytes, open files), from the last fdinfo round.
    gpu: HashMap<u32, (u64, Option<u64>, Option<u32>)>,
    gpu_at: Option<f64>,
    facts: Facts,
    facts_at: Option<f64>,
    disks: Vec<Disk>,
    node: Option<crate::sys_amdgpu::Node>,
    node_tried: bool,
    hz: u64,
}

/// statfs(2) of a real mount point: (used, total) bytes.
pub fn statfs_real(_: &Root, at: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(at).ok()?;
    // SAFETY: statvfs writes into `s`, a plain struct that outlives the call; `c` is a valid C string.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let f = s.f_frsize as u64;
    Some(((s.f_blocks as u64).saturating_sub(s.f_bfree as u64) * f, s.f_blocks as u64 * f))
}

/// A fixture's `statfs` file: `path used total` lines.
pub fn statfs_fixture(r: &Root, at: &str) -> Option<(u64, u64)> {
    r.read("/statfs")?.lines().find_map(|l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        (w.first() == Some(&at)).then(|| Some((w.get(1)?.parse().ok()?, w.get(2)?.parse().ok()?)))?
    })
}

fn rate(now: u64, before: Option<u64>, dt: f64) -> Option<f64> {
    let b = before?;
    (dt > 0.0).then(|| now.saturating_sub(b) as f64 / dt)
}

impl Sampler {
    pub fn new(root: Root) -> Sampler {
        let statfs: Statfs = if root.is_real() { statfs_real } else { statfs_fixture };
        // SAFETY: sysconf only reads a constant.
        let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok().filter(|h| *h > 0).unwrap_or(100);
        Sampler {
            root,
            statfs,
            prev_t: None,
            prev_stat: None,
            prev_net: HashMap::new(),
            prev_disk: HashMap::new(),
            prev_proc: HashMap::new(),
            prev_calls: None,
            prev_ctxt: None,
            argv: HashMap::new(),
            gpu: HashMap::new(),
            gpu_at: None,
            facts: Facts::default(),
            facts_at: None,
            disks: Vec::new(),
            node: None,
            node_tried: false,
            hz,
        }
    }

    /// One look; `t` is `now_s()`, `wall` unix seconds.
    pub fn sample(&mut self, want: Want, t: f64, wall: i64) -> Snapshot {
        let r = self.root.clone();
        let dt = self.prev_t.map_or(0.0, |p| t - p);
        if self.facts_at.is_none_or(|a| t - a >= 30.0) {
            self.facts = src::facts(&r);
            self.disks = src::disks(&r);
            self.facts_at = Some(t);
        }
        let stat = r.read("/proc/stat").as_deref().and_then(parse::stat);
        let cpu = self.cpu(&r, stat.as_ref(), dt);
        let (mut heat, sclk) = src::hwmon(&r);
        let mut gpu = src::drm(&r);
        gpu.temp = heat.gpu;
        gpu.power = heat.gpu_power;
        gpu.sclk_mhz = sclk;
        if r.is_real() && (gpu.busy.is_none() || gpu.vram.is_none()) {
            if !self.node_tried {
                self.node_tried = true;
                self.node = crate::sys_amdgpu::Node::open();
            }
            gpu = crate::sys_amdgpu::merge(gpu, self.node.as_ref().map(|n| n.read()));
        }
        heat.gpu = heat.gpu.or(gpu.temp);
        heat.gpu_power = heat.gpu_power.or(gpu.power);
        let metrics = r.read("/proc/bitsos/metrics").map(|t| parse::metrics(&t)).unwrap_or_default();
        heat.cpu_power = heat.cpu_power.or_else(|| metric_any(&metrics, &["power.pkg", "power.package", "power.cpu"]));
        let mut snap = Snapshot {
            t: t as u64,
            wall,
            cpu,
            mem: Memory { mem: r.read("/proc/meminfo").as_deref().and_then(parse::meminfo) },
            gpu,
            disks: self.disks(&r, dt),
            mounts: self.mounts(&r),
            nets: self.nets(&r, dt),
            heat,
            kernel: self.kernel(&r, dt, wall, &metrics),
            facts: self.facts.clone(),
            hz: self.hz,
            metrics,
            ..Snapshot::default()
        };
        let n = r.list("/proc").iter().filter(|d| d.bytes().all(|b| b.is_ascii_digit())).count();
        snap.proc_count = (n > 0).then_some(n);
        if want.procs {
            let total = stat.as_ref().map(|s| s.all.total());
            let before = self.prev_stat.as_ref().map(|s| s.all.total());
            snap.procs = self.procs(&r, total.zip(before).map(|(a, b)| a.saturating_sub(b)), dt);
            snap.procs_read = true;
            if want.gpu_procs && self.gpu_at.is_none_or(|a| t - a >= 5.0) {
                self.gpu_round(&r, &snap.procs);
                self.gpu_at = Some(t);
            }
            for p in &mut snap.procs {
                if let Some((start, g, f)) = self.gpu.get(&p.pid).filter(|(s, _, _)| *s == p.start) {
                    let _ = start;
                    p.gpu_mem = *g;
                    p.files = *f;
                }
            }
        } else {
            self.prev_proc.clear();
        }
        if let Some(s) = stat {
            self.prev_ctxt = s.ctxt;
            self.prev_stat = Some(s);
        }
        self.prev_t = Some(t);
        snap
    }

    fn cpu(&mut self, r: &Root, stat: Option<&parse::Stat>, dt: f64) -> Cpu {
        let mut c = Cpu::default();
        let threads = self.facts.threads.unwrap_or(1) as usize;
        (c.cur_khz, c.max_khz) = src::cpufreq(r, threads);
        if let Some((a, b, cc, run, all)) = r.read("/proc/loadavg").as_deref().and_then(parse::loadavg) {
            c.load = Some((a, b, cc));
            c.all_threads = Some(all);
            c.running = Some(run);
        }
        let (Some(s), Some(p)) = (stat, self.prev_stat.as_ref()) else { return c };
        let share = |now: &parse::CpuTimes, was: &parse::CpuTimes, f: fn(&parse::CpuTimes) -> u64| {
            let all = now.total().saturating_sub(was.total());
            (all > 0).then(|| f(now).saturating_sub(f(was)) as f64 * 100.0 / all as f64)
        };
        let busy = |x: &parse::CpuTimes| x.programs() + x.kernel() + x.steal;
        c.busy = share(&s.all, &p.all, busy);
        c.programs = share(&s.all, &p.all, parse::CpuTimes::programs);
        c.kernel = share(&s.all, &p.all, parse::CpuTimes::kernel);
        c.threads = s.cpus.iter().zip(&p.cpus).map(|(n, w)| share(n, w, busy)).collect();
        c.switches = s.ctxt.and_then(|n| rate(n, self.prev_ctxt, dt));
        c
    }

    fn nets(&mut self, r: &Root, dt: f64) -> Vec<Net> {
        let wifi = r.read("/proc/net/wireless").map(|t| parse::wireless(&t)).unwrap_or_default();
        let mut out = Vec::new();
        let mut seen = HashMap::new();
        for (name, rx, tx) in r.read("/proc/net/dev").map(|t| parse::net_dev(&t)).unwrap_or_default() {
            if name == "lo" {
                continue;
            }
            let before = self.prev_net.get(&name).copied();
            let b = format!("/sys/class/net/{name}");
            out.push(Net {
                rx: rate(rx, before.map(|b| b.0), dt),
                tx: rate(tx, before.map(|b| b.1), dt),
                rx_total: rx,
                tx_total: tx,
                up: r.line(&format!("{b}/operstate")).map(|s| s == "up"),
                speed_mbps: r.num::<i64>(&format!("{b}/speed")).filter(|s| *s > 0).map(|s| s as u64),
                wifi: wifi.iter().find(|w| w.0 == name).map(|w| (w.1, w.2)),
                name: name.clone(),
            });
            seen.insert(name, (rx, tx));
        }
        self.prev_net = seen;
        out
    }

    fn disks(&mut self, r: &Root, dt: f64) -> Vec<Disk> {
        let stats = r.read("/proc/diskstats").map(|t| parse::diskstats(&t)).unwrap_or_default();
        let mut out = self.disks.clone();
        let mut seen = HashMap::new();
        for d in &mut out {
            if let Some((_, rd, wr, _)) = stats.iter().find(|s| s.0 == d.name) {
                let (rb, wb) = (rd * 512, wr * 512);
                let before = self.prev_disk.get(&d.name).copied();
                d.read = rate(rb, before.map(|b| b.0), dt);
                d.write = rate(wb, before.map(|b| b.1), dt);
                d.read_total = Some(rb);
                d.write_total = Some(wb);
                seen.insert(d.name.clone(), (rb, wb));
            }
        }
        self.prev_disk = seen;
        out
    }

    fn mounts(&self, r: &Root) -> Vec<Mount> {
        let text = r.read("/proc/mounts").or_else(|| r.read("/proc/self/mounts")).unwrap_or_default();
        src::mounts(&text)
            .into_iter()
            .filter_map(|(dev, at)| {
                let (used, total) = (self.statfs)(r, &at)?;
                (total > 0).then_some(Mount { dev, path: at, used, total })
            })
            .collect()
    }

    fn kernel(&mut self, r: &Root, dt: f64, wall: i64, m: &[(String, f64, String)]) -> Kernel {
        let uptime = r.read("/proc/uptime").as_deref().and_then(parse::uptime);
        let calls = r.read("/proc/bitsos/syscalls").as_deref().and_then(parse::syscalls);
        let k = Kernel {
            generation: r.read("/proc/bitsos/generation").as_deref().and_then(parse::generation),
            boot_ms: r.read("/proc/bitsos/boot").as_deref().and_then(parse::boot_ms),
            boot_wall: uptime.map(|u| wall - u as i64),
            uptime,
            crash: r.read("/proc/bitsos/crash").as_deref().and_then(parse::crash),
            // Lane SO's `crash.programs` counts this start; `crash.kernel.previous_start` is about the start before.
            crashes: metric_any(m, &["crash.since_boot", "crash.count", "crash.programs"])
                .or_else(|| metric_prefix(&m.iter().filter(|(k, _, _)| !k.starts_with("crash.kernel.")).cloned().collect::<Vec<_>>(), "crash.")),
            calls: calls.and_then(|c| rate(c.0, self.prev_calls.map(|p| p.0), dt)),
            refused: calls.zip(self.prev_calls).and_then(|(c, p)| {
                let n = c.0.saturating_sub(p.0);
                (n > 0).then(|| c.1.saturating_sub(p.1) as f64 * 100.0 / n as f64)
            }),
            oom: metric_any(m, &["mm.oom.kills"]).or_else(|| metric_prefix(m, "mm.oom.")),
            reclaim: metric_any(m, &["mm.reclaim.runs"]).or_else(|| metric_prefix(m, "mm.reclaim.")),
        };
        self.prev_calls = calls;
        k
    }

    fn procs(&mut self, r: &Root, all_ticks: Option<u64>, dt: f64) -> Vec<Proc> {
        let mut out = Vec::new();
        let mut seen = HashMap::new();
        for d in r.list("/proc").into_iter().filter(|d| d.bytes().all(|b| b.is_ascii_digit())) {
            let base = format!("/proc/{d}");
            let Some(st) = r.read(&format!("{base}/stat")).as_deref().and_then(parse::pid_stat) else { continue };
            let status = r.read(&format!("{base}/status")).as_deref().and_then(parse::pid_status);
            let io = r.read(&format!("{base}/io")).as_deref().and_then(parse::pid_io).map(|(a, b)| a + b);
            let key = (st.pid, st.start);
            if !self.argv.contains_key(&key) {
                let argv = std::fs::read(r.path(&format!("{base}/cmdline"))).ok().and_then(|b| crate::ending::split_cmdline(&b)).unwrap_or_default();
                self.argv.insert(key, argv);
            }
            let ticks = st.utime + st.stime;
            let before = self.prev_proc.get(&st.pid).filter(|b| b.0 == st.start);
            let cpu = match (before, all_ticks) {
                (Some(b), Some(all)) if all > 0 => Some(ticks.saturating_sub(b.1) as f64 * 100.0 / all as f64),
                _ => None,
            };
            let disk = io.and_then(|now| rate(now, before.and_then(|b| b.2), dt));
            seen.insert(st.pid, (st.start, ticks, io));
            out.push(Proc {
                pid: st.pid,
                ppid: st.ppid,
                pgrp: st.pgrp,
                uid: status.map(|s| s.0),
                argv: self.argv.get(&key).cloned().unwrap_or_default(),
                comm: st.comm,
                state: st.state,
                cpu,
                rss: status.map(|s| s.1).or(Some(st.rss_pages * 4096)),
                shared: status.map(|s| s.2),
                threads: st.threads,
                nice: st.nice,
                start: st.start,
                disk,
                gpu_mem: None,
                files: None,
            });
        }
        self.argv.retain(|k, _| seen.get(&k.0).is_some_and(|s| s.0 == k.1));
        self.prev_proc = seen;
        out
    }

    /// GPU memory and open files per process from fdinfo (readable only for the same user, as Linux's ptrace rule).
    fn gpu_round(&mut self, r: &Root, procs: &[Proc]) {
        let mut budget = 8192usize;
        self.gpu.clear();
        for p in procs {
            let dir = format!("/proc/{}/fdinfo", p.pid);
            let fds = r.list(&dir);
            if fds.is_empty() && !r.exists(&dir) {
                continue;
            }
            let readable = r.read(&format!("{dir}/{}", fds.first().map_or("0", String::as_str))).is_some() || fds.is_empty();
            if !readable {
                continue;
            }
            let mut clients: HashMap<u64, u64> = HashMap::new();
            for fd in fds.iter().take(budget) {
                if let Some((id, b)) = r.read(&format!("{dir}/{fd}")).as_deref().and_then(parse::drm_fdinfo) {
                    clients.insert(id, b);
                }
            }
            budget = budget.saturating_sub(fds.len());
            let gpu = (!clients.is_empty()).then(|| clients.values().sum());
            self.gpu.insert(p.pid, (p.start, gpu, Some(fds.len() as u32)));
            if budget == 0 {
                break;
            }
        }
    }
}

fn metric_any(m: &[(String, f64, String)], names: &[&str]) -> Option<f64> {
    names.iter().find_map(|n| m.iter().find(|(k, _, _)| k == n).map(|(_, v, _)| *v))
}

/// The sum of the counters under `prefix` (a K1 name the kernel side chose otherwise).
fn metric_prefix(m: &[(String, f64, String)], prefix: &str) -> Option<f64> {
    let v: Vec<f64> = m.iter().filter(|(k, _, u)| k.starts_with(prefix) && (u == "n" || u.is_empty())).map(|(_, v, _)| *v).collect();
    (!v.is_empty()).then(|| v.iter().sum())
}

#[cfg(test)]
#[path = "sys_sample_tests.rs"]
mod tests;

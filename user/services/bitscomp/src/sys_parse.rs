//! SYS-A: the text formats the program "System" reads, as pure functions (host tests): /proc/stat, meminfo,
//! loadavg, uptime, /proc/<pid>/{stat,status,io,fdinfo}, /proc/net/dev, /proc/diskstats, /proc/net/wireless, and
//! BitsOS's own /proc/bitsos/{generation,boot,crash,syscalls,metrics}. Layouts as linux-7.3.1/fs/proc and
//! Documentation/filesystems/proc.rst describe them; a line that does not fit is skipped, never guessed.

/// One `cpu` line of /proc/stat in clock ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    pub fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq + self.steal
    }
    /// Work for programs (user + nice).
    pub fn programs(&self) -> u64 {
        self.user + self.nice
    }
    /// Work of the kernel (system + interrupts).
    pub fn kernel(&self) -> u64 {
        self.system + self.irq + self.softirq
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stat {
    pub all: CpuTimes,
    pub cpus: Vec<CpuTimes>,
    pub ctxt: Option<u64>,
    pub btime: Option<u64>,
    pub running: Option<u64>,
}

fn cpu_line(words: &[&str]) -> Option<CpuTimes> {
    let n = |i: usize| words.get(i).and_then(|w| w.parse::<u64>().ok());
    Some(CpuTimes { user: n(1)?, nice: n(2)?, system: n(3)?, idle: n(4)?, iowait: n(5).unwrap_or(0), irq: n(6).unwrap_or(0), softirq: n(7).unwrap_or(0), steal: n(8).unwrap_or(0) })
}

pub fn stat(text: &str) -> Option<Stat> {
    let mut s = Stat::default();
    let mut seen = false;
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let Some(head) = w.first() else { continue };
        let num = || w.get(1).and_then(|v| v.parse::<u64>().ok());
        match *head {
            "cpu" => {
                s.all = cpu_line(&w)?;
                seen = true;
            }
            h if h.starts_with("cpu") && h[3..].bytes().all(|b| b.is_ascii_digit()) => {
                if let Some(c) = cpu_line(&w) {
                    s.cpus.push(c);
                }
            }
            "ctxt" => s.ctxt = num(),
            "btime" => s.btime = num(),
            "procs_running" => s.running = num(),
            _ => {}
        }
    }
    seen.then_some(s)
}

/// /proc/meminfo in bytes (the file says kB, which is KiB).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Mem {
    pub total: u64,
    pub free: u64,
    pub available: u64,
    pub buffers: u64,
    pub cached: u64,
    pub shmem: u64,
    pub reclaimable: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl Mem {
    /// What programs hold: total minus free, buffers and the page cache (as `free` counts "used").
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free + self.buffers + self.cached + self.reclaimable)
    }
    /// The page cache and buffers (what can be given back).
    pub fn cache(&self) -> u64 {
        self.buffers + self.cached + self.reclaimable
    }
}

pub fn meminfo(text: &str) -> Option<Mem> {
    let mut m = Mem::default();
    let mut avail = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let Some(n) = v.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()) else { continue };
        let b = n * 1024;
        match k.trim() {
            "MemTotal" => m.total = b,
            "MemFree" => m.free = b,
            "MemAvailable" => avail = Some(b),
            "Buffers" => m.buffers = b,
            "Cached" => m.cached = b,
            "Shmem" => m.shmem = b,
            "SReclaimable" => m.reclaimable = b,
            "SwapTotal" => m.swap_total = b,
            "SwapFree" => m.swap_free = b,
            _ => {}
        }
    }
    if m.total == 0 {
        return None;
    }
    m.available = avail.unwrap_or(m.free + m.buffers + m.cached);
    Some(m)
}

/// (1, 5, 15 minute load, running, all threads).
pub fn loadavg(text: &str) -> Option<(f64, f64, f64, u64, u64)> {
    let w: Vec<&str> = text.split_whitespace().collect();
    let (run, all) = w.get(3)?.split_once('/')?;
    Some((w.first()?.parse().ok()?, w.get(1)?.parse().ok()?, w.get(2)?.parse().ok()?, run.parse().ok()?, all.parse().ok()?))
}

pub fn uptime(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// The fields of /proc/<pid>/stat the program shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PidStat {
    pub pid: u32,
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    pub pgrp: u32,
    pub utime: u64,
    pub stime: u64,
    pub nice: i32,
    pub threads: u32,
    pub start: u64,
    pub rss_pages: u64,
}

pub fn pid_stat(text: &str) -> Option<PidStat> {
    // The command sits in parentheses and may hold spaces and ')': the last ')' ends it.
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let pid = text[..open].trim().parse().ok()?;
    let comm = text.get(open + 1..close)?.to_string();
    let w: Vec<&str> = text[close + 1..].split_whitespace().collect();
    // w[0] is field 3 (state).
    let f = |n: usize| w.get(n - 3).copied();
    let u = |n: usize| f(n).and_then(|v| v.parse::<u64>().ok());
    Some(PidStat {
        pid,
        comm,
        state: f(3)?.chars().next()?,
        ppid: f(4)?.parse().ok()?,
        pgrp: f(5)?.parse().ok()?,
        utime: u(14)?,
        stime: u(15)?,
        nice: f(19)?.parse().ok()?,
        threads: f(20)?.parse().ok()?,
        start: u(22)?,
        rss_pages: u(24).unwrap_or(0),
    })
}

/// From /proc/<pid>/status: (real uid, resident bytes, of it shared bytes (file and shmem)).
pub fn pid_status(text: &str) -> Option<(u32, u64, u64)> {
    let mut uid = None;
    let (mut rss, mut shared) = (0u64, 0u64);
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let first = v.split_whitespace().next();
        let kb = || first.and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * 1024;
        match k {
            "Uid" => uid = first.and_then(|x| x.parse().ok()),
            "VmRSS" => rss = kb(),
            "RssFile" | "RssShmem" => shared += kb(),
            _ => {}
        }
    }
    Some((uid?, rss, shared))
}

/// (bytes read, bytes written) from /proc/<pid>/io: what reached the disk, else what passed read/write.
pub fn pid_io(text: &str) -> Option<(u64, u64)> {
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix(':')?.trim().parse::<u64>().ok());
    match (get("read_bytes"), get("write_bytes")) {
        (Some(r), Some(w)) => Some((r, w)),
        _ => Some((get("rchar")?, get("wchar")?)),
    }
}

/// A DRM client's memory from one /proc/<pid>/fdinfo/<fd> (Documentation/gpu/drm-usage-stats.rst): its client id
/// and bytes, resident sizes preferred over the older `drm-memory-*` keys. None: not a DRM file.
pub fn drm_fdinfo(text: &str) -> Option<(u64, u64)> {
    let mut id = None;
    let (mut resident, mut memory) = (None::<u64>, None::<u64>);
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        if k == "drm-client-id" {
            id = v.parse().ok();
            continue;
        }
        let bytes = || {
            let mut it = v.split_whitespace();
            let n = it.next()?.parse::<u64>().ok()?;
            Some(match it.next() {
                Some("KiB") => n << 10,
                Some("MiB") => n << 20,
                Some("GiB") => n << 30,
                _ => n,
            })
        };
        if k.starts_with("drm-resident-") {
            *resident.get_or_insert(0) += bytes().unwrap_or(0);
        } else if k.starts_with("drm-memory-") {
            *memory.get_or_insert(0) += bytes().unwrap_or(0);
        }
    }
    Some((id?, resident.or(memory).unwrap_or(0)))
}

/// /proc/net/dev: (interface, received bytes, sent bytes).
pub fn net_dev(text: &str) -> Vec<(String, u64, u64)> {
    text.lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let w: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            (w.len() >= 9).then(|| (name.trim().to_string(), w[0], w[8]))
        })
        .collect()
}

/// /proc/diskstats: (device, sectors read, sectors written, milliseconds busy). Sectors are 512 bytes.
pub fn diskstats(text: &str) -> Vec<(String, u64, u64, u64)> {
    text.lines()
        .filter_map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let n = |i: usize| w.get(i).and_then(|x| x.parse::<u64>().ok());
            Some((w.get(2)?.to_string(), n(5)?, n(9)?, n(12)?))
        })
        .collect()
}

/// /proc/net/wireless: (interface, link quality, signal dBm, noise dBm) per line after the two heads.
pub fn wireless(text: &str) -> Vec<(String, f64, f64, f64)> {
    text.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let w: Vec<f64> = rest.split_whitespace().skip(1).take(3).filter_map(|x| x.trim_end_matches('.').parse().ok()).collect();
            (w.len() == 3).then(|| (name.trim().to_string(), w[0], w[1], w[2]))
        })
        .collect()
}

/// /proc/bitsos/metrics (K1): `name value unit` lines.
pub fn metrics(text: &str) -> Vec<(String, f64, String)> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let name = w.next()?.to_string();
            let v = w.next()?.parse().ok()?;
            Some((name, v, w.next().unwrap_or("").to_string()))
        })
        .collect()
}

/// `key=value` words of a line (the machine lines of /proc/bitsos/*).
pub fn word<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}

/// /proc/bitsos/generation: (number, kernel version).
pub fn generation(text: &str) -> Option<(u64, String)> {
    let l = text.lines().next()?;
    Some((word(l, "generation")?.parse().ok()?, word(l, "version").unwrap_or("").to_string()))
}

/// /proc/bitsos/boot: milliseconds until the last finished step (the start took so long).
pub fn boot_ms(text: &str) -> Option<u64> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next()? == "step").then_some(())?;
            let _id = w.next()?;
            let state = w.next()?;
            (state == "done" || state == "ok").then_some(())?;
            w.next()?.parse::<u64>().ok()
        })
        .max()
}

/// The newest crash record /proc/bitsos/crash names: (wall seconds, generation); None: no record.
pub fn crash(text: &str) -> Option<(i64, u64)> {
    let l = text.lines().next()?;
    if word(l, "source")? == "none" {
        return None;
    }
    Some((word(l, "wall")?.parse().ok()?, word(l, "generation").and_then(|g| g.parse().ok()).unwrap_or(0)))
}

/// /proc/bitsos/syscalls: (all calls, all errors).
pub fn syscalls(text: &str) -> Option<(u64, u64)> {
    let mut any = false;
    let (mut calls, mut errors) = (0u64, 0u64);
    for l in text.lines() {
        if let (Some(c), Some(e)) = (word(l, "calls").and_then(|v| v.parse::<u64>().ok()), word(l, "errors").and_then(|v| v.parse::<u64>().ok())) {
            calls += c;
            errors += e;
            any = true;
        }
    }
    any.then_some((calls, errors))
}

/// The processor's name from /proc/cpuinfo (`model name`), without the "w/ Radeon ..." tail and extra spaces.
pub fn cpu_model(text: &str) -> Option<String> {
    let v = text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "model name").then(|| v.trim().to_string())
    })?;
    let v = v.split(" w/ ").next().unwrap_or(&v);
    let v = v.trim_end_matches(char::is_whitespace).replace("(R)", "").replace("(TM)", "");
    let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
    let v = v.trim_end_matches(" Processor").trim_end_matches(" 8-Core").trim_end_matches(" 16-Core").to_string();
    (!v.is_empty()).then_some(v)
}

/// Cores from /proc/cpuinfo (`cpu cores` of the first processor).
pub fn cpu_cores(text: &str) -> Option<u32> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "cpu cores").then(|| v.trim().parse().ok())?
    })
}

#[cfg(test)]
#[path = "sys_parse_tests.rs"]
mod tests;

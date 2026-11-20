//! `bitsprof run`: one sampling cpu-clock event per CPU (system-wide, pid -1), each with its ring buffer; the
//! rings are drained when the kernel wakes the reader or every 200 ms, until SIGTERM/SIGINT or the time is up.
//! Every 30 s closes a window, every 60 s and at the end `profile.bin` and `meta.json` are written anew
//! (through a temporary file and a rename, so a reader never sees half a file).

use crate::model::Model;
use crate::store::Profile;
use crate::syms::Kernel;
use perf_abi::attr::{self, flag};
use perf_abi::record::{self, Record, Records};
use perf_abi::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use usys::perf::{PerfRing, block_into_signalfd, perf_event_open, perf_ioctl};

pub struct Opts {
    pub out: PathBuf,
    pub hz: u64,
    pub seconds: Option<u64>,
    /// The size cap of profile.bin (oldest windows dropped beyond it).
    pub max_bytes: usize,
    /// Data pages per CPU ring.
    pub pages: usize,
}

const WINDOW: Duration = Duration::from_secs(30);
const SAVE_EVERY: Duration = Duration::from_secs(60);
const SAMPLE_TYPE: u64 = SAMPLE_IP | SAMPLE_TID | SAMPLE_TIME | SAMPLE_CPU | SAMPLE_PERIOD | SAMPLE_CALLCHAIN;
const SIGINT: u64 = 2;
const SIGTERM: u64 = 15;

/// The CPUs online (`/sys/devices/system/cpu/online`, "0-15,18"), else as many as the program may use.
pub fn online_cpus() -> Vec<i32> {
    let text = std::fs::read_to_string("/sys/devices/system/cpu/online").unwrap_or_default();
    let mut v = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(a), Ok(b)) = (a.parse::<i32>(), b.parse::<i32>()) {
            v.extend(a..=b);
        }
    }
    if v.is_empty() {
        v = (0..std::thread::available_parallelism().map_or(1, |n| n.get()) as i32).collect();
    }
    v
}

fn attr_bytes(hz: u64) -> [u8; attr::SIZE] {
    let mut b = [0u8; attr::SIZE];
    b[0..4].copy_from_slice(&PERF_TYPE_SOFTWARE.to_le_bytes());
    b[4..8].copy_from_slice(&(attr::SIZE as u32).to_le_bytes());
    b[8..16].copy_from_slice(&SW_CPU_CLOCK.to_le_bytes());
    b[16..24].copy_from_slice(&hz.to_le_bytes());
    b[24..32].copy_from_slice(&SAMPLE_TYPE.to_le_bytes());
    let f = flag::DISABLED | flag::FREQ | flag::MMAP | flag::MMAP2 | flag::COMM | flag::COMM_EXEC | flag::TASK | flag::SAMPLE_ID_ALL;
    b[40..48].copy_from_slice(&f.to_le_bytes());
    b
}

struct Cpu {
    fd: usize,
    ring: PerfRing,
}

/// What a German reader is told when the kernel refuses the events.
fn refusal(e: usys::Errno) -> String {
    match e.0 {
        13 | 1 => "Keine Berechtigung für die Messung: bitsprof braucht Administratorrechte.".to_string(),
        2 | 95 => "Dieser Kern bietet die Messuhr nicht an.".to_string(),
        _ => format!("Die Messung ließ sich nicht starten ({e})."),
    }
}

pub fn run(o: &Opts) -> Result<(), String> {
    std::fs::create_dir_all(&o.out).map_err(|e| format!("Der Ordner {} lässt sich nicht anlegen: {e}", o.out.display()))?;
    let sig = block_into_signalfd(1 << (SIGTERM - 1) | 1 << (SIGINT - 1)).map_err(|e| format!("Signale: {e}"))?;
    let mut a = attr_bytes(o.hz);
    let mut cpus = Vec::new();
    for c in online_cpus() {
        let fd = match perf_event_open(&mut a, -1, c, -1, FLAG_FD_CLOEXEC as usize) {
            Ok(fd) => fd,
            // A CPU that went offline meanwhile.
            Err(e) if e.0 == 19 => continue,
            Err(e) => return Err(refusal(e)),
        };
        let ring = PerfRing::map(fd, o.pages).map_err(|e| format!("Der Puffer für Prozessor {c} fehlt ({e})."))?;
        cpus.push(Cpu { fd, ring });
    }
    if cpus.is_empty() {
        return Err("Kein Prozessor ließ sich messen.".into());
    }
    let k0 = kernel_cost();
    let kernel = Kernel::load();
    let ksyms = kernel.visible();
    let mut m = Model::new(kernel);
    m.scan_proc();
    let started = Instant::now();
    let wall_start = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    for c in &cpus {
        let _ = perf_ioctl(c.fd, IOC_ENABLE as usize, 0);
    }
    let mut win_start = Duration::ZERO;
    let mut last_save = Instant::now();
    let mut stop = false;
    let mut pfds: Vec<usys::PollFd> = cpus.iter().map(|c| usys::PollFd { fd: c.fd as i32, events: usys::POLLIN, revents: 0 }).collect();
    pfds.push(usys::PollFd { fd: sig as i32, events: usys::POLLIN, revents: 0 });
    while !stop {
        let _ = usys::poll(&mut pfds, 200);
        if pfds.last().is_some_and(|p| p.revents & usys::POLLIN != 0) {
            stop = true;
        }
        if o.seconds.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) {
            stop = true;
        }
        if stop {
            for c in &cpus {
                let _ = perf_ioctl(c.fd, IOC_DISABLE as usize, 0);
            }
        }
        drain(&cpus, &mut m);
        let now = started.elapsed();
        if stop || now - win_start >= WINDOW {
            m.close_window(win_start.as_millis() as u64, (now - win_start).as_millis() as u64);
            win_start = now;
        }
        if stop || last_save.elapsed() >= SAVE_EVERY {
            let k = kernel_cost().zip(k0).map(|(a, b)| (a.0 - b.0, a.1 - b.1));
            save(o, &m.b.p, &Meta { start: wall_start, seconds: now.as_secs_f64(), hz: o.hz, cpus: cpus.len(), samples: m.samples, idle: m.idle, lost: m.lost, ksyms, kernel: k })?;
            last_save = Instant::now();
        }
    }
    for c in cpus {
        drop(c.ring);
        let _ = usys::close(c.fd);
    }
    Ok(())
}

/// Takes what every ring holds: side-band records of all rings first (programs and their code), then the
/// samples, then the programs that ended are forgotten.
fn drain(cpus: &[Cpu], m: &mut Model) {
    let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(cpus.len());
    for c in cpus {
        let (head, tail) = (c.ring.head(), c.ring.tail());
        let n = head.wrapping_sub(tail);
        if n == 0 || n > c.ring.data_size() {
            chunks.push(Vec::new());
            continue;
        }
        let mut b = vec![0u8; n as usize];
        c.ring.copy(tail, &mut b);
        c.ring.set_tail(head);
        chunks.push(b);
    }
    for b in &chunks {
        for r in (Records { bytes: b, at: 0 }) {
            if let Some(rec) = record::parse(r, SAMPLE_TYPE, 0)
                && !matches!(rec, Record::Sample(_))
            {
                m.side_band(&rec);
            }
        }
    }
    for b in &chunks {
        for r in (Records { bytes: b, at: 0 }) {
            if let Some(Record::Sample(s)) = record::parse(r, SAMPLE_TYPE, 0) {
                m.sample(&s);
            }
        }
    }
    m.purge();
}

pub struct Meta {
    pub start: u64,
    pub seconds: f64,
    pub hz: u64,
    pub cpus: usize,
    pub samples: u64,
    pub idle: u64,
    pub lost: u64,
    pub ksyms: bool,
    /// The kernel's sampling interrupts during the run: (count, ns), when it says (BitsOS's /proc/bitsos/perf).
    pub kernel: Option<(u64, u64)>,
}

/// (sampler_ticks, sampler_ns) of `/proc/bitsos/perf`; None on a kernel without it.
fn kernel_cost() -> Option<(u64, u64)> {
    let t = std::fs::read_to_string("/proc/bitsos/perf").ok()?;
    let get = |k: &str| t.lines().find_map(|l| l.strip_prefix(k)?.trim().parse::<u64>().ok());
    Some((get("sampler_ticks ")?, get("sampler_ns ")?))
}

/// The profiler's own CPU time so far (ms; `/proc/self/stat` utime + stime at USER_HZ 100).
fn own_cpu_ms() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = s.rsplit_once(')').map_or("", |r| r.1);
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state (0), ppid, ..., utime (11), stime (12).
    let n = |i: usize| f.get(i).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
    (n(11) + n(12)) * 10
}

/// Writes `profile.bin` (oldest windows dropped until it fits the cap) and `meta.json`.
fn save(o: &Opts, p: &Profile, meta: &Meta) -> Result<(), String> {
    let mut from = 0;
    let mut bytes = p.encode(0);
    while bytes.len() > o.max_bytes && from < p.windows.len() {
        from += 1;
        bytes = p.encode(from);
    }
    write_atomic(&o.out.join("profile.bin"), &bytes)?;
    let system = if Path::new("/proc/bitsos").exists() { "BitsOS" } else { "Linux" };
    // The profiler's share of all CPU time of the run: its own program plus the kernel's sampling work. The
    // interrupt entry and exit around that work is not timed, so the share is a little low (meta `cost_covers`).
    let cpu_ms = own_cpu_ms();
    let kns = meta.kernel.map_or(0, |k| k.1);
    let all_ns = meta.seconds * 1e9 * meta.cpus.max(1) as f64;
    let cost = if all_ns > 0.0 { (cpu_ms as f64 * 1e6 + kns as f64) / all_ns } else { 0.0 };
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let json = format!(
        "{{\n  \"version\": 1,\n  \"system\": \"{system}\",\n  \"kernel\": {},\n  \"start_unix\": {},\n  \"seconds\": {:.1},\n  \"hz\": {},\n  \"cpus\": {},\n  \"samples\": {},\n  \"idle_samples\": {},\n  \"lost_records\": {},\n  \"kernel_symbols\": {},\n  \"windows\": {},\n  \"windows_dropped\": {},\n  \"profile_bytes\": {},\n  \"profiler_cpu_ms\": {},\n  \"kernel_sampler_ns\": {},\n  \"cost_share\": {:.4},\n  \"cost_covers\": \"profiler program and sampling work, without the interrupt entry and exit\"\n}}\n",
        crate::report::json_str(release.trim()),
        meta.start,
        meta.seconds,
        meta.hz,
        meta.cpus,
        meta.samples,
        meta.idle,
        meta.lost,
        meta.ksyms,
        p.windows.len() - from,
        from,
        bytes.len(),
        cpu_ms,
        kns,
        cost
    );
    write_atomic(&o.out.join("meta.json"), json.as_bytes())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("{} lässt sich nicht schreiben: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{} lässt sich nicht ersetzen: {e}", path.display()))
}

//! The status bar's building blocks with real values only (WY-5; the block protocol itself is WY-6): CPU load
//! from /proc/stat (the `cpu` line: user nice system idle iowait irq softirq steal, linux-6.6/fs/proc/stat.c:127),
//! the network link from /sys/class/net/*/operstate ("up", or "unknown" with carrier 1 for drivers that report no
//! operstate; linux-6.6/net/core/net-sysfs.c:294 and :192), the temperature
//! only where /sys/class/hwmon has a temp*_input (millidegrees, Documentation/hwmon/sysfs-interface.rst), the clock
//! (local time when a valid time zone is set, else UTC and marked so). A block without a source is left out, not
//! faked. Sampled every 5 s and on the minute; a frame is drawn only when a shown value changes, and the CPU value
//! only counts as changed when it moved by CPU_STEP points (an idle desktop's 0-2 % jitter draws nothing).

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SAMPLE_EVERY: Duration = Duration::from_secs(5);
/// The CPU value shown is replaced only when the new one differs by at least this many points.
pub const CPU_STEP: u64 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub label: String,
    pub value: String,
}

#[derive(Default)]
pub struct Status {
    /// The last (busy, total) jiffies of the `cpu` line.
    cpu_last: Option<(u64, u64)>,
    pub cpus: usize,
    pub blocks: Vec<Block>,
    pub clock: String,
    /// Test mode (`--fixed-status`): fixed values, nothing is read, so screenshots compare.
    pub fixed: bool,
    next: Option<Duration>,
}

/// (busy, total, cpu count) from /proc/stat's text.
pub fn parse_stat(s: &str) -> Option<(u64, u64, usize)> {
    let first = s.lines().next()?;
    let mut f = first.split_whitespace();
    if f.next()? != "cpu" {
        return None;
    }
    let v: Vec<u64> = f.take(8).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    let cpus = s.lines().filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit)).count();
    Some((total - idle, total, cpus))
}

/// "Kabel", "WLAN", "getrennt", or `None` without any interface but lo.
pub fn link(dir: &Path) -> Option<String> {
    let mut any = false;
    let mut up: Option<String> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        any = true;
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
        // "unknown": the driver tells no operstate (tun, some USB adapters); then the carrier decides.
        let is_up = match read("operstate").trim() {
            "up" => true,
            "unknown" => read("carrier").trim() == "1",
            _ => false,
        };
        if is_up {
            let wireless = e.path().join("wireless").exists() || e.path().join("phy80211").exists();
            let kind = if wireless { "WLAN" } else { "Kabel" };
            // A cable wins over WLAN when both are up.
            if up.as_deref() != Some("Kabel") {
                up = Some(kind.into());
            }
        }
    }
    any.then(|| up.unwrap_or_else(|| "getrennt".into()))
}

/// The CPU temperature in °C: the k10temp/coretemp sensor first, else the first one with a temp1_input.
pub fn temperature(dir: &Path) -> Option<i64> {
    let mut best: Option<(bool, i64)> = None;
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = std::fs::read_to_string(p.join("name")).unwrap_or_default();
        let cpu = matches!(name.trim(), "k10temp" | "coretemp" | "zenpower");
        let Some(milli) = std::fs::read_to_string(p.join("temp1_input")).ok().and_then(|s| s.trim().parse::<i64>().ok()) else {
            continue;
        };
        if best.is_none_or(|(c, _)| cpu && !c) {
            best = Some((cpu, (milli + 500) / 1000));
        }
    }
    best.map(|(_, c)| c)
}

const DAYS: [&str; 7] = ["So", "Mo", "Di", "Mi", "Do", "Fr", "Sa"];

/// A zoneinfo file: glibc ignores anything without the TZif magic and silently uses UTC.
fn tzif(p: &Path) -> bool {
    use std::io::Read as _;
    let mut m = [0u8; 4];
    std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut m)).is_ok() && &m == b"TZif"
}

/// Whether localtime_r gives a real local time: TZ names a zoneinfo file or is a POSIX rule with an offset
/// ("CET-1CEST,M3.5.0,M10.5.0/3"), or without TZ /etc/localtime is a zoneinfo file. Anything else glibc turns
/// into UTC without a word, and then the clock must say UTC.
pub fn zone_known(tz: Option<&str>, localtime: &Path, zoneinfo: &Path) -> bool {
    match tz {
        None => tzif(localtime),
        Some("") => tzif(localtime),
        Some(t) => {
            let t = t.strip_prefix(':').unwrap_or(t);
            if t.starts_with('/') {
                return tzif(Path::new(t));
            }
            if !t.contains("..") && tzif(&zoneinfo.join(t)) {
                return true;
            }
            // A POSIX rule: a name of at least three letters, then an offset.
            let name = t.chars().take_while(char::is_ascii_alphabetic).count();
            name >= 3 && t[name..].starts_with(|c: char| c.is_ascii_digit() || c == '+' || c == '-')
        }
    }
}

/// Broken-down time of `secs` since the epoch: local when a valid time zone is set, else UTC.
fn now_parts(secs: i64) -> (libc::tm, bool) {
    let tz = std::env::var("TZ").ok();
    let local = zone_known(tz.as_deref(), Path::new("/etc/localtime"), Path::new("/usr/share/zoneinfo"));
    let t = secs as libc::time_t;
    // SAFETY: an all-zero tm is a valid value; localtime_r/gmtime_r only read `t` and write into `tm`.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let ok = if local { !libc::localtime_r(&t, &mut tm).is_null() } else { !libc::gmtime_r(&t, &mut tm).is_null() };
        (tm, local && ok)
    }
}

/// "Di 21.10. · 14:32", with " UTC" when there is no time zone.
pub fn clock_text(tm: &libc::tm, local: bool) -> String {
    let day = DAYS[tm.tm_wday.clamp(0, 6) as usize];
    let utc = if local { "" } else { " UTC" };
    format!("{day} {:02}.{:02}. · {:02}:{:02}{utc}", tm.tm_mday, tm.tm_mon + 1, tm.tm_hour, tm.tm_min)
}

/// The broken-down time of `secs`: UTC in test mode (`fixed`), else as the clock shows it.
pub fn tm_of(secs: i64, fixed: bool) -> libc::tm {
    if fixed { utc(secs) } else { now_parts(secs).0 }
}

/// The start page's date line and greeting.
pub fn date_and_greeting(tm: &libc::tm) -> (String, &'static str) {
    const LONG: [&str; 7] = ["SONNTAG", "MONTAG", "DIENSTAG", "MITTWOCH", "DONNERSTAG", "FREITAG", "SAMSTAG"];
    const MONTHS: [&str; 12] =
        ["JANUAR", "FEBRUAR", "MÄRZ", "APRIL", "MAI", "JUNI", "JULI", "AUGUST", "SEPTEMBER", "OKTOBER", "NOVEMBER", "DEZEMBER"];
    let greet = match tm.tm_hour {
        5..=10 => "Guten Morgen",
        11..=17 => "Guten Tag",
        _ => "Guten Abend",
    };
    (format!("{}, {}. {}", LONG[tm.tm_wday.clamp(0, 6) as usize], tm.tm_mday, MONTHS[tm.tm_mon.clamp(0, 11) as usize]), greet)
}

/// "heute 14:02", "gestern 22:14", else "21.10.", for `then` seen at `now`.
pub fn when_text(then: &libc::tm, now: &libc::tm) -> String {
    let day = |t: &libc::tm| (t.tm_year, t.tm_yday);
    let hm = format!("{:02}:{:02}", then.tm_hour, then.tm_min);
    if day(then) == day(now) {
        format!("heute {hm}")
    } else if (then.tm_year == now.tm_year && then.tm_yday + 1 == now.tm_yday) || (then.tm_year + 1 == now.tm_year && now.tm_yday == 0) {
        format!("gestern {hm}")
    } else {
        format!("{:02}.{:02}.", then.tm_mday, then.tm_mon + 1)
    }
}

fn utc(secs: i64) -> libc::tm {
    let t = secs as libc::time_t;
    // SAFETY: an all-zero tm is a valid value; gmtime_r only reads `t` and writes into `tm`.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&t, &mut tm);
        tm
    }
}

/// Whether `pct` differs enough from the shown "N %" to be drawn.
fn cpu_moved(shown: &str, pct: u64) -> bool {
    shown.trim_end_matches(" %").parse::<u64>().map_or(true, |o| o.abs_diff(pct) >= CPU_STEP)
}

pub fn epoch_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The fixed test values (`--fixed-status`): a clock that never moves, a load that never changes.
pub const FIXED_SECS: i64 = 1_792_593_120; // Wed 2026-10-21 14:32 UTC

impl Status {
    pub fn fixed() -> Status {
        let mut s = Status { fixed: true, cpus: 16, ..Default::default() };
        s.blocks = vec![Block { label: "CPU".into(), value: "12 %".into() }, Block { label: "Netz".into(), value: "Kabel".into() }];
        s.clock = clock_text(&utc(FIXED_SECS), false);
        s
    }

    /// When the next sample is due (`pacer` time); `None` in test mode.
    pub fn due(&self) -> Option<Duration> {
        if self.fixed { None } else { Some(self.next.unwrap_or(Duration::ZERO)) }
    }

    /// Reads the sources when due; true when anything shown changed.
    pub fn tick(&mut self, now: Duration) -> bool {
        if self.fixed || self.next.is_some_and(|n| now < n) {
            return false;
        }
        let secs = epoch_secs();
        // The next sample: in 5 s, or at the next whole minute when that comes first (the clock).
        let to_minute = Duration::from_secs((60 - secs.rem_euclid(60)) as u64);
        // The CPU value needs two samples: the second one comes after 1 s, so the block is there early.
        let every = if self.cpu_last.is_none() { Duration::from_secs(1) } else { SAMPLE_EVERY };
        self.next = Some(now + every.min(to_minute));
        let mut blocks = Vec::new();
        if let Some((busy, total, cpus)) = std::fs::read_to_string("/proc/stat").ok().as_deref().and_then(parse_stat) {
            self.cpus = cpus;
            if let Some((b0, t0)) = self.cpu_last
                && total > t0
            {
                let pct = ((busy.saturating_sub(b0)) * 100 + (total - t0) / 2) / (total - t0);
                let old = self.blocks.iter().find(|b| b.label == "CPU");
                blocks.push(match old {
                    Some(o) if !cpu_moved(&o.value, pct) => o.clone(),
                    _ => Block { label: "CPU".into(), value: format!("{pct} %") },
                });
            } else if let Some(old) = self.blocks.iter().find(|b| b.label == "CPU") {
                blocks.push(old.clone());
            }
            self.cpu_last = Some((busy, total));
        }
        if let Some(l) = link(Path::new("/sys/class/net")) {
            blocks.push(Block { label: "Netz".into(), value: l });
        }
        if let Some(c) = temperature(Path::new("/sys/class/hwmon")) {
            blocks.push(Block { label: "Temperatur".into(), value: format!("{c} °C") });
        }
        let (tm, local) = now_parts(secs);
        let clock = clock_text(&tm, local);
        let changed = blocks != self.blocks || clock != self.clock;
        (self.blocks, self.clock) = (blocks, clock);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cpu_line_gives_busy_and_total() {
        let s = "cpu  10 2 3 80 5 0 0 0 0 0\ncpu0 5 1 1 40 2 0 0 0 0 0\ncpu1 5 1 2 40 3 0 0 0 0 0\nintr 0\n";
        assert_eq!(parse_stat(s), Some((15, 100, 2)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat(""), None);
    }

    #[test]
    fn the_clock_is_german_and_says_utc_without_a_zone() {
        let tm = utc(FIXED_SECS);
        assert_eq!(clock_text(&tm, false), "Mi 21.10. · 14:32 UTC");
        assert!(!clock_text(&tm, true).contains("UTC"));
    }

    #[test]
    fn only_a_valid_zone_counts_as_local_time() {
        let d = std::env::temp_dir().join(format!("bitscomp-tz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("zi/Europe")).unwrap();
        std::fs::write(d.join("zi/Europe/Berlin"), b"TZif2\0\0").unwrap();
        std::fs::write(d.join("good"), b"TZif2\0\0").unwrap();
        std::fs::write(d.join("empty"), b"").unwrap();
        std::fs::write(d.join("junk"), b"not a zone").unwrap();
        let (zi, no) = (d.join("zi"), d.join("missing"));
        assert!(zone_known(None, &d.join("good"), &zi));
        assert!(!zone_known(None, &d.join("empty"), &zi), "an empty /etc/localtime is UTC");
        assert!(!zone_known(None, &d.join("junk"), &zi));
        assert!(!zone_known(None, &no, &zi));
        assert!(!zone_known(Some(""), &no, &zi));
        assert!(zone_known(Some("Europe/Berlin"), &no, &zi));
        assert!(zone_known(Some(":Europe/Berlin"), &no, &zi));
        assert!(!zone_known(Some("Europe/Nowhere"), &no, &zi), "an unknown zone name is UTC");
        assert!(!zone_known(Some("../../good"), &no, &zi));
        assert!(zone_known(Some("CET-1CEST,M3.5.0,M10.5.0/3"), &no, &zi));
        assert!(!zone_known(Some("garbage"), &no, &zi));
        assert!(zone_known(Some(&d.join("good").to_string_lossy()), &no, &zi));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn small_cpu_changes_draw_nothing() {
        assert!(!cpu_moved("1 %", 2));
        assert!(!cpu_moved("1 %", 0));
        assert!(cpu_moved("1 %", 4));
        assert!(cpu_moved("40 %", 37));
        assert!(cpu_moved("odd", 0));
    }

    #[test]
    fn dates_and_times_are_german() {
        let now = utc(FIXED_SECS);
        assert_eq!(date_and_greeting(&now), ("MITTWOCH, 21. OKTOBER".to_string(), "Guten Tag"));
        assert_eq!(when_text(&utc(FIXED_SECS - 600), &now), "heute 14:22");
        assert_eq!(when_text(&utc(FIXED_SECS - 86_400), &now), "gestern 14:32");
        assert_eq!(when_text(&utc(FIXED_SECS - 5 * 86_400), &now), "16.10.");
    }

    #[test]
    fn link_and_temperature_read_sysfs_trees() {
        let d = std::env::temp_dir().join(format!("bitscomp-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let net = d.join("net");
        for (n, s) in [("lo", "unknown"), ("enp1s0", "up"), ("wlan0", "down")] {
            std::fs::create_dir_all(net.join(n)).unwrap();
            std::fs::write(net.join(n).join("operstate"), format!("{s}\n")).unwrap();
        }
        assert_eq!(link(&net).as_deref(), Some("Kabel"));
        std::fs::write(net.join("enp1s0").join("operstate"), "down\n").unwrap();
        assert_eq!(link(&net).as_deref(), Some("getrennt"));
        // A driver without operstate: "unknown", the carrier decides; a "wl" name alone is no WLAN.
        std::fs::create_dir_all(net.join("wlx0")).unwrap();
        std::fs::write(net.join("wlx0").join("operstate"), "unknown\n").unwrap();
        std::fs::write(net.join("wlx0").join("carrier"), "0\n").unwrap();
        assert_eq!(link(&net).as_deref(), Some("getrennt"));
        std::fs::write(net.join("wlx0").join("carrier"), "1\n").unwrap();
        assert_eq!(link(&net).as_deref(), Some("Kabel"));
        std::fs::create_dir_all(net.join("wlx0").join("phy80211")).unwrap();
        assert_eq!(link(&net).as_deref(), Some("WLAN"));
        let hw = d.join("hwmon");
        assert_eq!(temperature(&hw), None, "no hwmon: no block");
        for (n, name, t) in [("hwmon0", "nvme", "38850"), ("hwmon1", "k10temp", "47600")] {
            std::fs::create_dir_all(hw.join(n)).unwrap();
            std::fs::write(hw.join(n).join("name"), format!("{name}\n")).unwrap();
            std::fs::write(hw.join(n).join("temp1_input"), format!("{t}\n")).unwrap();
        }
        assert_eq!(temperature(&hw), Some(48));
        let _ = std::fs::remove_dir_all(&d);
    }
}

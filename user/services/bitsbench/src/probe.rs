//! Facts the runner reads from the system: the power supplies, the bytes received, the running generation and
//! this boot's own timestamps (`/proc/bitsos/boot`, the boot screen's step list with the time of each step).

use crate::rules::Power;
use std::path::Path;

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// The supplies under `dir` (/sys/class/power_supply): (type, online, scope, status) each.
pub fn supplies(dir: &Path) -> Vec<(String, Option<bool>, String, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.filter_map(|e| e.ok())
        .take(64)
        .map(|e| {
            let p = e.path();
            let online = match read(&p.join("online")).as_str() {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            };
            (read(&p.join("type")), online, read(&p.join("scope")), read(&p.join("status")))
        })
        .collect()
}

pub fn power() -> Power {
    crate::rules::power_of(&supplies(Path::new("/sys/class/power_supply")))
}

/// Bytes received by every interface but loopback (/sys/class/net/<if>/statistics/rx_bytes).
pub fn rx_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .take(256)
        .filter(|e| e.file_name() != "lo")
        .filter_map(|e| read(&e.path().join("statistics/rx_bytes")).parse::<u64>().ok())
        .sum()
}

/// `generation=<n> ...` of /proc/bitsos/generation (None on a kernel without it, or another Linux).
pub fn generation() -> Option<u64> {
    parse_generation(&read(Path::new("/proc/bitsos/generation")))
}

pub fn parse_generation(text: &str) -> Option<u64> {
    text.split_whitespace().find_map(|w| w.strip_prefix("generation="))?.parse().ok().filter(|n| *n > 0)
}

/// Where the running generation came from. Level 1 builds nothing: every generation is hand-built until level 2
/// marks its own (`origin=` on the generation line, e.g. `selbst optimiert, Nacht vom 14.11.`).
pub fn origin() -> String {
    origin_of(&read(Path::new("/proc/bitsos/generation")))
}

pub fn origin_of(text: &str) -> String {
    match text.split_whitespace().find_map(|w| w.strip_prefix("origin=")) {
        Some(o) if !o.is_empty() => o.replace('_', " "),
        _ => "von Hand gebaut".into(),
    }
}

/// This boot's time to a ready desktop in ms: the latest step time of `/proc/bitsos/boot` once every step is
/// final (done, failed or skipped). None while the start still runs, or when the text is not the boot list.
pub fn boot_ms(text: &str) -> Option<u64> {
    if text.lines().next()? != "bitsos-boot 1" {
        return None;
    }
    let mut last = None;
    for l in text.lines().filter(|l| l.starts_with("step ")) {
        let w: Vec<&str> = l.split_whitespace().collect();
        let (state, t) = (*w.get(2)?, w.get(3)?.parse::<u64>().ok()?);
        if !matches!(state, "done" | "failed" | "skipped") {
            return None;
        }
        last = Some(last.map_or(t, |m: u64| m.max(t)));
    }
    last
}

/// Seconds since boot (/proc/uptime).
pub fn uptime_s() -> Option<f64> {
    read(Path::new("/proc/uptime")).split_whitespace().next()?.parse().ok()
}

pub fn is_bitsos() -> bool {
    Path::new("/proc/bitsos").is_dir()
}

/// The kernel's sound output line (`mixer=.. playing=.. clients=N hdmi=..`, crates/snd output.rs).
pub const SOUND_STATUS: &str = "/sys/bitsos/snd_output_status";
/// The music player's socket (bitsmusik, uid 1000); `BITSBENCH_MUSIC_SOCKET` for tests.
pub const MUSIC_SOCKET: &str = "/run/user/1000/bitsmusik.sock";

/// Programs with sound open at the kernel's mixer, from its status line (None: not that line).
pub fn mixer_clients(text: &str) -> Option<u64> {
    text.split_whitespace().find_map(|w| w.strip_prefix("clients="))?.parse().ok()
}

/// Whether the music player's `status` answer says it plays.
pub fn music_playing(answer: &str) -> bool {
    mcpreg::Json::parse(answer.trim()).ok().and_then(|j| j.get("playing").and_then(mcpreg::Json::as_bool)).unwrap_or(false)
}

fn ask_music(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(path).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok()?;
    s.set_write_timeout(Some(std::time::Duration::from_secs(2))).ok()?;
    s.write_all(b"{\"op\":\"status\"}\n").ok()?;
    let mut line = String::new();
    BufReader::new(s.take(64 << 10)).read_line(&mut line).ok()?;
    Some(line)
}

/// Whether anything plays sound now: the music player says so, or the kernel's mixer has a program with sound
/// open (a video in Brave, a game, the player itself). Another Linux has neither and says no.
pub fn sound_playing() -> bool {
    let music = std::env::var_os("BITSBENCH_MUSIC_SOCKET").map_or_else(|| std::path::PathBuf::from(MUSIC_SOCKET), std::path::PathBuf::from);
    mixer_clients(&read(Path::new(SOUND_STATUS))).is_some_and(|n| n > 0) || ask_music(&music).is_some_and(|a| music_playing(&a))
}

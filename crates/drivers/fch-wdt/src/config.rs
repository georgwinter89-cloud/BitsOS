//! CONFIG keys of the watchdog (pure; host-tested).
//!
//! - `watchdog=on|off`: default on on hardware, off under a hypervisor (QEMU has no FCH; a test turns it on).
//! - `watchdog_timeout=<s>`: the chip's count, default 30 s (V1 used 60 s, `V1:hw/watchdog.asm:17`; the brief of
//!   RB-3 asks for 30: with the 10 s liveness window a hang resets after about 40 s and the NMI report of a
//!   stalled CPU still has 30 s to reach the mirror). 5..=65535.
//! - `watchdog_alive=<s>`: the liveness window, default 10 s. 2..=600, and at most timeout - 3 s.
//! - `watchdog_grace=<s>`: the count set before a reboot (`V1:hw/watchdog.asm:18`, WDT_GRACE), default 240 s.
//! - `watchdog_reload=stop|grace`: what a Reload does with it, default stop (the brief of RB-3; the new kernel
//!   arms it again); `grace` keeps it armed with the grace count as V1 did.

pub const DEFAULT_TIMEOUT_S: u32 = 30;
pub const DEFAULT_ALIVE_S: u32 = 10;
pub const DEFAULT_GRACE_S: u32 = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub on: bool,
    pub timeout_s: u32,
    pub alive_s: u32,
    pub grace_s: u32,
    pub reload_grace: bool,
    /// Where `on` came from: true = CONFIG said so.
    pub on_from_config: bool,
}

fn num(v: Option<&str>, lo: u32, hi: u32, default: u32, key: &'static str, problem: &mut Option<&'static str>) -> u32 {
    match v.map(str::trim) {
        None | Some("") => default,
        Some(s) => match s.trim_end_matches('s').parse::<u32>() {
            Ok(n) if (lo..=hi).contains(&n) => n,
            _ => {
                problem.get_or_insert(key);
                default
            }
        },
    }
}

/// Values as CONFIG gives them (None = key missing). Returns the config and the first key that was bad (its
/// default is used).
pub fn parse(
    on: Option<&str>,
    timeout: Option<&str>,
    alive: Option<&str>,
    grace: Option<&str>,
    reload: Option<&str>,
    hypervisor: bool,
) -> (Config, Option<&'static str>) {
    let mut problem = None;
    let (on, on_from_config) = match on.map(|s| s.trim().to_ascii_lowercase()) {
        None => (!hypervisor, false),
        Some(s) if s.is_empty() => (!hypervisor, false),
        Some(s) if s == "on" || s == "1" || s == "yes" => (true, true),
        Some(s) if s == "off" || s == "0" || s == "no" => (false, true),
        Some(_) => {
            problem = Some("watchdog");
            (!hypervisor, false)
        }
    };
    let timeout_s = num(timeout, 5, crate::regs::MAX_SECONDS, DEFAULT_TIMEOUT_S, "watchdog_timeout", &mut problem);
    let mut alive_s = num(alive, 2, 600, DEFAULT_ALIVE_S, "watchdog_alive", &mut problem);
    if alive_s + 3 > timeout_s {
        problem.get_or_insert("watchdog_alive");
        alive_s = (timeout_s - 3).max(2);
    }
    let grace_s = num(grace, 30, crate::regs::MAX_SECONDS, DEFAULT_GRACE_S, "watchdog_grace", &mut problem);
    let reload_grace = match reload.map(str::trim) {
        None | Some("") | Some("stop") => false,
        Some("grace") => true,
        Some(_) => {
            problem.get_or_insert("watchdog_reload");
            false
        }
    };
    (Config { on, timeout_s, alive_s, grace_s, reload_grace, on_from_config }, problem)
}

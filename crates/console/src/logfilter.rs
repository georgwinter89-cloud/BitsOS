//! Which kernel log lines the screen console shows (IN-quiet). The console is one klog sink among several: the
//! ring, the network mirror and the MCP `log` tool keep every record whatever the console shows.
//!
//! Levels (`console_log=` in CONFIG, `log level <level>` on the monitor):
//!
//! | level | the console shows |
//! |---|---|
//! | `off` | nothing |
//! | `error` | errors |
//! | `warn` (default) | errors, warnings and the short list of important Info events (`IMPORTANT`) |
//! | `info` | everything down to Info |
//! | `debug` | everything down to Debug |
//! | `all` | everything the kernel logs (Trace too) |
//!
//! The console only ever sees what klog keeps: CONFIG `loglevel=` decides what reaches the ring at all (Debug lines
//! need `loglevel=debug` there too).
//!
//! The sink gets rendered lines (`[    1.234567] I net link.up ifname=igc0 ...`, klog's `render::prefix`), so the
//! important-event check reads subsystem and name back from the line's head (`head`). Pure functions, host-tested.

use klog::Level;

pub const OFF: u8 = 0;
pub const ERROR: u8 = 1;
pub const WARN: u8 = 2;
pub const INFO: u8 = 3;
pub const DEBUG: u8 = 4;
pub const ALL: u8 = 5;

/// The console's level until CONFIG or the monitor changes it: quiet (Georg types at the monitor on the Mini-PC).
pub const DEFAULT: u8 = WARN;

const NAMES: [&str; 6] = ["off", "error", "warn", "info", "debug", "all"];

/// `off|none, error, warn, info, debug, all|trace` (or 0..=5).
pub fn parse(s: &str) -> Option<u8> {
    Some(match s.trim() {
        "off" | "none" | "0" => OFF,
        "error" | "1" => ERROR,
        "warn" | "2" => WARN,
        "info" | "3" => INFO,
        "debug" | "4" => DEBUG,
        "all" | "trace" | "5" => ALL,
        _ => return None,
    })
}

pub fn name(level: u8) -> &'static str {
    NAMES[level.min(ALL) as usize]
}

/// What `parse` accepts, for messages.
pub const CHOICES: &str = "off, error, warn, info, debug or all";

/// Info events the quiet console still shows: (subsystem, name). A name ending in '.' matches every name with that
/// prefix. Boot milestones, the network's link and address, MCP reachable, the watchdog, init, Reload, power,
/// input devices coming and going (Georg's replug test). Warn and Error are shown anyway (link down, DHCP lost,
/// init died, reload failed).
pub const IMPORTANT: &[(&str, &str)] = &[
    ("kernel", "start"),
    ("kernel", "ready"),
    ("net", "link.up"),
    ("net", "iface.link_up"),
    ("net", "dhcp.lease"),
    ("net", "addr.static"),
    ("mcp", "server.start"),
    ("mcp", "reboot.requested"),
    ("watchdog", "armed"),
    ("watchdog", "absent"),
    ("watchdog", "resumed"),
    ("ul", "init."),
    ("kexec", "reload."),
    ("power", "reboot.requested"),
    ("power", "off.requested"),
    ("input", "device.registered"),
    ("input", "device.removed"),
];

/// GP-ROUND (`plan/spec/gaming-pc.md` section 8, raised lines): under the profile gaming-pc the quiet console also shows
/// these Info events, so a photo of the screen holds them even when the report page never comes: the DMA verdict,
/// the clock's decision, HWP per CPU, the CPUs that came up, and the network card's stages. Nothing else is raised,
/// and no other machine ever sees them on its screen.
pub const PROFILE_IMPORTANT: &[(&str, &str)] = &[
    ("acpi", "dma."),
    ("time", "hpet.untrusted"),
    ("time", "clock.cross_check"),
    ("cpu", "hwp.on"),
    ("cpu", "hwp.failed"),
    ("smp", "done"),
    ("smp", "shape"),
    ("net", "rtl8126.stage"),
    // The report page's own thread: a page that never comes leaves the line naming the getter it waited for.
    ("kernel", "gp.report"),
    ("kernel", "gp.report.reading"),
];

/// Whether (subsystem, name) is on the important list.
pub fn important(subsystem: &str, name: &str) -> bool {
    IMPORTANT.iter().any(|&(s, n)| s == subsystem && if n.ends_with('.') { name.starts_with(n) } else { name == n })
}

/// (level letter, subsystem, name) from a rendered line's head `[ secs.micros] L subsystem name ...`; None for
/// anything else (klog's own `[... n records lost]`).
pub fn head(line: &str) -> Option<(char, &str, &str)> {
    let rest = line.strip_prefix('[')?;
    let rest = &rest[rest.find("] ")? + 2..];
    let mut words = rest.splitn(4, ' ');
    let mut letter = words.next()?.chars();
    let l = letter.next()?;
    if letter.next().is_some() {
        return None;
    }
    let subsystem = words.next().filter(|s| !s.is_empty())?;
    let name = words.next().filter(|s| !s.is_empty())?;
    Some((l, subsystem, name))
}

/// Whether (subsystem, name) is on the profile's own list (`PROFILE_IMPORTANT`).
pub fn profile_important(subsystem: &str, name: &str) -> bool {
    PROFILE_IMPORTANT.iter().any(|&(s, n)| s == subsystem && if n.ends_with('.') { name.starts_with(n) } else { name == n })
}

/// Whether the console at `level` shows a line of klog level `l`.
pub fn shows(level: u8, l: Level, line: &str) -> bool {
    shows_with(level, l, line, false)
}

/// `shows`; `profile`: the Gaming-PC's profile is on, so `PROFILE_IMPORTANT` counts as important too.
pub fn shows_with(level: u8, l: Level, line: &str, profile: bool) -> bool {
    match level {
        OFF => false,
        ERROR => l == Level::Error,
        WARN => match l {
            Level::Error | Level::Warn => true,
            Level::Info => head(line).is_some_and(|(_, s, n)| important(s, n) || (profile && profile_important(s, n))),
            _ => false,
        },
        INFO => l <= Level::Info,
        DEBUG => l <= Level::Debug,
        _ => true,
    }
}

//! Log levels per subsystem from CONFIG `loglevel=` (`info` or `warn,net=debug,usb.xhci=trace`).
//!
//! A key matches its subsystem and every dotted sub-subsystem (`net` matches `net.dhcp`); the longest key wins.
//! `event!` checks the most verbose level of all settings first (one atomic load, macro unchanged); the exact
//! per-subsystem decision follows in `emit`.

use crate::Level;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use ksync::RwSpinLock;

static DEFAULT: AtomicU8 = AtomicU8::new(Level::Info as u8);
/// The most verbose level any setting allows (the macro's cheap first check).
static MOST: AtomicU8 = AtomicU8::new(Level::Info as u8);
static HAS_OVERRIDES: AtomicBool = AtomicBool::new(false);
static OVERRIDES: RwSpinLock<Vec<(String, Level)>> = RwSpinLock::new(Vec::new());
/// CPU tag of a writer of OVERRIDES, so an NMI on that CPU never waits for its own write lock.
static WRITER: AtomicUsize = AtomicUsize::new(0);

pub fn from_u8(v: u8) -> Level {
    match v {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        3 => Level::Debug,
        _ => Level::Trace,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub default: Option<Level>,
    pub overrides: Vec<(String, Level)>,
}

/// Parses `info`, `net=debug` or `warn,net=debug,usb=trace`. The error names the first bad part.
pub fn parse(s: &str) -> Result<Spec, String> {
    let mut spec = Spec { default: None, overrides: Vec::new() };
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('=') {
            Some((k, v)) => {
                let k = k.trim();
                let l = Level::parse(v.trim()).ok_or_else(|| String::from(part))?;
                if k.is_empty() || k.contains(char::is_whitespace) {
                    return Err(String::from(part));
                }
                spec.overrides.retain(|(o, _)| o != k);
                spec.overrides.push((String::from(k), l));
            }
            None => spec.default = Some(Level::parse(part).ok_or_else(|| String::from(part))?),
        }
    }
    Ok(spec)
}

/// Whether `key` covers `subsystem` (equal, or a dotted prefix of it).
pub fn covers(key: &str, subsystem: &str) -> bool {
    subsystem.strip_prefix(key).is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

fn recompute_most(overrides: &[(String, Level)]) {
    let mut most = DEFAULT.load(Ordering::Relaxed);
    for (_, l) in overrides {
        most = most.max(*l as u8);
    }
    MOST.store(most, Ordering::Relaxed);
}

pub fn set_default(l: Level) {
    DEFAULT.store(l as u8, Ordering::Relaxed);
    recompute_most(&OVERRIDES.read());
}

pub fn default() -> Level {
    from_u8(DEFAULT.load(Ordering::Relaxed))
}

pub fn most_verbose() -> Level {
    from_u8(MOST.load(Ordering::Relaxed))
}

/// Applies a parsed spec: the default (if given) and the overrides replace the previous overrides.
pub fn apply(spec: Spec) {
    if let Some(d) = spec.default {
        DEFAULT.store(d as u8, Ordering::Relaxed);
    }
    let me = crate::cpu_tag();
    WRITER.store(me, Ordering::Release);
    {
        let mut o = OVERRIDES.write();
        *o = spec.overrides;
        HAS_OVERRIDES.store(!o.is_empty(), Ordering::Release);
        recompute_most(&o);
    }
    WRITER.store(0, Ordering::Release);
}

/// The current settings as text (`info,net=debug`).
pub fn describe() -> String {
    let mut s = String::from(default().name());
    for (k, l) in OVERRIDES.read().iter() {
        s.push(',');
        s.push_str(k);
        s.push('=');
        s.push_str(l.name());
    }
    s
}

/// The level that applies to `subsystem`. `nonblocking`: the panic/NMI path, which never waits for the table.
pub fn effective(subsystem: &str, nonblocking: bool) -> Level {
    if !HAS_OVERRIDES.load(Ordering::Acquire) {
        return default();
    }
    if nonblocking || WRITER.load(Ordering::Acquire) == crate::cpu_tag() {
        return most_verbose();
    }
    let o = OVERRIDES.read();
    o.iter().filter(|(k, _)| covers(k, subsystem)).max_by_key(|(k, _)| k.len()).map(|(_, l)| *l).unwrap_or_else(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_and_overrides() {
        let s = parse(" warn , net=debug,usb.xhci=trace ").unwrap();
        assert_eq!(s.default, Some(Level::Warn));
        assert_eq!(s.overrides, [(String::from("net"), Level::Debug), (String::from("usb.xhci"), Level::Trace)]);
        assert_eq!(parse("info").unwrap().overrides.len(), 0);
        assert_eq!(parse("net=loud"), Err(String::from("net=loud")));
        assert_eq!(parse("chatty"), Err(String::from("chatty")));
        assert_eq!(parse("=debug"), Err(String::from("=debug")));
    }

    #[test]
    fn keys_cover_dotted_subsystems_only() {
        assert!(covers("net", "net"));
        assert!(covers("net", "net.dhcp"));
        assert!(!covers("net", "netx"));
        assert!(!covers("net.dhcp", "net"));
    }
}

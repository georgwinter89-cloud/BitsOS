//! The lines of `/dev/bitsos/events` (format in crates/klogd/src/devevents.rs). The journal stores the lines it
//! keeps as they came (text still escaped), so a stored line parses with the same code.

/// One parsed line; `text` is still escaped (`unescape` for display).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item<'a> {
    Boot { id: u64, reload: u64, next: u64, wall: Option<u64>, off_min: i32 },
    Event { seq: u64, time_ns: u64, wall: Option<u64>, level: u8, cpu: u32, subsystem: &'a str, name: &'a str, text: &'a str },
    Trace { cpu: u32, pos: u64, time_ns: u64, wall: Option<u64>, level: u8, subsystem: &'a str, name: &'a str, text: &'a str },
    /// `pairs`: `name=<value><unit> ...`.
    Metric { round: u64, time_ns: u64, wall: Option<u64>, pairs: &'a str },
    Gap { before: u64, count: u64 },
    TraceGap { cpu: u32, count: u64 },
    Flush { target: u64 },
    Idle { next: u64, wall: Option<u64> },
}

pub const LEVELS: [char; 5] = ['E', 'W', 'I', 'D', 'T'];

pub fn level_of(s: &str) -> Option<u8> {
    let mut c = s.chars();
    let l = c.next()?;
    if c.next().is_some() {
        return None;
    }
    LEVELS.iter().position(|&x| x == l).map(|p| p as u8)
}

fn wall(s: &str) -> Option<Option<u64>> {
    if s == "-" { Some(None) } else { s.parse().ok().map(Some) }
}

pub fn parse(line: &str) -> Option<Item<'_>> {
    let (kind, rest) = line.split_once(' ')?;
    let n = |s: Option<&str>| s.and_then(|s| s.parse::<u64>().ok());
    match kind {
        "B" => {
            let f: Vec<&str> = rest.split(' ').collect();
            if f.len() != 6 {
                return None;
            }
            Some(Item::Boot { id: u64::from_str_radix(f[0], 16).ok()?, reload: f[1].parse().ok()?, next: f[2].parse().ok()?, wall: wall(f[4])?, off_min: f[5].parse().ok()? })
        }
        "E" => {
            let f: Vec<&str> = rest.splitn(9, ' ').collect();
            if f.len() < 8 {
                return None;
            }
            Some(Item::Event {
                seq: f[0].parse().ok()?,
                time_ns: f[1].parse().ok()?,
                wall: wall(f[2])?,
                level: level_of(f[3])?,
                cpu: f[4].parse().ok()?,
                subsystem: f[6],
                name: f[7],
                text: f.get(8).copied().unwrap_or(""),
            })
        }
        "T" => {
            let f: Vec<&str> = rest.splitn(8, ' ').collect();
            if f.len() < 7 {
                return None;
            }
            Some(Item::Trace {
                cpu: f[0].parse().ok()?,
                pos: f[1].parse().ok()?,
                time_ns: f[2].parse().ok()?,
                wall: wall(f[3])?,
                level: level_of(f[4])?,
                subsystem: f[5],
                name: f[6],
                text: f.get(7).copied().unwrap_or(""),
            })
        }
        "M" => {
            let f: Vec<&str> = rest.splitn(4, ' ').collect();
            if f.len() < 3 {
                return None;
            }
            Some(Item::Metric { round: f[0].parse().ok()?, time_ns: f[1].parse().ok()?, wall: wall(f[2])?, pairs: f.get(3).copied().unwrap_or("").trim() })
        }
        "G" | "g" => {
            let mut w = rest.split(' ');
            let (a, b) = (n(w.next())?, n(w.next())?);
            Some(if kind == "G" { Item::Gap { before: a, count: b } } else { Item::TraceGap { cpu: a as u32, count: b } })
        }
        "F" => Some(Item::Flush { target: rest.trim().parse().ok()? }),
        "I" => {
            let f: Vec<&str> = rest.split(' ').collect();
            if f.len() != 3 {
                return None;
            }
            Some(Item::Idle { next: f[0].parse().ok()?, wall: wall(f[2])? })
        }
        _ => None,
    }
}

/// The text as it was logged (`\\`, `\n`, `\r` undone).
pub fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// `12.5%` -> (12.5, "%"); `3B/s` -> (3, "B/s").
pub fn value(v: &str) -> Option<(f64, &str)> {
    let end = v.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(v.len());
    Some((v[..end].parse().ok()?, &v[end..]))
}

/// The `name=value` pairs of a metric line.
pub fn pairs(p: &str) -> impl Iterator<Item = (&str, f64, &str)> {
    p.split(' ').filter_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        let (x, unit) = value(v)?;
        Some((k, x, unit))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_kind_parses() {
        assert_eq!(parse("B 00000000deadbeef 3 120 5000 1792749600000 120"), Some(Item::Boot { id: 0xdeadbeef, reload: 3, next: 120, wall: Some(1_792_749_600_000), off_min: 120 }));
        assert_eq!(
            parse("E 7 2500000000 1800000002500 W 1 42 pm conn.closed peer=1.2.3.4:443 | two\\nlines"),
            Some(Item::Event { seq: 7, time_ns: 2_500_000_000, wall: Some(1_800_000_002_500), level: 1, cpu: 1, subsystem: "pm", name: "conn.closed", text: "peer=1.2.3.4:443 | two\\nlines" })
        );
        assert!(matches!(parse("E 8 1 - I 0 0 klog boot"), Some(Item::Event { wall: None, text: "", .. })));
        assert!(matches!(parse("T 3 9 2000123000 - W irq trace.irq_storm vector=65 per_s=120000"), Some(Item::Trace { cpu: 3, pos: 9, level: 1, .. })));
        assert!(matches!(parse("M 4 8000000000 1792749600000 cpu.all.busy=12.5% mem.free=100B"), Some(Item::Metric { round: 4, .. })));
        assert_eq!(parse("G 100 4"), Some(Item::Gap { before: 100, count: 4 }));
        assert_eq!(parse("g 2 5"), Some(Item::TraceGap { cpu: 2, count: 5 }));
        assert_eq!(parse("F 99"), Some(Item::Flush { target: 99 }));
        assert_eq!(parse("I 5 77 -"), Some(Item::Idle { next: 5, wall: None }));
        for bad in ["", "X 1", "E 1 2", "B 1 2", "E x 1 - I 0 0 a b", "E 1 1 - Q 0 0 a b"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(unescape("a\\nb\\\\n\\x"), "a\nb\\n\\x");
        let p: Vec<_> = pairs("cpu.all.busy=12.5% net.igc0.rx_bytes=300B/s bad").collect();
        assert_eq!(p, [("cpu.all.busy", 12.5, "%"), ("net.igc0.rx_bytes", 300.0, "B/s")]);
    }
}

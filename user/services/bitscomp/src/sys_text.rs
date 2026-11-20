//! SYS-A: numbers as the program "System" shows them, in German: decimal comma, a space between thousands, binary
//! units named GB/MB/KB (as other desktops' task managers do: 24 GiB of memory reads "24 GB"), and "–" where a
//! source is missing. Never an invented value: every function takes an Option and gives "–" for None.

pub const NONE: &str = "–";
/// Why a tile shows "–": the measurement path is not there (yet).
pub const NO_SOURCE: &str = "Diese Zahl liefert der Kern noch nicht.";

/// `v` with `d` decimals and a decimal comma.
pub fn dec(v: f64, d: usize) -> String {
    let s = format!("{v:.d$}");
    let (int, frac) = s.split_once('.').map_or((s.as_str(), ""), |(a, b)| (a, b));
    let (neg, digits) = int.strip_prefix('-').map_or((false, int), |r| (true, r));
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(' ');
        }
        grouped.push(c);
    }
    let sign = if neg && grouped.chars().any(|c| c != '0' && c != ' ') { "-" } else { "" };
    if frac.is_empty() { format!("{sign}{grouped}") } else { format!("{sign}{grouped},{frac}") }
}

pub fn int(v: u64) -> String {
    dec(v as f64, 0)
}

/// A share in percent: whole numbers from 10 on, one decimal below (the table's "4,1 %").
pub fn pct(v: Option<f64>) -> String {
    match v {
        Some(v) if v >= 9.95 => format!("{} %", dec(v, 0)),
        Some(v) => format!("{} %", dec(v.max(0.0), 1)),
        None => NONE.into(),
    }
}

/// A share rounded to whole percent (the big numbers).
pub fn pct0(v: Option<f64>) -> String {
    v.map_or_else(|| NONE.into(), |v| format!("{} %", dec(v.max(0.0), 0)))
}

/// Bytes with one decimal from GB on and below 10 of a unit ("3,1 GB", "612 MB", "18 MB").
pub fn bytes(v: Option<u64>) -> String {
    let Some(b) = v else { return NONE.into() };
    let units = [("TB", 1u64 << 40), ("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (name, size) in units {
        if b >= size {
            let x = b as f64 / size as f64;
            let d = if name == "GB" || name == "TB" || x < 10.0 { 1 } else { 0 };
            return format!("{} {name}", dec(x, d));
        }
    }
    if b == 0 { "0".into() } else { format!("{b} Byte") }
}

/// A rate in bytes per second ("2,4 MB/s"; nothing worth a KB is "0 KB/s").
pub fn rate(v: Option<f64>) -> String {
    match v {
        None => NONE.into(),
        Some(r) if r < 512.0 => "0 KB/s".into(),
        Some(r) => format!("{}/s", bytes(Some(r as u64))),
    }
}

/// A rate in the task manager's table, where nothing is a plain "0" (as in the design).
pub fn rate_cell(v: Option<f64>) -> String {
    match v {
        Some(r) if r < 512.0 => "0".into(),
        v => rate(v),
    }
}

/// Seconds as "1 Std 54 Min", "3 Min", "45 s", "2 Tage 3 Std".
pub fn duration(secs: Option<f64>) -> String {
    let Some(s) = secs.filter(|s| s.is_finite() && *s >= 0.0) else { return NONE.into() };
    let s = s as u64;
    let (d, h, m) = (s / 86_400, s / 3600 % 24, s / 60 % 60);
    if d > 0 {
        return format!("{d} {} {h} Std", if d == 1 { "Tag" } else { "Tage" });
    }
    if h > 0 {
        return format!("{h} Std {m} Min");
    }
    if m > 0 { format!("{m} Min") } else { format!("{s} s") }
}

pub fn celsius(v: Option<f64>) -> String {
    v.map_or_else(|| NONE.into(), |c| format!("{} °C", dec(c, 0)))
}

pub fn watt(v: Option<f64>) -> String {
    v.map_or_else(|| NONE.into(), |w| format!("{} W", dec(w, if w < 10.0 { 1 } else { 0 })))
}

/// kHz as "3,8 GHz" or "800 MHz".
pub fn khz(v: Option<u64>) -> String {
    match v {
        None => NONE.into(),
        Some(k) if k >= 1_000_000 => format!("{} GHz", dec(k as f64 / 1e6, 1)),
        Some(k) => format!("{} MHz", dec(k as f64 / 1e3, 0)),
    }
}

/// Milliseconds with one decimal ("5,6 ms").
pub fn ms(v: Option<f64>) -> String {
    v.map_or_else(|| NONE.into(), |m| format!("{} ms", dec(m, 1)))
}

/// A wall clock time `secs` (unix) with the local offset as "11:20".
pub fn clock(secs: i64, off_min: i32) -> String {
    let t = (secs + off_min as i64 * 60).rem_euclid(86_400);
    format!("{:02}:{:02}", t / 3600, t / 60 % 60)
}

/// A wall clock date and time as "13.11. 18:32".
pub fn date_time(secs: i64, off_min: i32) -> String {
    let local = secs + off_min as i64 * 60;
    let days = local.div_euclid(86_400);
    let (_, m, d) = civil(days);
    format!("{d}.{m}. {}", clock(secs, off_min))
}

/// Days since 1970-01-01 as (year, month, day) (H. Hinnant's civil_from_days).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn german_numbers_and_units() {
        assert_eq!(dec(8400.0, 0), "8 400");
        assert_eq!(dec(353_450.0, 0), "353 450");
        assert_eq!(dec(0.74, 2), "0,74");
        assert_eq!(dec(-0.04, 1), "0,0");
        assert_eq!(pct(Some(4.12)), "4,1 %");
        assert_eq!(pct(Some(31.4)), "31 %");
        assert_eq!(pct(None), "–");
        assert_eq!(pct0(Some(6.4)), "6 %");
        assert_eq!(bytes(Some(3_328_599_654)), "3,1 GB");
        assert_eq!(bytes(Some(612 << 20)), "612 MB");
        assert_eq!(bytes(Some(18 << 20)), "18 MB");
        assert_eq!(bytes(Some(25_560_000_000)), "23,8 GB");
        assert_eq!(bytes(Some(0)), "0");
        assert_eq!(bytes(Some(1 << 40)), "1,0 TB");
        assert_eq!(bytes(None), "–");
        assert_eq!(rate(Some(2.4 * 1048576.0)), "2,4 MB/s");
        assert_eq!(rate(Some(100.0)), "0 KB/s");
        assert_eq!(rate_cell(Some(100.0)), "0");
        assert_eq!(rate_cell(None), "–");
        assert_eq!(rate(Some(40_960.0)), "40 KB/s");
        assert_eq!(duration(Some(6840.0)), "1 Std 54 Min");
        assert_eq!(duration(Some(45.0)), "45 s");
        assert_eq!(duration(Some(180.0)), "3 Min");
        assert_eq!(duration(Some(97_200.0)), "1 Tag 3 Std");
        assert_eq!(duration(None), "–");
        assert_eq!(celsius(Some(51.6)), "52 °C");
        assert_eq!(watt(Some(18.2)), "18 W");
        assert_eq!(watt(Some(8.94)), "8,9 W");
        assert_eq!(khz(Some(3_812_000)), "3,8 GHz");
        assert_eq!(khz(Some(800_000)), "800 MHz");
        assert_eq!(ms(Some(5.63)), "5,6 ms");
    }

    #[test]
    fn clock_and_date() {
        // 2026-11-13 17:32 UTC, an hour east.
        let t = 1_794_591_120;
        assert_eq!(clock(t, 60), "18:32");
        assert_eq!(date_time(t, 60), "13.11. 18:32");
    }
}

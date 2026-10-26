//! Calendar arithmetic and the time-zone rule for the RTC's local time (pure; host-tested). spec/core.md §7.3:
//! CLOCK_REALTIME is UTC; the RTC holds local time (Windows keeps it so), converted with CONFIG `tz=`. The EU rule
//! (summer time from the last Sunday of March 01:00 UTC to the last Sunday of October 01:00 UTC) covers Georg's
//! Europe/Berlin; the doubled October hour counts as summer (YK-PLAN 5), the missing March hour as winter.

/// A civil date and time (no zone).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime {
    pub year: u32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
}

fn leap(y: u32) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

pub fn days_in_month(y: u32, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

impl DateTime {
    pub fn valid(&self) -> bool {
        (1970..=9999).contains(&self.year)
            && (1..=12).contains(&self.month)
            && self.day >= 1
            && self.day <= days_in_month(self.year, self.month)
            && self.hour < 24
            && self.min < 60
            && self.sec < 60
    }
    /// Seconds since 1970-01-01 00:00 of this date and time read as UTC.
    pub fn unix(&self) -> i64 {
        days_from_civil(self.year as i64, self.month as i64, self.day as i64) * 86_400
            + self.hour as i64 * 3600
            + self.min as i64 * 60
            + self.sec as i64
    }
    pub fn from_unix(secs: i64) -> DateTime {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        DateTime { year: y as u32, month: m as u8, day: d as u8, hour: (rem / 3600) as u8, min: (rem % 3600 / 60) as u8, sec: (rem % 60) as u8 }
    }
}

/// Days since 1970-01-01 (H. Hinnant's `days_from_civil`).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// 0 = Sunday.
fn weekday(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

fn last_sunday(y: u32, m: u8) -> i64 {
    let last = days_from_civil(y as i64, m as i64, days_in_month(y, m) as i64);
    last - weekday(last)
}

/// A zone: a fixed offset, or a standard offset with the EU summer-time rule (+60 minutes in summer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tz {
    Fixed { minutes: i32 },
    Eu { std_minutes: i32 },
}

/// The default (Georg's machines).
pub const EUROPE_BERLIN: Tz = Tz::Eu { std_minutes: 60 };

impl Tz {
    /// Minutes east of UTC at UTC second `utc`.
    pub fn offset_at(&self, utc: i64) -> i32 {
        match *self {
            Tz::Fixed { minutes } => minutes,
            Tz::Eu { std_minutes } => {
                let y = DateTime::from_unix(utc).year;
                let start = last_sunday(y, 3) * 86_400 + 3600;
                let end = last_sunday(y, 10) * 86_400 + 3600;
                if (start..end).contains(&utc) { std_minutes + 60 } else { std_minutes }
            }
        }
    }
    /// UTC seconds of a local date and time. Of two readings (October's doubled hour) the summer one wins; a
    /// local time that does not exist (March's skipped hour) is read with the standard offset.
    pub fn to_utc(&self, local: &DateTime) -> i64 {
        let l = local.unix();
        match *self {
            Tz::Fixed { minutes } => l - minutes as i64 * 60,
            Tz::Eu { std_minutes } => {
                let summer = l - (std_minutes as i64 + 60) * 60;
                if self.offset_at(summer) == std_minutes + 60 {
                    return summer;
                }
                l - std_minutes as i64 * 60
            }
        }
    }
    pub fn describe(&self) -> &'static str {
        match self {
            Tz::Fixed { .. } => "fixed",
            Tz::Eu { .. } => "eu",
        }
    }
}

/// CONFIG `tz=`: a zone name of the EU rule's zones, `UTC`, a POSIX string for the EU rule (`CET-1CEST,...`), or
/// a fixed offset (`UTC+2`, `+01:00`, `-0530`). `None` for anything else (the caller warns and keeps the default).
pub fn parse(s: &str) -> Option<Tz> {
    let s = s.trim();
    let lower = |a: &str| a.eq_ignore_ascii_case(s);
    const CET: [&str; 14] = [
        "europe", "Europe/Berlin", "Europe/Vienna", "Europe/Zurich", "Europe/Paris", "Europe/Amsterdam", "Europe/Brussels",
        "Europe/Luxembourg", "Europe/Copenhagen", "Europe/Oslo", "Europe/Stockholm", "Europe/Rome", "Europe/Madrid", "CET",
    ];
    if CET.iter().any(|z| lower(z)) || s.starts_with("CET-1CEST") {
        return Some(EUROPE_BERLIN);
    }
    if ["Europe/London", "Europe/Dublin", "Europe/Lisbon", "WET"].iter().any(|z| lower(z)) || s.starts_with("GMT0BST") || s.starts_with("WET0WEST") {
        return Some(Tz::Eu { std_minutes: 0 });
    }
    if ["Europe/Helsinki", "Europe/Athens", "Europe/Riga", "Europe/Tallinn", "Europe/Vilnius", "Europe/Sofia", "Europe/Bucharest", "EET"].iter().any(|z| lower(z))
        || s.starts_with("EET-2EEST")
    {
        return Some(Tz::Eu { std_minutes: 120 });
    }
    if ["UTC", "GMT", "Etc/UTC", "Z", "Zulu"].iter().any(|z| lower(z)) {
        return Some(Tz::Fixed { minutes: 0 });
    }
    let rest = s.strip_prefix("UTC").or_else(|| s.strip_prefix("GMT")).unwrap_or(s);
    let (sign, digits) = match rest.as_bytes().first()? {
        b'+' => (1, &rest[1..]),
        b'-' => (-1, &rest[1..]),
        _ => return None,
    };
    let (h, m) = match digits.split_once(':') {
        Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
        None if digits.len() == 4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?),
        None => (digits.parse().ok()?, 0),
    };
    if !(0..=14).contains(&h) || !(0..60).contains(&m) || (h == 14 && m > 0) {
        return None;
    }
    Some(Tz::Fixed { minutes: sign * (h * 60 + m) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(year: u32, month: u8, day: u8, hour: u8, min: u8) -> DateTime {
        DateTime { year, month, day, hour, min, sec: 0 }
    }

    #[test]
    fn calendar_round_trips() {
        assert_eq!(dt(1970, 1, 1, 0, 0).unix(), 0);
        assert_eq!(dt(2000, 3, 1, 0, 0).unix(), 951_868_800);
        assert_eq!(dt(2026, 10, 15, 12, 0).unix(), 1_792_065_600);
        for s in [0i64, 951_868_799, 1_792_065_600, 4_102_444_800, 253_402_300_799] {
            assert_eq!(DateTime::from_unix(s).unix(), s);
        }
        assert!(!dt(2025, 2, 29, 0, 0).valid());
        assert!(dt(2024, 2, 29, 0, 0).valid());
        assert!(!dt(1969, 12, 31, 23, 0).valid());
    }

    #[test]
    fn berlin_offsets_2026() {
        let b = EUROPE_BERLIN;
        // 2026: summer time from 29 March 01:00 UTC to 25 October 01:00 UTC.
        assert_eq!(b.offset_at(dt(2026, 3, 29, 0, 59).unix()), 60);
        assert_eq!(b.offset_at(dt(2026, 3, 29, 1, 0).unix()), 120);
        assert_eq!(b.offset_at(dt(2026, 10, 25, 0, 59).unix()), 120);
        assert_eq!(b.offset_at(dt(2026, 10, 25, 1, 0).unix()), 60);
        assert_eq!(b.offset_at(dt(2026, 10, 15, 12, 0).unix()), 120, "today: summer time");
        assert_eq!(b.offset_at(dt(2026, 12, 24, 18, 0).unix()), 60);
    }

    #[test]
    fn local_to_utc_with_the_doubled_and_the_missing_hour() {
        let b = EUROPE_BERLIN;
        assert_eq!(b.to_utc(&dt(2026, 10, 15, 14, 0)), dt(2026, 10, 15, 12, 0).unix(), "summer: -2 h");
        assert_eq!(b.to_utc(&dt(2026, 1, 10, 9, 30)), dt(2026, 1, 10, 8, 30).unix(), "winter: -1 h");
        // 25 Oct 2026 02:30 local happens twice; the summer reading (00:30 UTC) wins.
        assert_eq!(b.to_utc(&dt(2026, 10, 25, 2, 30)), dt(2026, 10, 25, 0, 30).unix());
        assert_eq!(b.to_utc(&dt(2026, 10, 25, 3, 30)), dt(2026, 10, 25, 2, 30).unix(), "after the doubled hour: winter");
        // 29 Mar 2026 02:30 local does not exist: read as winter time.
        assert_eq!(b.to_utc(&dt(2026, 3, 29, 2, 30)), dt(2026, 3, 29, 1, 30).unix());
        assert_eq!(b.to_utc(&dt(2026, 3, 29, 3, 30)), dt(2026, 3, 29, 1, 30).unix(), "summer from 03:00 local");
    }

    #[test]
    fn zone_names_and_offsets() {
        assert_eq!(parse("Europe/Berlin"), Some(EUROPE_BERLIN));
        assert_eq!(parse("europe"), Some(EUROPE_BERLIN));
        assert_eq!(parse("CET-1CEST,M3.5.0,M10.5.0/3"), Some(EUROPE_BERLIN));
        assert_eq!(parse("Europe/London"), Some(Tz::Eu { std_minutes: 0 }));
        assert_eq!(parse("EET"), Some(Tz::Eu { std_minutes: 120 }));
        assert_eq!(parse("utc"), Some(Tz::Fixed { minutes: 0 }));
        assert_eq!(parse("UTC+2"), Some(Tz::Fixed { minutes: 120 }));
        assert_eq!(parse("+01:00"), Some(Tz::Fixed { minutes: 60 }));
        assert_eq!(parse("-0530"), Some(Tz::Fixed { minutes: -330 }));
        assert_eq!(parse("UTC+15"), None);
        assert_eq!(parse("Mars/Olympus"), None);
        assert_eq!(parse(""), None);
        assert_eq!(Tz::Fixed { minutes: 120 }.to_utc(&dt(2026, 1, 1, 2, 0)), dt(2026, 1, 1, 0, 0).unix());
    }
}

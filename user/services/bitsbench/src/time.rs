//! Wall time as the night rules need it: UTC seconds, the local offset bitscomp answers (it owns the time zone,
//! `ctl idle offset_min=`), the local date and the minute of the day.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// (year, month, day) of a day number since 1970-01-01 (the proleptic Gregorian calendar, eras of 400 years).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Local {
    pub y: i64,
    pub m: u32,
    pub d: u32,
    /// Minutes since local midnight.
    pub minute: u32,
}

impl Local {
    pub fn of(utc_s: i64, offset_min: i32) -> Local {
        let s = utc_s + i64::from(offset_min) * 60;
        let (y, m, d) = civil(s.div_euclid(86_400));
        Local { y, m, d, minute: (s.rem_euclid(86_400) / 60) as u32 }
    }

    /// `2026-11-14`: the name of a night's folder.
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.y, self.m, self.d)
    }
}

/// `14.11.` of a folder name `2026-11-14` (the page's and the report's way to name a night).
pub fn short_date(date: &str) -> String {
    let p: Vec<&str> = date.split('-').collect();
    match p.as_slice() {
        [_, m, d] => format!("{}.{}.", d.trim_start_matches('0'), m.trim_start_matches('0')),
        _ => date.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_minutes_follow_the_offset() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_771), (2026, 11, 14));
        // 2026-11-14 01:30 UTC is 02:30 in Berlin (UTC+1).
        let t = 20_771 * 86_400 + 90 * 60;
        assert_eq!(Local::of(t, 60), Local { y: 2026, m: 11, d: 14, minute: 150 });
        // 23:30 UTC the day before, an hour east: already the next day.
        assert_eq!(Local::of(t - 2 * 3600, 60).date(), "2026-11-14");
        assert_eq!(Local::of(t - 2 * 3600, 0).date(), "2026-11-13");
        assert_eq!(short_date("2026-11-04"), "4.11.");
        assert_eq!(short_date("x"), "x");
    }
}

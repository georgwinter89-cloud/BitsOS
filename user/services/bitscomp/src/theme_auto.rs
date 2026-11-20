//! WY-SET: theme.mode `auto` ("Nach Uhrzeit"): light from theme.auto.light_from to theme.auto.dark_from (whole
//! hours of the local time, Georg 14.11.: 07:00 to 19:00), dark the rest of the day. The theme is built from the
//! values (`Theme::build`); this file says which mode `auto` means now and re-applies the theme when the hour
//! crosses a boundary (checked with the status bar's wake-ups, so no timer of its own).

use std::time::Duration;

use crate::settings::Values;
use crate::state::App;

pub const MODE: &str = "theme.mode";
pub const LIGHT_FROM: &str = "theme.auto.light_from";
pub const DARK_FROM: &str = "theme.auto.dark_from";
const CHECK_EVERY: Duration = Duration::from_secs(20);

/// Whether `auto` is dark at `hour` (0..24): light in [from, to), also across midnight when `from` > `to`;
/// `from` == `to` is never light (no hours given to it).
pub fn dark_at(from: i64, to: i64, hour: i64) -> bool {
    let light = if from <= to { (from..to).contains(&hour) } else { hour >= from || hour < to };
    !light
}

/// Whether the settings mean dark now: `light` is light, `auto` follows the clock, anything else is dark.
pub fn dark_now(v: &Values) -> bool {
    match v.text(MODE).unwrap_or("dark") {
        "light" => false,
        "auto" => dark_at(hours(v, LIGHT_FROM, 7), hours(v, DARK_FROM, 19), local_hour()),
        _ => true,
    }
}

fn hours(v: &Values, k: &str, d: i64) -> i64 {
    v.int(k).unwrap_or(d).clamp(0, 23)
}

/// The hour of the local time (UTC when no time zone is known, as the status bar's clock says then).
pub fn local_hour() -> i64 {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: an all-zero tm is a valid value; localtime_r only reads `secs` and writes into `tm`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return ((secs / 3600) % 24) as i64;
    }
    i64::from(tm.tm_hour)
}

impl App {
    /// With `auto`, the theme follows the clock: a mode that no longer fits the hour applies the settings anew.
    pub fn theme_auto_tick(&mut self, now: Duration) {
        if self.theme_auto_next.is_some_and(|t| now < t) {
            return;
        }
        self.theme_auto_next = Some(now + CHECK_EVERY);
        if self.settings.text(MODE) == Some("auto") && dark_now(&self.settings) != self.theme.dark {
            self.apply_settings("theme.mode auto (Uhrzeit)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_light_between_its_hours_also_across_midnight() {
        assert!(dark_at(7, 19, 6) && !dark_at(7, 19, 7) && !dark_at(7, 19, 18) && dark_at(7, 19, 19) && dark_at(7, 19, 23));
        assert!(!dark_at(22, 6, 23) && !dark_at(22, 6, 0) && dark_at(22, 6, 6) && dark_at(22, 6, 12));
        assert!((0..24).all(|h| dark_at(9, 9, h)), "no light hours");
        let mut v = Values::default();
        assert!(dark_now(&v), "no value: dark");
        v.set(MODE, "light");
        assert!(!dark_now(&v));
        v.set(MODE, "dark");
        assert!(dark_now(&v));
        v.set(MODE, "auto");
        v.set(LIGHT_FROM, "0");
        v.set(DARK_FROM, "0");
        assert!(dark_now(&v), "auto with no light hours is dark");
        v.set(DARK_FROM, "24");
        assert!(dark_now(&v) == dark_at(0, 23, local_hour()), "an hour out of range is clamped");
    }
}

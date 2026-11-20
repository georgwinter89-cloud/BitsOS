//! D5: look for updates daily (`pkg.updates.check`), never install unless `pkg.updates.install = night` (then
//! between 1 and 5 UTC, the old version kept). The check is PK-6e's `update --check` after a refresh; what it
//! finds is the event `pkg.updates.available n=` and the record `update-check` the page reads. R9: nothing is
//! fetched in the first `FIRST_DELAY` after the start, so no QEMU run (the longest is 3000 s) ever reaches a
//! mirror from here; Georg's machine runs long enough for its daily look. And nothing is fetched before somebody
//! asked for the first refresh (a source has an accepted database): a machine nobody installs programs on never
//! talks to a mirror by itself.

use crate::{Ctx, work};
use bitspkg::repo::sources::{Install, Updates};
use bitspkg::repo::{refresh, update};
use std::sync::Arc;
use std::time::Duration;

pub const FIRST_DELAY: Duration = Duration::from_secs(2 * 3600);
pub const TICK: Duration = Duration::from_secs(600);
const CHECK_DEADLINE: Duration = Duration::from_secs(1800);
const NIGHT_DEADLINE: Duration = Duration::from_secs(4 * 3600);

/// A check that did not finish (no record written: a mirror down) is tried again after this, not every tick.
pub const RETRY: i64 = 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    Wait,
    Check,
    InstallNight,
}

/// What the timer itself tried (in memory: a restart may try once more, never a storm).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tried {
    /// Unix seconds of the last check started.
    pub check: Option<i64>,
    /// The UTC day (`now / 86400`) of the last night install: one try per night, done or failed.
    pub night: Option<i64>,
}

/// What the timer does now: `up` since the start, `now` Unix seconds, `hour` the UTC hour, `last` the record,
/// `tried` what it tried before, `loaded` whether a source was ever refreshed.
pub fn next(up: Duration, now: i64, hour: u32, u: Updates, last: Option<(i64, usize)>, tried: Tried, loaded: bool) -> Next {
    if up < FIRST_DELAY || !loaded {
        return Next::Wait;
    }
    if update::check_due(u.check, last.map(|l| l.0), now) && tried.check.is_none_or(|t| now.saturating_sub(t) >= RETRY || now < t) {
        return Next::Check;
    }
    let night_done = tried.night == Some(now.div_euclid(86_400));
    if u.install == Install::Night && !night_done && last.is_some_and(|l| l.1 > 0) && (1..5).contains(&hour) { Next::InstallNight } else { Next::Wait }
}

fn check(cx: &Ctx, install: bool) -> (usize, usize) {
    let st = cx.store();
    let cfg = match cx.config(&st) {
        Ok(c) => c,
        Err(e) => {
            crate::event("pkg.updates.check_failed", &[("reason", &e)]);
            return (0, 1);
        }
    };
    let fetcher = bitspkg::repo::download::Fetcher::standard();
    let ucx = update::Ctx { st: &st, cfg: &cfg, fetcher: &fetcher, now: crate::now() };
    if install {
        let r = update::run(&ucx, &[], None, false);
        crate::event("pkg.updates.installed", &[("failed", &r.failed.to_string())]);
    }
    let rs = match bitspkg::repo::cli::refresh_all(&st, &cfg, &fetcher) {
        Ok(rs) => rs,
        Err(e) => {
            crate::event("pkg.updates.check_failed", &[("reason", &e)]);
            return (0, 1);
        }
    };
    let r = update::run(&ucx, &rs, None, true);
    (r.found, r.failed)
}

pub fn spawn(cx: Arc<Ctx>) {
    let r = std::thread::Builder::new().name("pkg-timer".into()).spawn(move || {
        if let Err(e) = usys::sched_setscheduler(0, usys::SCHED_IDLE, 0) {
            crate::event("pkg.daemon.class", &[("run", "timer"), ("errno", &e.to_string())]);
        }
        let mut tried = Tried::default();
        loop {
            std::thread::sleep(TICK);
            let st = cx.store();
            let Ok(cfg) = cx.config(&st) else { continue };
            let now = crate::now();
            let hour = (now.rem_euclid(86_400) / 3600) as u32;
            let sync = refresh::sync_dir(&st.root);
            let loaded = cfg.sources.iter().any(|src| refresh::read_fresh(&sync, &src.name).accepted > 0);
            let what = next(cx.started.elapsed(), now, hour, cfg.updates, update::last_check(&cx.root), tried, loaded);
            if what == Next::Wait {
                continue;
            }
            let night = what == Next::InstallNight;
            tried.check = Some(now);
            if night {
                tried.night = Some(now.div_euclid(86_400));
            }
            let (label, deadline) = if night { ("night update", NIGHT_DEADLINE) } else { ("update check", CHECK_DEADLINE) };
            match work::run(&cx, label, deadline, work::BUSY_WAIT, move |cx| check(cx, night), &mut |_| {}) {
                Ok((found, failed)) => crate::event("pkg.updates.available", &[("n", &found.to_string()), ("failed", &failed.to_string()), ("run", label)]),
                Err(e) => crate::event("pkg.updates.check_failed", &[("reason", &e)]),
            }
        }
    });
    if let Err(e) = r {
        crate::event("pkg.daemon.timer_thread", &[("reason", &e.to_string())]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitspkg::repo::sources::Check;

    const DAY: i64 = 86_400;
    const T: Tried = Tried { check: None, night: None };

    #[test]
    fn daily_never_installs_and_waits_after_start() {
        let d = Updates::default();
        let up = FIRST_DELAY;
        assert_eq!(next(FIRST_DELAY - Duration::from_secs(1), 10 * DAY, 3, d, None, T, true), Next::Wait, "R9: nothing in the first two hours");
        assert_eq!(next(up, 10 * DAY, 3, d, None, T, true), Next::Check);
        assert_eq!(next(up, 10 * DAY, 3, d, Some((10 * DAY - 3600, 4)), T, true), Next::Wait, "D5: found updates wait for Georg's click");
        assert_eq!(next(up, 10 * DAY, 3, d, Some((9 * DAY, 4)), T, true), Next::Check);
        assert_eq!(next(up, 10 * DAY, 3, d, None, T, false), Next::Wait, "R9: not before the first refresh by hand");
        let never = Updates { check: Check::Never, ..d };
        assert_eq!(next(up, 10 * DAY, 3, never, None, T, true), Next::Wait);
        let night = Updates { install: Install::Night, ..d };
        assert_eq!(next(up, 10 * DAY, 3, night, Some((10 * DAY - 3600, 4)), T, true), Next::InstallNight);
        assert_eq!(next(up, 10 * DAY, 12, night, Some((10 * DAY - 3600, 4)), T, true), Next::Wait, "only at night");
        assert_eq!(next(up, 10 * DAY, 3, night, Some((10 * DAY - 3600, 0)), T, true), Next::Wait);
        // One night install per night: a failed or blocked one (found stays > 0) is not run again every tick.
        let tonight = Tried { check: Some(10 * DAY - 600), night: Some(10) };
        assert_eq!(next(up, 10 * DAY + 600, 3, night, Some((10 * DAY - 3600, 4)), tonight, true), Next::Wait, "tried tonight");
        assert_eq!(next(up, 11 * DAY + 600, 3, night, Some((11 * DAY - 3600, 4)), tonight, true), Next::InstallNight, "the next night again");
        // A check that wrote no record (mirror down) is tried again after RETRY, not every tick.
        let failed = Tried { check: Some(10 * DAY), night: None };
        assert_eq!(next(up, 10 * DAY + 600, 3, d, Some((8 * DAY, 0)), failed, true), Next::Wait);
        assert_eq!(next(up, 10 * DAY + RETRY, 3, d, Some((8 * DAY, 0)), failed, true), Next::Check);
    }
}

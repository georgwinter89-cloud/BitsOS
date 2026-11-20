//! Brave: start, page load and scroll, through its remote debugging protocol (`cdp`). Start: from the launch to
//! the first answer that lists a tab. Load: navigating that tab to the runner's own page (`seite/index.html`,
//! written by bitsbench, never the network) until its load event. Scroll: wheel input events into the page, 60
//! steps 16 ms apart; the value is the wall time per frame the compositor counted meanwhile (`ctl stats`), none
//! when the counters cannot be read or did not move.
//! Brave runs muted (`--mute-audio`) with a profile of its own under /data/leistung/tmp, so Georg's is untouched.
//! Brave is tested only on the Mini-PC (Georg 30.10.); QEMU runs `bitsbench fake brave` in its place.

use super::{Ctx, Fail, ms};
use crate::cdp::Browser;
use mcpreg::Json;
use std::path::Path;
use std::time::{Duration, Instant};

/// A cold start from the stick in QEMU took about a minute (BR-3); the Mini-PC is far below.
const START_LIMIT: Duration = Duration::from_secs(240);
const LOAD_LIMIT: Duration = Duration::from_secs(60);
const WHEEL_STEPS: usize = 60;

const PAGE: &str = include_str!("seite.html");

pub struct Three {
    pub start: Vec<f64>,
    pub load: Vec<f64>,
    /// Err: why the scroll has no value tonight (the first run without one says it).
    pub scroll: Result<Vec<f64>, String>,
}

/// Writes the page (each night, so a changed runner brings its page along).
pub fn page(root: &Path) -> Result<String, String> {
    let p = root.join("seite").join("index.html");
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, PAGE).map_err(|e| e.to_string())?;
    Ok(format!("file://{}", p.display()))
}

fn first_page(b: &mut Browser, end: Instant, stop: &dyn Fn() -> bool) -> Result<String, String> {
    loop {
        if stop() {
            return Err("stopped".into());
        }
        // Each ask is short: a Brave that does not answer yet is asked again, input is looked for between.
        let r = b.call("Target.getTargets", Json::obj(vec![]), None, end.min(Instant::now() + Duration::from_secs(2)), stop);
        let id = r.ok().and_then(|r| {
            r.get("targetInfos").and_then(Json::as_array).and_then(|a| {
                a.iter().find(|t| t.get("type").and_then(Json::as_str) == Some("page")).and_then(|t| t.get("targetId")?.as_str().map(String::from))
            })
        });
        if let Some(id) = id {
            return Ok(id);
        }
        if Instant::now() >= end {
            return Err("Brave öffnete keinen Tab".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn frames(ctx: &Ctx) -> Option<f64> {
    ctx.comp.and_then(|c| c.stats(false).ok()).and_then(|s| crate::comp::get(&s, "frames"))
}

/// ms per frame of the scroll: its wall time over the frames the compositor counted meanwhile. Without readable
/// counters, or when no frame was drawn, there is no value: the number of wheel events is no frame count.
pub fn per_frame(wall_ms: f64, before: Option<f64>, after: Option<f64>) -> Result<f64, String> {
    match (before, after) {
        (Some(a), Some(z)) if z > a => Ok(wall_ms / (z - a)),
        (Some(_), Some(_)) => Err("während des Scrollens wurde kein Bild gezeichnet".into()),
        _ => Err("Bildzähler der Arbeitsfläche nicht lesbar".into()),
    }
}

type One = (f64, f64, Result<f64, String>);

fn one(ctx: &Ctx, url: &str, i: usize) -> Result<One, Fail> {
    // An error that came because input came is a stop, not a missing value.
    let miss = |e: String| if (ctx.stop)() { Fail::Stopped } else { Fail::Missing(format!("Brave: {e}")) };
    let t0 = Instant::now();
    let mut b = Browser::start(&ctx.cfg.brave, &ctx.log(&format!("brave-{i}")), ctx.kids).map_err(miss)?;
    let guard = |b: Browser, r: Result<One, Fail>| {
        b.close(Duration::from_secs(10));
        r
    };
    let target = match first_page(&mut b, t0 + START_LIMIT, ctx.stop) {
        Ok(t) => t,
        Err(e) => return guard(b, Err(miss(e))),
    };
    let start = ms(t0.elapsed());
    if let Err(f) = ctx.check() {
        return guard(b, Err(f));
    }
    let end = Instant::now() + LOAD_LIMIT;
    let session = match b.call("Target.attachToTarget", Json::obj(vec![("targetId", Json::str(&target)), ("flatten", Json::Bool(true))]), None, end, ctx.stop) {
        Ok(r) => r.get("sessionId").and_then(Json::as_str).unwrap_or("").to_string(),
        Err(e) => return guard(b, Err(miss(e))),
    };
    let s = Some(session.as_str());
    if let Err(e) = b.call("Page.enable", Json::obj(vec![]), s, end, ctx.stop) {
        return guard(b, Err(miss(e)));
    }
    b.forget_events();
    let t1 = Instant::now();
    let loaded = b.call("Page.navigate", Json::obj(vec![("url", Json::str(url))]), s, end, ctx.stop).and_then(|_| b.event("Page.loadEventFired", end, ctx.stop));
    if let Err(e) = loaded {
        return guard(b, Err(miss(e)));
    }
    let load = ms(t1.elapsed());
    let f0 = frames(ctx);
    let t2 = Instant::now();
    for _ in 0..WHEEL_STEPS {
        if let Err(f) = ctx.check() {
            return guard(b, Err(f));
        }
        let ev = Json::obj(vec![
            ("type", Json::str("mouseWheel")),
            ("x", Json::Num(400.0)),
            ("y", Json::Num(300.0)),
            ("deltaX", Json::Num(0.0)),
            ("deltaY", Json::Num(100.0)),
        ]);
        if let Err(e) = b.call("Input.dispatchMouseEvent", ev, s, Instant::now() + Duration::from_secs(5), ctx.stop) {
            return guard(b, Err(miss(e)));
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    let wall = ms(t2.elapsed());
    let scroll = per_frame(wall, f0, frames(ctx));
    guard(b, Ok((start, load, scroll)))
}

pub fn run(ctx: &Ctx) -> Result<Three, Fail> {
    if !crate::config::present(&ctx.cfg.brave) {
        return Err(Fail::Missing("Brave ist auf diesem Rechner nicht da".into()));
    }
    let url = page(&ctx.cfg.root).map_err(|e| Fail::Missing(format!("Testseite nicht geschrieben: {e}")))?;
    let mut t = Three { start: Vec::new(), load: Vec::new(), scroll: Ok(Vec::new()) };
    for i in 0..ctx.cfg.runs {
        ctx.check()?;
        let (a, b, c) = one(ctx, &url, i)?;
        t.start.push(a);
        t.load.push(b);
        if let Ok(v) = &mut t.scroll {
            match c {
                Ok(x) => v.push(x),
                Err(why) => t.scroll = Err(why),
            }
        }
        // Brave's helpers end with it; a short pause keeps one run's end out of the next start.
        ctx.sleep(Duration::from_secs(2))?;
    }
    Ok(t)
}

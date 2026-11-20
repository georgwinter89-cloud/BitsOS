//! Desktop frame times from the compositor's own counters: `bitscomp ctl stats` says how long its drawing
//! (`render_avg_ms`) and handing the frame to the screen (`present_avg_ms`) took per frame, averaged over every
//! frame since bitscomp started, so the day's real use is what is measured. The value is their sum, in ms. Nothing
//! is drawn for the bench: no window of Georg's is opened, moved or closed, and the screen does not change.

use super::{Ctx, Fail, Values, repeat};
use crate::comp::{get, pairs};
use std::time::Duration;

/// The value from one `stats` answer (None before the first frame or when a counter is missing).
pub fn value(stats: &str) -> Option<f64> {
    let s = pairs(stats);
    if get(&s, "frames")? <= 0.0 {
        return None;
    }
    Some(get(&s, "render_avg_ms")? + get(&s, "present_avg_ms")?)
}

pub fn run(ctx: &Ctx) -> Values {
    let comp = ctx.comp.ok_or_else(|| Fail::Missing("die Arbeitsfläche antwortet nicht".into()))?;
    let miss = |e: String| Fail::Missing(format!("Arbeitsfläche: {e}"));
    repeat(ctx, |_| {
        let out = comp.send("stats", Duration::from_secs(5)).map_err(miss)?;
        let v = value(&out).ok_or_else(|| miss(format!("noch kein Bild gezeichnet oder unerwartete Antwort {:?}", out.trim())))?;
        ctx.sleep(Duration::from_millis(500))?;
        Ok(v)
    })
}

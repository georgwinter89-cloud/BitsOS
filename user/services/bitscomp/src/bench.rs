//! `bitscomp ctl bench [N]`: the cost of copying frames out (WY-1a: on V2 the copy into fb0's deferred-io RAM,
//! including the page faults after each kernel flush). Full frames and a 256x256 and a 32x32 rectangle (a pointer
//! move), each N times back to back ("hot") and N times one period apart ("paced", as real frames: every page is
//! write-protected again by the kernel's flush in between).
//!
//! It runs as a few steps per loop turn, never as one long call: the loop keeps serving clients, input, signals and
//! Ctrl+Alt+Q meanwhile (B4; a 600-frame bench used to freeze the loop for 30 s). Hot presents go at most
//! `HOT_PER_TURN` per turn; paced ones one per period, woken by the pacer. N is at most `MAX_N`.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use smithay::utils::{Physical, Rectangle};

use crate::backend::Backend;
use crate::render::Compositor;
use crate::state::App;

pub const MAX_N: u32 = 120;
const HOT_PER_TURN: u32 = 4;
const SETS: [(&str, i32); 3] = [("full", 0), ("r256", 256), ("r32", 32)];

pub struct Run {
    stream: UnixStream,
    n: u32,
    set: usize,
    hot_done: u32,
    paced_done: u32,
    hot: Duration,
    paced: Duration,
    /// When the next paced present is due (pacer clock).
    due: Duration,
    out: String,
}

/// Starts the first queued `bench` when none runs (later ones wait for it).
pub fn take(app: &mut App, comp: &Compositor, run: &mut Option<Run>) {
    if run.is_some() {
        return;
    }
    let Some(i) = app.ctl_queue.iter().position(|(c, _)| c.split_whitespace().next() == Some("bench")) else { return };
    let (cmd, stream) = app.ctl_queue.remove(i);
    let n = cmd.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(20u32).clamp(1, MAX_N);
    let (w, h) = comp.size();
    *run = Some(Run {
        stream,
        n,
        set: 0,
        hot_done: 0,
        paced_done: 0,
        hot: Duration::ZERO,
        paced: Duration::ZERO,
        due: crate::pacer::now(),
        out: format!("bench size={w}x{h} n={n}"),
    });
}

/// One turn's share of the bench; the answer goes out when the last set is done. Returns when the loop must wake
/// next for it (None: finished).
pub fn step(app: &mut App, comp: &Compositor, backend: &mut dyn Backend, run: &mut Option<Run>, period: Duration) -> Option<Duration> {
    let r = run.as_mut()?;
    let (w, h) = comp.size();
    let (name, side) = SETS[r.set];
    let (rw, rh) = if side == 0 { (w, h) } else { (side.min(w), side.min(h)) };
    let rect = [Rectangle::<i32, Physical>::new(((w - rw) / 2, (h - rh) / 2).into(), (rw, rh).into())];
    // The bench draws whatever the shadow holds everywhere: the next frame repaints all.
    app.needs_frame = true;
    if r.hot_done < r.n {
        for _ in 0..HOT_PER_TURN.min(r.n - r.hot_done) {
            let t = Instant::now();
            backend.present(comp, &rect);
            r.hot += t.elapsed();
            r.hot_done += 1;
        }
        r.due = crate::pacer::now() + period;
        return Some(if r.hot_done < r.n { crate::pacer::now() } else { r.due });
    }
    if crate::pacer::now() < r.due {
        return Some(r.due);
    }
    let t = Instant::now();
    backend.present(comp, &rect);
    r.paced += t.elapsed();
    r.paced_done += 1;
    r.due += period;
    if r.paced_done < r.n {
        return Some(r.due);
    }
    let ms = |d: Duration| d.as_secs_f64() * 1e3 / r.n as f64;
    let mpx = (rw as f64 * rh as f64) / 1e6;
    let (hot, paced) = (ms(r.hot), ms(r.paced));
    r.out.push_str(&format!(" {name}_hot_ms={hot:.3} {name}_paced_ms={paced:.3} {name}_paced_ms_per_mpx={:.3}", paced / mpx));
    if r.set + 1 < SETS.len() {
        r.set += 1;
        (r.hot_done, r.paced_done, r.hot, r.paced) = (0, 0, Duration::ZERO, Duration::ZERO);
        return Some(crate::pacer::now());
    }
    let mut done = run.take()?;
    // A peer that does not read cannot stall the loop: the answer is one short line, sent with a short timeout.
    let _ = done.stream.set_write_timeout(Some(Duration::from_millis(100)));
    let _ = done.stream.write_all(format!("{}\n", done.out).as_bytes());
    None
}

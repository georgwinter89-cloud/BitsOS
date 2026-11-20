//! A game's frame times: vkQuake's own `timedemo` (the game BitsOS already tests on the Mini-PC, GM-8), started
//! with `-nosound`. Quake prints `<frames> frames <seconds> seconds <fps> fps` when the demo ends; the value is
//! the mean time per frame in ms. The game is ended after that line (it would stay at its console). Games run
//! only on the Mini-PC (Georg 30.10.); QEMU runs `bitsbench fake game` in its place. Georg's Farthest Frontier
//! scene is measured by the desk's tools and entered by hand (`bitsbench enter farthest_frontier MS`).

use super::{Ctx, Fail, Values, log_file, repeat};
use crate::group::Group;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

/// The first start reads 200 MB from the stick and builds shaders (GM-8's bound of a stuck program).
const LIMIT: Duration = Duration::from_secs(300);

/// ms per frame of a timedemo line.
pub fn timedemo(line: &str) -> Option<f64> {
    let w: Vec<&str> = line.split_whitespace().collect();
    let i = w.iter().position(|x| *x == "frames")?;
    let frames: f64 = w.get(i.checked_sub(1)?)?.parse().ok()?;
    let j = w.iter().position(|x| *x == "seconds")?;
    let secs: f64 = w.get(j.checked_sub(1)?)?.parse().ok()?;
    (frames > 0.0 && secs > 0.0 && w.contains(&"fps")).then(|| secs * 1000.0 / frames)
}

fn one(ctx: &Ctx, i: usize) -> Result<f64, Fail> {
    let cmd = &ctx.cfg.game;
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(log_file(&ctx.log(&format!("game-{i}"))));
    let mut child = Group::spawn(&mut c, ctx.kids).map_err(|e| Fail::Missing(format!("Spiel startet nicht ({e})")))?;
    let out = child.child().stdout.take();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let Some(out) = out else { return };
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if let Some(v) = timedemo(&line) {
                let _ = tx.send(v);
                return;
            }
        }
    });
    let end = Instant::now() + LIMIT;
    let r = loop {
        if let Err(f) = ctx.check() {
            break Err(f);
        }
        if let Ok(v) = rx.recv_timeout(Duration::from_millis(250)) {
            break Ok(v);
        }
        if Instant::now() >= end || !matches!(child.child().try_wait(), Ok(None)) {
            break Err(Fail::Missing("das Spiel meldete kein Ergebnis der Zeitmessung".into()));
        }
    };
    child.end(Duration::from_secs(10));
    r
}

pub fn run(ctx: &Ctx) -> Values {
    if ctx.cfg.game.is_empty() || !crate::config::present(&ctx.cfg.game) {
        return Err(Fail::Missing("vkQuake ist auf diesem Rechner nicht da".into()));
    }
    repeat(ctx, |i| {
        let v = one(ctx, i)?;
        ctx.sleep(Duration::from_secs(2))?;
        Ok(v)
    })
}

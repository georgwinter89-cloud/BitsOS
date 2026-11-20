//! Steam: from the launch to its first window in the compositor's list (`ctl list`, app id `steam`; a game's
//! `steam_app_*` never counts), then Steam is asked to end (BitsOS's launcher: `steam --stop`; another Linux:
//! `steam -shutdown`), that is waited for, and the launcher's process group ended. A Steam that already runs is
//! left alone and this benchmark has no value that night. Steam runs only on the Mini-PC (Georg 30.10.); QEMU
//! starts a stand-in window in its place.

use super::{Ctx, Fail, Values, log_file, ms, repeat};
use crate::comp::Comp;
use crate::group::{Group, Kids};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(300);
/// The BitsOS launcher's `--stop` asks Steam, TERMs what is left after 30 s, KILLs it after 40 s and waits up to
/// 15 s more for its own launcher to end: about 60 s at worst.
pub const STOP_LIMIT: Duration = Duration::from_secs(80);

pub fn is_steam(app: &str, want: &str) -> bool {
    let a = app.to_ascii_lowercase();
    !a.starts_with("steam_app_") && a.starts_with(&want.to_ascii_lowercase())
}

fn spawn(cmd: &[String], log: &std::path::Path, kids: &Kids) -> Result<Group, String> {
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdin(Stdio::null()).stdout(log_file(log)).stderr(log_file(log));
    Group::spawn(&mut c, kids).map_err(|e| format!("{}: {e}", cmd[0]))
}

fn window(comp: &Comp, want: &str) -> bool {
    comp.windows().ok().is_some_and(|l| !crate::comp::window_ids(&l, &|a: &str| is_steam(a, want)).is_empty())
}

/// Asks Steam to end with the stop command and waits for that command (it ends Steam itself on BitsOS); another
/// Linux's `steam -shutdown` returns at once, so the launcher and its window are waited for too. Input during the
/// wait changes nothing: Steam is already on its way out.
fn stop(ctx: &Ctx, comp: &Comp, steam: &mut Group, log: &std::path::Path) {
    if ctx.cfg.steam_stop.is_empty() {
        return;
    }
    let Ok(mut stop) = spawn(&ctx.cfg.steam_stop, log, ctx.kids) else { return };
    let end = Instant::now() + STOP_LIMIT;
    while Instant::now() < end {
        let stopping = matches!(stop.child().try_wait(), Ok(None));
        let up = matches!(steam.child().try_wait(), Ok(None)) && window(comp, &ctx.cfg.steam_window);
        if !stopping && !up {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    stop.end(Duration::from_secs(5));
}

fn one(ctx: &Ctx, i: usize) -> Result<f64, Fail> {
    let comp = ctx.comp.ok_or_else(|| Fail::Missing("die Arbeitsfläche antwortet nicht".into()))?;
    let want = &ctx.cfg.steam_window;
    let list = comp.windows().map_err(Fail::Missing)?;
    if !crate::comp::window_ids(&list, &|a: &str| is_steam(a, want)).is_empty() {
        return Err(Fail::Missing("Steam lief schon, der Start wurde nicht gemessen".into()));
    }
    let log = ctx.log(&format!("steam-{i}"));
    let t0 = Instant::now();
    let mut child = spawn(&ctx.cfg.steam, &log, ctx.kids).map_err(|e| Fail::Missing(format!("Steam startet nicht ({e})")))?;
    let r = loop {
        if let Err(f) = ctx.check() {
            break Err(f);
        }
        if window(comp, want) {
            break Ok(ms(t0.elapsed()));
        }
        if t0.elapsed() >= LIMIT {
            break Err(Fail::Missing("Steam zeigte kein Fenster".into()));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    stop(ctx, comp, &mut child, &log);
    child.end(Duration::from_secs(10));
    r
}

pub fn run(ctx: &Ctx) -> Values {
    if ctx.cfg.steam.is_empty() || !crate::config::present(&ctx.cfg.steam) {
        return Err(Fail::Missing("Steam ist auf diesem Rechner nicht da".into()));
    }
    repeat(ctx, |i| {
        let v = one(ctx, i)?;
        ctx.sleep(Duration::from_secs(3))?;
        Ok(v)
    })
}

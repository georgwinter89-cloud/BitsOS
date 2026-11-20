//! The benchmarks of the set. Each takes the night's context and gives one value per run, or says why it has
//! none (`Missing`, plain German), or stops because input came (`Stopped`: the night is thrown away).

pub mod boot;
pub mod brave;
pub mod desktop;
pub mod files;
pub mod game;
pub mod memory;
pub mod steam;

use crate::comp::Comp;
use crate::config::Config;
use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Fail {
    Stopped,
    Missing(String),
}

pub type Values = Result<Vec<f64>, Fail>;

pub struct Ctx<'a> {
    pub cfg: &'a Config,
    pub comp: Option<&'a Comp>,
    /// True once input came (checked between and inside the runs).
    pub stop: &'a dyn Fn() -> bool,
    /// The night's folder (logs of the programs go there).
    pub dir: PathBuf,
    /// Where the programs the run starts are noted (ended at the next start if this run is killed).
    pub kids: &'a crate::group::Kids,
}

impl Ctx<'_> {
    pub fn check(&self) -> Result<(), Fail> {
        if (self.stop)() { Err(Fail::Stopped) } else { Ok(()) }
    }

    /// Sleeps `d` in short steps, stopping early when input came.
    pub fn sleep(&self, d: Duration) -> Result<(), Fail> {
        let end = Instant::now() + d;
        loop {
            self.check()?;
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(Duration::from_millis(250)));
        }
    }

    pub fn log(&self, name: &str) -> PathBuf {
        self.dir.join("logs").join(format!("{name}.log"))
    }
}

/// SIGTERM to the child's process group, then SIGKILL after `grace`; always reaped.
pub fn end_group(child: &mut Child, grace: Duration) {
    let pid = child.id() as isize;
    let end = Instant::now() + grace;
    if matches!(child.try_wait(), Ok(None)) {
        let _ = usys::kill(-pid, 15);
        while Instant::now() < end {
            if !matches!(child.try_wait(), Ok(None)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    // Leftovers of the group (helpers the launcher started) go too.
    let _ = usys::kill(-pid, 9);
    let _ = child.kill();
    let _ = child.wait();
}

/// `runs` values of `one`, checking for input before each.
pub fn repeat(ctx: &Ctx, mut one: impl FnMut(usize) -> Result<f64, Fail>) -> Values {
    let mut v = Vec::with_capacity(ctx.cfg.runs);
    for i in 0..ctx.cfg.runs {
        ctx.check()?;
        v.push(one(i)?);
    }
    Ok(v)
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// A log file for a program, its folder made (None: output goes nowhere).
pub fn log_file(p: &std::path::Path) -> std::process::Stdio {
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    std::fs::OpenOptions::new().create(true).append(true).open(p).map_or_else(|_| std::process::Stdio::null(), std::process::Stdio::from)
}

//! One run of the whole set: sound off, the profiler on, every benchmark of the catalog in order (5 runs each),
//! then sound back, the profiler's report, the results, the history and the morning line. Input during the run
//! (bitscomp's count of physical input moved) stops it at once: the programs it started end, the sound comes
//! back, and the night's unfinished folder is removed (Georg 13.11.: a partial night is thrown away).

use crate::bench::{self, Ctx, Fail};
use crate::comp::Comp;
use crate::config::Config;
use crate::events::{event, q};
use crate::group::Kids;
use crate::store::{self, Night, Outcome};
use crate::ton::Mixer;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq)]
pub enum End {
    Done { night: String, line: String },
    Stopped,
    Refused(String),
}

/// How often the run asks bitscomp whether input came.
const POLL: Duration = Duration::from_millis(500);

pub struct Run<'a> {
    pub cfg: &'a Config,
    pub comp: Option<&'a Comp>,
    pub mixer: &'a dyn Mixer,
    /// Without a sound card that can be muted the run refuses (BitsOS); the CachyOS run may go without (its
    /// guide has Georg turn the sound off himself).
    pub need_mute: bool,
}

/// Grace of the groups a killed run left behind.
const REAP_GRACE: Duration = Duration::from_secs(10);

/// What a run that was killed or lost its power left behind, removed by the next holder of `run.pid`: the
/// programs it started, its unfinished night folders of any date, the test folder (Brave's profile, the read and
/// mix files) and the memory benchmark's fill in /tmp of a runner that is gone. Returns how many things went.
pub fn tidy(root: &std::path::Path) -> usize {
    let mut n = crate::group::reap(&root.join("run.children"), REAP_GRACE);
    if let Ok(rd) = std::fs::read_dir(store::bench_dir(root)) {
        for e in rd.flatten() {
            if e.file_name().to_str().is_some_and(|f| f.ends_with(".neu")) && std::fs::remove_dir_all(e.path()).is_ok() {
                n += 1;
            }
        }
    }
    if std::fs::remove_dir_all(root.join("tmp")).is_ok() {
        n += 1;
    }
    n + crate::bench::memory::tidy_fills(std::path::Path::new("/tmp"))
}

impl Run<'_> {
    pub fn go(&self) -> End {
        let root = &self.cfg.root;
        let start_desk = self.comp.and_then(|c| c.idle().ok());
        // Without the desktop's count at the start no input could ever stop the run: no run.
        if self.comp.is_some() && start_desk.is_none() {
            return End::Refused("die Arbeitsfläche antwortet nicht".into());
        }
        let left = tidy(root);
        if left > 0 {
            event("run.tidied", &format!("things={left}"));
        }
        let kids = Kids::at(root);
        let offset = start_desk.map_or(0, |d| d.offset_min);
        let started_ms = crate::time::now_ms();
        let date = crate::time::Local::of(crate::time::now_s(), offset).date();
        // The night is measured into `<date>.neu` and takes its name only when it is complete: a stopped run never
        // touches a night measured earlier on the same date.
        let done_dir = store::bench_dir(root).join(&date);
        let dir = store::bench_dir(root).join(format!("{date}.neu"));
        if let Err(e) = std::fs::create_dir_all(dir.join("logs")) {
            return End::Refused(format!("{}: {e}", dir.display()));
        }
        crate::ton::recover(self.mixer, root);
        let muted = match crate::ton::mute(self.mixer, root) {
            Ok(m) => Some(m),
            Err(e) if self.need_mute => {
                let _ = std::fs::remove_dir_all(&dir);
                return End::Refused(format!("Ton nicht stumm: {e}"));
            }
            Err(e) => {
                event("sound.not_muted", &format!("why={}", q(&e)));
                None
            }
        };
        event("run.start", &format!("night={date} dir={}", dir.display()));
        let stopped = Cell::new(false);
        let last = RefCell::new(Instant::now() - POLL);
        let stop = || {
            if stopped.get() {
                return true;
            }
            let (Some(c), Some(d0)) = (self.comp, start_desk) else { return false };
            if last.borrow().elapsed() < POLL {
                return false;
            }
            *last.borrow_mut() = Instant::now();
            // A desktop that stops answering cannot say whether someone came: the safe side is to stop.
            let came = c.idle().map_or(true, |d| crate::rules::input_came(&d0, &d));
            stopped.set(came);
            came
        };
        let prof = crate::prof::start(&self.cfg.bitsprof, &dir, &kids);
        let ctx = Ctx { cfg: self.cfg, comp: self.comp, stop: &stop, dir: dir.clone(), kids: &kids };
        let mut benches = BTreeMap::new();
        let mut take = |id: &str, r: Result<Vec<f64>, Fail>| -> bool {
            match r {
                Ok(v) => {
                    event("bench.done", &format!("id={id} runs={} median={:.3}", v.len(), crate::stats::median(&v).unwrap_or(0.0)));
                    benches.insert(id.to_string(), Outcome::Runs(v));
                    true
                }
                Err(Fail::Missing(why)) => {
                    event("bench.missing", &format!("id={id} why={}", q(&why)));
                    benches.insert(id.to_string(), Outcome::Missing(why));
                    true
                }
                Err(Fail::Stopped) => false,
            }
        };
        let mut ok = true;
        for b in crate::catalog::SET.iter().filter(|b| !b.by_hand && self.cfg.wants(b.id)) {
            if !ok || stop() {
                ok = false;
                break;
            }
            ok = match b.id {
                "boot" => take("boot", bench::boot::run(&ctx)),
                "brave_start" => match bench::brave::run(&ctx) {
                    Ok(t) => take("brave_start", Ok(t.start)) && take("brave_load", Ok(t.load)) && take("brave_scroll", t.scroll.map_err(Fail::Missing)),
                    Err(Fail::Missing(w)) => ["brave_start", "brave_load", "brave_scroll"].iter().all(|id| take(id, Err(Fail::Missing(w.clone())))),
                    Err(Fail::Stopped) => false,
                },
                // Measured with Brave's start.
                "brave_load" | "brave_scroll" => true,
                "desktop_frame" => take("desktop_frame", bench::desktop::run(&ctx)),
                "game_frame" => take("game_frame", bench::game::run(&ctx)),
                "steam_start" => take("steam_start", bench::steam::run(&ctx)),
                "file_read" => take("file_read", bench::files::read(&ctx)),
                "file_mix" => take("file_mix", bench::files::mix(&ctx)),
                "memory_load" => take("memory_load", bench::memory::run(&ctx)),
                _ => true,
            };
        }
        let _ = std::fs::remove_dir_all(self.cfg.tmp());
        if !ok || stop() {
            if let Ok(p) = prof {
                p.abandon();
            }
            drop(muted);
            kids.clear();
            let _ = std::fs::remove_dir_all(&dir);
            event("run.stopped", &format!("night={date} why=input"));
            return End::Stopped;
        }
        drop(muted);
        let profile = prof.and_then(|p| p.finish(&kids));
        kids.clear();
        let _ = std::fs::remove_dir_all(&done_dir);
        if let Err(e) = std::fs::rename(&dir, &done_dir) {
            let _ = std::fs::remove_dir_all(&dir);
            return End::Refused(format!("{}: {e}", done_dir.display()));
        }
        let night = Night {
            night: date.clone(),
            started_ms,
            ended_ms: crate::time::now_ms(),
            generation: crate::probe::generation(),
            origin: crate::probe::origin(),
            system: if self.cfg.bitsos { "bitsos".into() } else { "linux".into() },
            benches,
            profile,
        };
        match finish(self.cfg, &night) {
            Ok(line) => End::Done { night: date, line },
            Err(e) => End::Refused(e),
        }
    }
}

/// Results, history, the morning line and its file; old nights pruned. Returns the line.
pub fn finish(cfg: &Config, night: &Night) -> Result<String, String> {
    let root = &cfg.root;
    let dir = store::bench_dir(root).join(&night.night);
    store::write_json(&dir.join("results.json"), &night.to_json())?;
    if !cfg.bitsos {
        // The CachyOS run is the comparison, not a night of BitsOS's history.
        return Ok(format!("Vergleichsmessung gespeichert: {}", dir.join("results.json").display()));
    }
    let history = store::load_history(root);
    let before: Vec<store::Entry> = history.iter().filter(|e| e.night < night.night).cloned().collect();
    let (medians, few) = (night.medians(), night.few(cfg.runs));
    let tonight = crate::report::Tonight { medians: &medians, few: &few, generation: night.generation, origin: &night.origin };
    let line = crate::report::line(&tonight, &before);
    store::write_json(&store::bench_dir(root).join("history.json"), &store::history_to_json(&store::history_with(history, store::Entry::of(night))))?;
    store::write_json(&store::bench_dir(root).join("report.json"), &store::report_json(&night.night, &line, crate::time::now_ms()))?;
    for gone in store::prune(root) {
        event("night.pruned", &format!("night={gone}"));
    }
    event("report", &format!("night={} text={}", night.night, q(&line)));
    Ok(line)
}

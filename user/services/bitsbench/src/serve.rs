//! `bitsbench serve`: bitscomp starts it once per session (uid 1000, like the music player). Every minute it
//! reads the settings and the facts and starts the set when the rules allow (`rules::decide`). A refused run
//! (no sound card to mute, the folder not writable) is not tried again that night; a run stopped by input may
//! start again once nobody used the machine for 30 minutes. Only one runner at a time (`claim`).

use crate::comp::Comp;
use crate::config::Config;
use crate::events::{event, q};
use crate::night::{End, Run};
use crate::rules::{self, Facts};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const TICK: Duration = Duration::from_secs(60);

/// A claim on `path` (pid inside): one runner and one run at a time. A claim whose process is gone is taken over.
pub struct Claim(PathBuf);

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Whether `pid` is a bitsbench that still runs.
pub fn alive(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains("bitsbench"))
}

pub fn claim(path: &Path) -> Result<Claim, String> {
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                return Ok(Claim(path.to_path_buf()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let pid = std::fs::read_to_string(path).ok().and_then(|t| t.trim().parse::<u32>().ok());
                if let Some(p) = pid.filter(|p| *p != std::process::id() && alive(*p)) {
                    return Err(format!("another bitsbench (pid {p}) holds {}", path.display()));
                }
                let _ = std::fs::remove_file(path);
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Err(format!("{}: cannot be claimed", path.display()))
}

/// When the last run that counts for the night ended or was refused (UTC s), from report.json.
fn last_done(root: &Path) -> Option<i64> {
    let j = crate::store::read_json(&crate::store::bench_dir(root).join("report.json")).ok()??;
    j.get("done_ms").and_then(mcpreg::Json::as_u64).map(|ms| (ms / 1000) as i64)
}

pub fn serve(root: &Path) -> i32 {
    if let Err(e) = std::fs::create_dir_all(root) {
        event("serve.no_dir", &format!("dir={} why={}", root.display(), q(&e.to_string())));
    }
    let _claim = match claim(&root.join("serve.pid")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bitsbench: {e}");
            return 1;
        }
    };
    let bitsos = crate::probe::is_bitsos();
    let comp = Comp::from_env();
    event("serve.start", &format!("dir={} ctl={}", root.display(), comp.path.display()));
    // What a killed run left (its programs, its sound switched off, its files) is put right, unless a run is in
    // progress right now (a manual `bitsbench run` holds run.pid and its own sound and files).
    if let Ok(_run) = claim(&root.join("run.pid")) {
        crate::ton::recover(&crate::ton::System, root);
        let n = crate::night::tidy(root);
        if n > 0 {
            event("serve.tidied", &format!("things={n}"));
        }
    }
    let mut booted = None;
    let mut refused_at: Option<i64> = None;
    let mut rx_last: Option<(Instant, u64)> = None;
    let mut said = String::new();
    loop {
        if booted.is_none() && bitsos {
            booted = crate::bench::boot::record(root);
            if let Some(ms) = booted {
                event("boot.noted", &format!("ms={ms}"));
            }
        }
        let (s, note) = crate::settings::load(&crate::settings::socket());
        let rx = crate::probe::rx_bytes(Path::new("/sys/class/net"));
        let rx_bps = rx_last.map(|(t, b)| rx.saturating_sub(b) as f64 / t.elapsed().as_secs_f64().max(1.0));
        rx_last = Some((Instant::now(), rx));
        let facts = Facts {
            now_s: crate::time::now_s(),
            desk: comp.idle().ok(),
            power: crate::probe::power(),
            rx_bps,
            last_done_s: last_done(root).max(refused_at),
            sound: crate::probe::sound_playing(),
        };
        let verdict = rules::decide(&s, &facts);
        let word = match verdict {
            Ok(()) => "run".to_string(),
            Err(w) => w.word().to_string(),
        };
        if word != said {
            event("wait", &format!("state={word}{}", note.as_deref().map(|n| format!(" settings={}", q(n))).unwrap_or_default()));
            said = word;
        }
        if verdict.is_ok() {
            let cfg = Config::load(root, bitsos);
            let end = match claim(&root.join("run.pid")) {
                Ok(_run) => Run { cfg: &cfg, comp: Some(&comp), mixer: &crate::ton::System, need_mute: true }.go(),
                Err(e) => End::Refused(e),
            };
            match end {
                End::Done { night, line } => event("run.done", &format!("night={night} text={}", q(&line))),
                End::Stopped => {}
                End::Refused(why) => {
                    event("run.refused", &format!("why={}", q(&why)));
                    refused_at = Some(crate::time::now_s());
                }
            }
            said.clear();
        }
        std::thread::sleep(TICK);
    }
}

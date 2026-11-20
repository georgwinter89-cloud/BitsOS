//! The profiler beside the set (contract with lane SO): `bitsprof run --out DIR --hz 1000 --seconds N` samples the
//! whole system until SIGTERM or N seconds and writes DIR/profile.bin and DIR/meta.json; `bitsprof report DIR --top N --json` names the
//! hottest functions, kernel and per program. The runner only starts and stops it and keeps the report as
//! `profil/report.json` next to the night's results.

use crate::bench::log_file;
use crate::group::{Group, Kids};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SUB: &str = "profil";
const STOP_GRACE: Duration = Duration::from_secs(30);
const REPORT_LIMIT: Duration = Duration::from_secs(120);
const REPORT_MAX: u64 = 4 << 20;
pub const TOP: u32 = 30;
/// The sampling ends by itself after this long even when nobody stops it (a runner killed mid-run): longer than
/// any night's set, short enough that an orphan never samples into Georg's day.
pub const LIMIT_S: u64 = 4 * 3600;

pub struct Prof {
    cmd: Vec<String>,
    child: Group,
    dir: PathBuf,
}

/// The arguments of the sampling run after the profiler's own command.
pub fn run_args(dir: &Path) -> Vec<String> {
    vec!["run".into(), "--out".into(), dir.display().to_string(), "--hz".into(), "1000".into(), "--seconds".into(), LIMIT_S.to_string()]
}

pub fn start(cmd: &[String], night: &Path, kids: &Kids) -> Result<Prof, String> {
    if cmd.is_empty() || !crate::config::present(cmd) {
        return Err("Der Profiler ist auf diesem Rechner nicht da.".into());
    }
    let dir = night.join(SUB);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let log = log_file(&night.join("logs").join("bitsprof.log"));
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).args(run_args(&dir)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    let child = Group::spawn(&mut c, kids).map_err(|e| format!("Der Profiler startete nicht ({e})."))?;
    crate::events::event("profiler.start", &format!("pid={} out={}", child.id(), dir.display()));
    Ok(Prof { cmd: cmd.to_vec(), child, dir })
}

impl Prof {
    /// Stops the sampling (SIGTERM: it writes its files) and keeps the report. Ok: the report's path relative to
    /// the night's folder.
    pub fn finish(self, kids: &Kids) -> Result<String, String> {
        self.child.end(STOP_GRACE);
        let mut c = Command::new(&self.cmd[0]);
        c.args(&self.cmd[1..]).arg("report").arg(&self.dir).args(["--top", &TOP.to_string(), "--json"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut g = Group::spawn(&mut c, kids).map_err(|e| format!("Der Bericht des Profilers kam nicht ({e})."))?;
        let child = g.child();
        let mut out = child.stdout.take().ok_or("no pipe")?;
        let reader = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = (&mut out).take(REPORT_MAX).read_to_end(&mut v);
            v
        });
        let end = Instant::now() + REPORT_LIMIT;
        while Instant::now() < end && matches!(child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let ok = matches!(child.try_wait(), Ok(Some(s)) if s.success());
        g.end(Duration::from_secs(1));
        let text = reader.join().unwrap_or_default();
        if !ok || text.is_empty() {
            return Err("Der Profiler gab keinen Bericht.".into());
        }
        std::fs::write(self.dir.join("report.json"), &text).map_err(|e| e.to_string())?;
        crate::events::event("profiler.report", &format!("bytes={}", text.len()));
        Ok(format!("{SUB}/report.json"))
    }

    pub fn abandon(self) {
        self.child.end(Duration::from_secs(2));
    }
}

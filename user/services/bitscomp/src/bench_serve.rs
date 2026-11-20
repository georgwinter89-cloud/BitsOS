//! SELFOPT-1: bitscomp starts the nightly benchmark runner `/bin/bitsbench serve` once per session, as the
//! session's uid 1000 (it starts programs into this desktop and reads its `ctl idle`), on a thread of its own, the
//! way `sofa_music_serve` keeps the music player: exit 1 is another runner (not restarted), any other end restarts
//! after a wait that doubles up to a minute. Whether it measures anything is its own business (settings key
//! `leistung.nachts`, on by default); idle it only looks once a minute.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::sofa_music_serve::{next_wait, restart};

pub const EXE: &str = "/bin/bitsbench";

/// The runner's program: `/bin/bitsbench` when it is there.
pub fn exe() -> Option<PathBuf> {
    Some(PathBuf::from(EXE)).filter(|p| crate::spawn::executable(p))
}

pub fn start(euid: u32, socket: &str) {
    let Some(exe) = exe() else { return };
    if euid != 1000 {
        eprintln!("bitscomp: bench: not starting {} as uid {euid} (only the session's uid 1000)", exe.display());
        return;
    }
    let socket = socket.to_string();
    if let Err(e) = std::thread::Builder::new().name("bitscomp-benchd".into()).spawn(move || keep(&exe, &socket)) {
        eprintln!("bitscomp: bench: no keeper thread: {e} (no night measurements this session)");
    }
}

fn keep(exe: &Path, socket: &str) {
    let mut wait = Duration::ZERO;
    loop {
        let began = Instant::now();
        let code = match std::process::Command::new(exe).arg("serve").env("WAYLAND_DISPLAY", socket).stdin(std::process::Stdio::null()).spawn() {
            Ok(mut c) => {
                eprintln!("bitscomp: bench: started {} serve (pid {})", exe.display(), c.id());
                match c.wait() {
                    Ok(s) => s.code(),
                    Err(e) => {
                        eprintln!("bitscomp: bench: waiting for the runner failed: {e} (not restarted)");
                        return;
                    }
                }
            }
            Err(e) => {
                eprintln!("bitscomp: bench: {} did not start: {e} (not restarted)", exe.display());
                return;
            }
        };
        if !restart(code) {
            eprintln!("bitscomp: bench: another runner already holds its folder (exit 1), not restarted");
            return;
        }
        wait = next_wait(wait, began.elapsed());
        eprintln!("bitscomp: bench: runner ended ({}), restart in {} s", code.map_or("signal".to_string(), |c| format!("exit {c}")), wait.as_secs());
        std::thread::sleep(wait);
    }
}

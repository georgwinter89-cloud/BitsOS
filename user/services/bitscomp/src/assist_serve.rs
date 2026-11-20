//! CL-3b: bitscomp starts the assistant's helper `/bin/assist serve` once per session, as the session's own uid
//! 1000 (never as root; the keys are bound to the program /bin/assist of that user, bitscomp never reads one), on
//! a thread of its own (B4), as `sofa_music_serve` starts the player. Its stderr is bitscomp's. Any end restarts
//! it after the player's doubling wait (1 s up to 60 s, back to 1 s once a run lasted a minute). Exit code 1
//! means the kernel named this user's /bin/assist as the listener that already answers on the socket (the
//! helper of a bitscomp before this one; a listener that is anybody else's is replaced by the helper itself
//! and never ends it with 1). That does not end the keeper: it waits while the reader's guard (`assist_peer`)
//! serves an accepted helper and tries again otherwise. Every other end (2: no socket could be made, 3: the
//! helper's worker thread ended, a signal) is a plain restart after the wait.
//!
//! The helper calls `listen` in the very process started here (assist `serve/bind.rs`: no fork on the way),
//! which is what lets the guard accept it by its pid.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::assist_peer::Guard;
use crate::sofa_music_serve::next_wait;

pub const EXE: &str = "/bin/assist";
/// How often the keeper looks whether the helper that answers instead of its own is still there.
const LOOK: Duration = Duration::from_secs(5);

/// The helper's program: `/bin/assist` when it is there; on BitsOS the helper is the one program the keys belong to.
pub fn exe() -> Option<PathBuf> {
    Some(PathBuf::from(EXE)).filter(|p| crate::spawn::executable(p))
}

/// Starts the keeper thread when there is a helper and bitscomp runs as uid 1000.
pub fn start(euid: u32, guard: Arc<Guard>) {
    let Some(exe) = exe() else { return };
    if euid != 1000 {
        eprintln!("bitscomp: assist: not starting {} as uid {euid} (only the session's uid 1000)", exe.display());
        return;
    }
    if let Err(e) = std::thread::Builder::new().name("bitscomp-assistd".into()).spawn(move || keep(&exe, &guard)) {
        eprintln!("bitscomp: assist: no keeper thread: {e} (the sidebar says the assistant does not answer)");
    }
}

fn keep(exe: &Path, guard: &Guard) {
    let mut wait = Duration::ZERO;
    let mut last = None::<Option<i32>>;
    loop {
        let began = Instant::now();
        let code = match std::process::Command::new(exe).arg("serve").stdin(std::process::Stdio::null()).spawn() {
            Ok(mut c) => {
                // The socket's guard accepts this very process as the one that listens.
                guard.set_helper(c.id());
                if last != Some(Some(1)) {
                    eprintln!("bitscomp: assist: started {} serve (pid {})", exe.display(), c.id());
                }
                let end = c.wait();
                guard.set_helper(0);
                match end {
                    Ok(s) => s.code(),
                    Err(e) => {
                        eprintln!("bitscomp: assist: waiting for the helper failed: {e} (not restarted)");
                        return;
                    }
                }
            }
            Err(e) => {
                eprintln!("bitscomp: assist: {} did not start: {e} (not restarted)", exe.display());
                return;
            }
        };
        wait = next_wait(wait, began.elapsed());
        if code == Some(1) {
            // An assistant already answers on the socket: the helper of a bitscomp before this one, which
            // the guard accepts and the reader is served by. The keeper only waits until it is gone. Should
            // the guard not accept it after all (the file at /bin/assist was replaced meanwhile), the own
            // helper is tried again and again. The same end is written once.
            if last != Some(code) {
                eprintln!("bitscomp: assist: an assistant already answers on the socket (exit 1): the helper is tried again while nobody accepted answers there");
            }
            last = Some(code);
            std::thread::sleep(wait);
            while guard.served() {
                std::thread::sleep(LOOK);
            }
            continue;
        }
        last = Some(code);
        eprintln!("bitscomp: assist: helper ended ({}), restart in {} s", code.map_or("signal".to_string(), |c| format!("exit {c}")), wait.as_secs());
        std::thread::sleep(wait);
    }
}

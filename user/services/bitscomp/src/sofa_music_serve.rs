//! INT-GM6 (GM-6m's change request 1): bitscomp starts the music player `/bin/bitsmusik serve` once per session,
//! as the session's own uid 1000 (never as root: init's services run as root), on a thread of its own (B4). Its
//! stderr is bitscomp's: those lines are its B6 events (a uid-1000 process cannot write /dev/kmsg). Exit code 1
//! means another instance already answers on the socket: no restart. Any other end restarts it after a wait that
//! doubles from 1 s up to 60 s, and starts again at 1 s once a run lasted a minute.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const EXE: &str = "/bin/bitsmusik";
const FIRST: Duration = Duration::from_secs(1);
const MOST: Duration = Duration::from_secs(60);

/// The player's program: `/bin/bitsmusik` when it is there.
pub fn exe() -> Option<PathBuf> {
    Some(PathBuf::from(EXE)).filter(|p| crate::spawn::executable(p))
}

/// The wait before the next start, after a run of `ran` that followed a wait of `prev`.
pub fn next_wait(prev: Duration, ran: Duration) -> Duration {
    if ran >= MOST { FIRST } else { (prev * 2).clamp(FIRST, MOST) }
}

/// What to do after the player ended with `code` (None: by a signal).
pub fn restart(code: Option<i32>) -> bool {
    code != Some(1)
}

/// Starts the keeper thread when there is a player and bitscomp runs as uid 1000.
pub fn start(euid: u32) {
    let Some(exe) = exe() else { return };
    if euid != 1000 {
        eprintln!("bitscomp: music: not starting {} as uid {euid} (only the session's uid 1000)", exe.display());
        return;
    }
    if let Err(e) = std::thread::Builder::new().name("bitscomp-musikd".into()).spawn(move || keep(&exe)) {
        eprintln!("bitscomp: music: no keeper thread: {e} (the music card stays greyed)");
    }
}

fn keep(exe: &Path) {
    let mut wait = Duration::ZERO;
    loop {
        let began = Instant::now();
        let code = match std::process::Command::new(exe).arg("serve").stdin(std::process::Stdio::null()).spawn() {
            Ok(mut c) => {
                eprintln!("bitscomp: music: started {} serve (pid {})", exe.display(), c.id());
                match c.wait() {
                    Ok(s) => s.code(),
                    Err(e) => {
                        eprintln!("bitscomp: music: waiting for the player failed: {e} (not restarted)");
                        return;
                    }
                }
            }
            Err(e) => {
                eprintln!("bitscomp: music: {} did not start: {e} (not restarted)", exe.display());
                return;
            }
        };
        if !restart(code) {
            eprintln!("bitscomp: music: another player already answers (exit 1), not restarted");
            return;
        }
        wait = next_wait(wait, began.elapsed());
        eprintln!("bitscomp: music: player ended ({}), restart in {} s", code.map_or("signal".to_string(), |c| format!("exit {c}")), wait.as_secs());
        std::thread::sleep(wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_up_to_a_minute_and_resets_after_a_long_run() {
        let s = Duration::from_secs;
        assert_eq!(next_wait(Duration::ZERO, s(0)), s(1));
        assert_eq!(next_wait(s(1), s(2)), s(2));
        assert_eq!(next_wait(s(32), s(2)), s(60));
        assert_eq!(next_wait(s(60), s(2)), s(60));
        assert_eq!(next_wait(s(60), s(61)), s(1));
    }

    #[test]
    fn exit_one_is_another_player_and_ends_the_keeper() {
        assert!(!restart(Some(1)));
        assert!(restart(Some(0)));
        assert!(restart(Some(2)));
        assert!(restart(None));
    }
}

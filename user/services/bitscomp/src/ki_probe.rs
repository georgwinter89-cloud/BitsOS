//! CL-5: "Testen" on the page "KI" runs the assistant's own test, `/bin/assist --test NAME` (CL-5s, assist's
//! probe.rs): one small question to that provider, one German line on stdout, the exit code says how it went.
//! bitscomp never sees a key: the test program reads it from settingsd itself. Each test runs on a thread of its
//! own (B4) and ends by itself within 60 s; the thread gives it `WAIT`, then ends it by its pid and says "late".
//! A timer on the main loop says the same where the thread cannot. The program's stderr (technical lines, keys
//! scrubbed by assist) is bitscomp's, as the helper's is.

use std::io::{BufRead, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::state::App;

/// assist's own limit is 60 s for everything; a little more, then the program is ended (plan/spec/assistant.md §5).
pub const WAIT: Duration = Duration::from_secs(65);
/// More than one line's worth is never read from the test program.
const OUT_MAX: u64 = 16 << 10;

#[derive(Clone, Debug, PartialEq)]
pub enum End {
    /// The program ended: its exit code (None: a signal) and the first line it said.
    Exit(Option<i32>, String),
    Late,
    /// It did not start; the German reason.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    pub id: String,
    pub run: u64,
    pub end: End,
}

pub struct Prober {
    tx: Sender<Msg>,
    handle: LoopHandle<'static, App>,
    /// The keys belong to /bin/assist of the session's user, whoever that is: only as the administrator
    /// (bitscomp before it gave root up) no test is started.
    allowed: bool,
}

pub fn start(handle: &LoopHandle<'static, App>, euid: u32) -> Result<Prober, String> {
    let (tx, rx) = channel::channel::<Msg>();
    handle
        .insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg(m) = ev {
                app.ki_probe_msg(m);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Prober { tx, handle: handle.clone(), allowed: euid != 0 })
}

impl Prober {
    /// Starts test `run` of provider `id`; Err: the German reason nothing was started.
    pub fn send(&self, id: &str, run: u64) -> Result<(), String> {
        if !self.allowed {
            return Err("Der Test läuft nur in deiner eigenen Sitzung, nicht als Verwalter.".into());
        }
        let Some(exe) = crate::assist_serve::exe() else {
            return Err(format!("Das Programm {} fehlt: kein Test möglich.", crate::assist_serve::EXE));
        };
        let (tx, name) = (self.tx.clone(), id.to_string());
        std::thread::Builder::new()
            .name("bitscomp-ki-test".into())
            .spawn(move || {
                let end = run_test(&exe, &name, WAIT);
                let _ = tx.send(Msg { id: name, run, end });
            })
            .map_err(|e| {
                eprintln!("bitscomp: ki: no thread for the test of {id}: {e}");
                "Der Test konnte nicht gestartet werden.".to_string()
            })?;
        let name = id.to_string();
        let timer = self.handle.insert_source(Timer::from_duration(WAIT + Duration::from_secs(2)), move |_, _, app: &mut App| {
            app.ki_probe_msg(Msg { id: name.clone(), run, end: End::Late });
            TimeoutAction::Drop
        });
        if let Err(e) = timer {
            eprintln!("bitscomp: ki: no deadline timer for the test of {id}: {e}");
        }
        Ok(())
    }
}

/// The first line with something on it, without control characters, at most 400 characters.
fn first_line(out: &[u8]) -> String {
    let text = String::from_utf8_lossy(out);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    line.chars().filter(|c| !c.is_control()).take(400).collect()
}

/// Runs `exe --test id` and waits at most `wait` for its end.
pub fn run_test(exe: &Path, id: &str, wait: Duration) -> End {
    let began = Instant::now();
    let mut child = match Command::new(exe).arg("--test").arg(id).stdin(Stdio::null()).stdout(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bitscomp: ki: {} --test {id} did not start: {e}", exe.display());
            return End::Failed(format!("Das Programm {} ließ sich nicht starten.", exe.display()));
        }
    };
    eprintln!("bitscomp: ki: test of provider {id} started (pid {})", child.id());
    // Read on a thread of its own: a program the test started may keep the pipe open after the test ended.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    if let Some(out) = child.stdout.take() {
        let reader = std::thread::Builder::new().name("bitscomp-ki-out".into()).spawn(move || {
            // The first line with something on it is the answer; nothing after it is waited for.
            let mut rd = std::io::BufReader::new(out.take(OUT_MAX));
            let mut buf = Vec::new();
            while buf.iter().all(|b: &u8| b.is_ascii_whitespace()) {
                buf.clear();
                if !matches!(rd.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
                    break;
                }
            }
            let _ = tx.send(buf);
            // Read on to the end (bounded by OUT_MAX): a closed pipe would end a test that says more than one line
            // with SIGPIPE before its exit code (the gate of 13.11.: Exit(None) for a test that exits 0).
            let _ = std::io::copy(&mut rd, &mut std::io::sink());
        });
        if let Err(e) = reader {
            eprintln!("bitscomp: ki: no thread to read the test's line: {e}");
        }
    }
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code(),
            Ok(None) if began.elapsed() >= wait => {
                // Only this very child, by its handle.
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("bitscomp: ki: test of provider {id} ended after {} s without an answer", wait.as_secs());
                return End::Late;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => {
                eprintln!("bitscomp: ki: waiting for the test of {id} failed: {e}");
                let _ = child.kill();
                // Reaped, as after the deadline: no zombie stays behind.
                let _ = child.wait();
                return End::Failed("Der Test ließ sich nicht beobachten.".into());
            }
        }
    };
    let out = rx.recv_timeout(Duration::from_millis(500)).unwrap_or_default();
    let line = first_line(&out);
    eprintln!("bitscomp: ki: test of provider {id}: exit {} after {} ms", code.map_or("signal".to_string(), |c| c.to_string()), began.elapsed().as_millis());
    End::Exit(code, line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bitscomp-ki-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn the_test_program_gives_one_line_and_a_code_or_is_ended() {
        let ok = script("ok", "[ \"$1\" = --test ] || exit 9\necho\necho \"Der Anbieter „$2“ ist erreichbar und hat geantwortet.\"\necho zweite Zeile\nexit 0");
        assert_eq!(run_test(&ok, "lokal1", Duration::from_secs(10)), End::Exit(Some(0), "Der Anbieter „lokal1“ ist erreichbar und hat geantwortet.".into()));
        let no = script("no", "echo 'Der Anbieter ist nicht erreichbar.'; echo technisch >&2; exit 1");
        assert_eq!(run_test(&no, "x", Duration::from_secs(10)), End::Exit(Some(1), "Der Anbieter ist nicht erreichbar.".into()));
        let silent = script("silent", "exit 2");
        assert_eq!(run_test(&silent, "x", Duration::from_secs(10)), End::Exit(Some(2), String::new()));
        // A program that hangs is ended at the deadline, by its own pid.
        let hang = script("hang", "exec sleep 600");
        let began = Instant::now();
        assert_eq!(run_test(&hang, "x", Duration::from_millis(300)), End::Late);
        assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
        // A child of the test that keeps the pipe open does not hold the answer back.
        let orphan = script("orphan", "sleep 3 &\necho 'Antwort'\nexit 0");
        let began = Instant::now();
        assert_eq!(run_test(&orphan, "x", Duration::from_secs(10)), End::Exit(Some(0), "Antwort".into()));
        assert!(began.elapsed() < Duration::from_secs(2), "{:?}", began.elapsed());
        assert!(matches!(run_test(Path::new("/nonexistent/assist"), "x", Duration::from_secs(1)), End::Failed(_)));
        assert_eq!(first_line(b"\n  \n a\x07b \nc"), "ab");
    }
}

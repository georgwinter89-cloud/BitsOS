//! `befehl`: a shell command as Georg, in a process group of its own so that a timeout, Esc or `befehl_beenden`
//! ends everything it started. A command in the background outlives the question that started it (its output
//! is kept here, the newest part when it grows large) until it is ended, its output read after its end, or an
//! hour after its end. Background commands belong to Georg, not to one chat: any of his chats may read or end
//! one, as any of his programs may end another (`befehl kill` could anyway).

use super::{OUT_MAX, cap, path_of};
use crate::json::Json;
use crate::term::sys;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT: u64 = 120;
/// What a background command's output may take here; older output goes first.
const KEPT: usize = 1 << 20;
const TICK: Duration = Duration::from_millis(20);
/// After SIGTERM, before SIGKILL.
const GRACE: Duration = Duration::from_secs(2);

/// Output as it arrives from both pipes, with how much was dropped from its start.
#[derive(Default)]
struct Buf {
    bytes: Vec<u8>,
    dropped: usize,
    /// Bytes of `bytes` already handed out (background).
    read: usize,
}

impl Buf {
    fn push(&mut self, more: &[u8], keep: usize) {
        self.bytes.extend_from_slice(more);
        if self.bytes.len() > keep {
            let cut = self.bytes.len() - keep;
            self.bytes.drain(..cut);
            self.dropped += cut;
            self.read = self.read.saturating_sub(cut);
        }
    }
}

type Shared = Arc<Mutex<Buf>>;

fn lock(b: &Shared) -> std::sync::MutexGuard<'_, Buf> {
    b.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long the output of a command that ended is waited for: something it left running may hold the pipes.
const LAST_OUTPUT: Duration = Duration::from_millis(500);

/// Starts `command` with both pipes read into one buffer; the counter says how many pipes have closed.
fn start(command: &str, cwd: &std::path::Path, keep: usize) -> Result<(Child, Shared, Arc<AtomicU64>), String> {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(command).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = c.spawn().map_err(|e| format!("the command could not be started: {e}"))?;
    let buf: Shared = Arc::default();
    let closed = Arc::new(AtomicU64::new(0));
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)];
    for mut pipe in pipes.into_iter().flatten() {
        let (b, done) = (buf.clone(), closed.clone());
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                lock(&b).push(&chunk[..n], keep);
            }
            done.fetch_add(1, Ordering::SeqCst);
        });
    }
    Ok((child, buf, closed))
}

/// Ends the command's whole process group: SIGTERM, then SIGKILL after a grace.
fn end_group(child: &mut Child) {
    let group = child.id();
    sys::signal_group(group, false);
    let until = Instant::now() + GRACE;
    while Instant::now() < until && sys::group_lives(group) {
        std::thread::sleep(TICK);
    }
    sys::signal_group(group, true);
    let _ = child.wait();
}

fn text_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn cwd_of(args: &Json) -> Result<std::path::PathBuf, String> {
    let cwd = match args.str("cwd").filter(|c| !c.trim().is_empty()) {
        Some(c) => path_of(c)?,
        None => super::home(),
    };
    if !cwd.is_dir() {
        return Err(format!("cwd {} is not a folder", cwd.display()));
    }
    Ok(cwd)
}

pub fn run(args: &Json, stop: &dyn Fn() -> bool) -> Result<String, String> {
    let command = args.str("command").unwrap_or("");
    if command.trim().is_empty() || command.contains('\0') {
        return Err("command is empty".into());
    }
    let cwd = cwd_of(args)?;
    if args.get("background").and_then(Json::as_bool) == Some(true) {
        return background(command, &cwd);
    }
    let timeout = Duration::from_secs(args.get("timeout_s").and_then(Json::as_i64).and_then(|t| u64::try_from(t).ok()).filter(|t| (1..=3600).contains(t)).unwrap_or(DEFAULT_TIMEOUT));
    let (mut child, buf, closed) = start(command, &cwd, crate::limits::budget().max(OUT_MAX))?;
    let until = Instant::now() + timeout;
    let how = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().map_or_else(|| "ended by a signal".to_string(), |c| format!("exit code {c}")),
            Ok(None) => {}
            Err(e) => break format!("could not be waited for ({e})"),
        }
        if stop() {
            end_group(&mut child);
            break "stopped (Georg pressed Esc)".to_string();
        }
        if Instant::now() >= until {
            end_group(&mut child);
            break format!("ended after the timeout of {} s", timeout.as_secs());
        }
        std::thread::sleep(TICK);
    };
    // A program the command started and left running (a window, a server) stays, and may keep the pipes open.
    let until = Instant::now() + LAST_OUTPUT;
    while closed.load(Ordering::SeqCst) < 2 && Instant::now() < until {
        std::thread::sleep(TICK);
    }
    let b = lock(&buf);
    let out = text_of(&b.bytes);
    Ok(format!("{how}\n{}", cap(&out, OUT_MAX)))
}

struct Job {
    id: String,
    child: Child,
    buf: Shared,
    /// How many of its two pipes have closed.
    closed: Arc<AtomicU64>,
    ended: Option<String>,
    /// When its end was first seen: an ended command nobody reads is forgotten after [`KEEP_ENDED`].
    ended_at: Option<Instant>,
}

/// How long the output of a background command that ended waits to be read.
const KEEP_ENDED: Duration = Duration::from_secs(3600);

/// Looks at every job: an ended one is reaped (no process is left as a zombie), and one that ended long ago
/// and was never read is forgotten with its output.
fn sweep(all: &mut Vec<Job>) {
    for j in all.iter_mut() {
        state(j);
    }
    all.retain(|j| j.ended_at.is_none_or(|t| t.elapsed() < KEEP_ENDED));
}

fn jobs() -> &'static Mutex<Vec<Job>> {
    static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());
    &JOBS
}

static NEXT: AtomicU64 = AtomicU64::new(1);

fn background(command: &str, cwd: &std::path::Path) -> Result<String, String> {
    let (child, buf, closed) = start(command, cwd, KEPT)?;
    let id = format!("b{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let pid = child.id();
    let mut all = jobs().lock().unwrap_or_else(PoisonError::into_inner);
    sweep(&mut all);
    all.push(Job { id: id.clone(), child, buf, closed, ended: None, ended_at: None });
    Ok(format!("started in the background as {id} (process {pid}); read its output with befehl_ausgabe, end it with befehl_beenden"))
}

/// Looks whether the job ended; its state in words.
fn state(job: &mut Job) -> String {
    if job.ended.is_none()
        && let Ok(Some(status)) = job.child.try_wait()
    {
        job.ended = Some(status.code().map_or_else(|| "ended by a signal".to_string(), |c| format!("ended with exit code {c}")));
        job.ended_at = Some(Instant::now());
    }
    job.ended.clone().unwrap_or_else(|| "still runs".into())
}

/// The output since the last look.
fn fresh(job: &Job) -> String {
    let mut b = lock(&job.buf);
    let dropped = if b.read == 0 && b.dropped > 0 { format!("({} bytes of older output were dropped)\n", b.dropped) } else { String::new() };
    let new = text_of(&b.bytes[b.read..]);
    b.read = b.bytes.len();
    format!("{dropped}{}", cap(&new, OUT_MAX))
}

fn with_job<T>(id: &str, f: impl FnOnce(&mut Job) -> T) -> Result<T, String> {
    let mut all = jobs().lock().unwrap_or_else(PoisonError::into_inner);
    sweep(&mut all);
    let job = all.iter_mut().find(|j| j.id == id).ok_or_else(|| format!("no background command {id:?} (ended and read, or never started)"))?;
    Ok(f(job))
}

pub fn output(args: &Json, stop: &dyn Fn() -> bool) -> Result<String, String> {
    let id = args.str("id").unwrap_or("").trim().to_string();
    let wait = Duration::from_secs(args.get("wait_s").and_then(Json::as_i64).and_then(|w| u64::try_from(w).ok()).unwrap_or(0).min(60));
    let until = Instant::now() + wait;
    loop {
        let (ended, has_new) = with_job(&id, |j| {
            let s = state(j);
            let b = lock(&j.buf);
            (s != "still runs", b.bytes.len() > b.read)
        })?;
        if ended || has_new || Instant::now() >= until || stop() {
            break;
        }
        std::thread::sleep(TICK * 5);
    }
    // A command that ended may still have its last output on the way through the pipes.
    let closed = with_job(&id, |j| (j.ended.is_some(), j.closed.clone()))?;
    if closed.0 {
        let until = Instant::now() + LAST_OUTPUT;
        while closed.1.load(Ordering::SeqCst) < 2 && Instant::now() < until {
            std::thread::sleep(TICK);
        }
    }
    let (said, ended) = with_job(&id, |j| {
        let s = state(j);
        (format!("{id}: {s}\n{}", fresh(j)), j.ended.is_some())
    })?;
    if ended {
        // Ended and read: it is forgotten.
        jobs().lock().unwrap_or_else(PoisonError::into_inner).retain(|j| j.id != id);
    }
    Ok(said)
}

pub fn end(args: &Json) -> Result<String, String> {
    let id = args.str("id").unwrap_or("").trim().to_string();
    let mut job = {
        let mut all = jobs().lock().unwrap_or_else(PoisonError::into_inner);
        let i = all.iter().position(|j| j.id == id).ok_or_else(|| format!("no background command {id:?} (ended and read, or never started)"))?;
        all.remove(i)
    };
    let was = state(&mut job);
    if job.ended.is_none() {
        end_group(&mut job.child);
    }
    let last = fresh(&job);
    Ok(format!("{id}: {}\n{last}", if was == "still runs" { "ended now".to_string() } else { was }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn a(s: &str) -> Json {
        parse(s).unwrap()
    }

    #[test]
    fn a_command_its_code_its_output_a_timeout_and_esc() {
        let r = run(&a(r#"{"command":"echo eins; echo zwei >&2; exit 3","cwd":"/"}"#), &|| false).unwrap();
        assert!(r.starts_with("exit code 3\n") && r.contains("eins") && r.contains("zwei"), "{r}");
        let t = Instant::now();
        let r = run(&a(r#"{"command":"sleep 30 & sleep 30","timeout_s":1}"#), &|| false).unwrap();
        assert!(r.starts_with("ended after the timeout of 1 s") && t.elapsed() < Duration::from_secs(10), "{r}");
        let r = run(&a(r#"{"command":"sleep 30"}"#), &|| true).unwrap();
        assert!(r.starts_with("stopped"), "{r}");
        assert!(run(&a(r#"{"command":"  "}"#), &|| false).is_err());
        assert!(run(&a(r#"{"command":"true","cwd":"/no/such/folder"}"#), &|| false).unwrap_err().contains("not a folder"));
        let big = run(&a(r#"{"command":"head -c 200000 /dev/zero | tr '\\0' x"}"#), &|| false).unwrap();
        assert!(big.chars().count() < OUT_MAX + 200 && big.contains("characters left out"), "{}", big.len());
    }

    #[test]
    fn a_background_command_is_read_and_ended() {
        let started = run(&a(r#"{"command":"echo los; sleep 30","background":true}"#), &|| false).unwrap();
        let id = started.split_whitespace().find(|w| w.starts_with('b') && w[1..].bytes().all(|b| b.is_ascii_digit()) && w.len() > 1).unwrap().to_string();
        let out = output(&a(&format!(r#"{{"id":"{id}","wait_s":5}}"#)), &|| false).unwrap();
        assert!(out.contains("still runs") && out.contains("los"), "{out}");
        let again = output(&a(&format!(r#"{{"id":"{id}"}}"#)), &|| false).unwrap();
        assert!(!again.contains("los"), "only what is new: {again}");
        let ended = end(&a(&format!(r#"{{"id":"{id}"}}"#))).unwrap();
        assert!(ended.contains("ended now"), "{ended}");
        assert!(output(&a(&format!(r#"{{"id":"{id}"}}"#)), &|| false).is_err(), "gone after its end");
        let quick = run(&a(r#"{"command":"echo fertig","background":true}"#), &|| false).unwrap();
        let id = quick.split_whitespace().find(|w| w.starts_with('b') && w[1..].bytes().all(|b| b.is_ascii_digit()) && w.len() > 1).unwrap().to_string();
        let out = output(&a(&format!(r#"{{"id":"{id}","wait_s":5}}"#)), &|| false).unwrap();
        assert!(out.contains("ended with exit code 0") && out.contains("fertig"), "{out}");
        assert!(output(&a(&format!(r#"{{"id":"{id}"}}"#)), &|| false).is_err(), "ended and read: forgotten");
        assert!(end(&a(r#"{"id":"b999999"}"#)).is_err());
    }

    #[test]
    fn an_ended_command_nobody_reads_is_reaped_and_forgotten_in_time() {
        let started = run(&a(r#"{"command":"true","background":true}"#), &|| false).unwrap();
        let id = started.split_whitespace().find(|w| w.starts_with('b') && w[1..].bytes().all(|b| b.is_ascii_digit()) && w.len() > 1).unwrap().to_string();
        let until = Instant::now() + Duration::from_secs(10);
        let mine = |all: &mut Vec<Job>| all.iter().position(|j| j.id == id);
        loop {
            let mut all = jobs().lock().unwrap_or_else(PoisonError::into_inner);
            sweep(&mut all);
            let i = mine(&mut all).expect("kept while its end is new");
            if all[i].ended.is_some() {
                // Its end lies long back now.
                all[i].ended_at = Instant::now().checked_sub(KEEP_ENDED + Duration::from_secs(1));
                sweep(&mut all);
                assert!(mine(&mut all).is_none(), "forgotten after {KEEP_ENDED:?}");
                break;
            }
            drop(all);
            assert!(Instant::now() < until, "the command did not end");
            std::thread::sleep(TICK);
        }
    }
}

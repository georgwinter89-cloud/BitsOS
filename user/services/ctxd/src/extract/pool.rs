//! Extraction in child processes (DESIGN §5): `ctxd extract-worker` reads one JSON request per line on
//! stdin and answers one JSON line on stdout. The worker limits its own address space (RLIMIT_AS), its CPU
//! time per file (RLIMIT_CPU, moved forward before each file because a worker serves many files), its open
//! files (RLIMIT_NOFILE) and forbids itself to write files (RLIMIT_FSIZE = 0). The parent bounds the wall
//! time per file and kills a worker that overruns, so a hostile file ends one child, the document gets
//! `failed:<reason>`, and the service goes on. Network isolation (a namespace or seccomp) is CX-5's.

use super::{Extracted, Limits, extract_bytes};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Serialize, Deserialize)]
struct Request {
    path: PathBuf,
    max_text_bytes: usize,
}

#[derive(Serialize, Deserialize)]
struct Response {
    ok: Option<Extracted>,
    err: Option<String>,
}

/// Read and extract one file in this process, panics of the crates underneath turned into errors.
pub fn extract_path(path: &Path, limits: Limits) -> Result<Extracted, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(limits.max_text_bytes as u64 * 4).read_to_end(&mut bytes))
        .map_err(|e| format!("read: {}", e.kind()))?;
    match std::panic::catch_unwind(|| extract_bytes(path, &bytes, limits)) {
        Ok(r) => r,
        Err(_) => Err("extractor_panic".into()),
    }
}

/// CPU seconds this process has used so far (utime + stime from `/proc/self/stat`, USER_HZ = 100).
fn cpu_seconds() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The command name (field 2) is in parentheses and may contain spaces: count from after the last ')'.
    let mut f = s.get(s.rfind(')')? + 2..)?.split(' ');
    let utime: u64 = f.nth(11)?.parse().ok()?;
    let stime: u64 = f.next()?.parse().ok()?;
    Some((utime + stime).div_ceil(100))
}

/// Allow `secs` more CPU seconds from now (soft limit: SIGXCPU ends the worker, the parent sees it crash).
fn cpu_budget(secs: u64) {
    if secs == 0 {
        return;
    }
    let Some(used) = cpu_seconds() else { return };
    let hard = rlimit::getrlimit(rlimit::Resource::CPU).map(|(_, h)| h).unwrap_or(rlimit::INFINITY);
    let _ = rlimit::setrlimit(rlimit::Resource::CPU, (used + secs + 1).min(hard), hard);
}

/// Entry of `ctxd extract-worker <mem_mb> <cpu_seconds_per_file>`.
pub fn worker_main(mem_mb: u64, cpu_secs: u64) -> i32 {
    if mem_mb > 0 {
        let b = mem_mb << 20;
        let _ = rlimit::setrlimit(rlimit::Resource::AS, b, b);
    }
    // stdin, stdout, stderr, the file being read and /proc/self/stat: a parser has no use for more.
    let _ = rlimit::setrlimit(rlimit::Resource::NOFILE, 16, 16);
    let _ = rlimit::setrlimit(rlimit::Resource::FSIZE, 0, 0);
    // Panics are answered as errors; their default message would only clutter the parent's stderr.
    std::panic::set_hook(Box::new(|_| {}));
    // A worker stuck in a file must not outlive a killed ctxd: once reparented, it ends itself.
    let ppid = std::os::unix::process::parent_id();
    let _ = std::thread::Builder::new().name("ctxd-extract-watch".into()).spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if std::os::unix::process::parent_id() != ppid {
                std::process::exit(3);
            }
        }
    });
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        cpu_budget(cpu_secs);
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(r) => match extract_path(&r.path, Limits { max_text_bytes: r.max_text_bytes }) {
                Ok(e) => Response { ok: Some(e), err: None },
                Err(e) => Response { ok: None, err: Some(e) },
            },
            Err(_) => Response { ok: None, err: Some("bad_request".into()) },
        };
        let Ok(s) = serde_json::to_string(&resp) else { break };
        if writeln!(out, "{s}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    0
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
}

impl Worker {
    fn spawn(exe: &Path, mem_mb: u64, cpu_secs: u64) -> std::io::Result<Worker> {
        // B4: extraction is Background work. On CachyOS that is `nice 10` (no unsafe setpriority call in
        // ctxd); on V2 (CX-5) the worker gets the Background class at spawn.
        let nice = Path::new("/usr/bin/nice");
        let mut cmd = if nice.is_file() {
            let mut c = Command::new(nice);
            c.args(["-n", "10"]).arg(exe);
            c
        } else {
            Command::new(exe)
        };
        let mut child = cmd
            .arg("extract-worker")
            .arg(mem_mb.to_string())
            .arg(cpu_secs.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let (tx, rx) = channel();
        std::thread::Builder::new().name("ctxd-extract-rd".into()).spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        })?;
        Ok(Worker { child, stdin, rx })
    }

    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub enum Isolation {
    /// Child processes (the service).
    Process { exe: PathBuf, mem_mb: u64 },
    /// In this process (unit tests, `ctxd extract`).
    InProcess,
}

pub struct Pool {
    iso: Isolation,
    idle: Mutex<Vec<Worker>>,
    cv: Condvar,
    spawned: Mutex<usize>,
    size: usize,
    timeout: Duration,
    limits: Limits,
}

impl Pool {
    pub fn new(iso: Isolation, size: usize, timeout: Duration, limits: Limits) -> Pool {
        Pool {
            iso,
            idle: Mutex::new(Vec::new()),
            cv: Condvar::new(),
            spawned: Mutex::new(0),
            size: size.max(1),
            timeout,
            limits,
        }
    }

    fn take(&self, exe: &Path, mem_mb: u64) -> Result<Worker, String> {
        let mut idle = self.idle.lock().map_err(|_| "pool_poisoned")?;
        loop {
            if let Some(w) = idle.pop() {
                return Ok(w);
            }
            {
                let mut n = self.spawned.lock().map_err(|_| "pool_poisoned")?;
                if *n < self.size {
                    *n += 1;
                    drop(n);
                    drop(idle);
                    return Worker::spawn(exe, mem_mb, self.timeout.as_secs().max(1)).map_err(|e| {
                        if let Ok(mut n) = self.spawned.lock() {
                            *n -= 1;
                        }
                        format!("worker_spawn: {}", e.kind())
                    });
                }
            }
            idle = self.cv.wait(idle).map_err(|_| "pool_poisoned")?;
        }
    }

    fn give_back(&self, w: Option<Worker>) {
        match w {
            Some(w) => {
                if let Ok(mut idle) = self.idle.lock() {
                    idle.push(w);
                }
            }
            None => {
                if let Ok(mut n) = self.spawned.lock() {
                    *n -= 1;
                }
            }
        }
        self.cv.notify_one();
    }

    pub fn extract(&self, path: &Path) -> Result<Extracted, String> {
        let (exe, mem_mb) = match &self.iso {
            Isolation::InProcess => return extract_path(path, self.limits),
            Isolation::Process { exe, mem_mb } => (exe, *mem_mb),
        };
        let mut w = self.take(exe, mem_mb)?;
        let req =
            serde_json::to_string(&Request { path: path.to_path_buf(), max_text_bytes: self.limits.max_text_bytes })
                .map_err(|_| "bad_path")?;
        if writeln!(w.stdin, "{req}").and_then(|_| w.stdin.flush()).is_err() {
            w.kill();
            self.give_back(None);
            return Err("worker_died".into());
        }
        match w.rx.recv_timeout(self.timeout) {
            Ok(line) => {
                let r = serde_json::from_str::<Response>(&line).map_err(|_| "worker_garbled".to_string());
                self.give_back(Some(w));
                match r? {
                    Response { ok: Some(e), .. } => Ok(e),
                    Response { err, .. } => Err(err.unwrap_or_else(|| "unknown".into())),
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                w.kill();
                self.give_back(None);
                crate::event!("warn", "extract.timeout", seconds = self.timeout.as_secs());
                Err("timeout".into())
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The worker died on this file (memory limit, abort in a crate): only this file fails.
                w.kill();
                self.give_back(None);
                Err("worker_crashed".into())
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        if let Ok(mut idle) = self.idle.lock() {
            for w in idle.drain(..) {
                w.kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn own_cpu_time_is_readable() {
        assert!(super::cpu_seconds().is_some());
    }
}

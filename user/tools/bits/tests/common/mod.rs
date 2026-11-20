//! Fake services for bits's tests: bitscomp's control socket (one line, one answer), settingsd and bitspkgd (JSON
//! lines with `ok`), ctxd (`{"id","op","args"}`). Each answers by a closure and records what it was sent; the bits
//! binary runs against them through the environment variables BITS.md names.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::Value;

pub type Log = Arc<Mutex<Vec<String>>>;

pub struct World {
    pub dir: PathBuf,
    pub ctl: Log,
    pub settings: Log,
    pub ctxd: Log,
    pub pkg: Log,
    env: Vec<(String, String)>,
}

fn serve_lines(path: &Path, log: Log, many: bool, answer: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) {
    let l = UnixListener::bind(path).unwrap();
    let answer = Arc::new(answer);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (log, answer) = (log.clone(), answer.clone());
            std::thread::spawn(move || {
                let mut w = s.try_clone().unwrap();
                let mut r = BufReader::new(s);
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end().to_string();
                    log.lock().unwrap().push(line.clone());
                    for a in answer(&line) {
                        let _ = writeln!(w, "{a}");
                    }
                    if !many {
                        return;
                    }
                }
            });
        }
    });
}

impl World {
    pub fn new(name: &str) -> World {
        let dir = std::env::temp_dir().join(format!("bt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run")).unwrap();
        let s = |n: &str| dir.join("run").join(n).display().to_string();
        let env = vec![
            ("XDG_RUNTIME_DIR".into(), dir.join("run").display().to_string()),
            ("BITS_CTL".into(), s("comp.ctl")),
            ("SETTINGS_SOCKET".into(), s("settings.sock")),
            ("BITS_CTXD".into(), s("ctxd.sock")),
            ("BITS_PKG".into(), s("pkg.sock")),
            ("BITS_METRICS".into(), dir.join("metrics").display().to_string()),
            ("BITS_PAPIERKORB_MOUNTS".into(), dir.join("mnt").display().to_string()),
            // No kernel trash node here: bits moves the names itself.
            ("BITS_PAPIERKORB_NODE".into(), dir.join("no-node").display().to_string()),
            ("BITS_APPS".into(), dir.join("store").display().to_string()),
        ];
        World { dir, ctl: Log::default(), settings: Log::default(), ctxd: Log::default(), pkg: Log::default(), env }
    }

    pub fn sock(&self, n: &str) -> PathBuf {
        self.dir.join("run").join(n)
    }

    /// bitscomp's control socket: one line in, the answer (one or more lines) out, then the connection ends.
    pub fn comp(&self, answer: impl Fn(&str) -> String + Send + Sync + 'static) {
        let l = UnixListener::bind(self.sock("comp.ctl")).unwrap();
        let log = self.ctl.clone();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                let mut buf = Vec::new();
                let mut b = [0u8; 1];
                while s.read(&mut b).unwrap_or(0) == 1 && b[0] != b'\n' {
                    buf.push(b[0]);
                }
                let line = String::from_utf8_lossy(&buf).to_string();
                log.lock().unwrap().push(line.clone());
                let _ = s.write_all(format!("{}\n", answer(&line)).as_bytes());
            }
        });
    }

    pub fn settingsd(&self, answer: impl Fn(&Value) -> Value + Send + Sync + 'static) {
        serve_lines(&self.sock("settings.sock"), self.settings.clone(), false, move |l| vec![answer(&serde_json::from_str(l).unwrap()).to_string()]);
    }

    pub fn ctxd(&self, answer: impl Fn(&Value) -> Value + Send + Sync + 'static) {
        serve_lines(&self.sock("ctxd.sock"), self.ctxd.clone(), true, move |l| vec![answer(&serde_json::from_str(l).unwrap()).to_string()]);
    }

    /// bitspkgd: the answer lines (progress first, the `ok` line last).
    pub fn pkgd(&self, answer: impl Fn(&Value) -> Vec<Value> + Send + Sync + 'static) {
        serve_lines(&self.sock("pkg.sock"), self.pkg.clone(), false, move |l| answer(&serde_json::from_str(l).unwrap()).iter().map(Value::to_string).collect());
    }

    pub fn run_env(&self, args: &[&str], extra: &[(&str, &str)], stdin: Option<&str>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_bits"));
        c.args(args).env_clear().env("PATH", "/usr/bin:/bin").envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str()))).envs(extra.iter().copied());
        c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut ch = c.spawn().unwrap();
        if let Some(t) = stdin {
            ch.stdin.take().unwrap().write_all(t.as_bytes()).unwrap();
        }
        ch.wait_with_output().unwrap()
    }

    /// `bits --json ...`: (exit code, the one JSON object).
    pub fn json(&self, args: &[&str]) -> (i32, Value) {
        self.json_env(args, &[])
    }

    pub fn json_env(&self, args: &[&str], extra: &[(&str, &str)]) -> (i32, Value) {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let o = self.run_env(&a, extra, None);
        let out = String::from_utf8_lossy(&o.stdout).to_string();
        assert_eq!(out.lines().count(), 1, "exactly one JSON line for {args:?}, got {out:?} (stderr {:?})", String::from_utf8_lossy(&o.stderr));
        (o.status.code().unwrap_or(-1), serde_json::from_str(&out).unwrap())
    }

    /// In the assistant's tree (the marker the host tests use, `bitstree::FAKE_ENV`).
    pub fn tree(&self, args: &[&str]) -> (i32, Value) {
        self.json_env(args, &[("BITSOS_FAKE_TREE", "1")])
    }

    pub fn sent(log: &Log) -> Vec<String> {
        log.lock().unwrap().clone()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The keys of a JSON object, sorted (the pinned shapes).
pub fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    k.sort();
    k
}

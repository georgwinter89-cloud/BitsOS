//! `LlamaCpu` (DESIGN §6.2): llama.cpp's `llama-server --embeddings` as a supervised child process on the
//! CPU. Chosen over linking (open question 3): a crash or a bad model ends the child, not ctxd; the build
//! stays a plain upstream binary; it talks over a Unix socket in the data folder, so no TCP port is opened.
//! e5 models want "query: " / "passage: " prefixes.

use super::{EmbedError, EmbedKind, Embedder, Health, http, normalize};
use crate::config::EmbedderCfg;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// e5-small was trained with 512 positions (511 usable) per slot.
const SLOT_TOKENS: usize = 511;

pub struct LlamaCpu {
    cfg: EmbedderCfg,
    model: PathBuf,
    server: PathBuf,
    sock: PathBuf,
    log: PathBuf,
    child: Mutex<Option<Child>>,
    id: String,
    dim: usize,
    restarts: Mutex<(u32, Instant)>,
}

/// Closing the wrapper's stdin makes it kill llama-server; then the wrapper exits by itself.
fn stop_child(mut ch: Child) {
    drop(ch.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !matches!(ch.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = ch.kill();
    let _ = ch.wait();
}

fn model_fingerprint(model: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(model)?;
    let len = f.metadata()?.len();
    let mut buf = Vec::new();
    (&mut f).take(4 << 20).read_to_end(&mut buf)?;
    let mut h = blake3::Hasher::new();
    h.update(&len.to_le_bytes());
    h.update(&buf);
    Ok(h.finalize().to_hex()[..16].to_string())
}

fn socket_path(data: &Path) -> PathBuf {
    let p = data.join("tmp/llama.sock");
    // sun_path holds 108 bytes; a deep data folder falls back to the runtime directory.
    if p.as_os_str().len() < 100 {
        return p;
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join(format!("ctxd-llama-{}.sock", std::process::id()))
}

impl LlamaCpu {
    pub fn start(cfg: &EmbedderCfg, data: &Path) -> Result<LlamaCpu, String> {
        let model = cfg.model_path();
        let server = cfg.server_path();
        if !model.is_file() {
            return Err(format!("model file missing: {}", model.display()));
        }
        if !server.is_file() {
            return Err(format!("llama-server missing: {}", server.display()));
        }
        let fp = model_fingerprint(&model).map_err(|e| format!("model: {e}"))?;
        let stem = model.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut me = LlamaCpu {
            cfg: cfg.clone(),
            model,
            server,
            sock: socket_path(data),
            log: data.join("log/llama.log"),
            child: Mutex::new(None),
            id: String::new(),
            dim: 0,
            restarts: Mutex::new((0, Instant::now())),
        };
        me.spawn()?;
        let v = me
            .call(&["passage: probe".to_string()], Duration::from_secs(cfg.startup_seconds.max(5)))
            .map_err(|e| format!("probe: {e}"))?;
        me.dim = v[0].len();
        me.id = format!("llama/{stem}/{fp}/{}/e5", me.dim);
        crate::event!("info", "embedder.ready", dim = me.dim, threads = cfg.threads);
        Ok(me)
    }

    fn spawn(&self) -> Result<(), String> {
        let _ = std::fs::remove_file(&self.sock);
        if let Some(d) = self.sock.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Some(d) = self.log.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let log = OpenOptions::new().create(true).append(true).open(&self.log).map_err(|e| format!("log: {e}"))?;
        let log2 = log.try_clone().map_err(|e| format!("log: {e}"))?;
        let t = self.cfg.threads.max(1);
        let slots = self.cfg.slots.max(1);
        if let Ok(mut c) = self.child.lock()
            && let Some(old) = c.take()
        {
            stop_child(old);
        }
        // A small sh wrapper ends llama-server when ctxd's end of the stdin pipe closes, also after a
        // kill -9 of ctxd (no orphaned model process), and exits when llama-server exits.
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"exec 3<&0; "$0" "$@" </dev/null & p=$!; ( read _ <&3; kill $p 2>/dev/null ) & wait $p"#)
            .arg(&self.server)
            .arg("-m")
            .arg(&self.model)
            .args(["--embeddings", "--pooling", "mean", "--offline", "--no-ui", "-lv", "2"])
            .args(["-t", &t.to_string(), "-tb", &t.to_string()])
            .args(["-np", &slots.to_string(), "-c", &(slots * SLOT_TOKENS).to_string()])
            .args(["-b", "1024", "-ub", "1024"])
            .arg("--host")
            .arg(&self.sock)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log2))
            .spawn()
            .map_err(|e| format!("spawn llama-server: {e}"))?;
        crate::event!("info", "embedder.spawn", pid = child.id());
        *self.child.lock().map_err(|_| "poisoned")? = Some(child);
        let deadline = Instant::now() + Duration::from_secs(self.cfg.startup_seconds.max(5));
        while Instant::now() < deadline {
            if let Ok((200, _)) = http::request(&self.sock, "GET", "/health", b"", Duration::from_secs(2)) {
                return Ok(());
            }
            if self.exited() {
                return Err("llama-server exited during start (see log/llama.log)".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err("llama-server did not become ready".into())
    }

    fn exited(&self) -> bool {
        match self.child.lock() {
            Ok(mut c) => match c.as_mut() {
                Some(ch) => !matches!(ch.try_wait(), Ok(None)),
                None => true,
            },
            Err(_) => true,
        }
    }

    /// Restart a dead child, at most a few times per minute (a broken model must not loop hot).
    fn revive(&self) -> Result<(), EmbedError> {
        let mut r = self.restarts.lock().map_err(|_| EmbedError::Unavailable("poisoned".into()))?;
        if r.1.elapsed() > Duration::from_secs(60) {
            *r = (0, Instant::now());
        }
        if r.0 >= 3 {
            return Err(EmbedError::Unavailable("llama-server keeps dying".into()));
        }
        r.0 += 1;
        crate::event!("warn", "embedder.restart", count = r.0);
        self.spawn().map_err(EmbedError::Unavailable)
    }

    fn call(&self, inputs: &[String], timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        let body = serde_json::json!({ "input": inputs, "encoding_format": "float" }).to_string();
        let (status, resp) = match http::request(&self.sock, "POST", "/v1/embeddings", body.as_bytes(), timeout) {
            Ok(x) => x,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err(EmbedError::Timeout);
            }
            Err(e) => return Err(EmbedError::Unavailable(e.kind().to_string())),
        };
        let v: serde_json::Value = serde_json::from_slice(&resp).map_err(|_| EmbedError::Bad("not json".into()))?;
        if status != 200 {
            let msg = v["error"]["message"].as_str().unwrap_or("").to_string();
            if msg.contains("too large") || msg.contains("exceeds") || msg.contains("context") {
                return Err(EmbedError::TooLong);
            }
            if status == 503 {
                return Err(EmbedError::Unavailable("loading".into()));
            }
            return Err(EmbedError::Bad(format!("status {status}")));
        }
        let data = v["data"].as_array().ok_or_else(|| EmbedError::Bad("no data".into()))?;
        let mut out = vec![Vec::new(); inputs.len()];
        for d in data {
            let i = d["index"].as_u64().unwrap_or(u64::MAX) as usize;
            let e = d["embedding"].as_array().ok_or_else(|| EmbedError::Bad("no embedding".into()))?;
            let mut vec: Vec<f32> = e.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect();
            normalize(&mut vec);
            if i < out.len() {
                out[i] = vec;
            }
        }
        if out.iter().any(Vec::is_empty) || (self.dim > 0 && out.iter().any(|v| v.len() != self.dim)) {
            return Err(EmbedError::Bad("missing or odd vectors".into()));
        }
        Ok(out)
    }

    /// One text that is too long for a slot is cut until it fits (the chunker keeps this rare).
    fn single(&self, text: &str, timeout: Duration) -> Result<Vec<f32>, EmbedError> {
        let mut t = text.to_string();
        loop {
            match self.call(std::slice::from_ref(&t), timeout) {
                Err(EmbedError::TooLong) if t.len() > 64 => {
                    t = crate::util::cut_bytes(&t, t.len() * 3 / 4).to_string();
                }
                r => return r.map(|mut v| v.remove(0)),
            }
        }
    }
}

impl Embedder for LlamaCpu {
    fn id(&self) -> &str {
        &self.id
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn max_tokens(&self) -> usize {
        SLOT_TOKENS
    }
    /// Small requests (four rounds of the server's slots): a query that arrives meanwhile waits for at
    /// most one request (the server queues tasks first come, first served), so indexing cannot starve the
    /// search deadline. Measured: throughput is the same as with larger batches (compute-bound).
    fn batch(&self) -> usize {
        self.cfg.slots.max(1) * 4
    }
    fn embed(&self, kind: EmbedKind, texts: &[&str], timeout: Duration) -> Result<Vec<Vec<f32>>, EmbedError> {
        let prefix = if kind == EmbedKind::Query { "query: " } else { "passage: " };
        let inputs: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        let mut r = self.call(&inputs, timeout);
        if matches!(r, Err(EmbedError::Unavailable(_))) && self.exited() {
            self.revive()?;
            r = self.call(&inputs, timeout);
        }
        match r {
            Err(EmbedError::TooLong) => inputs.iter().map(|t| self.single(t, timeout)).collect(),
            r => r,
        }
    }
    fn health(&self) -> Health {
        if self.exited() {
            return Health::Unavailable("llama-server not running".into());
        }
        match http::request(&self.sock, "GET", "/health", b"", Duration::from_millis(500)) {
            Ok((200, _)) => Health::Ready,
            Ok((503, _)) => Health::Loading,
            Ok((s, _)) => Health::Unavailable(format!("status {s}")),
            Err(e) => Health::Unavailable(e.kind().to_string()),
        }
    }
}

impl Drop for LlamaCpu {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock()
            && let Some(ch) = c.take()
        {
            stop_child(ch);
        }
        let _ = std::fs::remove_file(&self.sock);
    }
}

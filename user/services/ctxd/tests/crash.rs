//! Brief CX-1 / DESIGN §11.2: kill -9 during indexing and during a commit. After every kill the data
//! folder holds the old or the new generation, never a broken one; the final index equals a clean
//! rebuild (same top 10 for all 20 test queries).

mod common;
use common::*;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The test set eight times over, each copy made unique, so a run takes a few seconds.
fn corpus(dir: &Path) -> usize {
    let mut n = 0;
    for copy in 0..8 {
        let sub = dir.join(format!("kopie-{copy}"));
        std::fs::create_dir_all(&sub).unwrap();
        for e in std::fs::read_dir(eval_docs()).unwrap().flatten() {
            let mut b = std::fs::read(e.path()).unwrap();
            b.extend_from_slice(format!("\nKopie {copy}\n").as_bytes());
            std::fs::write(sub.join(e.file_name()), b).unwrap();
            n += 1;
        }
    }
    n
}

fn ctxd(data: &Path, root: &Path) -> Command {
    let mut c = Command::new(bin());
    c.args(["--data", data.to_str().unwrap(), "--root", root.to_str().unwrap(), "--embedder", "fake"]);
    c.env("CTXD_TEST_EMBED_DELAY_MS", "25");
    c
}

fn verify(data: &Path, root: &Path) -> Value {
    let out = ctxd(data, root).arg("verify").output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    assert!(out.status.success() && v["ok"] == Value::Bool(true), "broken data folder after kill: {v}");
    v
}

/// Top 10 as (score, path), sorted so that exact ties between the identical copies compare equal.
fn top10(data: &Path, root: &Path, q: &str) -> Vec<(String, String)> {
    let out = ctxd(data, root).env_remove("CTXD_TEST_EMBED_DELAY_MS").args(["search", q, "--json"]).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut r: Vec<(String, String)> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (format!("{:.3}", i["score"].as_f64().unwrap()), i["path"].as_str().unwrap().to_string()))
        .collect();
    r.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    // Equal scores at the cut may pick different copies; compare the scores there, paths above it.
    let cut = r.last().map(|x| x.0.clone()).unwrap_or_default();
    r.into_iter().map(|(s, p)| if s == cut { (s, String::new()) } else { (s, p) }).collect()
}

#[test]
fn kill9_during_indexing_and_commit() {
    let d = tmp("crash");
    let root = d.join("docs");
    let n = corpus(&root);
    assert!(n > 1000);

    // Reference: one clean run.
    let reference = d.join("ref");
    let t = Instant::now();
    let st = ctxd(&reference, &root).arg("index").stdout(Stdio::null()).status().unwrap();
    assert!(st.success());
    let full = t.elapsed();
    eprintln!("clean index of {n} docs: {full:?}");

    // Crash runs: kill -9 at spread-out moments of a running index.
    let data = d.join("crashed");
    let mut hits = 0;
    for frac in [0.05, 0.15, 0.3, 0.45, 0.6, 0.75, 0.9] {
        let mut c = ctxd(&data, &root).arg("index").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        std::thread::sleep(full.mul_f64(frac));
        if c.try_wait().unwrap().is_none() {
            hits += 1;
        }
        c.kill().unwrap(); // SIGKILL
        c.wait().unwrap();
        verify(&data, &root);
    }
    assert!(hits >= 4, "only {hits} kills hit a running index");

    // A kill exactly between the fsync of a new generation and the CURRENT switch.
    let before = verify(&data, &root)["generation"].clone();
    let extra = root.join("kopie-1/zusatz.txt");
    std::fs::write(&extra, "Zusatz für die Commit-Probe").unwrap();
    let mut c = ctxd(&data, &root)
        .env("CTXD_TEST_COMMIT_PAUSE_MS", "20000")
        .arg("index")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut r = BufReader::new(c.stderr.take().unwrap());
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        line.clear();
        if r.read_line(&mut line).unwrap() == 0 || line.contains("commit pause") {
            break;
        }
    }
    assert!(line.contains("commit pause"), "never reached the commit window");
    c.kill().unwrap();
    c.wait().unwrap();
    let after = verify(&data, &root);
    assert_eq!(after["generation"], before, "the half-committed generation must not be live");
    std::fs::remove_file(&extra).unwrap();

    // Restart: finishes from the caches and equals the clean rebuild.
    let st = ctxd(&data, &root).arg("index").stdout(Stdio::null()).status().unwrap();
    assert!(st.success());
    let v = verify(&data, &root);
    let r = verify(&reference, &root);
    for k in ["docs", "chunks", "content_sum", "chunks_sum", "vectors_sum", "keyword_docs"] {
        assert_eq!(v[k], r[k], "{k} differs from the clean rebuild");
    }
    let topics: Value =
        serde_json::from_slice(&std::fs::read(repo().join("host/cx-eval/topics.json")).unwrap()).unwrap();
    let mut queries = 0;
    for t in topics["topics"].as_array().unwrap() {
        for k in ["query", "query_alt"] {
            let q = t[k].as_str().unwrap();
            assert_eq!(top10(&data, &root, q), top10(&reference, &root, q), "query {q:?}");
            queries += 1;
        }
    }
    assert_eq!(queries, 20);
}

/// llama-server must not outlive a killed ctxd (it runs under a small wrapper that watches ctxd's pipe).
#[test]
fn llama_child_dies_with_ctxd() {
    if skip_without_model("llama_child_dies_with_ctxd") {
        return;
    }
    let d = tmp("llama-orphan");
    let root = d.join("docs");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "Lohnsteuerbescheinigung 2025").unwrap();
    let cfg = write_config(&d, 29545, &[(&root, false)], "");
    let mut s = serve(&d, &cfg, "llama", 29545);
    let data = d.join("data");
    let mine = || -> Vec<u32> {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let pid: u32 = e.file_name().to_str()?.parse().ok()?;
                let cmd = std::fs::read(e.path().join("cmdline")).ok()?;
                let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
                (cmd.contains("llama-server") && cmd.contains(data.to_str()?) && !cmd.contains("/bin/sh"))
                    .then_some(pid)
            })
            .collect()
    };
    assert_eq!(mine().len(), 1, "one llama-server for this data folder");
    s.child.kill().unwrap(); // SIGKILL ctxd
    s.child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !mine().is_empty() {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(mine().is_empty(), "llama-server outlived ctxd");
}

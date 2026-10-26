//! `test`, `fullrun` and `toolchain`: the gates the desk and every lane run.
use crate::{deps, ktest};
use std::os::unix::process::CommandExt;
use crate::image::BuildOpts;
use crate::util::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Crates that may contain `unsafe` (V2-PLAN trusted base).
const TRUSTED: &[&str] = &["hal", "ksync", "mm-core", "kreg"];

/// True when the inner attributes at the top of a lib.rs contain `#![forbid(unsafe_code)]`.
pub fn has_forbid_unsafe(src: &str) -> bool {
    const ITEMS: &[&str] = &["use ", "pub ", "mod ", "fn ", "extern ", "const ", "static ", "struct ", "enum ", "trait ", "impl", "type ", "macro_rules!", "unsafe ", "#["];
    for l in src.lines() {
        let t = l.trim();
        if t.starts_with("#![") && t.contains("forbid") && t.contains("unsafe_code") {
            return true;
        }
        if ITEMS.iter().any(|p| t.starts_with(p)) {
            return false;
        }
    }
    false
}

/// (crate name, dir) of every crate under crates/ and crates/drivers/.
fn crate_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    let mut v = Vec::new();
    for base in ["crates", "crates/drivers"] {
        if let Ok(rd) = std::fs::read_dir(root.join(base)) {
            for e in rd.flatten() {
                let p = e.path();
                if p.join("Cargo.toml").is_file() {
                    v.push((e.file_name().to_string_lossy().to_string(), p));
                }
            }
        }
    }
    v.sort();
    v
}

pub fn forbid_unsafe_gate(root: &Path) -> Result<(), String> {
    let mut bad = Vec::new();
    for (name, dir) in crate_dirs(root) {
        if TRUSTED.contains(&name.as_str()) {
            continue;
        }
        let lib = dir.join("src/lib.rs");
        match std::fs::read_to_string(&lib) {
            Ok(s) if has_forbid_unsafe(&s) => {}
            Ok(_) => bad.push(format!("{} lacks #![forbid(unsafe_code)] at the top", lib.display())),
            Err(_) => eprintln!("xtask: note: {} has no src/lib.rs, not checked", dir.display()),
        }
    }
    if bad.is_empty() {
        println!("forbid-unsafe gate: ok");
        Ok(())
    } else {
        for b in &bad {
            println!("forbid-unsafe gate: OFFENDER {b}");
        }
        Err(format!("forbid-unsafe gate: {} offender(s)", bad.len()))
    }
}

fn step(name: &str, r: Result<(), String>, results: &mut Vec<(String, bool)>) {
    if let Err(e) = &r {
        println!("xtask: {name}: {e}");
    }
    results.push((name.to_string(), r.is_ok()));
}

fn summary(results: &[(String, bool)]) -> Result<(), String> {
    let parts: Vec<String> = results.iter().map(|(n, ok)| format!("{n} {}", if *ok { "green" } else { "RED" })).collect();
    let green = results.iter().all(|(_, ok)| *ok);
    println!("{}: {}", if green { "GREEN" } else { "RED" }, parts.join(", "));
    if green { Ok(()) } else { Err("red".into()) }
}

/// Two HDA controllers with a duplex codec each, playing into WAV files of the run (never the host's sound
/// devices), so the sound ktests (sn.hda.*, sn.mix.two/switch/resample) run in the gate (SN-2 CR 4, SN-3 CR 1).
fn gate_audio(root: &Path) -> Vec<String> {
    let dir = root.join("target/xtask/runs/gate-audio");
    let _ = std::fs::create_dir_all(&dir);
    let mut v = Vec::new();
    for i in 0..2 {
        let wav = dir.join(format!("gate{i}.wav"));
        let _ = std::fs::remove_file(&wav);
        v.extend([
            "-audiodev".to_string(),
            format!("wav,id=a{i},path={},out.frequency=48000,out.channels=2,out.format=s16", wav.display()),
            "-device".to_string(),
            format!("intel-hda,id=h{i}"),
            "-device".to_string(),
            format!("hda-duplex,bus=h{i}.0,audiodev=a{i},mixer=off"),
        ]);
    }
    v
}

pub fn run_test(root: &Path, skip_ktest: bool) -> Result<(), String> {
    let mut res = Vec::new();
    let mut c = cargo(root);
    c.args(["test", "--workspace", "--exclude", "kernel"]);
    // Crates with optional fixtures (rtw89's fake chip) fail instead of skipping when this is set.
    c.env("BITSOS_GATE", "1");
    step("host tests", run_status(&mut c, "cargo test (host crates)"), &mut res);
    step("forbid-unsafe", forbid_unsafe_gate(root), &mut res);
    step("drop-bounds", crate::dropcheck::drop_bounds_gate(root), &mut res);
    if !skip_ktest {
        // INT-W9 (WL-4 CR 5): the gate's image carries the musl programs (/bin/sh, busybox, the ABI-diff set), so
        // the sysroot is built once in a worktree that has none (offline, from the read-only sources).
        step("sysroot", crate::abi_diff::sysroot(root).map(|_| ()), &mut res);
        let extra = gate_audio(root);
        step("ktest", ktest::run_ktests(root, &ktest::KtestOpts { filters: vec![String::new()], timeout: ktest::DEFAULT_TIMEOUT_S, build: BuildOpts::default(), net: "virtio".into(), extra, config: vec![] }), &mut res);
    }
    summary(&res)
}

pub fn cmd_test(root: &Path, mut a: Args) -> Result<(), String> {
    let skip = a.flag("--no-ktest");
    a.positionals()?;
    run_test(root, skip)
}

struct Script {
    lane: String,
    path: PathBuf,
    name: String,
}

fn is_slow(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|s| s.lines().take(5).any(|l| l.trim_start_matches('#').trim().starts_with("slow") && l.starts_with('#'))).unwrap_or(false)
}

/// Scenario scripts grouped by lane folder: `host/tests/<lane>/*.sh`.
fn find_scripts(root: &Path, quick: bool) -> Vec<(String, Vec<Script>)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root.join("host/tests")) else { return out };
    let mut dirs: Vec<_> = rd.flatten().filter(|e| e.path().is_dir()).collect();
    dirs.sort_by_key(|e| e.file_name());
    for d in dirs {
        let lane = d.file_name().to_string_lossy().to_lowercase();
        let mut sc: Vec<Script> = std::fs::read_dir(d.path())
            .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "sh").unwrap_or(false)).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !(quick && is_slow(p)))
            .map(|p| Script { lane: lane.clone(), name: p.file_stem().unwrap().to_string_lossy().to_string(), path: p })
            .collect();
        sc.sort_by(|a, b| a.path.cmp(&b.path));
        if !sc.is_empty() {
            out.push((lane, sc));
        }
    }
    out
}

fn run_script(root: &Path, s: &Script, timeout: Duration) -> (bool, String) {
    let run_dir = xdir(root).join("runs").join(format!("{}-{}", s.lane, s.name));
    let _ = std::fs::remove_dir_all(&run_dir);
    if let Err(e) = std::fs::create_dir_all(&run_dir) {
        return (false, format!("cannot create {}: {e}", run_dir.display()));
    }
    // Folder name -> lane code: MC's scenarios live in host/tests/mcp/, SC's freeze suite in host/tests/freeze/
    // (V2-LANES §2).
    let code = match s.lane.as_str() {
        "mcp" => "mc",
        "freeze" => "sc",
        l => l,
    };
    let (lo, hi) = lane_range(code).unwrap_or((20000, 20499));
    let log = run_dir.join("output.log");
    let Ok(lf) = std::fs::File::create(&log) else { return (false, "cannot create log".into()) };
    let lf2 = lf.try_clone().unwrap();
    let child = Command::new("bash")
        .arg(&s.path)
        .current_dir(root)
        .env("BITSOS_ROOT", root)
        .env("BITSOS_LANE", code)
        .env("BITSOS_PORT_LO", lo.to_string())
        .env("BITSOS_PORT_HI", hi.to_string())
        .env("BITSOS_RUN_DIR", &run_dir)
        .stdin(Stdio::null())
        .stdout(lf)
        .stderr(lf2)
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (false, format!("cannot start bash: {e}")),
    };
    let start = Instant::now();
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            return (st.success(), format!("{} (log {})", st, log.display()));
        }
        if start.elapsed() >= timeout {
            // The script's own process group only (its pid is the group id).
            let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).status();
            let _ = child.kill();
            let _ = child.wait();
            return (false, format!("timeout after {} s (log {})", timeout.as_secs(), log.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn cmd_fullrun(root: &Path, mut a: Args) -> Result<(), String> {
    let quick = a.flag("--quick");
    let timeout = Duration::from_secs(a.num::<u64>("--script-timeout")?.unwrap_or(600));
    a.positionals()?;
    let mut res = Vec::new();
    step("test", run_test(root, false), &mut res);
    step("deps", deps::run_deps(root), &mut res);
    let chains = find_scripts(root, quick);
    let n: usize = chains.iter().map(|(_, s)| s.len()).sum();
    println!("fullrun: {n} scenario script(s) in {} chain(s){}", chains.len(), if quick { " (quick)" } else { "" });
    let handles: Vec<_> = chains
        .into_iter()
        .map(|(lane, scripts)| {
            let root = root.to_path_buf();
            std::thread::spawn(move || {
                scripts.iter().map(|s| { let r = run_script(&root, s, timeout); (format!("{lane}/{}", s.name), r) }).collect::<Vec<_>>()
            })
        })
        .collect();
    for h in handles {
        for (name, (ok, msg)) in h.join().map_err(|_| "chain panicked".to_string())? {
            println!("fullrun: {name}: {} {msg}", if ok { "ok" } else { "FAIL" });
            res.push((format!("scenario {name}"), ok));
        }
    }
    summary(&res)
}

pub fn parse_rustc_date(v: &str) -> Option<String> {
    let open = v.rfind('(')?;
    v[open..].split_whitespace().nth(1).map(|d| d.trim_end_matches(')').to_string())
}

pub fn cmd_toolchain(root: &Path, a: Args) -> Result<(), String> {
    a.positionals()?;
    let o = Command::new("rustc").arg("-V").current_dir(root).output().map_err(|e| format!("rustc: {e}"))?;
    let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
    println!("{v}");
    match parse_rustc_date(&v) {
        Some(d) if d == crate::EXPECTED_RUSTC_DATE => println!("toolchain: ok (expected {})", crate::EXPECTED_RUSTC_DATE),
        Some(d) => println!("toolchain: WARNING: rustc date {d}, expected {} (the desk bumps EXPECTED_RUSTC_DATE in host/xtask/src/main.rs)", crate::EXPECTED_RUSTC_DATE),
        None => println!("toolchain: WARNING: cannot read the date from '{v}'"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbid_unsafe_detection() {
        assert!(has_forbid_unsafe("//! doc\n#![no_std]\n#![forbid(unsafe_code)]\npub fn f() {}\n"));
        assert!(has_forbid_unsafe("#![cfg_attr(not(test), no_std)]\n\n// c\n#![forbid(unsafe_code)]\n"));
        assert!(!has_forbid_unsafe("#![no_std]\npub fn f() {}\n#![forbid(unsafe_code)]\n"));
        assert!(!has_forbid_unsafe("#![no_std]\n// forbid(unsafe_code) in a comment\n"));
        assert!(!has_forbid_unsafe("#![deny(unsafe_code)]\n"));
        assert!(!has_forbid_unsafe(""));
    }

    #[test]
    fn rustc_date() {
        assert_eq!(parse_rustc_date("rustc 1.101.0-nightly (822182db4 2026-10-13)").as_deref(), Some("2026-10-13"));
        assert_eq!(parse_rustc_date("rustc 1.99.0 (abc 2026-01-01)").as_deref(), Some("2026-01-01"));
        assert_eq!(parse_rustc_date("nothing"), None);
    }

    #[test]
    fn slow_marker_in_first_lines() {
        let d = std::env::temp_dir().join(format!("xt-slow-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.sh"), "#!/bin/bash\n# slow\necho\n").unwrap();
        std::fs::write(d.join("b.sh"), "#!/bin/bash\necho slow\n").unwrap();
        assert!(is_slow(&d.join("a.sh")));
        assert!(!is_slow(&d.join("b.sh")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scenario_scripts_run_with_env_and_timeout() {
        let root = std::env::temp_dir().join(format!("xt-fullrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let d = root.join("host/tests/bt");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a_ok.sh"), "echo lane=$BITSOS_LANE $BITSOS_PORT_LO-$BITSOS_PORT_HI\ntest -d \"$BITSOS_RUN_DIR\"\n").unwrap();
        std::fs::write(d.join("b_bad.sh"), "exit 3\n").unwrap();
        std::fs::write(d.join("c_slow.sh"), "# slow\nsleep 30\n").unwrap();
        let chains = find_scripts(&root, false);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].1.len(), 3);
        assert_eq!(find_scripts(&root, true)[0].1.len(), 2);
        let sc = &chains[0].1;
        assert!(run_script(&root, &sc[0], Duration::from_secs(10)).0);
        let log = std::fs::read_to_string(xdir(&root).join("runs/bt-a_ok/output.log")).unwrap();
        assert!(log.contains("lane=bt 20500-20999"), "{log}");
        assert!(!run_script(&root, &sc[1], Duration::from_secs(10)).0);
        let t = Instant::now();
        let (ok, msg) = run_script(&root, &sc[2], Duration::from_secs(1));
        assert!(!ok && msg.contains("timeout") && t.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&root);
    }
}

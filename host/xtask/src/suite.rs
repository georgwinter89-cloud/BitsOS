//! `test`, `fullrun` and `toolchain`: the gates the desk and every lane run.
use crate::interim::Interim;
use crate::{deps, ktest};
use std::collections::BTreeSet;
use std::os::unix::process::CommandExt;
use crate::image::BuildOpts;
use crate::util::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A scenario script's time, unless `--script-timeout` says otherwise.
// GX-9b CR-4: 600 s went red on time alone beside other runs (a scenario took 859 s at load 6-13, nothing wrong).
const DEFAULT_SCRIPT_TIMEOUT_S: u64 = 1200;
/// Scripts the gate runs though no test class names them (user space only, so no ktest class can name them): the
/// Reload, DB2's access log, PK-0's /apps on /data and CL-1's settingsd on V2. The desktop has no scenario: it draws
/// only on the GPU, which neither QEMU nor this host lends it (Georg 18.11.); its logic is bitscomp's host tests.
const ALWAYS_IN_GATE: &[&str] = &["rb/20-reload", "ab/30-werhatwas", "pk/50-apps-on-data", "cl/20-settingsd-qemu"];

/// Crates that may contain `unsafe` (V2-PLAN trusted base).
const TRUSTED: &[&str] = &["hal", "ksync", "mm-core", "kreg"];
/// INT-W19 (AB-14 CR 6): user-mode code built into a kernel crate (outside its src/ tree) that may use `unsafe`,
/// with a SAFETY comment per block (CONTRACTS section 2). Every other .rs file outside a crate's src/ must not.
/// L7-SYNC: the vDSO's getrandom module, and the host test that maps the built image and calls into it.
const USER_MODE_UNSAFE: &[&str] = &["crates/proc/vdso/vdso.rs", "crates/proc/vdso/getrandom.rs", "crates/proc/tests/vdso_image.rs"];

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
        let mut extra = Vec::new();
        rs_outside_src(&dir, &dir, &mut extra);
        for f in extra {
            let rel = f.strip_prefix(root).unwrap_or(&f).to_string_lossy().to_string();
            match std::fs::read_to_string(&f) {
                Ok(s) if USER_MODE_UNSAFE.contains(&rel.as_str()) => {
                    let blocks = s.matches("unsafe {").count() + s.matches("unsafe fn").count();
                    let safety = s.matches("SAFETY").count();
                    if safety < blocks {
                        bad.push(format!("{rel}: {blocks} unsafe block(s) or fn(s) but {safety} SAFETY comment(s)"));
                    }
                }
                Ok(s) if mentions_unsafe(&s) => bad.push(format!("{rel} uses unsafe outside src/ (allowed only in {USER_MODE_UNSAFE:?})")),
                _ => {}
            }
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

/// Every .rs file below `dir` except the crate's own src/ tree and target/ dirs (build scripts' user-mode code).
fn rs_outside_src(krate: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let n = e.file_name();
            if (dir == krate && n == "src") || n == "target" || p.join("Cargo.toml").is_file() {
                continue;
            }
            rs_outside_src(krate, &p, out);
        } else if p.extension().is_some_and(|x| x == "rs") && !(dir == krate && e.file_name() == "build.rs") {
            out.push(p);
        }
    }
}

/// True when `src` has the word `unsafe` outside line comments.
fn mentions_unsafe(src: &str) -> bool {
    src.lines().any(|l| {
        let code = l.split("//").next().unwrap_or("");
        code.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == "unsafe")
    })
}

/// W22/C1: no home value in the public tree. Every tracked file outside plan/ is searched for the absolute home
/// path of the user who runs the gate (`$HOME`, e.g. the desk's own): defaults name `$HOME/...` or an environment
/// variable instead (rb/30-mirror's BITSOS_LISTENER), sample data a neutral home (/home/user). Skipped when HOME
/// is not an absolute path of at least two components.
pub fn home_path_gate(root: &Path) -> Result<(), String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let home = home.trim_end_matches('/');
    if !home.starts_with('/') || home.matches('/').count() < 2 {
        println!("home-path gate: skipped (HOME {home:?} is not an absolute home path)");
        return Ok(());
    }
    let out = Command::new("git").arg("-C").arg(root).args(["ls-files", "-z"]).output().map_err(|e| format!("git ls-files: {e}"))?;
    if !out.status.success() {
        return Err("git ls-files failed".into());
    }
    let mut bad = Vec::new();
    for f in out.stdout.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        let rel = String::from_utf8_lossy(f);
        if rel.starts_with("plan/") {
            continue;
        }
        let Ok(bytes) = std::fs::read(root.join(&*rel)) else { continue };
        for line in home_lines(&bytes, home) {
            bad.push(format!("{rel}:{line}"));
        }
    }
    if bad.is_empty() {
        println!("home-path gate: ok");
        Ok(())
    } else {
        for b in &bad {
            println!("home-path gate: OFFENDER {b} names {home}");
        }
        Err(format!("home-path gate: {} line(s) name the home {home} (use $HOME/... or an environment variable)", bad.len()))
    }
}

/// The 1-based numbers of the lines of `text` that hold `home` as a path (followed by `/`, a quote, a space, the
/// end or another character that cannot continue a user name).
fn home_lines(text: &[u8], home: &str) -> Vec<usize> {
    let h = home.as_bytes();
    let mut v = Vec::new();
    for (n, line) in text.split(|&b| b == b'\n').enumerate() {
        let hit = line.windows(h.len()).enumerate().any(|(i, w)| {
            w == h && line.get(i + h.len()).is_none_or(|&c| !(c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.'))
        });
        if hit {
            v.push(n + 1);
        }
    }
    v
}

fn step(name: &str, r: Result<(), String>, results: &mut Vec<(String, bool)>) {
    if let Err(e) = &r {
        println!("xtask: {name}: {e}");
    }
    results.push((name.to_string(), r.is_ok()));
}

fn summary(results: &[(String, bool)]) -> Result<(), String> {
    summary_line(results, None)
}

/// An interim run's line starts with `INTERIM` and says how many tests it left out: it is not the full gate.
fn summary_line(results: &[(String, bool)], interim: Option<&str>) -> Result<(), String> {
    let parts: Vec<String> = results.iter().map(|(n, ok)| format!("{n} {}", if *ok { "green" } else { "RED" })).collect();
    let green = results.iter().all(|(_, ok)| *ok);
    let word = if green { "GREEN" } else { "RED" };
    match interim {
        None => println!("{word}: {}", parts.join(", ")),
        Some(left) => println!("INTERIM {word} (interim run, not the integrator's full run; {left}): {}", parts.join(", ")),
    }
    if green { Ok(()) } else { Err("red".into()) }
}

/// The steps of one gate run; in an interim run it leaves out what `host/tests/stable.txt` lists.
struct Gate<'a> {
    root: &'a Path,
    res: Vec<(String, bool)>,
    interim: Option<Interim>,
    /// Whether the "sysroot" step ran (it runs once, before the first step that needs it).
    sysroot_done: bool,
    build_sysroot: fn(&Path) -> Result<(), String>,
}

impl Gate<'_> {
    fn step(&mut self, name: &str, f: impl FnOnce() -> Result<(), String>) {
        if self.interim.as_mut().is_some_and(|i| i.leaves_step(name)) {
            return;
        }
        let t = Instant::now();
        let r = f();
        // What an interim run saves is measured from these lines (host/tests/README.md "Interim runs").
        println!("xtask: step {name}: {} s", t.elapsed().as_secs());
        step(name, r, &mut self.res);
    }

    /// A `cargo test` step; `mk` makes its command (twice in an interim run: once to list its tests).
    fn cargo_step(&mut self, name: &str, mk: impl Fn() -> Command, what: &str) {
        let mut c = mk();
        if let Some(i) = self.interim.as_mut() {
            let t = Instant::now();
            c.args(i.host_args(self.root, name, mk()));
            println!("xtask: step {name}: listing its tests (and building them) {} s", t.elapsed().as_secs());
        }
        self.step(name, || run_status(&mut c, what));
    }

    /// L7-RESTE-AB 4: the "sysroot" step, once, before the first step whose image needs it (a fresh worktree has none).
    fn ensure_sysroot(&mut self) {
        if !self.sysroot_done {
            self.sysroot_done = true;
            let build = self.build_sysroot;
            let root = self.root;
            self.step("sysroot", || build(root));
        }
    }

    /// A test script as a step; first the sysroot when the script builds or boots V2.
    fn script(&mut self, name: &str, w: &mut Command, script: &str) {
        if crate::sysroot_need::needs_sysroot(self.root, &self.root.join(script)) {
            self.ensure_sysroot();
        }
        self.step(name, || run_status(w, script));
    }

    fn boot(&mut self, name: &str, o: ktest::KtestOpts) -> Option<ktest::KtestOpts> {
        match self.interim.as_mut() {
            Some(i) => i.boot(name, o),
            None => Some(o),
        }
    }

    fn finish(&self) -> Result<(), String> {
        summary_line(&self.res, self.interim.as_ref().map(Interim::line).as_deref())
    }
}

/// The gate, with the scenario scripts it ran (fullrun runs the others).
fn run_test_scripts(root: &Path, skip_ktest: bool, interim: Option<Interim>) -> (Result<(), String>, BTreeSet<String>) {
    let mut gate = Gate { root, res: Vec::new(), interim, sysroot_done: false, build_sysroot: |r| crate::abi_diff::sysroot(r).map(|_| ()) };
    let mut ran_scripts = BTreeSet::new();
    let host = || {
        let mut c = cargo(root);
        c.args(["test", "--workspace", "--exclude", "kernel"]);
        // Crates with optional fixtures (rtw89's fake chip) fail instead of skipping when this is set.
        c.env("BITSOS_GATE", "1");
        c
    };
    gate.cargo_step("host tests", host, "cargo test (host crates)");
    // INT-W16 (AC-3 CR): the user workspace's host-testable crates (MCP service glue, Brave's actions with the
    // staged Brave headless, settingsd). brave-act's end-to-end test needs the shared Brave stage
    // (stage-v2.sh) and fails without it.
    // INT-W17 (MC-8a/8b/9a CR 2): the MCP service bitsmcp (gate, TLS, pairing, the seccomp probe on the host's kernel;
    // its node interop test fails without node) and the installer bitsinstall.
    // C1: the package manager bitspkg (its settingsd sockets moved to a short folder; red for weeks outside the gate).
    // INT-GM6 (GM-6m CR 3): the music player bitsmusik.
    // INT-PK-B (PK-6f CR 1): the program service bitspkgd (its tests run nowhere else).
    // INT-CL3T (CL-3a CR 1, CL-T CR 3): the assistant (its tests hold a settingsd and a stand-in tools socket)
    // and usys (the peer of a socket: SO_PEERCRED, SO_PEERSEC against the host's kernel). `-p assist` also runs its
    // tests of `assist --mcp` (tests/mcp.rs: a scripted client on a pipe) and of provider kind program
    // (tests/program.rs: helper scripts the tests write, sh needed).
    // SELFOPT-1: the profiler bitsprof (profile.bin, ELF symbols, the report).
    // INT-DB: DB2's access service bitsaccess (its store, queries and the kernel's lines; tested nowhere else).
    // EINS-D: bits against its fake services, and bitstree (PLAN K5's query).
    // gpu-messung: how it reads the kernel's answer line.
    let user = || {
        let mut u = cargo(root);
        u.current_dir(root.join("user"))
            .args(["test", "-p", "mcpreg", "-p", "bitsact", "-p", "brave-act", "-p", "settingsd", "-p", "assist", "-p", "usys", "-p", "bitsmcp", "-p", "bitsinstall", "-p", "bitspkg", "-p", "bitspkgd", "-p", "bitsupdate", "-p", "reaper", "-p", "bitsmusik", "-p", "bitsprof", "-p", "uilist", "-p", "bitsaccess", "-p", "bits", "-p", "bitstree", "-p", "gpu-messung"]);
        u
    };
    gate.cargo_step("user host tests", user, "cargo test (user crates on the host)");
    // DB5a: atspid's own workspace (the bus, the registry and C3 against its fake program, end to end).
    let atspid = || {
        let mut a = Command::new("cargo");
        a.current_dir(root.join("user/services/atspid"))
            .env_remove("RUSTUP_TOOLCHAIN")
            .args(["test", "--offline", "--locked"])
            .env("CARGO_TARGET_DIR", root.join("target/atspid-test"));
        a
    };
    gate.cargo_step("atspid tests", atspid, "cargo test (user/services/atspid)");
    // INT-DB: ctxd's own workspace with its tests (DB4's habits, the memory's privacy and crash tests); the warnings
    // check only built them so far.
    let ctxd = || {
        let mut c = cargo(root);
        c.current_dir(root.join("user/services/ctxd")).args(["test", "--offline"]).env("CARGO_TARGET_DIR", root.join("target/ctxd-check"));
        c
    };
    gate.cargo_step("ctxd tests", ctxd, "cargo test (user/services/ctxd)");
    // GM-2b: the reaper's stand-in as the image gets it. Only the musl run tests that binary and checks it is static.
    let musl = || {
        let mut u = cargo(root);
        u.current_dir(root.join("user")).args(["test", "-p", "reaper", "--target", "x86_64-unknown-linux-musl"]);
        u
    };
    gate.cargo_step("reaper (musl)", musl, "cargo test -p reaper --target x86_64-unknown-linux-musl");
    // bitscomp's host tests (its own workspace) against the host's /usr as the Mesa tree it is linked for (build.rs);
    // none of them makes a renderer or draws: the desktop draws only on the GPU (Georg 18.11.).
    let comp = || {
        let mut g = cargo(root);
        g.current_dir(root.join("user/services/bitscomp"))
            .args(["test", "--release", "--offline", "--locked"])
            .env("BITSCOMP_GPU_ROOT", "/usr")
            .env("CARGO_TARGET_DIR", root.join("target/bitscomp"));
        g
    };
    gate.cargo_step("bitscomp tests", comp, "cargo test (user/services/bitscomp)");
    // INT-W28 (GM-1c CR-GM1c-3): the trace tools on the host, no QEMU; the host's render node only sees GET_CAP
    // and INFO; red without one.
    let run = root.join("target/xtask/runs/gate-gm30");
    let _ = std::fs::remove_dir_all(&run);
    let mut w = Command::new("bash");
    w.current_dir(root)
        .arg("host/tests/gm/30-drmtrace-tools.sh")
        .env("BITSOS_ROOT", root)
        .env("BITSOS_RUN_DIR", &run)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    gate.script("gm drmtrace tools", &mut w, "host/tests/gm/30-drmtrace-tools.sh");
    // GM-2a CR 9: the Steam launcher on a fake tree; a few seconds, no QEMU. 41-steam-stage.sh stays
    // in fullrun: it needs this machine's stage and 1.5 GB of scratch.
    let run = root.join("target/xtask/runs/gate-sx40");
    let mut w = Command::new("bash");
    w.current_dir(root)
        .arg("host/tests/sx/40-steam-launcher.sh")
        .env("BITSOS_ROOT", root)
        .env("BITSOS_RUN_DIR", &run)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    gate.script("steam launcher", &mut w, "host/tests/sx/40-steam-launcher.sh");
    // DB5a: the accessibility bus in the Brave, LibreOffice and Steam launchers (fake trees and socket), no QEMU.
    let run = root.join("target/xtask/runs/gate-us90");
    let mut w = Command::new("bash");
    w.current_dir(root).arg("host/tests/us/90-a11y-launchers.sh").env("BITSOS_ROOT", root).env("BITSOS_RUN_DIR", &run);
    gate.script("a11y launchers", &mut w, "host/tests/us/90-a11y-launchers.sh");
    // GM-8: the vkQuake launcher on made-up trees, no QEMU, no game (the game itself on the chip is Georg's check: the
    // Mesa tree has no software driver since DECISIONS 18.11.).
    let run = root.join("target/xtask/runs/gate-gm50");
    let mut w = Command::new("bash");
    w.current_dir(root)
        .arg("host/tests/gm/50-vkquake.sh")
        .env("BITSOS_ROOT", root)
        .env("BITSOS_RUN_DIR", &run)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    gate.script("gm vkquake", &mut w, "host/tests/gm/50-vkquake.sh");
    // TS-1: the Mini-PC scripts against a stand-in for the machine: no gate may start them against the real one, and
    // what they promise (the machine put back after a red run) must not wait for the first round to be tried.
    let mut w = Command::new("python3");
    w.current_dir(root).args(["-I", "host/tests/minipc/selftest.py", "target/xtask/runs/gate-minipc"]);
    gate.script("minipc scripts", &mut w, "host/tests/minipc/selftest.py");
    gate.step("forbid-unsafe", || forbid_unsafe_gate(root));
    gate.step("drop-bounds", || crate::dropcheck::drop_bounds_gate(root));
    gate.step("home paths", || home_path_gate(root));
    if !skip_ktest {
        // INT-W9 (WL-4 CR 5): the gate's image carries the musl programs (/bin/sh, busybox, the ABI-diff set), so
        // the sysroot is built once in a worktree that has none (offline, from the read-only sources); here at the
        // latest, earlier when a script step before boots V2.
        gate.ensure_sysroot();
        // TS-1: the ledger counts every boot from here on.
        gate.step("ledger start", || crate::ledger::start(root));
        let plain = ktest::KtestOpts { filters: vec![String::new()], timeout: ktest::DEFAULT_TIMEOUT_S, build: BuildOpts::default(), net: "virtio".into(), extra: vec![], config: vec![], exclude: vec![] };
        let plain = gate.boot("ktest", plain);
        let mut universe = Vec::new();
        gate.step("ktest", || {
            let o = plain.ok_or("the plain boot cannot leave out every test")?;
            let (u, r) = ktest::run_ktests_summaries(root, &o);
            universe = u;
            r
        });
        // INT-W18b (CP-8 CR-4): the nested-NMI check needs its CONFIG switch, so the plain run skips it. It needs a
        // second CPU only (rb.crash.nmi_nested), so it runs at the default -smp 2 (LO-1 review 30.10.: at -smp 16
        // OVMF failed to load bitsboot from the USB stick in about 2 of 3 boots, "No bootable option", whatever the
        // image; at -smp 2 every boot of 13 measured loaded it).
        let nest = ktest::KtestOpts {
            filters: vec!["rb.crash.nmi_nested".into()],
            timeout: ktest::DEFAULT_TIMEOUT_S,
            build: BuildOpts::default(),
            net: "none".into(),
            extra: vec![],
            config: vec!["rb_nmi_nest=on".into(), "ktest_run=gate/nmi-nested".into()],
            exclude: vec![],
        };
        if let Some(o) = gate.boot("ktest nmi-nested", nest) {
            gate.step("ktest nmi-nested", || ktest::run_ktests(root, &o));
        }
        // INT-W29 (RB-18 CR-6): the plain boot has no secret in CONFIG, so only a boot with made-up ones proves that
        // the record holds no value and that both reached the kernel first.
        let secret = ktest::KtestOpts {
            filters: vec!["kernel.config.secret".into()],
            timeout: ktest::DEFAULT_TIMEOUT_S,
            build: BuildOpts::default(),
            net: "virtio".into(),
            extra: vec![],
            config: vec!["token=rb18-fake-token-not-real".into(), "wlan_psk=rb18-fake-psk-not-real".into(), "rb18_expect_blanked=2".into()],
            exclude: vec![],
        };
        if let Some(o) = gate.boot("ktest config-secret", secret) {
            gate.step("ktest config-secret", || ktest::run_ktests(root, &o));
        }
        // INT-W21 (AB-16 CR): the NMI-backtrace storm against hostile sigreturn/iretq is QemuOnly and wants many
        // CPUs to sample, which the plain run at the default smp does not give.
        let storm = ktest::KtestOpts {
            filters: vec!["ab.signal.nmi_storm".into()],
            timeout: ktest::DEFAULT_TIMEOUT_S,
            build: BuildOpts::default(),
            net: "none".into(),
            extra: vec!["-smp".into(), "16".into()],
            config: vec![],
            exclude: vec![],
        };
        if let Some(o) = gate.boot("ktest nmi-storm", storm) {
            gate.step("ktest nmi-storm", || ktest::run_ktests(root, &o));
        }
        // PC-W31: the kernel-table lock under an unmap with a third CPU serving shootdowns; the plain run's -smp 2 has none.
        let paging = ktest::KtestOpts {
            filters: vec!["mm.paging.unmap_4g_under_load".into(), "mm.paging.kernel_tables_smp".into()],
            timeout: ktest::DEFAULT_TIMEOUT_S,
            build: BuildOpts::default(),
            net: "none".into(),
            extra: vec!["-smp".into(), "8".into()],
            config: vec![],
            exclude: vec![],
        };
        if let Some(o) = gate.boot("ktest paging smp8", paging) {
            gate.step("ktest paging smp8", || ktest::run_ktests(root, &o));
        }
        // C1 (A1 open point 6): the Mini-PC runs a release kernel and MCP runs the Safe tests on a 64 KiB thread,
        // so the release build's Safe and Display tests run too, as over MCP (ktest_hw=on), each measured against
        // 48 KiB (CONTRACTS section 5; a test over it is a red `KTEST note`).
        if let Some(o) = gate.boot("ktest release", release_ktest_opts()) {
            gate.step("ktest release", || {
                let (mut rel, r) = ktest::run_ktests_summaries(root, &o);
                universe.append(&mut rel);
                r
            });
        }
        // TS-1: a test's class names the boot that gives it its device or CONFIG line: the plain boot's list says
        // which scenario scripts the gate has to run; they run here as fullrun runs them, chain per folder.
        let named = universe.first().map(crate::ledger::scripts_named).unwrap_or_default();
        let mut wanted = named.clone();
        // No class names the Reload scenario, and CONTRACTS section 5 wants it in every gate (a Reload regression
        // crashed the Mini-PC on 20.10.).
        wanted.extend(ALWAYS_IN_GATE.iter().map(|s| s.to_string()));
        if let Some(i) = gate.interim.as_mut() {
            wanted = i.scripts(wanted, &named);
        }
        gate.step("scenario boots", || gate_scenarios(root, &wanted, &mut ran_scripts));
        let r = match &gate.interim {
            Some(i) => crate::ledger::check_interim(root, &universe, &i.list.ktest, &i.line()),
            None => crate::ledger::check(root, &universe),
        };
        gate.step("ledger", || r);
    }
    gate.step("measure build", || measure_build(root));
    gate.step("warnings", || warnings_gate(root, skip_ktest));
    (gate.finish(), ran_scripts)
}

/// The script of a run name, `<folder>/<script>`.
fn script_of(root: &Path, run: &str) -> Option<Script> {
    let (lane, name) = run.split_once('/')?;
    let path = root.join("host/tests").join(lane).join(format!("{name}.sh"));
    path.is_file().then(|| Script { lane: lane.to_string(), name: name.to_string(), path })
}

/// The scripts the classes name, chains per folder in parallel. A name without a script is red.
fn gate_scenarios(root: &Path, wanted: &BTreeSet<String>, ran: &mut BTreeSet<String>) -> Result<(), String> {
    let mut bad = Vec::new();
    let mut chains: std::collections::BTreeMap<String, Vec<Script>> = Default::default();
    for run in wanted {
        match script_of(root, run) {
            Some(s) if is_minipc(&s.path) => bad.push(format!("{run}: carries `# minipc:`, no QEMU run starts it")),
            Some(s) => chains.entry(s.lane.clone()).or_default().push(s),
            None => bad.push(format!("{run}: no such script host/tests/{run}.sh")),
        }
    }
    println!("scenario boots: {} script(s) named by test classes", wanted.len());
    let timeout = Duration::from_secs(DEFAULT_SCRIPT_TIMEOUT_S);
    for (name, (ok, msg)) in run_chains(root, chains.into_iter().collect(), timeout)? {
        println!("scenario boots: {name}: {} {msg}", if ok { "ok" } else { "FAIL" });
        ran.insert(name.clone());
        if !ok {
            bad.push(format!("{name}: {msg}"));
        }
    }
    if bad.is_empty() { Ok(()) } else { Err(bad.join("; ")) }
}

/// Runs the chains in parallel: one thread per folder, its scripts one after the other. The sysroot first when one
/// of them builds or boots V2 (once, before the threads: their image builds would race to make it).
fn run_chains(root: &Path, chains: Vec<(String, Vec<Script>)>, timeout: Duration) -> Result<Vec<(String, (bool, String))>, String> {
    if chains.iter().flat_map(|(_, sc)| sc).any(|s| crate::sysroot_need::needs_sysroot(root, &s.path)) {
        crate::abi_diff::sysroot(root)?;
    }
    let handles: Vec<_> = chains
        .into_iter()
        .map(|(lane, scripts)| {
            let root = root.to_path_buf();
            std::thread::spawn(move || {
                scripts.iter().map(|s| { let r = run_script(&root, s, timeout); (format!("{lane}/{}", s.name), r) }).collect::<Vec<_>>()
            })
        })
        .collect();
    let mut out = Vec::new();
    for h in handles {
        out.extend(h.join().map_err(|_| "chain panicked".to_string())?);
    }
    Ok(out)
}

/// The gate's release run: every Safe and Display test of a release kernel, with the stack budget of MCP's thread.
fn release_ktest_opts() -> ktest::KtestOpts {
    ktest::KtestOpts {
        filters: vec![String::new()],
        timeout: ktest::DEFAULT_TIMEOUT_S,
        build: BuildOpts { release: true, ..BuildOpts::default() },
        net: "virtio".into(),
        extra: vec![],
        config: vec!["ktest_hw=on".into(), "ktest_stack=48".into()],
        exclude: vec![],
    }
}

/// The `warning` lines of a build's output (rustc's and cargo's, the lint summaries included).
pub fn warning_lines(out: &str) -> Vec<String> {
    out.lines().map(str::trim_end).filter(|l| l.starts_with("warning")).map(String::from).collect()
}

/// CONTRACTS §4 (Georg 29.10., INT-W28 post-integration): not one compiler warning in any build the gate made. The
/// gate's builds again, mostly cached by now, so cargo only replays their diagnostics: the host crates with their
/// tests, the user workspace with its tests, bitscomp, ctxd and the Rust ABI programs (own workspaces, with their
/// tests), the measuring kernel and, with the ktests, the ktest kernel, bitsboot and the user programs. The lean
/// kernels have their own check (sc/30).
fn warnings_gate(root: &Path, skip_ktest: bool) -> Result<(), String> {
    let mut builds: Vec<(&str, Command)> = Vec::new();
    let mut c = cargo(root);
    c.args(["test", "--workspace", "--exclude", "kernel", "--no-run"]);
    builds.push(("host crates", c));
    // A `#[cfg(debug_assertions)]` on a test leaves its helpers unused in a release test build.
    let mut c = cargo(root);
    c.args(["test", "--workspace", "--exclude", "kernel", "--no-run", "--release"]);
    builds.push(("host crates release", c));
    // C1: every user crate's test cfg, not only the ones the gate runs (init, the tools and services too).
    let mut c = cargo(root);
    c.current_dir(root.join("user")).args(["check", "--tests", "--workspace"]);
    builds.push(("user workspace tests", c));
    // bitscomp and its tests as the gate's step builds them (its own workspace, the host's /usr as its Mesa tree).
    let comp = root.join("user/services/bitscomp");
    for (what, args) in [("bitscomp", &["build", "--release", "--offline", "--locked"][..]), ("bitscomp tests", &["test", "--no-run", "--release", "--offline", "--locked"][..])] {
        let mut c = cargo(root);
        c.current_dir(&comp).args(args).env("BITSCOMP_GPU_ROOT", "/usr").env("CARGO_TARGET_DIR", root.join("target/bitscomp"));
        builds.push((what, c));
    }
    // Own workspaces outside user/'s (C1): ctxd with its integration tests, and the ABI diff programs in Rust (built
    // for V2 by `abi-diff` with user/'s stable toolchain).
    let mut c = Command::new("cargo");
    c.current_dir(root.join("user/services/ctxd")).args(["check", "--tests", "--offline"]).env("CARGO_TARGET_DIR", root.join("target/ctxd-check"));
    builds.push(("ctxd tests", c));
    let mut c = Command::new("cargo");
    c.current_dir(root.join("user/services/atspid"))
        .env_remove("RUSTUP_TOOLCHAIN")
        .args(["check", "--tests", "--offline", "--locked"])
        .env("CARGO_TARGET_DIR", root.join("target/atspid-test"));
    builds.push(("atspid tests", c));
    let mut c = Command::new("cargo");
    c.current_dir(root.join("user/abi-tests/rust"))
        .env_remove("RUSTUP_TOOLCHAIN")
        .args(["check", "--tests", "--offline"])
        .env("CARGO_TARGET_DIR", root.join("target/abi-rs-check"));
    builds.push(("abi-rs tests", c));
    let cfg = std::fs::read_to_string(root.join(".cargo/config.toml")).map_err(|e| format!(".cargo/config.toml: {e}"))?;
    let mut flags = kernel_rustflags(&cfg);
    flags.extend(["--cfg".into(), "bitsos_fd_measure".into()]);
    let mut c = cargo(root);
    c.args(["build", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .env("CARGO_TARGET_DIR", root.join("target/measure-build"))
        .env("CARGO_TARGET_X86_64_UNKNOWN_NONE_RUSTFLAGS", flags.join(" "));
    builds.push(("measure kernel", c));
    if !skip_ktest {
        let mut c = cargo(root);
        c.args(["build", "-p", "kernel", "--target", "x86_64-unknown-none", "--features", "ktest"]);
        builds.push(("ktest kernel", c));
        let mut c = cargo(root);
        c.args(["build", "-p", "bitsboot", "--target", "x86_64-unknown-uefi"]);
        builds.push(("bitsboot", c));
        let mut c = Command::new("cargo");
        c.current_dir(root.join("user"))
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env("BITSOS_BUILD_EPOCH", crate::image::build_epoch(root))
            .args(["build", "--manifest-path", "Cargo.toml", "--target", "x86_64-unknown-linux-musl", "--release", "--target-dir"])
            .arg(root.join("target/user"));
        builds.push(("user programs", c));
    }
    let mut bad = Vec::new();
    for (what, mut c) in builds {
        let out = c.output().map_err(|e| format!("{what}: cannot start cargo: {e}"))?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            return Err(format!("{what}: the build failed ({})", out.status));
        }
        let w = warning_lines(&text);
        eprintln!("xtask: warnings in {what}: {}", w.len());
        for l in w.iter().take(20) {
            eprintln!("  {l}");
        }
        if !w.is_empty() {
            bad.push(format!("{what}: {}", w.len()));
        }
    }
    if bad.is_empty() { Ok(()) } else { Err(format!("compiler warnings (CONTRACTS §4): {}", bad.join(", "))) }
}

/// The kernel target's rustflags from .cargo/config.toml (the quoted words of `[target.x86_64-unknown-none]`'s
/// `rustflags` array; comments skipped).
fn kernel_rustflags(config: &str) -> Vec<String> {
    let mut v = Vec::new();
    let (mut in_target, mut in_array) = (false, false);
    for l in config.lines() {
        let t = l.split('#').next().unwrap_or("").trim();
        if t.starts_with('[') && !in_array {
            in_target = t == "[target.x86_64-unknown-none]";
            continue;
        }
        if in_target && t.starts_with("rustflags") {
            in_array = true;
        }
        if in_array {
            v.extend(t.split('"').skip(1).step_by(2).map(String::from));
            if t.contains(']') {
                in_array = false;
                in_target = false;
            }
        }
    }
    v
}

/// INT-W19 (AB-15 CR 7): the kernel with `--cfg bitsos_fd_measure` (FD-1's measuring build) still compiles
/// (hal::entry::steps, hal::prof, syscall's ab15 ktests). Build only, no run, in a target dir of its own so the
/// gate's kernel is not rebuilt.
fn measure_build(root: &Path) -> Result<(), String> {
    let cfg = std::fs::read_to_string(root.join(".cargo/config.toml")).map_err(|e| format!("measure build: .cargo/config.toml: {e}"))?;
    let mut flags = kernel_rustflags(&cfg);
    if flags.is_empty() {
        return Err("measure build: no kernel rustflags in .cargo/config.toml".into());
    }
    flags.extend(["--cfg".into(), "bitsos_fd_measure".into()]);
    let mut c = cargo(root);
    c.args(["build", "-q", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .env("CARGO_TARGET_DIR", root.join("target/measure-build"))
        .env("CARGO_TARGET_X86_64_UNKNOWN_NONE_RUSTFLAGS", flags.join(" "));
    run_status(&mut c, "cargo build (kernel, --cfg bitsos_fd_measure)")
}

/// `xtask test` is the full gate (the integrator's run); `--interim` leaves the stable tests out (crate::interim).
pub fn cmd_test(root: &Path, mut a: Args) -> Result<(), String> {
    let skip = a.flag("--no-ktest");
    let interim = a.flag("--interim");
    a.positionals()?;
    let interim = if interim { Some(Interim::load(root)?) } else { None };
    run_test_scripts(root, skip, interim).0
}

struct Script {
    lane: String,
    path: PathBuf,
    name: String,
}

fn is_slow(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|s| s.lines().take(5).any(|l| l.trim_start_matches('#').trim().starts_with("slow") && l.starts_with('#'))).unwrap_or(false)
}

/// Georg 30.10., programs only on the Mini-PC: a script that boots a program tree in QEMU says `# minipc: <where its
/// checks run now>` in its first five lines; no regular QEMU run starts it.
fn is_minipc(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|s| s.lines().take(5).any(|l| l.starts_with('#') && l.trim_start_matches('#').trim().starts_with("minipc:"))).unwrap_or(false)
}

/// A script that needs longer than the default says `# time-limit: <seconds>` in its first five lines (the queue for
/// `cargo xtask image` counts against it: vs/30 lost 18.5 of its 20 minutes that way, VS-H CR 1). It can only lengthen
/// the limit, so a test's own short limit still counts.
fn script_limit(path: &Path, timeout: Duration) -> Duration {
    let own = std::fs::read_to_string(path).ok().and_then(|s| {
        s.lines().take(5).find_map(|l| l.strip_prefix('#')?.trim().strip_prefix("time-limit:")?.trim().parse::<u64>().ok())
    });
    match own {
        Some(s) if timeout.as_secs() >= DEFAULT_SCRIPT_TIMEOUT_S => timeout.max(Duration::from_secs(s)),
        _ => timeout,
    }
}

/// VS-H CR 1: how long ago the newest `*serial*` file below `dir` last grew, the line that tells a guest that went
/// silent from a script that ran out of time. None: no such file.
fn newest_serial_age(dir: &Path, depth: u32) -> Option<Duration> {
    let mut best: Option<Duration> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        let age = if p.is_dir() {
            if depth == 0 { None } else { newest_serial_age(&p, depth - 1) }
        } else if e.file_name().to_string_lossy().contains("serial") {
            e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| t.elapsed().unwrap_or_default())
        } else {
            None
        };
        best = match (best, age) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    best
}

/// A helper the scenarios call with arguments says `# helper` in its first five lines: no run starts it by itself.
fn is_helper(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|s| s.lines().take(5).any(|l| l.starts_with('#') && l.trim_start_matches('#').trim().starts_with("helper"))).unwrap_or(false)
}

/// Scenario scripts grouped by lane folder: `host/tests/<lane>/*.sh`. Not `host/tests/minipc/` (the desk's runs against
/// the real machine), no script marked `# minipc:` and no helper.
fn find_scripts(root: &Path, quick: bool) -> Vec<(String, Vec<Script>)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root.join("host/tests")) else { return out };
    let mut dirs: Vec<_> = rd.flatten().filter(|e| e.path().is_dir() && e.file_name() != "minipc").collect();
    dirs.sort_by_key(|e| e.file_name());
    for d in dirs {
        let lane = d.file_name().to_string_lossy().to_lowercase();
        let mut sc: Vec<Script> = std::fs::read_dir(d.path())
            .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "sh").unwrap_or(false)).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !(quick && is_slow(p)) && !is_minipc(p) && !is_helper(p))
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
    // Read through a handle of our own: a script that empties its run dir (`rm -rf "$RUN_DIR"`, pk/20, pk/50, ul/31)
    // unlinks the log, and the skip check below would read nothing (INT-W38). The log is put back afterwards.
    let Ok(mut reader) = std::fs::File::open(&log) else { return (false, "cannot open log".into()) };
    let child = Command::new("bash")
        .arg(&s.path)
        .current_dir(root)
        .env("BITSOS_ROOT", root)
        .env("BITSOS_LANE", code)
        .env("BITSOS_PORT_LO", lo.to_string())
        .env("BITSOS_PORT_HI", hi.to_string())
        .env("BITSOS_RUN_DIR", &run_dir)
        .env("BITSOS_KTEST_RUN", format!("{}/{}", s.lane, s.name))
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
    let timeout = script_limit(&s.path, timeout);
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            crate::ledger::scan_run_dir(root, &format!("{}/{}", s.lane, s.name), &run_dir);
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut reader, &mut bytes);
            if !log.exists() {
                let _ = std::fs::create_dir_all(&run_dir).and_then(|()| std::fs::write(&log, &bytes));
            }
            let out = String::from_utf8_lossy(&bytes).into_owned();
            if let Some(l) = crate::ledger::skip_line(&out).filter(|_| st.success()) {
                return (false, format!("{st} but it skipped, a skip is red: '{l}' (log {})", log.display()));
            }
            return (st.success(), format!("{} (log {})", st, log.display()));
        }
        if start.elapsed() >= timeout {
            // The script's own process group only (its pid is the group id).
            let _ = Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).status();
            let _ = child.kill();
            let _ = child.wait();
            let serial = newest_serial_age(&run_dir, 3).map_or("no serial log".to_string(), |a| format!("newest serial log last grew {} s ago", a.as_secs()));
            return (false, format!("timeout after {} s, {serial} (log {})", timeout.as_secs(), log.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn cmd_fullrun(root: &Path, mut a: Args) -> Result<(), String> {
    let quick = a.flag("--quick");
    let timeout = Duration::from_secs(a.num::<u64>("--script-timeout")?.unwrap_or(DEFAULT_SCRIPT_TIMEOUT_S));
    a.positionals()?;
    let mut res = Vec::new();
    let (r, in_gate) = run_test_scripts(root, false, None);
    step("test", r, &mut res);
    step("deps", deps::run_deps(root), &mut res);
    // The scripts the gate has run are green or red already.
    let chains: Vec<_> = find_scripts(root, quick)
        .into_iter()
        .map(|(lane, sc)| {
            let sc: Vec<Script> = sc.into_iter().filter(|s| !in_gate.contains(&format!("{}/{}", s.lane, s.name))).collect();
            (lane, sc)
        })
        .filter(|(_, sc)| !sc.is_empty())
        .collect();
    let n: usize = chains.iter().map(|(_, s)| s.len()).sum();
    println!("fullrun: {n} scenario script(s) in {} chain(s){}", chains.len(), if quick { " (quick)" } else { "" });
    for (name, (ok, msg)) in run_chains(root, chains, timeout)? {
        println!("fullrun: {name}: {} {msg}", if ok { "ok" } else { "FAIL" });
        res.push((format!("scenario {name}"), ok));
    }
    summary(&res)
}

/// `xtask scenario <folder>/<script>`: one scenario script with the environment a run gives it (BITSOS_KTEST_RUN and
/// the rest), the way to repeat one by hand.
pub fn cmd_scenario(root: &Path, mut a: Args) -> Result<(), String> {
    let timeout = Duration::from_secs(a.num::<u64>("--script-timeout")?.unwrap_or(DEFAULT_SCRIPT_TIMEOUT_S));
    let pos = a.positionals()?;
    let run = pos.first().ok_or("usage: xtask scenario <folder>/<script> [--script-timeout SECS]")?;
    let run = run.strip_suffix(".sh").unwrap_or(run);
    let s = script_of(root, run).ok_or_else(|| format!("no script host/tests/{run}.sh"))?;
    if crate::sysroot_need::needs_sysroot(root, &s.path) {
        crate::abi_diff::sysroot(root)?;
    }
    let (ok, msg) = run_script(root, &s, timeout);
    println!("scenario {run}: {} {msg}", if ok { "ok" } else { "FAIL" });
    if ok { Ok(()) } else { Err("red".into()) }
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

    /// INT-W38: a script that empties its run dir (and so unlinks output.log) and then says it skipped is still red,
    /// and its log is back in the run dir.
    #[test]
    fn a_skip_after_the_run_dir_was_emptied_is_red() {
        let root = std::env::temp_dir().join(format!("xt-runscript-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("wipe.sh");
        std::fs::write(&path, "rm -rf \"$BITSOS_RUN_DIR\"; mkdir -p \"$BITSOS_RUN_DIR\"; echo 'SKIP: tool missing'; exit 0\n").unwrap();
        let s = Script { lane: "zz".into(), path, name: "wipe".into() };
        let (ok, msg) = run_script(&root, &s, Duration::from_secs(30));
        assert!(!ok && msg.contains("SKIP: tool missing"), "{msg}");
        let log = std::fs::read_to_string(xdir(&root).join("runs/zz-wipe/output.log")).unwrap();
        assert!(log.contains("SKIP: tool missing"), "{log}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// SK: every script the gate runs without a class is there and may run in QEMU.
    #[test]
    fn always_in_gate_scripts_exist() {
        let root = crate::util::root();
        for run in ALWAYS_IN_GATE {
            let s = script_of(&root, run).unwrap_or_else(|| panic!("{run}: no host/tests/{run}.sh"));
            assert!(!is_minipc(&s.path), "{run} carries `# minipc:`");
        }
    }

    #[test]
    fn home_lines_find_the_home_as_a_path_only() {
        let t = b"a\nx=/home/ann/BitsOS/y\n'/home/ann'\n/home/anna/x\n/home/ann.bak\n$HOME/BitsOS\n/home/ann";
        assert_eq!(home_lines(t, "/home/ann"), [2, 3, 7]);
        assert!(home_lines(b"/data/home/1000/Downloads\n", "/home/ann").is_empty());
    }

    #[test]
    fn the_tree_names_no_home_outside_plan() {
        let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
        if std::env::var("HOME").is_ok_and(|h| h.matches('/').count() >= 2) {
            home_path_gate(root).unwrap();
        }
    }

    #[test]
    fn the_release_ktest_step_runs_as_mcp_does() {
        let o = release_ktest_opts();
        assert!(o.build.release);
        assert_eq!(o.config, ["ktest_hw=on", "ktest_stack=48"]);
        assert_eq!(o.filters, [""]);
    }

    #[test]
    fn warning_lines_are_found() {
        let out = "   Compiling a v0.1.0\nwarning: unused import: `x`\n --> a.rs:1:5\nwarning: `a` (lib) generated 1 warning\n    Finished `dev`\n";
        assert_eq!(warning_lines(out).len(), 2);
        assert!(warning_lines("    Finished `dev` profile\nerror: no\n").is_empty());
    }

    #[test]
    fn kernel_rustflags_from_config() {
        let cfg = "[build]\nx = 1\n[target.x86_64-unknown-none]\nrustflags = [\n  \"-C\", \"code-model=kernel\",\n  # a \"comment\"\n  \"-C\", \"force-frame-pointers=yes\",\n]\n[target.x86_64-unknown-uefi]\nrustflags = [\"-C\", \"x\"]\n";
        assert_eq!(kernel_rustflags(cfg), ["-C", "code-model=kernel", "-C", "force-frame-pointers=yes"]);
        let real = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.cargo/config.toml")).unwrap();
        assert!(kernel_rustflags(&real).iter().any(|f| f == "code-model=kernel"));
    }

    #[test]
    fn unsafe_outside_src_detection() {
        assert!(mentions_unsafe("fn f() { unsafe { g() } }\n"));
        assert!(!mentions_unsafe("// unsafe here is only a comment\nfn f() {}\n"));
        assert!(!mentions_unsafe("fn unsafe_name() {}\n"));
    }

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
    fn a_script_can_only_lengthen_its_limit() {
        let d = std::env::temp_dir().join(format!("xt-limit-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.sh"), "#!/bin/bash\n# time-limit: 1500\necho\n").unwrap();
        std::fs::write(d.join("b.sh"), "#!/bin/bash\necho time-limit: 1500\n").unwrap();
        let dflt = Duration::from_secs(DEFAULT_SCRIPT_TIMEOUT_S);
        assert_eq!(script_limit(&d.join("a.sh"), dflt), Duration::from_secs(1500));
        assert_eq!(script_limit(&d.join("b.sh"), dflt), dflt);
        assert_eq!(script_limit(&d.join("a.sh"), Duration::from_secs(5)), Duration::from_secs(5), "a short limit asked for stays");
        assert_eq!(script_limit(&d.join("a.sh"), Duration::from_secs(2000)), Duration::from_secs(2000));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_timeout_names_the_newest_serial_log() {
        let d = std::env::temp_dir().join(format!("xt-serial-{}", std::process::id()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        assert_eq!(newest_serial_age(&d, 3), None);
        std::fs::write(d.join("output.log"), "x").unwrap();
        assert_eq!(newest_serial_age(&d, 3), None, "only serial logs count");
        std::fs::write(d.join("sub/serial-2.log"), "x").unwrap();
        assert!(newest_serial_age(&d, 3).is_some_and(|a| a < Duration::from_secs(60)));
        assert_eq!(newest_serial_age(&d, 0), None);
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
        std::fs::write(d.join("d_tree.sh"), "#!/bin/bash\n# minipc: the check list\nexit 9\n").unwrap();
        std::fs::write(d.join("e_help.sh"), "#!/bin/bash\n# helper: called by the others\nexit 9\n").unwrap();
        std::fs::create_dir_all(root.join("host/tests/minipc")).unwrap();
        std::fs::write(root.join("host/tests/minipc/desk.sh"), "exit 9\n").unwrap();
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

#[cfg(test)]
#[path = "suite_sysroot_tests.rs"]
mod sysroot_tests;

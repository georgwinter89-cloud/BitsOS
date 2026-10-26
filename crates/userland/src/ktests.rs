//! UL-2 ktests. All Safe and rerunnable in one boot: scratch names carry a run counter, every process a test
//! starts has ended (or was killed) before the test returns, scratch trees are removed.
//!
//! The programs come from the user workspace, which `xtask image` builds into `/bin` of every image; a missing
//! one fails the test (like AB's `ab.proc.real_programs`), it is not a skip.

use crate::compare;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ktest::{TestResult, kassert, ktest};
use proc::stdio::{Capture, Input};
use proc::{Process, SpawnOptions};

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const SIGKILL: u32 = uapi::ab::signal::SIGKILL;

mod bb;
mod brave;
mod cobalt;
mod fetch;
mod pkg;
mod settings;
mod tools;

static RUN: AtomicU32 = AtomicU32::new(0);

fn need(path: &str) -> TestResult {
    kassert!(vfs::root_ns().lookup(path).is_ok(), "{path} is not in this image (xtask builds the user workspace into /bin)");
    Ok(())
}

fn text(c: &Capture) -> String {
    String::from_utf8_lossy(&c.contents()).into_owned()
}

struct Started {
    p: Arc<Process>,
    out: Arc<Capture>,
    err: Arc<Capture>,
}

impl Started {
    /// stdout and stderr for a failure message.
    fn both(&self) -> String {
        format!("{}--- stderr ---\n{}", text(&self.out), text(&self.err))
    }
}

fn start(path: &str, argv: &[&str], parent: Option<&Arc<Process>>, held: bool) -> Result<Started, String> {
    start_as(path, argv, parent, held, None)
}

/// `start` with another uid than the spawn default (`Some(0)`: a root program, as busybox's root-only applets need).
fn start_as(path: &str, argv: &[&str], parent: Option<&Arc<Process>>, held: bool, uid: Option<u32>) -> Result<Started, String> {
    let out = Capture::new(1 << 20);
    let err = Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdin = Some(Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(err.clone() as Arc<dyn vfs::File>);
    o.parent = parent;
    o.held = held;
    if let Some(u) = uid {
        o.uid = u;
    }
    let p = proc::spawn_with(path, &o).map_err(|e| format!("spawn {path}: {e}"))?;
    Ok(Started { p, out, err })
}

/// Waits for the end; kills the process when it takes longer than `timeout_ns`.
fn finish(s: &Started, timeout_ns: u64) -> Result<i32, String> {
    match s.p.wait(Some(timeout_ns)) {
        Some(c) => Ok(c),
        None => {
            s.p.kill(SIGKILL);
            let _ = s.p.wait(Some(10 * SEC));
            Err(format!("{} did not end within {} s (killed); stdout so far:\n{}", s.p.name, timeout_ns / SEC, text(&s.out)))
        }
    }
}

/// Polls `cond` every 20 ms up to `timeout_ns`.
fn wait_for(timeout_ns: u64, mut cond: impl FnMut() -> bool) -> bool {
    let end = time::now_ns() + timeout_ns;
    loop {
        if cond() {
            return true;
        }
        if time::now_ns() >= end {
            return false;
        }
        sched::sleep_ns(20 * MS);
    }
}

fn scratch(what: &str) -> String {
    format!("/tmp/ul-{what}-{}-{}", RUN.fetch_add(1, Ordering::Relaxed), time::now_ns() % 1_000_000)
}

fn read(path: &str) -> String {
    vfs::root_ns().read_file(path, 1 << 20).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

/// The first DNS server init should write: the default card's, else any leased card's (init's own rule, simple
/// form: the test only needs the first line).
fn expected_first_dns(view: &str) -> Option<String> {
    let cards: Vec<&str> = view.lines().filter(|l| l.starts_with("iface=") && !l.contains(" addr=- ")).collect();
    let pick = |l: &&str| l.split_whitespace().find_map(|f| f.strip_prefix("dns=")).filter(|d| *d != "-").map(|d| d.split(',').next().unwrap_or("").to_string());
    cards.iter().filter(|l| l.contains(" default=yes")).find_map(pick).or_else(|| cards.iter().find_map(pick))
}

// ---------------------------------------------------------------------------------------------------------
// init

/// `/bin/init --once --root <scratch>`: one pass writes /etc/{hostname,hosts,resolv.conf} from /proc/net/dhcp
/// and makes the directories, then exits 0.
fn init_once() -> TestResult {
    need("/bin/init")?;
    // Give DHCP a moment (QEMU's user network answers within a second); without a lease the no-DNS form is checked.
    if let Some(st) = net::stack::kernel() {
        let _ = st.wait_configured(10 * SEC);
    }
    let view = read("/proc/net/dhcp");
    kassert!(view.starts_with(crate::dhcpview::HEADER), "/proc/net/dhcp: {view:?}");
    let root = scratch("init");
    let s = start("/bin/init", &["init", "--once", "--root", &root], None, false)?;
    let code = finish(&s, 30 * SEC)?;
    let out = text(&s.out);
    let r = (|| -> TestResult {
        kassert!(code == 0, "init --once ended with {code}: {out}\nstderr: {}", text(&s.err));
        for want in ["init: BitsOS init starting", "init: ready (host name bitsos", "init: network: ", "one round done (--once, reaped 0)"] {
            kassert!(out.contains(want), "init output lacks '{want}':\n{out}");
        }
        kassert!(read(&format!("{root}/etc/hostname")) == "bitsos\n", "hostname: {:?}\n{out}", read(&format!("{root}/etc/hostname")));
        let hosts = read(&format!("{root}/etc/hosts"));
        kassert!(hosts.contains("127.0.0.1\tlocalhost\n") && hosts.contains("\tbitsos\n"), "hosts: {hosts:?}\n{out}");
        let resolv = read(&format!("{root}/etc/resolv.conf"));
        let first_ns = resolv.lines().find(|l| l.starts_with("nameserver ")).unwrap_or("-");
        match expected_first_dns(&view) {
            Some(d) => kassert!(first_ns == format!("nameserver {d}"), "resolv.conf {resolv:?} for view {view:?}"),
            None => kassert!(resolv.contains("no DNS server known yet"), "resolv.conf {resolv:?} for view {view:?}"),
        }
        // Exact modes, sticky bit included (init's mkdir runs with umask 0; no chmod needed).
        for (d, mode) in [("tmp", 0o1777), ("run", 0o755), ("dev/shm", 0o1777), ("etc", 0o755), ("root", 0o700)] {
            let path = format!("{root}/{d}");
            let got = vfs::root_ns().lookup(&path).ok().filter(|x| x.is_dir()).map(|x| x.inode.meta.mode() & 0o7777);
            kassert!(got == Some(mode), "{path}: mode {:?} instead of {mode:o}:\n{out}", got.map(|m| format!("{m:o}")));
        }
        klog::event!(Info, "ul", "test.init_once", resolv = first_ns);
        Ok(())
    })();
    let _ = vfs::root_ns().remove_tree(&root);
    r
}
ktest!("ul.init.once", Safe, init_once);

/// init in its normal (never-ending) mode under a scratch root reaps a child that ends: the child leaves the
/// process table through init's wait4, init says so and keeps running.
fn init_reaps() -> TestResult {
    need("/bin/init")?;
    need("/bin/hello")?;
    let root = scratch("reap");
    let init = start("/bin/init", &["init", "--root", &root, "--interval-ms", "50"], None, false)?;
    let r = (|| -> TestResult {
        kassert!(wait_for(20 * SEC, || text(&init.out).contains("init: ready")), "init not ready:\n{}", text(&init.out));
        let child = start("/bin/hello", &["hello"], Some(&init.p), false)?;
        let pid = child.p.pid;
        kassert!(child.p.ppid() == init.p.pid, "child's parent {} is not init {}", child.p.ppid(), init.p.pid);
        let code = finish(&child, 20 * SEC)?;
        kassert!(code == 0, "hello ended with {code}");
        drop(child);
        kassert!(wait_for(20 * SEC, || proc::find(pid).is_none()), "pid {pid} still in the table (not reaped):\n{}", init.both());
        let want = format!("init: reaped pid {pid} (exit 0)");
        kassert!(wait_for(5 * SEC, || text(&init.out).contains(&want)), "init output lacks '{want}':\n{}", init.both());
        kassert!(!init.p.has_exited(), "init ended: {:?}\n{}", init.p.status(), init.both());
        Ok(())
    })();
    init.p.kill(SIGKILL);
    let ended = init.p.wait(Some(10 * SEC)).is_some();
    let _ = vfs::root_ns().remove_tree(&root);
    r?;
    kassert!(ended, "init did not end after SIGKILL");
    Ok(())
}
ktest!("ul.init.reaps", Safe, init_reaps);

/// `/bin/init` on the real file tree as the kernel starts it, except that it is pointed at a shell that does not
/// exist (on a running system the real init owns the console; a test must not start a second shell there): it
/// finds /proc, /sys and /dev mounted, logs what it cannot do, and keeps running.
fn init_real_mode() -> TestResult {
    need("/bin/init")?;
    let init = start("/bin/init", &["/bin/init", "--shell", "/bin/ul-test-no-shell"], None, false)?;
    let r = (|| -> TestResult {
        kassert!(wait_for(20 * SEC, || text(&init.out).contains("init: ready")), "init not ready:\n{}", init.both());
        kassert!(wait_for(10 * SEC, || text(&init.out).contains("no console shell (/bin/ul-test-no-shell missing")), "no shell line:\n{}", init.both());
        sched::sleep_ns(1500 * MS);
        let out = text(&init.out);
        kassert!(!init.p.has_exited(), "init ended: {:?}\n{}", init.p.status(), init.both());
        for want in ["/proc: mounted already", "/sys: mounted already", "/dev: mounted already"] {
            kassert!(out.contains(want), "init output lacks '{want}':\n{out}");
        }
        klog::event!(Info, "ul", "test.init_real_mode", lines = out.lines().count());
        Ok(())
    })();
    init.p.kill(SIGKILL);
    let _ = init.p.wait(Some(10 * SEC));
    r
}
ktest!("ul.init.real_mode", Safe, init_real_mode);

/// The supervisor's restart rule with a program that ends at once: it restarts until it gives up; a missing
/// program ends the supervision at once.
fn supervisor() -> TestResult {
    need("/bin/hello")?;
    let fast = crate::supervisor::Policy { first_delay_ns: MS, max_delay_ns: 4 * MS, healthy_ns: 60 * SEC, give_up_deaths: 3, window_ns: 600 * SEC };
    let st = crate::supervisor::supervise("/bin/hello", &["hello"], &fast, false);
    kassert!(st.starts == 3 && st.gave_up && !st.missing, "{st:?}");
    let st = crate::supervisor::supervise("/bin/ul-no-such-init", &["x"], &fast, false);
    kassert!(st.starts == 0 && st.missing && !st.gave_up, "{st:?}");
    Ok(())
}
ktest!("ul.init.supervisor", Safe, supervisor);

// ---------------------------------------------------------------------------------------------------------
// ABI transcripts

fn abi(wave: char, expected: &str, known: &str) -> TestResult {
    let path = format!("/bin/abi-{}", wave.to_ascii_lowercase());
    need(&path)?;
    let name = format!("abi-{}", wave.to_ascii_lowercase());
    let s = start(&path, &[&name], None, false)?;
    let code = finish(&s, 120 * SEC)?;
    let got = text(&s.out);
    let d = compare::compare(expected, &got, known);
    let w = format!("{wave}");
    let known_text = d.known_text();
    klog::event!(Info, "ul", "abi.result", wave = w.as_str(), code = code, same = d.same, known = d.known.len(), fixed = d.fixed.len(), new = d.new.len(), extra = d.extra.len(); "{}", known_text.as_str());
    let stderr = text(&s.err);
    kassert!(d.ok(), "abi-{wave} differs from CachyOS: {}\nexit {code}, stderr: {stderr}", d.summary());
    // A known difference may cut the transcript short (the end line is then listed as known); otherwise the
    // program must end like on Linux.
    kassert!(code == 0 || d.known.iter().any(|(id, _, _)| id.ends_with(" end")), "abi-{wave} ended with {code} (stderr: {stderr})");
    if !d.fixed.is_empty() {
        let ids = d.fixed.join(" ");
        klog::event!(Info, "ul", "abi.fixed", wave = w.as_str(), ids = ids.as_str(); "known differences that now equal Linux: remove them from the .known file");
    }
    Ok(())
}

fn wave_a() -> TestResult {
    abi('A', include_str!("../../../user/abi-tests/expected/wave-a.txt"), include_str!("../../../user/abi-tests/expected/wave-a.known"))
}
fn wave_b() -> TestResult {
    abi('B', include_str!("../../../user/abi-tests/expected/wave-b.txt"), include_str!("../../../user/abi-tests/expected/wave-b.known"))
}
fn wave_c() -> TestResult {
    abi('C', include_str!("../../../user/abi-tests/expected/wave-c.txt"), include_str!("../../../user/abi-tests/expected/wave-c.known"))
}
ktest!("ul.abi.wave_a", Safe, wave_a);
ktest!("ul.abi.wave_b", Safe, wave_b);
ktest!("ul.abi.wave_c", Safe, wave_c);

// ---------------------------------------------------------------------------------------------------------
// stress

fn cpu_set(ranges: &str, into: &mut BTreeSet<u32>) {
    for part in ranges.split(',').filter(|p| !p.is_empty()) {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
            for c in a..=b.min(a.saturating_add(4096)) {
                into.insert(c);
            }
        }
    }
}

/// One `stress -t 1` process per online CPU, released together: each counts for a second and reports the CPUs
/// it ran on. Functional here (all count, all end 0); how many CPUs they spread over is judged on the Mini-PC
/// (event `ul stress.procs`). Then one `stress` with threads (one per CPU once clone works, AB-4).
fn stress_procs() -> TestResult {
    need("/bin/stress")?;
    let n = sched::online_cpus().max(1);
    let mut started = Vec::new();
    for _ in 0..n {
        started.push(start("/bin/stress", &["stress", "-t", "1", "-s", "1"], None, true)?);
    }
    for s in &started {
        s.p.release();
    }
    let mut cpus = BTreeSet::new();
    let mut failures = Vec::new();
    for s in &started {
        match finish(s, 60 * SEC) {
            Ok(0) => {}
            Ok(c) => failures.push(format!("pid {} exit {c}: {}", s.p.pid, text(&s.out))),
            Err(e) => failures.push(e),
        }
        let out = text(&s.out);
        let counted = out.lines().filter_map(|l| l.strip_prefix("stress: worker ")).any(|l| {
            if let Some(c) = l.split_whitespace().find_map(|f| f.strip_prefix("cpus=")) {
                cpu_set(c, &mut cpus);
            }
            l.split_whitespace().find_map(|f| f.strip_prefix("count=")).and_then(|c| c.parse::<u64>().ok()).is_some_and(|c| c > 0)
        });
        if !counted {
            failures.push(format!("pid {} did not count: {out}", s.p.pid));
        }
    }
    let list: Vec<String> = cpus.iter().map(|c| c.to_string()).collect();
    klog::event!(Info, "ul", "stress.procs", procs = n, cpus_seen = cpus.len(), cpus = list.join(","));
    kassert!(failures.is_empty(), "{}", failures.join("\n"));
    let t = start("/bin/stress", &["stress", "-s", "1", "-q"], None, false)?;
    let code = finish(&t, 60 * SEC)?;
    let out = text(&t.out);
    let summary = out.lines().last().unwrap_or("").to_string();
    klog::event!(Info, "ul", "stress.threads", code = code, summary = summary.as_str());
    kassert!(code == 0 && summary.starts_with("stress: ") && summary.contains("workers"), "stress with threads: exit {code}: {out}");
    Ok(())
}
ktest!("ul.stress.procs", Safe, stress_procs);

//! AB-7a ktests: the files of `/proc/<pid>` of a held test process read by the kernel (as the MCP tool reads
//! them), and `abi-procfs` against CachyOS's transcript (`user/abi-tests/expected/ab7-procfs.txt`) with the
//! known differences of `ab7.known` skipped.
//!
//! All `Safe` and rerunnable: each test spawns its own processes and ends and reaps them; every wait has a
//! deadline.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

const SEC: u64 = 1_000_000_000;
const EXPECTED: &str = include_str!("../../../user/abi-tests/expected/ab7-procfs.txt");
const KNOWN: &str = include_str!("../../../user/abi-tests/expected/ab7.known");

pub(crate) fn text(path: &str) -> Result<String, String> {
    let b = vfs::root_ns().read_file(path, 1 << 20).map_err(|e| format!("{path}: errno {e}"))?;
    String::from_utf8(b).map_err(|_| format!("{path}: not UTF-8"))
}

pub(crate) fn have(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

fn held(argv: &[&str], env: &[&str]) -> Result<Arc<proc::Process>, String> {
    let mut o = proc::SpawnOptions::new(argv, env, policy::ProgramId(1));
    o.held = true;
    proc::spawn_with("/bin/hello", &o).map_err(|e| format!("spawn: {e}"))
}

fn end(p: &proc::Process) -> TestResult {
    p.kill(uapi::ab::signal::SIGKILL);
    kassert!(p.wait(Some(10 * SEC)).is_some(), "the held process did not end after SIGKILL");
    Ok(())
}

/// The `kB` number of `key` in a status/smaps text (first match).
fn kb_of(t: &str, key: &str) -> Option<u64> {
    t.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.trim().strip_suffix(" kB")?.parse().ok())
}

/// AB-7c: status, statm, smaps and smaps_rollup agree with each other and with MM's counters.
fn memory_numbers(status: &str, statm: &str, maps: &str, smaps: &str, roll: &str) -> TestResult {
    let k = |key: &str| kb_of(status, key).ok_or_else(|| format!("status has no {key}: {status}"));
    let (hwm, rss, anon, file, shmem, pte) = (k("VmHWM")?, k("VmRSS")?, k("RssAnon")?, k("RssFile")?, k("RssShmem")?, k("VmPTE")?);
    kassert!(hwm >= rss && rss > 0 && anon + file + shmem == rss && pte > 0 && k("VmPeak")? >= k("VmSize")?, "status: {status}");
    let m: Vec<u64> = statm.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    kassert!(m.len() == 7 && m[1] * 4 == rss && m[2] * 4 == file + shmem, "statm {statm} vs status VmRSS {rss}");
    // smaps: one entry per maps line, in the same order, and its numbers sum to the rollup.
    let heads: Vec<&str> = smaps.lines().filter(|l| l.split(' ').next().is_some_and(|w| !w.ends_with(':'))).collect();
    kassert!(heads.iter().copied().eq(maps.lines()), "smaps headers differ from maps:\n{smaps}\n--\n{maps}");
    kassert!(smaps.lines().filter(|l| l.starts_with("VmFlags: ")).count() == heads.len(), "VmFlags per entry: {smaps}");
    kassert!(smaps.lines().any(|l| l.starts_with("VmFlags: rd wr mr mw me gd ac ")), "no stack flags: {smaps}");
    let sum = |key: &str| smaps.lines().filter_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.trim().strip_suffix(" kB")?.parse::<u64>().ok()).sum::<u64>();
    let r = |key: &str| kb_of(roll, key).ok_or_else(|| format!("smaps_rollup has no {key}: {roll}"));
    let (rr, pss) = (r("Rss")?, r("Pss")?);
    kassert!(sum("Rss") == rr && rr == rss, "smaps Rss {} vs rollup {rr} vs VmRSS {rss}", sum("Rss"));
    kassert!(sum("Anonymous") == r("Anonymous")? && pss <= rr && pss > 0, "rollup: {roll}");
    // Every Pss column is truncated to kB on its own: the three parts may miss Pss by up to 2 kB.
    let parts = r("Pss_Anon")? + r("Pss_File")? + r("Pss_Shmem")?;
    kassert!(parts <= pss && pss - parts <= 2, "Pss parts {parts} vs Pss {pss}: {roll}");
    Ok(())
}

ktest!("ab7.proc.pid_files", Safe, pid_files);
/// Every name of /proc/<pid> and the content the kernel can check without the program running.
fn pid_files() -> TestResult {
    if !have("/bin/hello") {
        return ktest::skip("/bin/hello is not in this image");
    }
    let ns = vfs::root_ns();
    kassert!(ns.lookup("/proc/thread-self").err() == Some(ENOENT), "/proc/thread-self exists for a kernel thread");
    let p = held(&["hello", "a b", "c"], &["HOME=/", "X=1"])?;
    let pid = p.pid;
    let r = (|| -> TestResult {
        let dir = format!("/proc/{pid}");
        let names: Vec<String> = ns.list_dir(&dir).map_err(|e| format!("list {dir}: {e}"))?.into_iter().map(|(n, _)| String::from_utf8_lossy(&n).into_owned()).collect();
        let want = [
            "task", "fd", "fdinfo", "environ", "auxv", "status", "personality", "limits", "comm", "cmdline", "stat", "statm", "maps", "mem", "cwd", "root",
            "exe", "mounts", "mountinfo", "smaps", "smaps_rollup", "cgroup", "oom_score", "oom_score_adj",
        ];
        kassert!(names.iter().map(String::as_str).eq(want.iter().copied()), "{dir} lists {names:?}");
        let stat = text(&format!("{dir}/stat"))?;
        let f: Vec<&str> = stat.split_whitespace().collect();
        kassert!(f.len() == 52 && f[0] == format!("{pid}") && f[1] == "(hello)", "stat: {stat}");
        kassert!(f[19] == "1" && f[37] == "17" && f[51] == "0", "threads, exit signal, code: {stat}");
        let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
        kassert!(n(25) > 0 && n(25) < n(26) && n(47) < n(48) && n(48) <= n(49) && n(49) < n(50), "addresses: {stat}");
        let statm = text(&format!("{dir}/statm"))?;
        kassert!(statm.split_whitespace().count() == 7 && statm.split_whitespace().next() == Some(&*format!("{}", n(22) / 4096)), "statm {statm} vs vsize {}", n(22));
        let status = text(&format!("{dir}/status"))?;
        for k in ["Name:\thello\n", "\nThreads:\t1\n", "\nSigBlk:\t0000000000000000\n", "\nKthread:\t0\n", "\nCpus_allowed_list:\t0-", "\nx86_Thread_features_locked:\t\n"] {
            kassert!(status.contains(k), "status has no {k:?}: {status}");
        }
        kassert!(ns.read_file(&format!("{dir}/cmdline"), 4096) == Ok(b"hello\0a b\0c\0".to_vec()), "cmdline");
        kassert!(ns.read_file(&format!("{dir}/environ"), 4096) == Ok(b"HOME=/\0X=1\0".to_vec()), "environ");
        let auxv = ns.read_file(&format!("{dir}/auxv"), 4096).map_err(|e| format!("auxv: {e}"))?;
        kassert!(auxv.len() % 16 == 0 && auxv.len() > 16 && auxv[auxv.len() - 16..].iter().all(|&b| b == 0), "auxv {} bytes", auxv.len());
        let exe = ns.lookup_path(b"/bin/hello", true).map_err(|e| format!("lookup: {e}"))?.attr();
        let maps = text(&format!("{dir}/maps"))?;
        let exe_lines: Vec<&str> = maps.lines().filter(|l| l.ends_with(" /bin/hello")).collect();
        kassert!(!exe_lines.is_empty() && exe_lines.iter().all(|l| l.split_whitespace().nth(4) == Some(&*format!("{}", exe.ino))), "exe lines: {maps}");
        kassert!(exe_lines[0].split_whitespace().nth(2) == Some("00000000"), "first exe line offset: {maps}");
        kassert!(maps.lines().any(|l| l.ends_with(" [stack]")), "no [stack]: {maps}");
        let roll = text(&format!("{dir}/smaps_rollup"))?;
        kassert!(roll.lines().next().is_some_and(|l| l.ends_with(" [rollup]")) && roll.lines().count() == 23, "smaps_rollup: {roll}");
        memory_numbers(&status, &statm, &maps, &text(&format!("{dir}/smaps"))?, &roll)?;
        let limits = text(&format!("{dir}/limits"))?;
        kassert!(limits.lines().count() == 17 && limits.contains("\nMax open files            "), "limits: {limits}");
        kassert!(ns.read_link(&format!("{dir}/exe")) == Ok(b"/bin/hello".to_vec()), "exe link");
        kassert!(ns.read_link(&format!("{dir}/cwd")) == Ok(b"/".to_vec()) && ns.read_link(&format!("{dir}/root")) == Ok(b"/".to_vec()), "cwd/root");
        let mounts = text(&format!("{dir}/mounts"))?;
        kassert!(mounts.lines().any(|l| l.split(' ').nth(1) == Some("/")) && mounts.contains("proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n"), "mounts: {mounts}");
        kassert!(mounts.contains("sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n") && mounts.contains("devtmpfs /dev devtmpfs rw,nosuid,mode=755,inode64 0 0\n"), "mounts: {mounts}");
        kassert!(text(&format!("{dir}/mountinfo"))?.lines().all(|l| l.contains(" - ")), "mountinfo");
        // oom_score_adj: the kernel writes as root; the range is checked.
        kassert!(text(&format!("{dir}/oom_score_adj"))? == "0\n");
        kassert!(ns.write_file(&format!("{dir}/oom_score_adj"), b"-300\n", None).is_ok(), "root lowers oom_score_adj");
        kassert!(text(&format!("{dir}/oom_score_adj"))? == "-300\n" && p.oom_score_adj() == -300);
        kassert!(ns.write_file(&format!("{dir}/oom_score_adj"), b"1001", None).err() == Some(EINVAL), "1001 accepted");
        // comm: only a thread of the same group may write (the kernel is none).
        kassert!(ns.write_file(&format!("{dir}/comm"), b"x", None).err() == Some(EINVAL), "a foreign comm write");
        // task/<tid> of the one thread, its stat with the thread's id.
        let tasks = ns.list_dir(&format!("{dir}/task")).map_err(|e| format!("task: {e}"))?;
        kassert!(tasks.len() == 1 && tasks[0].0 == format!("{pid}").as_bytes(), "task lists {} entries", tasks.len());
        kassert!(text(&format!("{dir}/task/{pid}/stat"))?.starts_with(&format!("{pid} (hello) ")));
        // fd and fdinfo with a timerfd put into the process's table.
        let tf = crate::timerfd::create(uapi::ab::clock::CLOCK_MONOTONIC, crate::timerfd::TfdClock::Monotonic, false, 0).map_err(|e| format!("timerfd: {e}"))?;
        let fd = p.fds.install(tf).map_err(|e| format!("install: {e}"))?;
        kassert!(ns.read_link(&format!("{dir}/fd/{fd}")) == Ok(b"anon_inode:[timerfd]".to_vec()), "timerfd link");
        let info = text(&format!("{dir}/fdinfo/{fd}"))?;
        kassert!(info.starts_with("pos:\t0\nflags:\t02\nmnt_id:\t") && info.ends_with("clockid: 1\nticks: 0\nsettime flags: 00\nit_value: (0, 0)\nit_interval: (0, 0)\n"), "fdinfo: {info}");
        kassert!(text(&format!("{dir}/fdinfo/{}", fd + 1000)).is_err(), "fdinfo of a closed fd");
        Ok(())
    })();
    end(&p)?;
    r?;
    // Reaped (the held process had no parent): the directory is gone or a zombie.
    if let Ok(s) = text(&format!("/proc/{pid}/stat")) {
        kassert!(s.split_whitespace().nth(2) == Some("Z"), "ended process not a zombie: {s}");
    }
    Ok(())
}

ktest!("ab7.proc.exec_and_gone", Safe, exec_and_gone);
/// A directory opened before the process ended answers ESRCH afterwards, never a panic or stale data.
fn exec_and_gone() -> TestResult {
    if !have("/bin/hello") {
        return ktest::skip("/bin/hello is not in this image");
    }
    let p = held(&["hello"], &[])?;
    let pid = p.pid;
    let dir = vfs::root_ns().lookup_path(format!("/proc/{pid}").as_bytes(), true).map_err(|e| format!("lookup: {e}"))?;
    end(&p)?;
    drop(p);
    // Wait (bounded) for the table to let go of it.
    let until = time::now_ns() + 5 * SEC;
    while proc::find(pid).is_some() && time::now_ns() < until {
        sched::sleep_ms(5);
    }
    let r = vfs::root_ns().read_file(&format!("/proc/{pid}/status"), 4096);
    kassert!(r.is_err() || proc::find(pid).is_some(), "status of a reaped process: {:?}", r.map(|v| v.len()));
    drop(dir);
    Ok(())
}

ktest!("ab7.proc.edges", Safe, edges);
/// Reads at offsets and odd writes: partial and past-the-end reads of cmdline and maps, oom_score_adj texts
/// as `kstrtoint` takes them (a NUL ends the text, 12 bytes are looked at), comm of an ended process.
fn edges() -> TestResult {
    if !have("/bin/hello") {
        return ktest::skip("/bin/hello is not in this image");
    }
    let ns = vfs::root_ns();
    let p = held(&["hello", "a b", "c"], &["X=1"])?;
    let pid = p.pid;
    let dir = format!("/proc/{pid}");
    let r = (|| -> TestResult {
        let f = ns.open_flags(format!("{dir}/cmdline").as_bytes(), 0, 0).map_err(|e| format!("open cmdline: {e}"))?;
        let mut b = [0u8; 64];
        let n = f.pread(3, &mut b).map_err(|e| format!("pread: {e}"))?;
        kassert!(&b[..n] == b"lo\0a b\0c\0", "cmdline at 3: {:?}", &b[..n]);
        kassert!(f.pread(1 << 20, &mut b) == Ok(0), "cmdline past the end");
        let whole = text(&format!("{dir}/maps"))?;
        let m = ns.open_flags(format!("{dir}/maps").as_bytes(), 0, 0).map_err(|e| format!("open maps: {e}"))?;
        let mut parts = Vec::new();
        let mut at = 0u64;
        loop {
            let mut c = [0u8; 37];
            let n = m.pread(at, &mut c).map_err(|e| format!("maps pread: {e}"))?;
            if n == 0 {
                break;
            }
            parts.extend_from_slice(&c[..n]);
            at += n as u64;
        }
        kassert!(parts == whole.as_bytes(), "maps read in pieces differs from one read");
        let adj = format!("{dir}/oom_score_adj");
        for (w, want) in [
            (&b"5\0junk"[..], Ok(5)),
            (b"   7\n", Ok(7)),
            (b"0x3e8", Ok(1000)),
            (b"-0", Ok(0)),
            (b"abcdefghijklmnop", Err(EINVAL)),
            (b"-1001", Err(EINVAL)),
            (b"2147483648", Err(ERANGE)),
            (b"1 2", Err(EINVAL)),
        ] {
            let got = ns.write_file(&adj, w, None).map(|_| ());
            kassert!(got == want.map(|_| ()), "oom_score_adj write {w:?}: {got:?}");
            if let Ok(v) = want {
                kassert!(p.oom_score_adj() == v, "oom_score_adj after {w:?}: {}", p.oom_score_adj());
            }
        }
        // 12 bytes are looked at and answered.
        kassert!(ns.write_file(&adj, b"100         junk", None) == Ok(12), "a long oom_score_adj write");
        kassert!(ns.write_file(&format!("{dir}/comm"), b"0123456789abcdefghij", None).err() == Some(EINVAL), "foreign long comm write");
        Ok(())
    })();
    let comm = ns.open_flags(format!("{dir}/task/{pid}/comm").as_bytes(), vfs::abi::O_WRONLY, 0);
    end(&p)?;
    r?;
    // The thread is gone: a comm write through a file opened before is refused, never silently dropped.
    if let Ok(c) = comm {
        let w = c.write(b"late");
        kassert!(w == Err(ESRCH), "comm write after the end: {w:?}");
    }
    Ok(())
}

/// A transcript line's label: the text before its first ": " or " = " (the whole line when it has neither).
fn label(line: &str) -> &str {
    let cut = [line.find(": "), line.find(" = ")].into_iter().flatten().min();
    cut.map_or(line, |n| &line[..n])
}

/// `ab7.known`: labels of transcript lines that differ for a reason written there ("<label> | <owner>: why").
/// The label must match exactly, so a known line never hides another line that starts the same way.
fn known(line: &str) -> bool {
    let l = label(line);
    KNOWN.lines().filter(|k| !k.starts_with('#') && !k.trim().is_empty()).filter_map(|k| k.split(" | ").next()).any(|k| k == l)
}

ktest!("ab7.procfs.program", Safe, program);
fn program() -> TestResult {
    // Another user's process for the program's permission checks (on CachyOS it is pid 1, owned by root).
    let mut ro = proc::SpawnOptions::new(&["hello"], &[], policy::ProgramId(1));
    ro.held = true;
    ro.uid = 0;
    let other = proc::spawn_with("/bin/hello", &ro).map_err(|e| format!("spawn root hello: {e}"))?;
    let other_env = format!("ABI_OTHER_PID={}", other.pid);
    let r = program_with("/bin/abi-procfs", &["abi-procfs", "a b", "c"], &[other_env.as_str()], EXPECTED);
    end(&other)?;
    r
}

/// Runs an ABI program of the image and compares its output with CachyOS's transcript `expected`, skipping
/// the lines `ab7.known` lists (AB-7b's `abi-sysfs` uses it too).
pub(crate) fn program_case(path: &str, argv: &[&str], expected: &str) -> TestResult {
    program_with(path, argv, &[], expected)
}

fn program_with(path: &str, argv: &[&str], extra_env: &[&str], expected: &str) -> TestResult {
    if !have(path) {
        if have("/bin/abi-ids") {
            return Err(format!("the image has ABI programs but not {path} (did it fail to build?)"));
        }
        return ktest::skip("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    let out = proc::stdio::Capture::new(1 << 20);
    let mut env = alloc::vec!["PATH=/bin", "HOME=/", "ABI_TEST=1"];
    env.extend_from_slice(extra_env);
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdin = Some(proc::stdio::Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(path, &o).map_err(|e| format!("spawn: {e}"))?;
    let code = match p.wait(Some(120 * SEC)) {
        Some(c) => c,
        None => {
            p.kill(9);
            let _ = p.wait(Some(10 * SEC));
            return Err(format!("{path} did not end within 120 s:\n{}", String::from_utf8_lossy(&out.contents())));
        }
    };
    let got = String::from_utf8_lossy(&out.contents()).into_owned();
    let g: Vec<&str> = got.lines().collect();
    let mut d = Vec::new();
    let mut skipped = 0;
    for (i, w) in expected.lines().enumerate() {
        let gl = g.get(i).copied().unwrap_or("<missing>");
        if gl != w {
            if known(w) {
                skipped += 1;
            } else {
                d.push(format!("line {}: linux '{w}' got '{gl}'", i + 1));
            }
        }
    }
    if g.len() != expected.lines().count() {
        d.push(format!("{} lines, linux {}", g.len(), expected.lines().count()));
    }
    kassert!(code == 0 && d.is_empty(), "{path} differs from CachyOS (exit {code}):\n{}", d.join("\n"));
    klog::event!(Info, "ktest", "ab7.program", lines = g.len(), known = skipped);
    Ok(())
}

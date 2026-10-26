//! FS-2 ktests that need processes, PCI or the console (the framework's own ktests are in `vfs::ktests`).
//! All Safe and rerunnable in one boot: unique scratch names, every spawned process ends before the test does.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

const SEC: u64 = 1_000_000_000;
const DEV_CASES: &str = include_str!("../../../host/tests/fs/dev-cases.txt");

static RUN: AtomicU32 = AtomicU32::new(0);

fn text(path: &str) -> Result<String, String> {
    let b = vfs::root_ns().read_file(path, 1 << 20).map_err(|e| format!("{path}: errno {e}"))?;
    String::from_utf8(b).map_err(|_| format!("{path}: not UTF-8"))
}

/// `/proc/<pid>/{stat,status,cmdline,comm,maps,exe}` of a process held before its first instruction, the
/// listing of `/proc`, and the directory gone (or a zombie) once the process ended.
fn pid_files() -> TestResult {
    let ns = vfs::root_ns();
    if ns.lookup("/bin/hello").is_err() {
        return ktest::skip("/bin/hello is not in this image");
    }
    // A kernel thread has no process: /proc/self is missing for it, as for Linux's kernel threads.
    kassert!(ns.lookup("/proc/self").err() == Some(ENOENT), "/proc/self exists for a kernel thread");
    let argv = ["hello", "a b", "c"];
    let env = ["HOME=/"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.held = true;
    let p = proc::spawn_with("/bin/hello", &o).map_err(|e| format!("spawn: {e}"))?;
    let pid = p.pid;
    let r = (|| -> TestResult {
        let dir = format!("/proc/{pid}");
        let listed = ns.list_dir("/proc").map_err(|e| format!("list /proc: {e}"))?;
        kassert!(listed.iter().any(|(n, _)| n == format!("{pid}").as_bytes()), "pid {pid} not listed in /proc");
        kassert!(listed.iter().any(|(n, _)| n == b"meminfo"), "meminfo not listed in /proc");
        let names: Vec<Vec<u8>> = ns.list_dir(&dir).map_err(|e| format!("list {dir}: {e}"))?.into_iter().map(|(n, _)| n).collect();
        for want in ["stat", "status", "cmdline", "maps", "comm", "exe"] {
            kassert!(names.iter().any(|n| n == want.as_bytes()), "{dir}/{want} not listed");
        }
        let stat = text(&format!("{dir}/stat"))?;
        let f: Vec<&str> = stat.split_whitespace().collect();
        kassert!(f.len() == 52, "stat has {} fields: {stat}", f.len());
        // AB-7a: a held process waits for its start: S (sleeping), as Linux shows a blocked task.
        kassert!(f[0] == format!("{pid}") && f[1] == "(hello)" && (f[2] == "R" || f[2] == "S") && f[3] == "0", "stat: {stat}");
        kassert!(f[22].parse::<u64>().unwrap_or(0) > 0, "vsize 0: {stat}");
        let status = text(&format!("{dir}/status"))?;
        kassert!(status.starts_with("Name:\thello\n"), "status: {status}");
        kassert!(status.contains(&format!("\nPid:\t{pid}\n")) && status.contains("\nPPid:\t0\n"), "status: {status}");
        kassert!(status.contains(&format!("\nUid:\t{u}\t{u}\t{u}\t{u}\n", u = p.uid)), "status uid: {status}");
        let cmdline = ns.read_file(&format!("{dir}/cmdline"), 4096).map_err(|e| format!("cmdline: {e}"))?;
        kassert!(cmdline == b"hello\0a b\0c\0", "cmdline {cmdline:?}");
        kassert!(text(&format!("{dir}/comm"))? == "hello\n");
        let maps = text(&format!("{dir}/maps"))?;
        kassert!(maps.lines().any(|l| l.ends_with("[stack]")), "no [stack] in maps: {maps}");
        kassert!(maps.lines().any(|l| l.contains(" r-xp ")), "no code mapping in maps: {maps}");
        let exe = ns.read_link(&format!("{dir}/exe")).map_err(|e| format!("exe: {e}"))?;
        kassert!(exe == b"/bin/hello", "exe -> {:?}", String::from_utf8_lossy(&exe));
        Ok(())
    })();
    p.kill(uapi::ab::signal::SIGKILL);
    kassert!(p.wait(Some(10 * SEC)).is_some(), "the held process did not end after SIGKILL");
    r?;
    // Ended: either gone from /proc or shown as a zombie until it is reaped.
    match text(&format!("/proc/{pid}/stat")) {
        Ok(s) => kassert!(s.split_whitespace().nth(2) == Some("Z"), "ended process not a zombie: {s}"),
        Err(_) => {}
    }
    drop(p);
    Ok(())
}
ktest!("fs.proc.pid_files", Safe, pid_files);

/// `/proc/cpuinfo` (one block per online CPU), `/proc/meminfo` (Linux's column layout, sane numbers),
/// `/proc/uptime` (two numbers, growing).
fn sysinfo() -> TestResult {
    let cpu = text("/proc/cpuinfo")?;
    let n = cpu.lines().filter(|l| l.starts_with("processor\t: ")).count();
    kassert!(n == sched::online_cpus().max(1), "{n} processor blocks for {} CPUs", sched::online_cpus());
    // The flags follow CPUID (not R10's wish): avx2 listed exactly when this CPU (or QEMU's model) has it.
    let flags = cpu.lines().find_map(|l| l.strip_prefix("flags\t\t: ")).ok_or("no flags line")?;
    let has = hal::cpu::live().has("avx2");
    kassert!(flags.split_whitespace().any(|f| f == "avx2") == has, "avx2 in the flags differs from CPUID ({has})");
    kassert!(cpu.contains("model name\t: "), "no model name");
    let mem = text("/proc/meminfo")?;
    let val = |k: &str| -> Option<u64> {
        let l = mem.lines().find(|l| l.starts_with(k))?;
        kassert_line(l)?;
        l[16..l.len() - 3].trim().parse().ok()
    };
    let total = val("MemTotal:").ok_or_else(|| format!("MemTotal: {mem}"))?;
    let free = val("MemFree:").ok_or_else(|| format!("MemFree: {mem}"))?;
    kassert!(total > 0 && free <= total, "MemTotal {total} MemFree {free}");
    kassert!(val("SwapTotal:") == Some(0));
    let up = |s: &str| -> Option<f64> { s.split_whitespace().next()?.parse().ok() };
    let a = text("/proc/uptime")?;
    kassert!(a.split_whitespace().count() == 2 && a.ends_with('\n'), "uptime: {a:?}");
    let t0 = up(&a).ok_or("uptime not a number")?;
    sched::sleep_ns(20_000_000);
    let t1 = up(&text("/proc/uptime")?).ok_or("uptime not a number")?;
    kassert!(t0 > 0.0 && t1 >= t0, "uptime {t0} then {t1}");
    Ok(())
}
ktest!("fs.proc.sysinfo", Safe, sysinfo);

/// A meminfo line: the name padded to 16 columns, the number right-aligned to 8, " kB".
fn kassert_line(l: &str) -> Option<()> {
    (l.len() == 27 && l.ends_with(" kB")).then_some(())
}

/// `/sys` shows PCI's view: every function of the scan is under `/sys/bus/pci/devices` (a link into
/// `/sys/devices`) with Linux's ID files.
fn pci_view() -> TestResult {
    let ns = vfs::root_ns();
    let nodes = pci::tree();
    if nodes.is_empty() {
        return ktest::skip("no PCI functions were scanned");
    }
    let sys = ns.lookup_path(b"/sys", true).map_err(|e| format!("/sys: {e}"))?;
    kassert!(sys.mnt.sb.fstype == "sysfs", "/sys is {}", sys.mnt.sb.fstype);
    for n in &nodes {
        let name = pci::sysfs::name(n.loc);
        let link = format!("/sys/bus/pci/devices/{name}");
        let target = ns.read_link(&link).map_err(|e| format!("{link}: {e}"))?;
        kassert!(target.starts_with(b"../../../devices/pci"), "{link} -> {}", String::from_utf8_lossy(&target));
        let vendor = text(&format!("{link}/vendor"))?;
        kassert!(vendor == format!("0x{:04x}\n", n.vendor), "{link}/vendor {vendor:?}");
        let class = text(&format!("{link}/class"))?;
        kassert!(class == format!("0x{:06x}\n", n.class), "{link}/class {class:?}");
        let st = ns.lookup(&format!("{link}/device")).map_err(|e| format!("{link}/device: {e}"))?.attr();
        kassert!(st.mode == uapi::S_IFREG | 0o444 && st.size == 4096, "{link}/device mode {:o} size {}", st.mode, st.size);
    }
    let listed = ns.list_dir("/sys/bus/pci/devices").map_err(|e| format!("list: {e}"))?;
    kassert!(listed.len() == nodes.len(), "{} listed for {} functions", listed.len(), nodes.len());
    Ok(())
}
ktest!("fs.sys.pci_view", Safe, pci_view);

/// `/dev/console` reaches the screen console: a write shows up in the console's capture (through IN's
/// backend once it is set, through the adopted G0 console device before).
fn console_out() -> TestResult {
    let ns = vfs::root_ns();
    let n = RUN.fetch_add(1, Ordering::Relaxed);
    let marker = format!("fs.dev.console marker {n}\n");
    for path in ["/dev/console", "/dev/tty"] {
        let f = ns.open(path).map_err(|e| format!("{path}: {e}"))?;
        let st = ns.lookup(path).map_err(|e| format!("{path}: {e}"))?.attr();
        kassert!(st.mode & uapi::S_IFMT == uapi::S_IFCHR, "{path} is not a character device");
        let wrote = f.write_at(0, marker.as_bytes()).map_err(|e| format!("write {path}: {e}"))?;
        kassert!(wrote == marker.len());
        let cap = console::captured();
        let tail = &cap[cap.len().saturating_sub(4096)..];
        kassert!(tail.windows(marker.len()).any(|w| w == marker.as_bytes()), "{path}: the write did not reach the console");
    }
    Ok(())
}
ktest!("fs.dev.console_out", Safe, console_out);

/// host/tests/fs/dev-cases.txt (checked against Linux) run by `/bin/fs-cases` through the real syscall path,
/// as uid 1000: every device, /proc and /sys case including the ones about the runner's own process.
fn dev_cases() -> TestResult {
    let ns = vfs::root_ns();
    if ns.lookup("/bin/fs-cases").is_err() {
        return ktest::skip("/bin/fs-cases is not in this image (xtask change request 3 in plan/notes/fs-FS-1.md)");
    }
    let n = RUN.fetch_add(1, Ordering::Relaxed);
    let cases = format!("/tmp/dev-cases-{n}.txt");
    let base = format!("/tmp/dev-cases-k{n}");
    ns.create_file(&cases, 0o644, Vec::from(DEV_CASES.as_bytes())).map_err(|e| format!("{cases}: {e}"))?;
    let out = proc::stdio::Capture::new(1 << 16);
    let err = proc::stdio::Capture::new(1 << 16);
    let argv = ["fs-cases", cases.as_str(), base.as_str()];
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(err.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with("/bin/fs-cases", &o).map_err(|e| format!("spawn: {e}"))?;
    let code = p.wait(Some(60 * SEC));
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    let etext = String::from_utf8_lossy(&err.contents()).into_owned();
    let _ = ns.remove_tree(&base);
    let _ = ns.remove(&cases);
    for l in text.lines().filter(|l| l.starts_with("FAIL")).take(20) {
        klog::event!(Warn, "fs", "case.fail", line = l);
    }
    kassert!(code == Some(0), "fs-cases ended {:?} ({:?}): {} stderr {etext:?}", code, p.end_message(), text.lines().last().unwrap_or(""));
    kassert!(text.contains(" passed, 0 failed"), "output: {text}");
    Ok(())
}
ktest!("fs.calls.dev_cases", Safe, dev_cases);

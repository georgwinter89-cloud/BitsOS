//! AB-7b ktests: the system-wide /proc and /sys files read by the kernel (as the MCP tool reads them), and
//! `abi-sysfs` against CachyOS's transcript (`user/abi-tests/expected/ab7-sysfs.txt`).
//!
//! All `Safe` and rerunnable: they only read, except the hostname, domain name, pid_max and swappiness checks,
//! which put the old values back before their verdict.
//! Time-dependent values are checked for shape only (no timing in QEMU).

use crate::ab7_ktests::{have, program_case, text};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

const EXPECTED: &str = include_str!("../../../user/abi-tests/expected/ab7-sysfs.txt");

fn lines_with<'a>(s: &'a str, prefix: &str) -> Vec<&'a str> {
    s.lines().filter(|l| l.starts_with(prefix)).collect()
}

fn kb(mem: &str, key: &str) -> Result<u64, String> {
    let l = mem.lines().find(|l| l.starts_with(key)).ok_or_else(|| format!("no {key} in meminfo"))?;
    l[16..l.len() - 3].trim().parse().map_err(|_| format!("bad line {l:?}"))
}

ktest!("ab7.sys.proc_files", Safe, proc_files);
fn proc_files() -> TestResult {
    let n = sched::online_cpus().max(1);
    let mem = text("/proc/meminfo")?;
    kassert!(kb(&mem, "CommitLimit:")? == mm::commit::limit().saturating_mul(4), "CommitLimit differs from mm::commit::limit()");
    let total = kb(&mem, "MemTotal:")?;
    kassert!(kb(&mem, "MemAvailable:")? <= total && kb(&mem, "Committed_AS:")? > 0, "meminfo numbers:\n{mem}");
    kassert!(mem.lines().count() >= 50 && mem.ends_with("Hugetlb:               0 kB\n"), "meminfo keys:\n{mem}");
    let st = text("/proc/stat")?;
    kassert!(st.starts_with("cpu  ") && lines_with(&st, "cpu").len() == n + 1, "stat cpu lines for {n} CPUs:\n{st}");
    kassert!(st.lines().all(|l| !l.starts_with("cpu") || l.split(' ').filter(|f| !f.is_empty()).count() == 11), "stat cpu fields:\n{st}");
    for k in ["intr ", "ctxt ", "btime ", "processes ", "procs_running ", "procs_blocked ", "softirq "] {
        kassert!(lines_with(&st, k).len() == 1, "stat has no single {k:?} line");
    }
    let la = text("/proc/loadavg")?;
    let f: Vec<&str> = la.split_whitespace().collect();
    kassert!(f.len() == 5 && f[..3].iter().all(|x| x.len() >= 4 && x.as_bytes()[x.len() - 3] == b'.'), "loadavg {la:?}");
    let (run, all) = f[3].split_once('/').ok_or("loadavg without running/total")?;
    kassert!(run.parse::<u64>().is_ok_and(|r| r >= 1) && all.parse::<u64>().is_ok_and(|a| a >= 1), "loadavg {la:?}");
    let up = text("/proc/uptime")?;
    kassert!(up.split_whitespace().count() == 2 && up.ends_with('\n'), "uptime {up:?}");
    let v = text("/proc/version")?;
    kassert!(v.starts_with(&format!("Linux version {} (", crate::info::RELEASE)) && v.ends_with(&format!(" {}\n", crate::info::VERSION)), "version {v:?}");
    let fs = text("/proc/filesystems")?;
    kassert!(fs.contains("nodev\tproc\n") && fs.contains("nodev\tsysfs\n") && fs.contains("\tvfat\n") && fs.contains("\text4\n"), "filesystems {fs:?}");
    let ci = text("/proc/cpuinfo")?;
    kassert!(lines_with(&ci, "processor\t: ").len() == n && lines_with(&ci, "apicid\t\t: ").len() == n, "cpuinfo blocks for {n} CPUs");
    Ok(())
}

ktest!("ab7.sys.sysctl", Safe, sysctl);
fn sysctl() -> TestResult {
    // Every global the checks change is put back whatever they find, before any verdict.
    let (host, pid_max) = (crate::uts::name(crate::uts::HOST), proc::pid_max());
    let r = sysctl_checks();
    crate::uts::set(crate::uts::HOST, &host);
    let back = proc::set_pid_max(pid_max as u64);
    r?;
    kassert!(back.is_ok() && proc::pid_max() == pid_max, "pid_max not restored");
    kassert!(crate::uts::name(crate::uts::HOST) == host, "hostname not restored");
    Ok(())
}

fn nodename() -> Vec<u8> {
    let uts = crate::info::utsname();
    let node = &uts[65..130];
    node[..node.iter().position(|&b| b == 0).unwrap_or(65)].to_vec()
}

fn raw(path: &str) -> Result<Vec<u8>, String> {
    vfs::root_ns().read_file(path, 1 << 16).map_err(|e| format!("{path}: errno {e}"))
}

fn sysctl_checks() -> TestResult {
    let k = "/proc/sys/kernel";
    kassert!(raw(&format!("{k}/hostname"))? == [nodename().as_slice(), b"\n"].concat(), "hostname differs from uname");
    // proc_dostring: up to the first newline; the whole count is answered.
    kassert!(wr(&format!("{k}/hostname"), b"ktest-box\nrest") == Ok(14));
    kassert!(text(&format!("{k}/hostname"))? == "ktest-box\n" && nodename() == b"ktest-box", "hostname after the write");
    // Any byte: the file and uname show the same bytes.
    kassert!(wr(&format!("{k}/hostname"), b"\xffx") == Ok(2));
    kassert!(raw(&format!("{k}/hostname"))? == b"\xffx\n" && nodename() == b"\xffx", "a high byte in the host name");
    // Read-only files refuse root at open (sysctl_perm).
    for f in ["osrelease", "ostype", "version", "cap_last_cap", "ngroups_max"] {
        let got = wr(&format!("{k}/{f}"), b"x");
        kassert!(got == Err(EACCES), "write {f}: {got:?}, Linux EACCES");
    }
    kassert!(text(&format!("{k}/osrelease"))? == format!("{}\n", crate::info::RELEASE));
    kassert!(text(&format!("{k}/ostype"))? == "Linux\n");
    kassert!(text(&format!("{k}/cap_last_cap"))? == "40\n" && text(&format!("{k}/ngroups_max"))? == "65536\n");
    kassert!(text(&format!("{k}/randomize_va_space"))? == "0\n", "V2 does not randomise");
    kassert!(text(&format!("{k}/overflowuid"))? == "65534\n" && text(&format!("{k}/overflowgid"))? == "65534\n");
    kassert!(text(&format!("{k}/tainted"))?.trim().parse::<u64>().is_ok());
    // pid_max: 301..=4194304 as proc_dointvec_minmax; the value read back is the one in force.
    let pm = format!("{k}/pid_max");
    kassert!(text(&pm)? == format!("{}\n", proc::pid_max()));
    for (v, want) in [(&b"300"[..], Err(EINVAL)), (b"4194305", Err(EINVAL)), (b"abc", Err(EINVAL)), (b"4194303\n", Ok(8))] {
        let got = wr(&pm, v);
        kassert!(got == want, "pid_max {:?}: {got:?}, Linux {want:?}", String::from_utf8_lossy(v));
    }
    kassert!(text(&pm)? == "4194303\n" && proc::pid_max() == 4_194_303, "pid_max after the write");
    let tm = format!("{k}/threads-max");
    let threads = text(&tm)?;
    kassert!(threads.trim().parse::<u64>().is_ok_and(|t| t >= 20), "threads-max {threads:?}");
    kassert!(wr(&tm, b"19") == Err(EINVAL) && wr(&tm, threads.as_bytes()) == Ok(threads.len()), "threads-max writes");
    let (a, b) = (text(&format!("{k}/random/uuid"))?, text(&format!("{k}/random/uuid"))?);
    kassert!(a.len() == 37 && a.as_bytes()[14] == b'4' && a != b, "uuid {a:?} then {b:?}");
    let boot = text(&format!("{k}/random/boot_id"))?;
    kassert!(boot.len() == 37 && text(&format!("{k}/random/boot_id"))? == boot, "boot_id changes");
    let vm = "/proc/sys/vm";
    let oc = format!("{vm}/overcommit_memory");
    kassert!(text(&oc)? == "2\n");
    // 0..=2 accepted as Linux does; V2 stays strict, so the file stays 2.
    for (v, want) in [(&b"0"[..], Ok(1)), (b"1\n", Ok(2)), (b"2", Ok(1)), (b"3", Err(EINVAL)), (b"abc", Err(EINVAL)), (b"-1", Err(EINVAL))] {
        let got = wr(&oc, v);
        kassert!(got == want, "overcommit_memory {:?}: {got:?}, Linux {want:?}", String::from_utf8_lossy(v));
    }
    kassert!(text(&oc)? == "2\n", "overcommit_memory changed");
    let ratio: u64 = text(&format!("{vm}/overcommit_ratio"))?.trim().parse().map_err(|_| "overcommit_ratio")?;
    kassert!((1..=100).contains(&ratio), "overcommit_ratio {ratio}");
    kassert!(text(&format!("{vm}/overcommit_kbytes"))? == "0\n" && text(&format!("{vm}/mmap_min_addr"))? == "65536\n");
    kassert!(text(&format!("{vm}/max_map_count"))? == "2147483642\n");
    let sw = format!("{vm}/swappiness");
    let old = text(&sw)?;
    kassert!(wr(&sw, b"201") == Err(EINVAL) && wr(&sw, old.as_bytes()) == Ok(old.len()) && text(&sw)? == old, "swappiness");
    // FS's memfd_noexec and AB's files share the directory.
    let names: Vec<Vec<u8>> = vfs::root_ns().list_dir(vm).map_err(|e| format!("list vm: {e}"))?.into_iter().map(|(n, _)| n).collect();
    kassert!(names.iter().any(|n| n == b"memfd_noexec") && names.iter().any(|n| n == b"overcommit_memory") && names.iter().any(|n| n == b"max_map_count"), "/proc/sys/vm lists both sources");
    Ok(())
}

/// A write as the kernel (root) does it, with the errno.
fn wr(path: &str, data: &[u8]) -> Result<usize, i64> {
    vfs::root_ns().write_file(path, data, Some(0))
}

/// A held process of `uid` whose syscalls the test makes through `dispatch`, with scratch memory.
struct Caller {
    p: Arc<proc::Process>,
    th: Arc<proc::Thread>,
    ctx: hal::entry::UserContext,
    mem: u64,
    _pin: proc::MmPin,
}

const EXIT0: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];

impl Caller {
    fn new(uid: u32) -> Result<Caller, String> {
        let image = crate::ktests::elf(&EXIT0);
        let mut o = proc::SpawnOptions::new(&["uts"], &[], policy::ProgramId(1));
        o.held = true;
        o.uid = uid;
        let p = proc::spawn_image_with("uts", &image, &o).map_err(|e| format!("spawn: {e}"))?;
        let _pin = proc::pin_mm(&p);
        let mem = p.mm.sys_mmap(0, 4096, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        let th = p.threads().into_iter().next().ok_or("the held process has no main thread")?;
        Ok(Caller { p, th, ctx: hal::entry::UserContext::new(0x40_0078, hal::USER_TOP - 4096), mem, _pin })
    }
    fn call(&mut self, n: usize, a0: u64, a1: u64) -> i64 {
        let r = &mut self.ctx.regs;
        (r.rax, r.rdi, r.rsi) = (n as u64, a0, a1);
        crate::dispatch(&self.p, &self.th, &mut self.ctx);
        self.ctx.regs.rax as i64
    }
    fn set(&mut self, n: usize, name: &[u8], len: u64) -> i64 {
        let _ = self.p.mm.copy_to_user(self.mem, name);
        self.call(n, self.mem, len)
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        self.p.kill(9);
        let _ = self.p.wait(Some(10_000_000_000));
    }
}

ktest!("ab7.sys.sethostname", Safe, sethostname);
fn sethostname() -> TestResult {
    let (host, dom) = (crate::uts::name(crate::uts::HOST), crate::uts::name(crate::uts::DOMAIN));
    let r = sethostname_checks();
    crate::uts::set(crate::uts::HOST, &host);
    crate::uts::set(crate::uts::DOMAIN, &dom);
    r
}

fn sethostname_checks() -> TestResult {
    use uapi::ab::nr_more::{SETDOMAINNAME, SETHOSTNAME};
    let mut root = Caller::new(0)?;
    let k = "/proc/sys/kernel";
    kassert!(root.set(SETHOSTNAME, b"abc", 3) == 0, "sethostname as root");
    kassert!(nodename() == b"abc" && text(&format!("{k}/hostname"))? == "abc\n", "uname and the file after sethostname");
    // Bytes past a NUL are kept but end the name (strlen), as Linux shows them.
    kassert!(root.set(SETHOSTNAME, b"a\0b", 3) == 0 && text(&format!("{k}/hostname"))? == "a\n" && nodename() == b"a");
    kassert!(root.set(SETHOSTNAME, &[b'x'; 64], 64) == 0 && nodename().len() == 64, "64 bytes");
    kassert!(root.set(SETHOSTNAME, &[b'y'; 65], 65) == -EINVAL, "65 bytes: EINVAL");
    kassert!(root.call(SETHOSTNAME, root.mem, -1i64 as u64) == -EINVAL, "negative length: EINVAL");
    kassert!(root.call(SETHOSTNAME, 0x10, 3) == -EFAULT, "bad pointer: EFAULT");
    kassert!(root.call(SETHOSTNAME, 0x10, 0) == 0 && nodename().is_empty(), "length 0 copies nothing");
    kassert!(root.set(SETDOMAINNAME, b"dom", 3) == 0 && text(&format!("{k}/domainname"))? == "dom\n", "setdomainname");
    let uts = crate::info::utsname();
    kassert!(&uts[325..329] == b"dom\0", "uname domainname");
    let mut user = Caller::new(1000)?;
    kassert!(user.set(SETHOSTNAME, b"u", 1) == -EPERM && user.set(SETDOMAINNAME, b"u", 1) == -EPERM, "a user: EPERM");
    kassert!(text(&format!("{k}/domainname"))? == "dom\n", "a refused call changed the name");
    Ok(())
}

ktest!("ab7.sys.cpu", Safe, cpu);
fn cpu() -> TestResult {
    let n = sched::online_cpus().max(1);
    let d = "/sys/devices/system/cpu";
    let online = text(&format!("{d}/online"))?;
    let want = if n == 1 { String::from("0\n") } else { format!("0-{}\n", n - 1) };
    kassert!(online == want, "online {online:?}, {n} CPUs");
    kassert!(text(&format!("{d}/kernel_max"))? == "8191\n");
    kassert!(!have(&format!("{d}/cpu0/online")), "cpu0 has an online file");
    let ci = text("/proc/cpuinfo")?;
    let field = |block: &str, k: &str| block.lines().find_map(|l| l.strip_prefix(k)).map(String::from);
    let mut threads_seen = 0;
    for block in ci.split("\n\n").filter(|b| !b.is_empty()) {
        let cpu = field(block, "processor\t: ").ok_or("processor line")?;
        let t = format!("{d}/cpu{cpu}/topology");
        let core = text(&format!("{t}/core_id"))?;
        kassert!(Some(String::from(core.trim())) == field(block, "core id\t\t: "), "cpu{cpu}: core_id differs from cpuinfo");
        kassert!(Some(String::from(text(&format!("{t}/physical_package_id"))?.trim())) == field(block, "physical id\t: "), "cpu{cpu}: package");
        let pkg = text(&format!("{t}/package_cpus_list"))?;
        let sib = field(block, "siblings\t: ").and_then(|s| s.parse::<usize>().ok()).ok_or("siblings")?;
        kassert!(count_list(pkg.trim()) == sib, "cpu{cpu}: package_cpus_list {pkg:?} vs siblings {sib}");
        let thr = text(&format!("{t}/thread_siblings_list"))?;
        kassert!(thr == text(&format!("{t}/core_cpus_list"))? && in_list(thr.trim(), cpu.parse().unwrap_or(usize::MAX)), "cpu{cpu}: thread_siblings_list {thr:?}");
        threads_seen += count_list(thr.trim());
        if cpu != "0" {
            kassert!(text(&format!("{d}/cpu{cpu}/online"))? == "1\n");
        }
    }
    kassert!(threads_seen >= n, "thread sibling lists cover {threads_seen} of {n} CPUs");
    let (active, control) = (text(&format!("{d}/smt/active"))?, text(&format!("{d}/smt/control"))?);
    kassert!((active == "1\n" && control == "on\n") || (active == "0\n" && (control == "on\n" || control == "notsupported\n")), "smt {active:?} {control:?}");
    let want = if control == "on\n" { Ok(3) } else { Err(ENODEV) };
    kassert!(wr(&format!("{d}/smt/control"), b"on\n") == want, "smt/control on");
    // Caches of CPU 0 (none when CPUID lists none, as under some QEMU models).
    let mut levels = Vec::new();
    for i in 0.. {
        let ix = format!("{d}/cpu0/cache/index{i}");
        if !have(&ix) {
            break;
        }
        let level: u32 = text(&format!("{ix}/level"))?.trim().parse().map_err(|_| "cache level")?;
        let ty = text(&format!("{ix}/type"))?;
        let size = text(&format!("{ix}/size"))?;
        let list = text(&format!("{ix}/shared_cpu_list"))?;
        kassert!((1..=4).contains(&level) && ["Data\n", "Instruction\n", "Unified\n"].contains(&ty.as_str()), "index{i}: level {level} type {ty:?}");
        kassert!(size.ends_with("K\n") && in_list(list.trim(), 0), "index{i}: size {size:?} shared {list:?}");
        levels.push(level);
    }
    kassert!(levels.windows(2).all(|w| w[0] <= w[1]), "cache levels out of order: {levels:?}");
    Ok(())
}

/// The number of CPUs in a "0-3,8" list.
fn count_list(s: &str) -> usize {
    s.split(',').filter(|p| !p.is_empty()).map(|p| p.split_once('-').map_or(1, |(a, b)| b.parse::<usize>().unwrap_or(0) + 1 - a.parse::<usize>().unwrap_or(0))).sum()
}

fn in_list(s: &str, c: usize) -> bool {
    s.split(',').any(|p| match p.split_once('-') {
        Some((a, b)) => a.parse::<usize>().is_ok_and(|a| a <= c) && b.parse::<usize>().is_ok_and(|b| c <= b),
        None => p.parse::<usize>() == Ok(c),
    })
}

ktest!("ab7.sys.net", Safe, net_files);
fn net_files() -> TestResult {
    let link = vfs::root_ns().read_link("/sys/class/net/lo").map_err(|e| format!("lo link: errno {e}"))?;
    kassert!(link == b"../../devices/virtual/net/lo", "lo link {:?}", String::from_utf8_lossy(&link));
    let lo = "/sys/class/net/lo";
    for (f, want) in [("address", "00:00:00:00:00:00\n"), ("operstate", "unknown\n"), ("mtu", "65536\n"), ("flags", "0x9\n"), ("type", "772\n"), ("ifindex", "1\n"), ("carrier", "1\n"), ("dev_id", "0x0\n")] {
        let got = text(&format!("{lo}/{f}"))?;
        kassert!(got == want, "lo/{f} = {got:?}, Linux {want:?}");
    }
    kassert!(text(&format!("{lo}/statistics/rx_nohandler"))? == "0\n");
    kassert!(text(&format!("{lo}/tx_queue_len"))? == "1000\n");
    // lo has no ethtool link settings: speed and duplex answer EINVAL.
    let sp = vfs::root_ns().read_file(&format!("{lo}/speed"), 64);
    kassert!(sp == Err(EINVAL) && vfs::root_ns().read_file(&format!("{lo}/duplex"), 64) == Err(EINVAL), "lo speed {sp:?}");
    // Root writes as netdev_store answers them: the value it has, a bad number, one V2 cannot set.
    for (f, v, want) in [
        ("mtu", &b"65536\n"[..], Ok(6)),
        ("mtu", b"abc", Err(EINVAL)),
        ("mtu", b"70000", Err(EOPNOTSUPP)),
        ("mtu", b"4294967295", Err(EINVAL)),
        ("tx_queue_len", b"1000", Ok(4)),
        ("tx_queue_len", b"4294967296", Err(ERANGE)),
        ("carrier", b"1", Err(EOPNOTSUPP)),
        ("flags", b"0x9", Ok(3)),
        ("uevent", b"change", Ok(6)),
        ("uevent", b"boom", Err(EINVAL)),
        ("address", b"00:00:00:00:00:01", Err(EACCES)),
        ("statistics/rx_packets", b"0", Err(EACCES)),
    ] {
        let got = wr(&format!("{lo}/{f}"), v);
        kassert!(got == want, "write lo/{f} {:?}: {got:?}, Linux {want:?}", String::from_utf8_lossy(v));
    }
    kassert!(text(&format!("{lo}/mtu"))? == "65536\n", "lo mtu changed");
    // Every interface of the stack has its directory and its counters.
    for l in net::ifview::links() {
        let p = format!("/sys/class/net/{}", l.name);
        kassert!(text(&format!("{p}/ifindex"))? == format!("{}\n", l.index), "{p}/ifindex");
        kassert!(text(&format!("{p}/statistics/rx_packets"))?.trim().parse::<u64>().is_ok(), "{p} counters");
        if !l.loopback {
            let speed = text(&format!("{p}/speed"))?;
            kassert!(speed.trim().parse::<i64>().is_ok_and(|s| s == -1 || s > 0), "{p}/speed {speed:?}");
            kassert!(l.carrier || speed == "-1\n", "{p}: speed {speed:?} without carrier");
        }
    }
    kassert!(!have("/sys/class/net/nosuchif0"));
    Ok(())
}

ktest!("ab7.sysfs.program", Safe, program);
fn program() -> TestResult {
    program_case("/bin/abi-sysfs", &["abi-sysfs"], EXPECTED)
}

/// A device that is not there: every page reads as zeroes (a clean, reclaimable file cache).
struct Zeroes;

impl vfs::pagecache::Backend for Zeroes {
    fn read_page(&self, _pgoff: u64, _page: &Arc<vfs::pagecache::CachePage>) -> Result<(), i64> {
        Ok(())
    }
    fn write_run(&self, _first: u64, _pages: &[Arc<vfs::pagecache::CachePage>], _size: u64) -> Result<(), i64> {
        Ok(())
    }
}

ktest!("ab7.sys.meminfo_available", Safe, meminfo_available);
/// MM-9 CR1: MemAvailable counts the clean file cache (si_mem_available, linux-6.6/mm/show_mem.c:32). The file
/// is big enough that what stays cached (half of it, at most the low watermark) and the reserve (the high
/// watermark) still leave MemAvailable above MemFree. Its pages go when the cache is dropped (rerunnable).
fn meminfo_available() -> TestResult {
    const P: u64 = 4096;
    let w = mm::reclaim::watermarks();
    let pages = (2 * (w.high + w.low) + 4096).max(4096);
    let free = mm_core::frames().map_or(0, |f| f.stats().free_bytes) / P;
    if free < 4 * pages {
        return ktest::skip("too little free memory for the test file");
    }
    let c = vfs::pagecache::PageCache::with_backend(Arc::new(Zeroes), pages * P);
    let mut b = [0u8; 8];
    for i in 0..pages {
        c.read_at(i * P, &mut b).map_err(|e| format!("read page {i}: {e}"))?;
    }
    kassert!(c.pages() == pages, "the file holds {} of {pages} pages", c.pages());
    let mem = text("/proc/meminfo")?;
    let (free, avail, cached) = (kb(&mem, "MemFree:")?, kb(&mem, "MemAvailable:")?, kb(&mem, "Cached:")?);
    drop(c);
    kassert!(avail > free && avail <= free + cached, "MemAvailable {avail} kB vs MemFree {free} kB + Cached {cached} kB ({pages} pages read)");
    Ok(())
}

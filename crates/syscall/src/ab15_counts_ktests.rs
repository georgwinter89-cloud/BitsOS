//! AB-15 fixer: the per-CPU call and error counters (`counts`) and the slow-call trace's "it blocked" through
//! the program thread's own scheduler handle, run through real programs. QemuOnly: exact counts and a trace
//! limit of 1 ns need a quiet system; on the Mini-PC other programs make the same calls at the same time.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use uapi::nr;

const SEC: u64 = 1_000_000_000;
const K: u32 = 1000;

fn spawn_held(name: &str, code: &[u8]) -> Result<(Arc<proc::Process>, Arc<proc::Thread>), String> {
    let image = crate::ktests::elf(code);
    let argv = [name];
    let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
    o.held = true;
    let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
    let end = time::now_ns() + 5 * SEC;
    while p.thread_id() == 0 {
        kassert!(time::now_ns() < end, "{name}: its thread did not start within 5 s");
        sched::sleep_ms(1);
    }
    let th = p.threads().into_iter().next().ok_or("the held process has no main thread")?;
    Ok((p, th))
}

fn wait0(name: &str, p: &proc::Process) -> TestResult {
    match p.wait(Some(30 * SEC)) {
        Some(0) => Ok(()),
        c => Err(format!("{name} ended with {c:?}")),
    }
}

/// `mov r12d, K; 1: getppid; dup(-1); dec r12; jnz 1b; exit_group(0)`.
fn calls_and_errors() -> Vec<u8> {
    let mut c = alloc::vec![0x41, 0xBC];
    c.extend_from_slice(&K.to_le_bytes());
    c.extend_from_slice(&[0xB8, nr::GETPPID as u8, 0, 0, 0, 0x0F, 0x05]);
    c.extend_from_slice(&[0xB8, nr::DUP as u8, 0, 0, 0, 0xBF, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x05]);
    c.extend_from_slice(&[0x49, 0xFF, 0xCC, 0x75, 0xE8]);
    c.extend_from_slice(&[0xB8, 231, 0, 0, 0, 0x31, 0xFF, 0x0F, 0x05]);
    c
}

ktest!("ab.syscall.counts_per_cpu", QemuOnly, counts_per_cpu);
/// Programs pinned to different CPUs make K getppid and K failing dup(-1) calls each: the sums over the CPUs'
/// arrays grow by exactly N*K calls and N*K errors, and the calls really were spread over several arrays.
fn counts_per_cpu() -> TestResult {
    let cpus: Vec<usize> = (0..hal::percpu::cpu_count()).filter(|&c| hal::smp::is_online(c)).take(4).collect();
    if cpus.len() < 2 {
        return ktest::skip("needs two CPUs online (run with -- -smp 4)");
    }
    let n = cpus.len() as u64;
    let k = u64::from(K);
    let before = [crate::counts::calls(nr::GETPPID), crate::counts::errors(nr::GETPPID), crate::counts::calls(nr::DUP), crate::counts::errors(nr::DUP)];
    let code = calls_and_errors();
    let mut ps = Vec::new();
    for &cpu in &cpus {
        let name = format!("ab15-counts-{cpu}");
        let (p, th) = spawn_held(&name, &code)?;
        let mut mask = alloc::vec![0u64; cpu / 64 + 1];
        mask[cpu / 64] |= 1 << (cpu % 64);
        sched::attr::set_affinity(th.kthread(), &mask).map_err(|e| format!("{name}: affinity {e:?}"))?;
        ps.push((name, p));
    }
    for (_, p) in &ps {
        p.release();
    }
    for (name, p) in &ps {
        wait0(name, p)?;
    }
    let after = [crate::counts::calls(nr::GETPPID), crate::counts::errors(nr::GETPPID), crate::counts::calls(nr::DUP), crate::counts::errors(nr::DUP)];
    let d: Vec<u64> = after.iter().zip(before).map(|(a, b)| a - b).collect();
    kassert!(d[0] == n * k, "getppid calls grew by {} (want {})", d[0], n * k);
    kassert!(d[1] == 0, "getppid errors grew by {}", d[1]);
    kassert!(d[2] == n * k && d[3] == n * k, "dup(-1): calls +{} errors +{} (want {} each)", d[2], d[3], n * k);
    let spread = crate::counts::cpus_with_calls(nr::DUP);
    kassert!(spread >= 2, "dup was counted on {spread} CPU(s) only: the pinned programs did not spread");
    Ok(())
}

ktest!("ab.syscall.counts_past_table", QemuOnly, counts_past_table);
/// Numbers past the table (its first free number and one from 4096 on) answer ENOSYS without touching any
/// registered number's counters: only the program's exit_group is counted.
fn counts_past_table() -> TestResult {
    let len = crate::table().len();
    let total = || (0..len).fold((0, 0), |(c, e), i| (c + crate::counts::calls(i), e + crate::counts::errors(i)));
    let (c0, e0) = total();
    let (first0, other0) = (crate::enosys_count(len), crate::enosys_count(usize::MAX));
    // mov r12d, K; 1: mov eax, len; syscall; mov eax, 5000; syscall; dec r12; jnz 1b; exit_group(0)
    let mut c = alloc::vec![0x41, 0xBC];
    c.extend_from_slice(&K.to_le_bytes());
    c.push(0xB8);
    c.extend_from_slice(&(len as u32).to_le_bytes());
    c.extend_from_slice(&[0x0F, 0x05, 0xB8]);
    c.extend_from_slice(&5000u32.to_le_bytes());
    c.extend_from_slice(&[0x0F, 0x05, 0x49, 0xFF, 0xCC, 0x75, 0xED]);
    c.extend_from_slice(&[0xB8, 231, 0, 0, 0, 0x31, 0xFF, 0x0F, 0x05]);
    let (p, _) = spawn_held("ab15-past-table", &c)?;
    p.release();
    wait0("ab15-past-table", &p)?;
    let (c1, e1) = total();
    kassert!(c1 - c0 == 1 && e1 == e0, "registered counters moved: calls +{} errors +{} (want +1 for exit_group, +0)", c1 - c0, e1 - e0);
    if len < 4096 {
        kassert!(crate::enosys_count(len) == first0 + u64::from(K), "ENOSYS count of {len}: {first0} -> {}", crate::enosys_count(len));
    }
    kassert!(crate::enosys_count(usize::MAX) == other0 + u64::from(K), "numbers from 4096 on: {other0} -> {}", crate::enosys_count(usize::MAX));
    Ok(())
}

/// `getppid; nanosleep({0, 20 ms}, NULL); exit_group(nanosleep's answer)`.
const SLEEPER: [u8; 37] = [
    0xB8, 110, 0, 0, 0, 0x0F, 0x05, // getppid
    0x68, 0x00, 0x2D, 0x31, 0x01, // push 20_000_000 (tv_nsec)
    0x6A, 0x00, // push 0 (tv_sec)
    0x48, 0x89, 0xE7, // mov rdi, rsp
    0x31, 0xF6, // xor esi, esi
    0xB8, 35, 0, 0, 0, 0x0F, 0x05, // nanosleep
    0x89, 0xC7, // mov edi, eax
    0xB8, 231, 0, 0, 0, 0x0F, 0x05, // exit_group
    0x90, 0x90,
];

ktest!("ab.syscall.slow_trace_blocked", QemuOnly, slow_trace_blocked);
/// The slow-call trace reads "it blocked" from the program thread's own scheduler handle (`switches`): with
/// the limit at 1 ns a real program's getppid is recorded and its 20 ms nanosleep, which blocks, is not. The
/// handle the run loop uses is the one of the thread's kernel thread.
fn slow_trace_blocked() -> TestResult {
    use klog::trace::{self, Kind};
    if trace::rings().next().is_none() {
        return ktest::skip("no trace rings in this boot");
    }
    let (kinds, limits) = (trace::kinds(), trace::limits());
    if !trace::enabled(Kind::Syscall) {
        let spec = if kinds == "off" { String::from("syscall") } else { format!("{kinds},syscall") };
        trace::set_kinds(&spec)?;
    }
    trace::set_limits(trace::Limits { syscall_ns: 1, ..limits });
    let r = sleeper_round();
    trace::set_limits(limits);
    let _ = trace::set_kinds(&kinds);
    r
}

fn sleeper_round() -> TestResult {
    use klog::trace::{self, Kind};
    let (p, th) = spawn_held("ab15-sleeper", &SLEEPER)?;
    let kt = th.kthread();
    let own = th.sched_thread().map(|k| k.id);
    kassert!(own == Some(kt), "the thread's scheduler handle is {own:?}, its kernel thread {kt}");
    let starts: Vec<(u32, u64)> = trace::rings().map(|r| (r.cpu(), r.span().1)).collect();
    p.release();
    wait0("ab15-sleeper", &p)?;
    let (mut getppid, mut nanosleep) = (0, 0);
    for r in trace::rings() {
        let from = starts.iter().find(|(c, _)| *c == r.cpu()).map_or(0, |s| s.1);
        r.scan(true, |e| {
            if e.pos < from {
                return false;
            }
            if e.kind == Kind::Syscall && e.thread == kt {
                match (e.a & 0xFFFF_FFFF) as usize {
                    nr::GETPPID => getppid += 1,
                    nr::NANOSLEEP => nanosleep += 1,
                    _ => {}
                }
            }
            true
        });
    }
    kassert!(getppid == 1, "{getppid} trace entries for the program's getppid (want 1: not blocked, over 1 ns)");
    kassert!(nanosleep == 0, "{nanosleep} trace entries for its nanosleep: a call that blocked was taken as slow");
    Ok(())
}

/// (calls, errors) of number `nr`'s line in `/proc/bitsos/syscalls`, read through the file system as a program
/// would.
fn stats_file_line(nr: usize) -> Result<(u64, u64), String> {
    let text = vfs::root_ns().read_file("/proc/bitsos/syscalls", 1 << 20).map_err(|e| format!("read /proc/bitsos/syscalls: {e:?}"))?;
    let text = String::from_utf8(text).map_err(|_| String::from("/proc/bitsos/syscalls is not UTF-8"))?;
    let num = |w: Option<&str>, key: &str| w.and_then(|w| w.strip_prefix(key)).and_then(|v| v.parse::<u64>().ok());
    for l in text.lines() {
        let mut w = l.split_whitespace();
        if w.next().and_then(|n| n.parse::<usize>().ok()) == Some(nr) {
            let _name = w.next();
            let calls = num(w.next(), "calls=").ok_or_else(|| format!("bad line {l:?}"))?;
            let errors = num(w.next(), "errors=").ok_or_else(|| format!("bad line {l:?}"))?;
            return Ok((calls, errors));
        }
    }
    Ok((0, 0))
}

ktest!("ab.syscall.stats_file", Safe, stats_file);
/// C1: a program's K getppid and K failing dup(-1) calls show up in `/proc/bitsos/syscalls` (the counters' one
/// reader in the running system). At least K each: other programs may call them meanwhile.
fn stats_file() -> TestResult {
    let (dup_c0, dup_e0) = stats_file_line(nr::DUP)?;
    let (pp_c0, _) = stats_file_line(nr::GETPPID)?;
    let (p, _) = spawn_held("c1-stats-file", &calls_and_errors())?;
    p.release();
    wait0("c1-stats-file", &p)?;
    let (dup_c1, dup_e1) = stats_file_line(nr::DUP)?;
    let (pp_c1, _) = stats_file_line(nr::GETPPID)?;
    let k = u64::from(K);
    kassert!(dup_c1 >= dup_c0 + k && dup_e1 >= dup_e0 + k, "dup: calls {dup_c0} -> {dup_c1}, errors {dup_e0} -> {dup_e1} (want +{k} each)");
    kassert!(pp_c1 >= pp_c0 + k, "getppid: calls {pp_c0} -> {pp_c1} (want +{k})");
    Ok(())
}

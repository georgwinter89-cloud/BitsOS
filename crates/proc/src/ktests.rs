//! AB-2 ktests: the loader (ET_EXEC and static-PIE images built here around hal's test programs, and the real
//! musl C and Rust std programs of the initramfs), the initial stack read back from the program's own address
//! space, user faults and their V2 message, exit status to the parent with wait4 (zombies, orphans, WNOHANG,
//! kills), spawn refusals, and the `run` MCP tool over the kernel loopback.

use crate::elf::{self, ET_DYN, ET_EXEC, PF_R, PF_W, PF_X, PT_GNU_STACK, PT_LOAD, PT_PHDR};
use crate::stdio::Capture;
use crate::wait::Select;
use crate::{ExitStatus, Process, SpawnError, SpawnOptions, load, stack};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use hal::entry::test_programs;
use ktest::{TestResult, kassert, ktest};
use mcp::Json;
use net::{Ipv4, SockAddr, TcpStream};
use policy::ProgramId;
use uapi::ab::signal::*;
use uapi::auxv::*;
use vfs::File;

pub(crate) const SEC: u64 = 1_000_000_000;
pub(crate) const PROG: ProgramId = ProgramId(1);
const PHNUM: usize = 4;
const HDRS: u64 = 64 + 56 * PHNUM as u64;
const DATA: &[u8] = b"AB-2 data segment\0";
const BSS: u64 = 0x2345;

/// A test image around `code`: R+X segment (headers + code), RW segment with `DATA` and `BSS` zero bytes,
/// PT_GNU_STACK (RW), PT_PHDR. ET_EXEC at 0x400000 or static-PIE at 0.
pub(crate) fn image(code: &[u8], pie: bool) -> Vec<u8> {
    let base: u64 = if pie { 0 } else { 0x40_0000 };
    let text_len = HDRS + code.len() as u64;
    let data_off = text_len.next_multiple_of(4096);
    let data_va = base + 0x20_0000 + data_off;
    let mut b = vec![0u8; data_off as usize];
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 2;
    b[5] = 1;
    b[6] = 1;
    b[16..18].copy_from_slice(&(if pie { ET_DYN } else { ET_EXEC }).to_le_bytes());
    b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&(base + HDRS).to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[52..54].copy_from_slice(&64u16.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&(PHNUM as u16).to_le_bytes());
    let phs: [(u32, u32, u64, u64, u64, u64, u64); PHNUM] = [
        (PT_PHDR, PF_R, 64, base + 64, 56 * PHNUM as u64, 56 * PHNUM as u64, 8),
        (PT_LOAD, PF_R | PF_X, 0, base, text_len, text_len, 0x1000),
        (PT_LOAD, PF_R | PF_W, data_off, data_va, DATA.len() as u64, DATA.len() as u64 + BSS, 0x1000),
        (PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 16),
    ];
    for (i, p) in phs.iter().enumerate() {
        let o = 64 + i * 56;
        b[o..o + 4].copy_from_slice(&p.0.to_le_bytes());
        b[o + 4..o + 8].copy_from_slice(&p.1.to_le_bytes());
        b[o + 8..o + 16].copy_from_slice(&p.2.to_le_bytes());
        b[o + 16..o + 24].copy_from_slice(&p.3.to_le_bytes());
        b[o + 24..o + 32].copy_from_slice(&p.3.to_le_bytes());
        b[o + 32..o + 40].copy_from_slice(&p.4.to_le_bytes());
        b[o + 40..o + 48].copy_from_slice(&p.5.to_le_bytes());
        b[o + 48..o + 56].copy_from_slice(&p.6.to_le_bytes());
    }
    b[HDRS as usize..HDRS as usize + code.len()].copy_from_slice(code);
    b.extend_from_slice(DATA);
    b
}

pub(crate) fn need(code: &'static [u8]) -> Result<&'static [u8], String> {
    if code.is_empty() { Err(String::from("no test program in this build")) } else { Ok(code) }
}

pub(crate) fn start(name: &str, img: &[u8], argv: &[&str], parent: Option<&Arc<Process>>, held: bool) -> Result<(Arc<Process>, Arc<Capture>), String> {
    start_as(name, img, argv, parent, held, SpawnOptions::new(&[], &[], PROG).uid)
}

pub(crate) fn start_as(name: &str, img: &[u8], argv: &[&str], parent: Option<&Arc<Process>>, held: bool, uid: u32) -> Result<(Arc<Process>, Arc<Capture>), String> {
    let out = Capture::new(1 << 16);
    let env = ["HOME=/", "AB2=yes"];
    let mut o = SpawnOptions::new(argv, &env, PROG);
    o.stdout = Some(out.clone() as Arc<dyn File>);
    o.parent = parent;
    o.held = held;
    o.uid = uid;
    let p = crate::spawn_image_with(name, img, &o).map_err(|e| format!("spawn {name}: {e}"))?;
    Ok((p, out))
}

pub(crate) fn ended(p: &Process, secs: u64) -> Result<ExitStatus, String> {
    p.wait(Some(secs * SEC)).ok_or_else(|| format!("{} did not end within {secs} s", p.name))?;
    p.status().ok_or_else(|| String::from("no status"))
}

// ---------------------------------------------------------------------------------------------------------

fn initial_stack(pie: bool) -> TestResult {
    let code = need(test_programs::stack_report())?;
    let img = image(code, pie);
    let path = if pie { "/ab-ktest/stack-pie" } else { "/ab-ktest/stack-exec" };
    let argv = ["stack-report", "first arg", "", "x=3"];
    // The PIE one runs as uid 0 (init's case): auxv carries the process's own ids.
    let uid = if pie { 0 } else { 1000 };
    // Held before its first instruction: the stack is read now, because the memory goes when the program ends
    // (Linux's exit_mm, AB-4). The loader's layout is deterministic, so the expected rsp is computed here.
    let (p, out) = start_as(path, &img, &argv, None, true, uid)?;
    let e = elf::Elf::parse(&img, hal::USER_TOP)?;
    let plan = load::plan(&e, mm::MMAP_TOP, crate::STACK_TOP - crate::STACK_SIZE - crate::image::STACK_GAP)?;
    let aux_list = crate::image::auxv(&e, &plan, 0, &crate::cred::Cred::new(uid, uid));
    let a: Vec<&[u8]> = argv.iter().map(|s| s.as_bytes()).collect();
    let env: [&[u8]; 2] = [b"HOME=/", b"AB2=yes"];
    let rsp = stack::build(crate::STACK_TOP, crate::STACK_SIZE, &a, &env, path.as_bytes(), [0; 16], &aux_list).map_err(|_| "stack build")?.rsp;
    kassert!(rsp % 16 == 0, "rsp {rsp:#x} not 16-byte aligned at entry");
    kassert!(rsp < crate::STACK_TOP && rsp > crate::STACK_TOP - 4096, "rsp {rsp:#x} not in the top stack page");
    let mm = &p.mm;
    let mut mem = |a: u64, n: usize| -> Option<Vec<u8>> {
        let mut b = vec![0u8; n];
        mm.copy_from_user(&mut b, a).ok().map(|_| b)
    };
    let parsed = stack::parse(rsp, &mut mem, 64).map_err(String::from)?;
    // Lazy fill: an 8 MiB stack and the image cost a handful of pages (the loader's own writes; counted before
    // this test reads more).
    let resident = mm.resident_pages();
    kassert!(resident <= 6, "{resident} pages resident before the program ran");
    let want_argv: Vec<Vec<u8>> = argv.iter().map(|s| s.as_bytes().to_vec()).collect();
    kassert!(parsed.argv == want_argv, "argv {:?}", parsed.argv);
    kassert!(parsed.envp == [b"HOME=/".to_vec(), b"AB2=yes".to_vec()], "envp {:?}", parsed.envp);
    let keys: Vec<u64> = parsed.auxv.iter().map(|(k, _)| *k).collect();
    let linux_order = [AT_MINSIGSTKSZ, AT_HWCAP, AT_PAGESZ, AT_CLKTCK, AT_PHDR, AT_PHENT, AT_PHNUM, AT_BASE, AT_FLAGS, AT_ENTRY, AT_UID, AT_EUID, AT_GID, AT_EGID, AT_SECURE, AT_RANDOM, AT_HWCAP2, AT_EXECFN, AT_PLATFORM];
    kassert!(keys == linux_order, "auxv keys {keys:?}");
    let aux = |k: u64| parsed.auxv.iter().find(|(x, _)| *x == k).map(|(_, v)| *v).unwrap_or(u64::MAX);
    let base = if pie { 0 } else { 0x40_0000 };
    // A static-PIE lands top-down below the mmap base (Linux's placement): its last page ends there.
    let image_end = base + 0x20_0000 + (HDRS + code.len() as u64).next_multiple_of(4096) + (DATA.len() as u64 + BSS).next_multiple_of(4096);
    let bias = if pie { mm::MMAP_TOP - image_end } else { 0 };
    kassert!(aux(AT_ENTRY) == bias + base + HDRS, "AT_ENTRY {:#x}", aux(AT_ENTRY));
    kassert!(aux(AT_PHDR) == bias + base + 64, "AT_PHDR {:#x}", aux(AT_PHDR));
    kassert!(aux(AT_PHENT) == 56 && aux(AT_PHNUM) == PHNUM as u64 && aux(AT_PAGESZ) == 4096 && aux(AT_CLKTCK) == 100);
    kassert!(aux(AT_BASE) == 0 && aux(AT_FLAGS) == 0 && aux(AT_SECURE) == 0);
    let id = uid as u64;
    kassert!(p.cred().euid == uid && aux(AT_UID) == id && aux(AT_EUID) == id && aux(AT_GID) == id && aux(AT_EGID) == id, "auxv ids {} {} {} {} for uid {uid}", aux(AT_UID), aux(AT_EUID), aux(AT_GID), aux(AT_EGID));
    kassert!(aux(AT_HWCAP) == hal::cpu::cpuid(1, 0).edx as u64, "AT_HWCAP {:#x}", aux(AT_HWCAP));
    kassert!(aux(AT_HWCAP2) & !2 == 0 && (aux(AT_HWCAP2) == 2) == hal::entry::user_fsgsbase(), "AT_HWCAP2 {:#x}", aux(AT_HWCAP2));
    kassert!(aux(AT_MINSIGSTKSZ) == crate::min_sigstack_size() && aux(AT_MINSIGSTKSZ) >= 1024 && aux(AT_MINSIGSTKSZ) % 16 == 0, "AT_MINSIGSTKSZ {}", aux(AT_MINSIGSTKSZ));
    // The program headers the program finds at AT_PHDR are its own.
    let ph = mem(aux(AT_PHDR), 56 * PHNUM).ok_or("AT_PHDR unreadable")?;
    kassert!(ph[..] == img[64..64 + 56 * PHNUM], "the bytes at AT_PHDR are not the program headers");
    let s = |a: u64, n: usize| mem(a, n).unwrap_or_default();
    kassert!(s(aux(AT_EXECFN), path.len() + 1) == [path.as_bytes(), b"\0"].concat(), "AT_EXECFN");
    kassert!(s(aux(AT_PLATFORM), 7) == b"x86_64\0", "AT_PLATFORM");
    let rnd = aux(AT_RANDOM);
    kassert!(rnd > rsp && rnd + 16 <= crate::STACK_TOP && s(rnd, 16).len() == 16, "AT_RANDOM {rnd:#x}");
    kassert!(s(rnd, 16) != [0u8; 16], "AT_RANDOM is all zero");
    // Linux's string order: argv[0] lowest, the file name last, 8 zero bytes at the very top.
    kassert!(s(crate::STACK_TOP - 8, 8) == [0u8; 8]);
    kassert!(aux(AT_EXECFN) == crate::STACK_TOP - 8 - path.len() as u64 - 1, "AT_EXECFN not at the top");
    // Segments: data copied, bss zero, protections as asked, the break right after the image.
    let data_va = bias + base + 0x20_0000 + (HDRS + code.len() as u64).next_multiple_of(4096);
    kassert!(s(data_va, DATA.len()) == DATA, "data segment");
    kassert!(s(data_va + DATA.len() as u64, 64) == [0u8; 64] && s(data_va + DATA.len() as u64 + BSS - 8, 8) == [0u8; 8], "bss not zero");
    let vmas = mm.vmas();
    let text = vmas.iter().find(|v| v.start == bias + base).ok_or("no text mapping")?;
    kassert!(text.prot.read && text.prot.exec && !text.prot.write, "text {:?}", text.prot);
    let data = vmas.iter().find(|v| v.start <= data_va && data_va < v.end).ok_or("no data mapping")?;
    kassert!(data.prot.write && !data.prot.exec, "data {:?}", data.prot);
    let stackv = vmas.iter().find(|v| v.end == crate::STACK_TOP).ok_or("no stack mapping")?;
    kassert!(stackv.end - stackv.start == crate::STACK_SIZE && !stackv.prot.exec, "stack {:?}", stackv);
    let want_brk = if pie { load::PIE_BRK } else { (data_va + DATA.len() as u64 + BSS).next_multiple_of(4096) };
    kassert!(mm.current_brk() == want_brk, "brk {:#x}, want {want_brk:#x}", mm.current_brk());
    // Now it runs: the rsp it reports is the one the loader built.
    p.release();
    let st = ended(&p, 10)?;
    kassert!(st == ExitStatus::Exited(0), "{path}: {} {:?}", st.describe(), p.end_message());
    let o = out.contents();
    kassert!(o.len() == 8, "{path} wrote {} bytes", o.len());
    let got = u64::from_le_bytes(o[..8].try_into().map_err(|_| "rsp")?);
    kassert!(got == rsp, "the program saw rsp {got:#x}, the loader built {rsp:#x}");
    kassert!(p.mm.resident_pages() == 0, "the ended program's memory was not given back");
    klog::event!(Info, "ktest", "ab.initial_stack", pie = pie, rsp = klog::Hex(rsp), random = klog::Hex(rnd), minsigstksz = aux(AT_MINSIGSTKSZ), resident = resident);
    Ok(())
}

fn initial_stack_exec() -> TestResult {
    initial_stack(false)
}
ktest!("ab.proc.initial_stack_exec", Safe, initial_stack_exec);

fn initial_stack_pie() -> TestResult {
    initial_stack(true)
}
ktest!("ab.proc.initial_stack_pie", Safe, initial_stack_pie);

// ---------------------------------------------------------------------------------------------------------

/// The initramfs programs (UL-1's static musl C built by xtask, Rust std static-PIE) with captured output.
fn real_programs() -> TestResult {
    let ns = vfs::root_ns();
    // xtask builds both into every image (smoke.hello_c/hello_rs need them too): missing is a failure.
    for (path, line, want_pie) in [("/bin/hello", "hello from musl C on BitsOS V2\n", None), ("/bin/hello-rs", "hello from Rust std on BitsOS V2, sum=10\n", Some(true))] {
        let img = crate::read_program(&ns, path).map_err(|e| format!("{path} not in the initramfs: {e}"))?;
        let e = elf::Elf::parse(&img, hal::USER_TOP).map_err(|e| format!("{path}: {e}"))?;
        if let Some(p) = want_pie {
            kassert!(e.is_pie == p, "{path}: is_pie {}", e.is_pie);
        }
        let out = Capture::new(1 << 16);
        let err = Capture::new(1 << 16);
        let env = ["HOME=/", "PATH=/bin"];
        let argv = [path];
        let mut o = SpawnOptions::new(&argv, &env, PROG);
        o.stdout = Some(out.clone() as Arc<dyn File>);
        o.stderr = Some(err.clone() as Arc<dyn File>);
        let p = crate::spawn_with(path, &o).map_err(|e| format!("{path}: {e}"))?;
        let st = ended(&p, 10)?;
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        kassert!(st == ExitStatus::Exited(0), "{path}: {} {:?} stdout {text:?} stderr {:?}", st.describe(), p.end_message(), String::from_utf8_lossy(&err.contents()));
        kassert!(text == line, "{path} printed {text:?}");
        kassert!(err.is_empty(), "{path} wrote to stderr");
        klog::event!(Info, "ktest", "ab.real_program", path = path, pie = e.is_pie);
    }
    Ok(())
}
ktest!("ab.proc.real_programs", Safe, real_programs);

// ---------------------------------------------------------------------------------------------------------

fn faults() -> TestResult {
    let code = need(test_programs::faults())?;
    let img = image(code, true);
    let seq = klog::next_seq();
    // argc -> (signal, words the message must contain); None = exits 0.
    let cases: [(usize, Option<(u32, &str)>); 9] = [
        (1, Some((SIGSEGV, "Page fault at 0x0: not mapped"))),
        (2, Some((SIGILL, "Invalid instruction at rip"))),
        (3, Some((SIGSEGV, "write to read-only memory"))),
        (4, Some((SIGFPE, "Divide error at rip"))),
        (5, Some((SIGSEGV, "Page fault at 0x10: not mapped"))),
        (6, None),
        (7, Some((SIGSEGV, "not mapped"))),
        (8, Some((SIGTRAP, "Breakpoint at rip"))),
        (9, Some((SIGSEGV, "execute of non-executable memory"))),
    ];
    let args = ["f", "a", "b", "c", "d", "e", "g", "h", "i"];
    for (argc, want) in cases {
        let (p, _) = start("/ab-ktest/faults", &img, &args[..argc], None, false)?;
        let st = ended(&p, 10)?;
        match want {
            None => kassert!(st == ExitStatus::Exited(0), "argc {argc}: {} {:?}", st.describe(), p.end_message()),
            Some((sig, words)) => {
                kassert!(st == ExitStatus::Signaled(sig), "argc {argc}: {} (want signal {sig})", st.describe());
                let m = p.end_message().unwrap_or_default();
                kassert!(m.starts_with(&format!("Program faults (pid {}, thread ", p.pid)) && m.contains(words), "argc {argc}: message {m:?}");
            }
        }
    }
    let filter = klog::Filter { level: None, subsystem: Some("proc"), name: Some("fault") };
    let n = klog::read(seq, 1000, &filter).records.len();
    kassert!(n >= 8, "{n} proc fault events for 8 faults");
    Ok(())
}
ktest!("ab.proc.faults", Safe, faults);

// ---------------------------------------------------------------------------------------------------------

fn wait4_family() -> TestResult {
    let waiter = need(test_programs::waiter())?;
    let sleeper = image(need(test_programs::sleep_exit())?, false);
    let faulter = image(need(test_programs::faults())?, true);

    // A parent (held until its children exist) collects three children: exit 2, exit 7, SIGSEGV.
    let (parent, pout) = start("/ab-ktest/waiter", &image(waiter, true), &["waiter"], None, true)?;
    let (c1, _) = start("/ab-ktest/sleep", &sleeper, &["s", "x"], Some(&parent), false)?;
    let (c2, _) = start("/ab-ktest/sleep", &sleeper, &["s", "1", "2", "3", "4", "5", "6"], Some(&parent), false)?;
    let (c3, _) = start("/ab-ktest/faults", &faulter, &["f"], Some(&parent), false)?;
    for c in [&c1, &c2, &c3] {
        kassert!(c.ppid() == parent.pid && c.pgid() == parent.pgid(), "child {}: ppid {} pgid {}", c.pid, c.ppid(), c.pgid());
    }
    kassert!(parent.ppid() == 0 && parent.pgid() == parent.pid, "kernel child leads its group");
    parent.release();
    let st = ended(&parent, 10)?;
    kassert!(st == ExitStatus::Exited(0), "waiter: {} (check number = exit code)", st.describe());
    let o = pout.contents();
    kassert!(o.len() == 48, "waiter wrote {} bytes", o.len());
    let mut got: Vec<(u32, u32)> = o.chunks(16).map(|c| (u64::from_le_bytes(c[..8].try_into().unwrap_or_default()) as u32, u64::from_le_bytes(c[8..].try_into().unwrap_or_default()) as u32)).collect();
    got.sort();
    let mut want = vec![(c1.pid, 2 << 8), (c2.pid, 7 << 8), (c3.pid, SIGSEGV)];
    want.sort();
    kassert!(got == want, "wait4 gave {got:?}, want {want:?}");
    for c in [&c1, &c2, &c3] {
        kassert!(crate::find(c.pid).is_none(), "child {} still in the table after wait4", c.pid);
    }

    // WNOHANG and kills, from the kernel side, with a parent that never runs.
    let (q, _) = start("/ab-ktest/held", &sleeper, &["held"], None, true)?;
    let forever: Vec<&str> = (0..100).map(|_| "z").collect();
    let (c4, _) = start("/ab-ktest/sleep", &sleeper, &forever, Some(&q), false)?;
    sched::sleep_ns(30_000_000);
    kassert!(crate::wait_child(&q, Select::Any, true) == Ok(None), "WNOHANG with a running child");
    kassert!(crate::wait_child(&q, Select::Pid(c4.pid), true) == Ok(None));
    kassert!(crate::wait_child(&q, Select::Pid(99_999), true) == Err(uapi::errno::ECHILD));
    kassert!(crate::wait_child(&q, Select::Group(q.pgid() + 12345), true) == Err(uapi::errno::ECHILD));
    let t0 = time::now_ns();
    c4.kill(SIGKILL);
    ended(&c4, 5)?;
    kassert!(time::now_ns() - t0 < 2 * SEC, "a kill did not end the sleep at once");
    kassert!(crate::find(c4.pid).is_some(), "the killed child is a zombie until its parent collects it");
    let r = crate::wait_child(&q, Select::OwnGroup, true);
    kassert!(r.map(|o| o.map(|x| (x.pid, x.status, x.uid))) == Ok(Some((c4.pid, ExitStatus::Signaled(SIGKILL), c4.real_uid()))), "{r:?}");
    kassert!(crate::wait_child(&q, Select::Any, true) == Err(uapi::errno::ECHILD));

    // Orphans: a zombie whose parent ends is collected; a running child goes to the reaper and leaves the table
    // when it ends. The reaper is init (pid 1) when it runs (the Mini-PC; it collects with wait4(-1) once per
    // second, D3 of INT-A1d), else the kernel (QEMU ktest boots, which start no init), which collects at once.
    let init = crate::find(1).filter(|i| !i.has_exited());
    let reaper = init.as_ref().map_or(0, |i| i.pid);
    let (c5, _) = start("/ab-ktest/sleep", &sleeper, &["s"], Some(&q), false)?;
    let (r_parent, _) = start("/ab-ktest/sleep", &sleeper, &["s"], None, false)?;
    let (c6, _) = start("/ab-ktest/sleep", &sleeper, &["s", "1", "2", "3", "4", "5", "6", "7", "8", "9"], Some(&r_parent), false)?;
    ended(&c5, 5)?;
    kassert!(crate::find(c5.pid).is_some(), "zombie c5 missing");
    q.kill(SIGKILL);
    kassert!(ended(&q, 5)? == ExitStatus::Signaled(SIGKILL), "held parent did not end on kill");
    kassert!(crate::find(q.pid).is_none(), "the kernel's child q left in the table");
    if let Some(z) = crate::find(c5.pid) {
        kassert!(reaper != 0 && z.ppid() == reaper, "zombie c5 left with ppid {} (reaper {reaper})", z.ppid());
    }
    kassert!(left_table(c5.pid, 5), "zombie c5 not collected by its reaper (pid {reaper}) within 5 s");
    ended(&r_parent, 5)?;
    kassert!(c6.ppid() == reaper, "orphan has ppid {}, want the reaper {reaper}", c6.ppid());
    kassert!(ended(&c6, 5)? == ExitStatus::Exited(10));
    kassert!(crate::find(r_parent.pid).is_none(), "kernel child left in the table");
    kassert!(left_table(c6.pid, 5), "orphan c6 not collected by its reaper (pid {reaper}) within 5 s");
    Ok(())
}

/// Whether `pid` leaves the process table within `secs` (a reaper collects it).
pub(crate) fn left_table(pid: u32, secs: u64) -> bool {
    let end = time::now_ns() + secs * SEC;
    loop {
        if crate::find(pid).is_none() {
            return true;
        }
        if time::now_ns() >= end {
            return false;
        }
        sched::sleep_ns(10_000_000);
    }
}
ktest!("ab.proc.wait4", Safe, wait4_family);

/// A fatal signal (kill/tkill/tgkill reach `send_signal`) ends a program spinning in user mode without a
/// syscall at once: its CPU is interrupted (review INT-A1d user 1). Before, only `Process::kill` did that, and
/// a tickless CPU let the spinner run on until some unrelated interrupt.
fn fatal_signal_stops_a_spinner() -> TestResult {
    let spin = image(&[0xEB, 0xFE], false); // jmp .
    let (p, _) = start("/ab-ktest/spin", &spin, &["spin"], None, false)?;
    let until = time::now_ns() + 2 * SEC;
    while p.thread_id() == 0 && time::now_ns() < until {
        sched::sleep_ns(10_000_000);
    }
    // In user mode by now (the loader is done long before).
    sched::sleep_ns(50_000_000);
    let t0 = time::now_ns();
    p.send_signal(SIGTERM);
    let r = ended(&p, 3);
    if r.is_err() {
        p.kill(SIGKILL);
        let _ = ended(&p, 5);
    }
    let st = r?;
    let ms = (time::now_ns() - t0) / 1_000_000;
    kassert!(st == ExitStatus::Signaled(SIGTERM), "{}", st.describe());
    kassert!(ms < 1000, "the spinner ended {ms} ms after SIGTERM");
    Ok(())
}
ktest!("ab.proc.signal_stops_spinner", Safe, fatal_signal_stops_a_spinner);

// ---------------------------------------------------------------------------------------------------------

fn spawn_refusals() -> TestResult {
    let before = crate::processes().len();
    let code = need(test_programs::stack_report())?;
    let img = image(code, false);
    let o = |argv: &'static [&'static str]| SpawnOptions::new(argv, &[], PROG);
    let e = crate::spawn_image_with("/x", b"\x7fELF garbage", &o(&["x"])).err();
    kassert!(matches!(e, Some(SpawnError::NotExecutable(_))), "{e:?}");
    let mut interp = img.clone();
    interp[64 + 3 * 56..64 + 3 * 56 + 4].copy_from_slice(&elf::PT_INTERP.to_le_bytes());
    let e = crate::spawn_image_with("/x", &interp, &o(&["x"])).err();
    kassert!(e == Some(SpawnError::NotExecutable("PT_INTERP path size not in 2..=4096")), "a PT_INTERP without a path: {e:?}");
    kassert!(crate::spawn_with("/no/such/program", &o(&["x"])).err() == Some(SpawnError::NotFound));
    kassert!(crate::spawn_with("/bin", &o(&["x"])).err() == Some(SpawnError::Io(uapi::errno::EACCES)), "a directory");
    let big = String::from_utf8(vec![b'a'; 128 * 1024]).map_err(|_| "utf8")?;
    let many: Vec<&str> = (0..20).map(|_| big.as_str()).collect();
    let e = crate::spawn_image_with("/x", &img, &SpawnOptions::new(&many, &[], PROG)).err();
    kassert!(e == Some(SpawnError::TooBig), "E2BIG: {e:?}");
    kassert!(SpawnError::TooBig.errno() == uapi::errno::E2BIG && SpawnError::NotFound.errno() == uapi::errno::ENOENT);
    kassert!(crate::processes().len() == before, "a refused spawn left a process in the table");

    // Hostile files through the whole spawn path (file tree, parser, loader, stack), never run: whatever the
    // loader accepts is held at its first instruction and killed.
    let ns = vfs::root_ns();
    ns.create_file("/ab-ktest/garbage", 0o755, b"\x7fELF\x02\x01\x01 not a program".to_vec()).map_err(|e| format!("create: {e}"))?;
    let e = crate::spawn_with("/ab-ktest/garbage", &o(&["g"])).err();
    kassert!(matches!(e, Some(SpawnError::NotExecutable(_))), "garbage file: {e:?}");
    ns.create_file("/ab-ktest/empty", 0o755, Vec::new()).map_err(|e| format!("create: {e}"))?;
    let e = crate::spawn_with("/ab-ktest/empty", &o(&["e"])).err();
    kassert!(matches!(e, Some(SpawnError::NotExecutable(_))), "empty file: {e:?}");
    let real = crate::read_program(&ns, "/bin/hello-rs").map_err(|e| format!("/bin/hello-rs: {e}"))?;
    let try_spawn = |img: &[u8]| -> Result<bool, String> {
        let mut o = SpawnOptions::new(&["h"], &[], PROG);
        o.held = true;
        match crate::spawn_image_with("/ab-ktest/hostile", img, &o) {
            Err(_) => Ok(false),
            Ok(p) => {
                p.kill(SIGKILL);
                kassert!(ended(&p, 5)? == ExitStatus::Signaled(SIGKILL), "a held hostile program did not end on kill");
                Ok(true)
            }
        }
    };
    let (mut refused, mut accepted) = (0, 0);
    let mut cuts: Vec<usize> = vec![0, 1, 4, 16, 63, 64, 65, 120, 64 + 56, 64 + 56 * 3 + 7, 4095, 4096, real.len() / 3, real.len() / 2, real.len() - 1];
    cuts.retain(|&n| n < real.len());
    for n in cuts {
        if try_spawn(&real[..n])? { accepted += 1 } else { refused += 1 }
    }
    // Garbled headers (deterministic): random bytes in the ELF header and program-header table.
    let hdr = (64 + 56 * u16::from_le_bytes([real[56], real[57]]) as usize).min(real.len());
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    for _ in 0..120 {
        let mut img = real.clone();
        for _ in 0..1 + (x % 4) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            img[(x as usize) % hdr] = (x >> 32) as u8;
        }
        if try_spawn(&img)? { accepted += 1 } else { refused += 1 }
    }
    kassert!(refused >= 10, "only {refused} of {} hostile images refused", refused + accepted);
    kassert!(crate::processes().len() == before, "hostile images left a process in the table");
    klog::event!(Info, "ktest", "ab.hostile_images", refused = refused, accepted_and_killed = accepted);
    Ok(())
}
ktest!("ab.proc.spawn_refusals", Safe, spawn_refusals);

// ---------------------------------------------------------------------------------------------------------
// The run tool over the kernel loopback (a server of its own on 127.0.0.1, port in AB's range).

const RUN_PORT: u16 = 23510;
const TOKEN: &str = "ab2-ktest-token";

struct Client {
    sock: &'static dyn net::KSocket,
    conn: Option<Box<dyn TcpStream>>,
    id: u64,
}

struct Reply {
    error: bool,
    rpc_error: Option<i64>,
    text: String,
}

impl Client {
    fn answer(s: &dyn TcpStream) -> Result<String, String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = time::now_ns() + 20 * SEC;
        loop {
            if let Some(h) = mcp::http::head_end(&buf, 0) {
                let head = String::from_utf8_lossy(&buf[..h]).into_owned();
                let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                if buf.len() >= h + len {
                    return Ok(String::from_utf8_lossy(&buf[h..h + len]).into_owned());
                }
            }
            let now = time::now_ns();
            if now >= end {
                return Err(format!("no whole answer after 20 s ({} bytes)", buf.len()));
            }
            match s.read(&mut chunk, Some(end - now)) {
                Ok(0) => return Err(String::from("closed before the answer")),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => return Err(format!("read: {e:?}")),
            }
        }
    }
    fn call(&mut self, name: &str, args: &str) -> Result<Reply, String> {
        if self.conn.is_none() {
            let to = SockAddr { ip: Ipv4::LOCALHOST, port: RUN_PORT };
            self.conn = Some(self.sock.tcp_connect(to, Some(SEC)).map_err(|e| format!("connect: {e:?}"))?);
        }
        self.id += 1;
        let body = format!(r#"{{"jsonrpc":"2.0","id":{},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#, self.id);
        let req = format!("POST /mcp HTTP/1.1\r\nHost: bitsos\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let c = self.conn.as_deref().ok_or("no connection")?;
        c.write_all(req.as_bytes(), Some(5 * SEC)).map_err(|e| format!("write: {e:?}"))?;
        let b = Self::answer(c)?;
        let j = Json::parse(&b).map_err(|_| format!("answer is no JSON: {b}"))?;
        let rpc_error = j.get("error").and_then(|e| e.get("code")).and_then(Json::as_i64);
        let r = j.get("result");
        let items = r.and_then(|r| r.get("content")).and_then(Json::as_array).unwrap_or(&[]);
        let text = items.iter().filter_map(|i| i.get("text").and_then(Json::as_str)).collect::<Vec<_>>().join("\n");
        Ok(Reply { error: r.and_then(|r| r.get("isError")).and_then(Json::as_bool) == Some(true), rpc_error, text })
    }
    fn ok(&mut self, args: &str) -> Result<String, String> {
        let r = self.call("run", args)?;
        if r.error || r.rpc_error.is_some() {
            return Err(format!("run {args}: error {:?} {}", r.rpc_error, r.text));
        }
        Ok(r.text)
    }
}

fn run_tool() -> TestResult {
    let sock = net::stack().ok_or("no socket stack")?;
    let t = mcp::find("run").ok_or("run not registered")?;
    kassert!(t.owner == "AB" && t.safety == mcp::Safety::Change);
    let ns = vfs::root_ns();
    let faults = image(need(test_programs::faults())?, true);
    let sleeper = image(need(test_programs::sleep_exit())?, false);
    ns.create_file("/ab-ktest/faults", 0o755, faults).map_err(|e| format!("create: {e}"))?;
    ns.create_file("/ab-ktest/sleep", 0o755, sleeper).map_err(|e| format!("create: {e}"))?;
    ns.create_file("/ab-ktest/flood", 0o755, image(need(test_programs::flood())?, true)).map_err(|e| format!("create: {e}"))?;
    let mut cfg = mcp::ServerConfig::new(TOKEN);
    cfg.addr = SockAddr { ip: Ipv4::LOCALHOST, port: RUN_PORT };
    let srv = mcp::server::start(sock, Arc::new(mcp::KernelEnv), cfg, mcp::TOOLS.entries()).map_err(|e| format!("start: {e:?}"))?;
    let mut c = Client { sock, conn: None, id: 0 };
    let r = (|| -> TestResult {
        let t = c.ok(r#"{"path":"hello"}"#)?;
        kassert!(t.starts_with("pid ") && t.contains(" ended: exit 0\n") && t.contains("--- stdout (31 bytes) ---\nhello from musl C on BitsOS V2\n--- stderr (0 bytes) ---"), "hello: {t}");
        let t = c.ok(r#"{"path":"/bin/hello-rs","args":["a","b"],"env":{"X":"1"}}"#)?;
        kassert!(t.contains("ended: exit 0") && t.contains("hello from Rust std on BitsOS V2, sum=10"), "hello-rs: {t}");
        // 3 MiB of output: the first MiB is kept, the rest counted, and the program never blocks.
        let t = c.ok(r#"{"path":"/ab-ktest/flood","args":["b","c"]}"#)?;
        let head = format!("--- stdout ({} bytes, last {} not kept) ---\n", 3 << 20, 2 << 20);
        kassert!(t.contains("ended: exit 0\n") && t.contains(&head), "flood: {}", t.get(..300).unwrap_or(&t));
        let kept = t.split(&head).nth(1).map(|r| r.bytes().take_while(|&b| b == b'A').count()).unwrap_or(0);
        kassert!(kept == crate::mcp_run::OUTPUT_CAP, "flood: {kept} bytes kept");
        let t = c.ok(r#"{"path":"/ab-ktest/faults"}"#)?;
        kassert!(t.contains("ended: signal 11 (SIGSEGV), code 139\nProgram faults (pid ") && t.contains("Page fault at 0x0: not mapped"), "fault: {t}");
        let t = c.ok(r#"{"path":"/ab-ktest/sleep","args":["1","2"]}"#)?;
        kassert!(t.contains("ended: exit 3"), "sleep: {t}");
        let forever = format!(r#"{{"path":"/ab-ktest/sleep","args":[{}],"kill_after_ms":200}}"#, (0..120).map(|_| "\"z\"").collect::<Vec<_>>().join(","));
        let t = c.ok(&forever)?;
        kassert!(t.contains("ended: signal 9 (SIGKILL), code 137\nkilled after kill_after_ms=200"), "kill_after: {t}");
        // Past the call's deadline: "Still running (call n)", then stop ends the program.
        let late = format!(r#"{{"path":"/ab-ktest/sleep","args":[{}],"timeout_ms":300}}"#, (0..120).map(|_| "\"z\"").collect::<Vec<_>>().join(","));
        let r = c.call("run", &late)?;
        kassert!(r.error && r.text.starts_with("Still running"), "late: {}", r.text);
        let n: u64 = r.text.split("(call ").nth(1).and_then(|s| s.split(')').next()).and_then(|s| s.parse().ok()).ok_or_else(|| format!("no call number in {}", r.text))?;
        let s = c.call("stop", &format!(r#"{{"call":{n}}}"#))?;
        kassert!(s.text.contains("signal 9 (SIGKILL)") && s.text.contains("stopped by the stop tool"), "stop: {}", s.text);
        let r = c.call("run", r#"{"path":"/no/such"}"#)?;
        kassert!(r.error && r.text.contains("no such file"), "missing: {}", r.text);
        // cwd (FS-1's working directory): a directory runs, a missing one or a file is a bad argument.
        let r = c.call("run", r#"{"path":"hello","cwd":"/tmp"}"#)?;
        kassert!(!r.error && r.text.contains("ended: exit 0"), "cwd /tmp: {}", r.text);
        for bad in [r#"{"path":"hello","cwd":"/no/such"}"#, r#"{"path":"hello","cwd":"/bin/hello"}"#] {
            let r = c.call("run", bad)?;
            kassert!(r.rpc_error == Some(-32602), "{bad}: {:?} {}", r.rpc_error, r.text);
        }
        for bad in [r#"{"path":5}"#, r#"{"path":"x","cwd":"tmp"}"#, r#"{"path":"x","args":[1]}"#, r#"{"path":"x","kill_after_ms":0}"#, r#"{}"#] {
            let r = c.call("run", bad)?;
            kassert!(r.rpc_error == Some(-32602), "{bad}: {:?} {}", r.rpc_error, r.text);
        }
        Ok(())
    })();
    if let Some(s) = c.conn.take() {
        s.close();
    }
    srv.stop();
    let stopped = srv.wait_stopped(5 * SEC);
    sched::reap();
    r?;
    kassert!(stopped, "server threads did not end");
    Ok(())
}
ktest!("ab.proc.run_tool", Safe, run_tool);

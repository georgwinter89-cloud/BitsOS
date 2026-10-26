//! AB-4 ktests: threads (a kill with threads spinning in user mode, exit_group from a thread, the
//! CLONE_CHILD_CLEARTID word), a fork whose zombie gives its memory and files back at once, execve of
//! programs, a #! script and the refusals, what fork needs from MM-5b's `AddressSpace::fork` on bare address
//! spaces (the loader's kinds of memory, the heap and its break, PROT_NONE content), and the libc programs of
//! user/abi-tests when the image carries them. All `Safe` and rerunnable: own processes, own files under
//! /tmp/ab4-*.

use crate::ktests::{PROG, SEC, ended, image, left_table, need, start};
use crate::stdio::{Capture, Input};
use crate::{ExitStatus, SpawnOptions};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::entry::ab4_progs;
use ktest::{TestResult, kassert, ktest};
use mm::{AddressSpace, Fill, Prot, VmaKind};
use uapi::ab::signal::SIGKILL;
use vfs::File;

const MS: u64 = 1_000_000;
const HELLO: &[u8] = b"hello from musl C on BitsOS V2\n";

fn until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = time::now_ns() + secs * SEC;
    loop {
        if f() {
            return true;
        }
        if time::now_ns() >= end {
            return false;
        }
        sched::sleep_ns(MS);
    }
}

ktest!("ab.ab4.kill_with_threads", Safe, kill_with_threads);
fn kill_with_threads() -> TestResult {
    let img = image(need(ab4_progs::thread_maker())?, true);
    let (p, _) = start("/ab4/threads", &img, &["t"], None, false)?;
    kassert!(until(5, || p.thread_count() == 4), "4 threads expected, {} running", p.thread_count());
    let tids: Vec<u32> = p.threads().iter().map(|t| t.tid()).collect();
    let mut sorted = tids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    kassert!(sorted.len() == 4 && tids.contains(&p.pid), "thread ids {tids:?} (pid {})", p.pid);
    kassert!(until(5, || tids.iter().all(|&t| crate::find_thread(t).is_some_and(|x| x.group().tgid == p.pid))), "a thread id does not lead to its process");
    p.kill(SIGKILL);
    let st = ended(&p, 5)?;
    kassert!(st == ExitStatus::Signaled(SIGKILL), "{}", st.describe());
    kassert!(p.thread_count() == 0, "{} threads left", p.thread_count());
    kassert!(tids.iter().all(|&t| crate::find_thread(t).is_none()), "a thread is still registered");
    Ok(())
}

ktest!("ab.ab4.exit_group_from_a_thread", Safe, exit_group_from_a_thread);
fn exit_group_from_a_thread() -> TestResult {
    let img = image(need(ab4_progs::thread_maker())?, true);
    let (p, _) = start("/ab4/threads", &img, &["t", "2"], None, false)?;
    let st = ended(&p, 10)?;
    kassert!(st == ExitStatus::Exited(7), "{}", st.describe());
    kassert!(p.thread_count() == 0);
    Ok(())
}

ktest!("ab.ab4.child_cleartid", Safe, child_cleartid);
fn child_cleartid() -> TestResult {
    let img = image(need(ab4_progs::thread_maker())?, true);
    let (p, _) = start("/ab4/threads", &img, &["t", "2", "3"], None, false)?;
    let st = ended(&p, 10)?;
    kassert!(st == ExitStatus::Exited(0), "{} (4: the word did not hold the thread id, 6: never zeroed)", st.describe());
    Ok(())
}

ktest!("ab.ab4.zombie_gives_back", Safe, zombie_gives_back);
fn zombie_gives_back() -> TestResult {
    let img = image(need(ab4_progs::zombie_maker())?, true);
    let (p, _) = start("/ab4/zombie", &img, &["z"], None, false)?;
    let mut child = None;
    kassert!(
        until(5, || {
            child = crate::processes().into_iter().find(|c| c.ppid() == p.pid && c.is_finished());
            child.is_some()
        }),
        "no zombie child of {} appeared",
        p.pid
    );
    let c = child.ok_or("zombie")?;
    kassert!(c.status() == Some(ExitStatus::Exited(5)), "zombie status {:?}", c.status());
    kassert!(!Arc::ptr_eq(&c.mm, &p.mm), "the fork shares the parent's address space");
    // Linux's exit_mm and exit_files: nothing of the zombie's memory or files is left.
    kassert!(c.mm.resident_pages() == 0, "the zombie still holds {} pages", c.mm.resident_pages());
    kassert!(c.fds.count() == 0, "the zombie still holds {} files", c.fds.count());
    kassert!(c.thread_count() == 0);
    let st = ended(&p, 5)?;
    kassert!(st == ExitStatus::Exited(0), "parent: {} (2: wait4 pid, 3: status)", st.describe());
    kassert!(left_table(c.pid, 5), "the collected zombie stayed in the table");
    Ok(())
}

ktest!("ab.ab4.execve", Safe, execve_cases);
fn execve_cases() -> TestResult {
    let img = image(need(ab4_progs::exec_arg())?, true);
    let ns = vfs::root_ns();
    let dir = "/tmp/ab4-exec";
    ns.mkdir_p(dir).map_err(|e| format!("mkdir: {e}"))?;
    for (name, mode, text) in [("script", 0o755, &b"#!/bin/hello\n"[..]), ("garbage", 0o755, b"garbage\n"), ("plain", 0o644, b"#!/bin/hello\n"), ("loop", 0o755, b"#!/tmp/ab4-exec/loop\n")] {
        ns.create_file(&format!("{dir}/{name}"), mode, text.to_vec()).map_err(|e| format!("create {name}: {e}"))?;
    }
    let cases: [(&str, Option<&[u8]>, ExitStatus); 7] = [
        ("/bin/hello", Some(HELLO), ExitStatus::Exited(0)),
        ("/tmp/ab4-exec/script", Some(HELLO), ExitStatus::Exited(0)),
        ("/tmp/ab4-exec/missing", None, ExitStatus::Exited(100 + 2)),
        ("/tmp/ab4-exec", None, ExitStatus::Exited(100 + 13)),
        ("/tmp/ab4-exec/garbage", None, ExitStatus::Exited(100 + 8)),
        ("/tmp/ab4-exec/plain", None, ExitStatus::Exited(100 + 13)),
        ("/tmp/ab4-exec/loop", None, ExitStatus::Exited(100 + 40)),
    ];
    let mut r = Ok(());
    for (target, out, want) in cases {
        let (p, o) = start("/ab4/exec", &img, &["e", target], None, false)?;
        let st = ended(&p, 10)?;
        if st != want {
            r = Err(format!("{target}: {} (want {})", st.describe(), want.describe()));
            break;
        }
        if let Some(text) = out
            && o.contents() != text
        {
            r = Err(format!("{target}: printed {:?}", String::from_utf8_lossy(&o.contents())));
            break;
        }
    }
    let _ = ns.remove_tree(dir);
    r
}

ktest!("ab.ab4.fork_address_space", Safe, fork_copy);
fn fork_copy() -> TestResult {
    const A: u64 = 0x10_0000_0000;
    let nomem = |e| format!("{e:?}");
    let parent = AddressSpace::with_fill(Fill::OnFault).map_err(nomem)?;
    parent.map_fixed(A, 16 * 4096, Prot::rw(), VmaKind::Anonymous).map_err(nomem)?;
    for (i, b) in [(0u64, 1u8), (3, 3), (7, 7)] {
        parent.copy_to_user(A + i * 4096, &[b; 64]).map_err(nomem)?;
    }
    parent.map_fixed(A + 0x10_0000, 4096, Prot { read: true, write: false, exec: true }, VmaKind::Image).map_err(nomem)?;
    parent.write_loader(A + 0x10_0000, b"code").map_err(nomem)?;
    parent.map_fixed(A + 0x20_0000, 4096, Prot::rw(), VmaKind::Anonymous).map_err(nomem)?;
    parent.copy_to_user(A + 0x20_0000, b"hidden").map_err(nomem)?;
    parent.protect(A + 0x20_0000, 4096, Prot::NONE).map_err(nomem)?;
    let brk0 = A + 0x40_0000;
    parent.init_brk(brk0);
    kassert!(parent.sys_brk(brk0 + 3 * 4096) == brk0 + 3 * 4096, "parent brk");
    parent.copy_to_user(brk0 + 4096, b"heap").map_err(nomem)?;
    parent.unmap(brk0 + 2 * 4096, 4096).map_err(nomem)?;

    let child = parent.fork().map_err(nomem)?;
    let shape = |a: &AddressSpace| a.vmas().iter().map(|v| (v.start, v.end, v.prot, v.kind)).collect::<Vec<(u64, u64, Prot, VmaKind)>>();
    kassert!(shape(&parent) == shape(&child), "VMAs differ:\n{:?}\n{:?}", shape(&parent), shape(&child));
    kassert!(child.current_brk() == parent.current_brk(), "brk {:#x} vs {:#x}", child.current_brk(), parent.current_brk());
    kassert!(child.resident_pages() == parent.resident_pages(), "resident {} vs {}", child.resident_pages(), parent.resident_pages());
    let read = |a: &AddressSpace, at: u64, n: usize| -> Vec<u8> {
        let mut b = alloc::vec![0u8; n];
        let _ = a.copy_from_user(&mut b, at);
        b
    };
    for (i, b) in [(0u64, 1u8), (3, 3), (7, 7)] {
        kassert!(read(&child, A + i * 4096, 64) == [b; 64], "page {i} not copied");
    }
    kassert!(hal::paging::translate(child.root(), A + 4096).is_none(), "a page the parent never touched is present in the child");
    kassert!(read(&child, A + 0x10_0000, 4) == b"code", "read-only page");
    child.protect(A + 0x20_0000, 4096, Prot::rw()).map_err(nomem)?;
    kassert!(read(&child, A + 0x20_0000, 6) == b"hidden", "PROT_NONE content lost");
    kassert!(read(&child, brk0 + 4096, 4) == b"heap", "heap page");
    let phys = |a: &AddressSpace, at: u64| hal::paging::translate(a.root(), at).map(|(p, _)| p);
    kassert!(phys(&parent, A) == phys(&child, A), "a page is not shared copy-on-write after the fork");
    child.copy_to_user(A, &[9; 64]).map_err(nomem)?;
    kassert!(phys(&parent, A) != phys(&child, A), "the child's write did not get its own frame");
    kassert!(read(&parent, A, 64) == [1; 64], "a child write reached the parent");
    parent.copy_to_user(A + 3 * 4096, &[8; 64]).map_err(nomem)?;
    kassert!(read(&child, A + 3 * 4096, 64) == [3; 64], "a parent write reached the child");
    kassert!(child.sys_brk(brk0 + 5 * 4096) == brk0 + 5 * 4096, "the child's brk does not grow");
    Ok(())
}

// The libc programs of user/abi-tests (the image has them when UL-1's sysroot was built): they must end with
// 0, print their last line and no failed yes/no check (`cargo xtask abi-diff` compares them with CachyOS).
ktest!("ab.ab4.libc_programs", Safe, libc_programs);
fn libc_programs() -> TestResult {
    let ns = vfs::root_ns();
    let progs = [("/bin/abi-fork", "fork: done"), ("/bin/abi-threads", "threads: done"), ("/bin/abi-pipeline", "pipeline: done"), ("/bin/abi-rs-threads", "rust threads: done")];
    let missing: Vec<&str> = progs.iter().filter(|(p, _)| ns.lookup(p).is_err()).map(|(p, _)| *p).collect();
    // Skipped only when the image carries no ABI programs at all (built without UL-1's sysroot). When it carries
    // the others (AB-3's /bin/abi-ids), a missing AB-4 program is a build failure the gate must show.
    if missing.len() == progs.len() && ns.lookup("/bin/abi-ids").is_err() {
        return ktest::skip("the user/abi-tests programs are not in this image (no musl sysroot when it was built)");
    }
    kassert!(missing.is_empty(), "the image has ABI programs but not {missing:?} (did it fail to build?)");
    for (path, last) in progs {
        let out = Capture::new(1 << 20);
        let err = Capture::new(1 << 16);
        let argv = [path];
        let env = ["PATH=/bin", "HOME=/", "ABI_TEST=1"];
        let mut o = SpawnOptions::new(&argv, &env, PROG);
        o.stdin = Some(Input::new(Vec::new()) as Arc<dyn File>);
        o.stdout = Some(out.clone() as Arc<dyn File>);
        o.stderr = Some(err.clone() as Arc<dyn File>);
        let p = crate::spawn_with(path, &o).map_err(|e| format!("{path}: {e}"))?;
        let st = ended(&p, 120)?;
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        kassert!(st == ExitStatus::Exited(0), "{path}: {} {:?}; stderr {:?}", st.describe(), p.end_message(), String::from_utf8_lossy(&err.contents()));
        kassert!(text.lines().last() == Some(last), "{path}: last line {:?}", text.lines().last());
        let failed: Vec<&str> = text.lines().filter(|l| l.ends_with(": no")).collect();
        kassert!(failed.is_empty(), "{path}: failed checks {failed:?}");
        klog::event!(Info, "ktest", "ab4.libc_program", path = path, lines = text.lines().count());
    }
    Ok(())
}

/// Process table entries, registered threads and free frames now.
fn counts() -> (usize, usize, u64) {
    let free = mm_core::frames().map_or(0, |f| f.stats().free_bytes / 4096);
    (crate::processes().len(), crate::task::thread_total(), free)
}

// Rounds of thread create/exit (with the CLEARTID wake), fork + exit + wait4, and spawn + execve: afterwards the
// table, the thread registry and the free frames are back where they started. A leak of one process image per
// round (page tables, stack, the fork child's 256 touched pages) is far above the allowance, which covers
// kernel-heap growth and other activity on a running system.
ktest!("ab.ab4.no_leaks", Safe, no_leaks);
fn no_leaks() -> TestResult {
    const ROUNDS: usize = 24;
    const FRAME_ALLOWANCE: u64 = 192;
    let threads = image(need(ab4_progs::thread_maker())?, true);
    let zombie = image(need(ab4_progs::zombie_maker())?, true);
    let exec = image(need(ab4_progs::exec_arg())?, true);
    let round = || -> TestResult {
        for (name, img, argv, want) in [
            ("/ab4/threads", &threads, &["t", "2", "3"][..], ExitStatus::Exited(0)),
            ("/ab4/zombie", &zombie, &["z", "fast"][..], ExitStatus::Exited(0)),
            ("/ab4/exec", &exec, &["e", "/bin/hello"][..], ExitStatus::Exited(0)),
        ] {
            let (p, _) = start(name, img, argv, None, false)?;
            let st = ended(&p, 10)?;
            kassert!(st == want, "{name}: {}", st.describe());
            kassert!(left_table(p.pid, 5), "{name}: pid {} stayed in the table", p.pid);
        }
        Ok(())
    };
    // Warm up (heap pools, stack slots, counters), then measure.
    round()?;
    round()?;
    let (procs0, threads0, free0) = counts();
    for _ in 0..ROUNDS {
        round()?;
    }
    let settled = until(5, || {
        let (p, t, _) = counts();
        p <= procs0 && t <= threads0
    });
    let (procs1, threads1, free1) = counts();
    kassert!(settled, "processes {procs0} -> {procs1}, threads {threads0} -> {threads1} after {ROUNDS} rounds");
    let lost = free0.saturating_sub(free1);
    klog::event!(Info, "ktest", "ab4.no_leaks", rounds = ROUNDS, frames_before = free0, frames_after = free1, lost = lost);
    kassert!(lost <= FRAME_ALLOWANCE, "{lost} frames fewer free after {ROUNDS} rounds (allowance {FRAME_ALLOWANCE})");
    Ok(())
}

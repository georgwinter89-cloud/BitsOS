//! abi-b: wave B ("threads and events", spec/rust-ecosystem.md §3.5): clone (threads), futex, gettid/tkill/
//! tgkill, sched_*, poll/ppoll/epoll_*, eventfd2, pipe2, dup*, fcntl, madvise/mremap/mincore/msync,
//! prlimit64/sysinfo/prctl, plus the stricter argument checks of wave-A calls that a no-op stub does not make
//! (signal masks, clock ids, real time). Prints a transcript equal to CachyOS's.
#![forbid(unsafe_code)]

use abitest::T;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use usys::*;

const O_NONBLOCK: usize = 0o4000;
const O_CLOEXEC: usize = 0o2000000;
const F_DUPFD: usize = 0;
const F_GETFD: usize = 1;
const F_SETFD: usize = 2;
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const F_DUPFD_CLOEXEC: usize = 1030;
const EPOLLIN: u32 = 1;
const EPOLLOUT: u32 = 4;
const PAGE: usize = 4096;

fn main() {
    let mut t = T::begin('B');
    let pid = getpid().unwrap_or(0);

    // --- scheduling
    t.sys("b.sched_yield", sched_yield());
    match sched_getaffinity(0, 128) {
        Ok(m) => t.check("b.sched_getaffinity.has_cpus", m.iter().any(|b| *b != 0)),
        Err(e) => t.say("b.sched_getaffinity.has_cpus", e),
    }
    t.kind("b.sched_getaffinity.odd_size", &sched_getaffinity(0, 1));
    t.sys("b.sched_getscheduler.self", sched_getscheduler(0));
    t.sys("b.sched_get_priority_max.fifo", sched_get_priority_max(1));
    t.sys("b.sched_get_priority_min.fifo", sched_get_priority_min(1));
    t.sys("b.sched_get_priority_max.other", sched_get_priority_max(0));
    t.sys("b.sched_get_priority_max.bad_policy", sched_get_priority_max(99));
    t.say("b.sched_getparam.self", sched_getparam(0).map(|p| p.to_string()).unwrap_or_else(|e| e.to_string()));
    t.check("b.available_parallelism", std::thread::available_parallelism().is_ok_and(|n| n.get() >= 1));

    // --- ids and signals to self (signal 0 = existence check)
    let tid = gettid().unwrap_or(0);
    t.check("b.gettid.main_equals_pid", tid == pid);
    t.sys("b.tkill.self_sig0", tkill(tid, 0));
    t.sys("b.tgkill.self_sig0", tgkill(pid, tid, 0));
    t.sys("b.tgkill.no_such_thread", tgkill(pid, 0x3fff_fff0, 0));
    t.sys("b.tgkill.bad_signal", tgkill(pid, tid, 99));
    t.sys("b.kill.self_sig0", kill(pid as isize, 0));
    t.sys("b.kill.bad_signal", kill(pid as isize, 99));
    t.sys("b.kill.no_such_process", kill(0x3fff_fff0, 0));

    // --- futex
    let word = AtomicU32::new(5);
    t.sys("b.futex.wake_nobody", futex_wake(&word, 1, false));
    t.sys("b.futex.wake_nobody_private", futex_wake(&word, 1, true));
    t.sys("b.futex.wait_value_changed", futex_wait(&word, 6, None, true));
    let s0 = mono_ns();
    t.sys("b.futex.wait_timeout_10ms", futex_wait(&word, 5, Some((0, 10_000_000)), true));
    t.check("b.futex.waited_at_least_10ms", mono_ns().saturating_sub(s0) >= 10_000_000);
    t.sys("b.futex.wait_bad_timeout", futex_wait(&word, 5, Some((0, 2_000_000_000)), true));
    t.sys("b.futex.bad_op", futex_bad_op(&word));

    // --- threads (std::thread = musl pthread_create = clone + futex)
    let h = std::thread::Builder::new().stack_size(64 * 1024).spawn(|| (gettid().unwrap_or(0), getpid().unwrap_or(0), 6 * 7));
    t.kind("b.thread.spawn", &h.as_ref().map(|_| ()).map_err(|e| Errno(e.raw_os_error().unwrap_or(0))));
    match h.map(|h| h.join()) {
        Ok(Ok((ttid, tpid, v))) => {
            t.say("b.thread.result", v);
            t.check("b.thread.own_tid", ttid != tid && ttid != 0);
            t.check("b.thread.same_pid", tpid == pid);
        }
        _ => {
            for id in ["b.thread.result", "b.thread.own_tid", "b.thread.same_pid"] {
                t.skip(id, "no thread");
            }
        }
    }
    // Ping-pong through a Mutex + Condvar (futex wait/wake between two threads), 200 rounds.
    let pair = Arc::new((Mutex::new(0u32), Condvar::new()));
    let p2 = pair.clone();
    let h = std::thread::Builder::new().stack_size(64 * 1024).spawn(move || {
        let (m, c) = &*p2;
        for i in 0..100u32 {
            let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
            while *g != 2 * i + 1 {
                g = c.wait(g).unwrap_or_else(|e| e.into_inner());
            }
            *g += 1;
            c.notify_all();
        }
    });
    match h {
        Ok(h) => {
            let (m, c) = &*pair;
            for i in 0..100u32 {
                let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
                while *g != 2 * i {
                    g = c.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                *g += 1;
                c.notify_all();
            }
            let _ = h.join();
            t.say("b.thread.condvar_pingpong", *m.lock().unwrap_or_else(|e| e.into_inner()));
        }
        Err(_) => t.skip("b.thread.condvar_pingpong", "no thread"),
    }
    // Eight threads count into one atomic.
    let total = Arc::new(AtomicU32::new(0));
    let hs: Vec<_> = (0..8)
        .filter_map(|_| {
            let c = total.clone();
            std::thread::Builder::new().stack_size(64 * 1024).spawn(move || {
                for _ in 0..1000 {
                    c.fetch_add(1, Ordering::Relaxed);
                }
            }).ok()
        })
        .collect();
    let started = hs.len();
    for h in hs {
        let _ = h.join();
    }
    t.say("b.thread.eight_counters", format!("{started} threads, total {}", total.load(Ordering::Relaxed)));
    t.sys("b.clone.sighand_without_vm", clone_invalid(0x800));
    t.sys("b.clone.thread_without_sighand", clone_invalid(0x10000));

    // --- fcntl and dup on a plain descriptor (/dev/null)
    match open("/dev/null", 2 | O_CLOEXEC, 0) {
        Ok(f) => {
            t.sys("b.fcntl.getfd_cloexec", fcntl(f, F_GETFD, 0));
            t.sys("b.fcntl.getfl_rdwr", fcntl(f, F_GETFL, 0));
            t.sys("b.fcntl.setfl_nonblock", fcntl(f, F_SETFL, O_NONBLOCK));
            t.check("b.fcntl.getfl_has_nonblock", fcntl(f, F_GETFL, 0).is_ok_and(|v| v & O_NONBLOCK != 0));
            t.sys("b.fcntl.setfd_clear", fcntl(f, F_SETFD, 0));
            t.sys("b.fcntl.getfd_cleared", fcntl(f, F_GETFD, 0));
            let d = dup(f);
            t.kind("b.dup", &d);
            if let Ok(d) = d {
                t.sys("b.dup.no_cloexec", fcntl(d, F_GETFD, 0));
                let _ = close(d);
            }
            t.sys("b.dup2.to_50", dup2(f, 50));
            t.sys("b.dup2.same_fd", dup2(50, 50));
            t.sys("b.dup3.to_50_cloexec", dup3(f, 50, O_CLOEXEC));
            t.sys("b.dup3.cloexec_set", fcntl(50, F_GETFD, 0));
            t.sys("b.dup3.same_fd", dup3(50, 50, 0));
            t.sys("b.dup3.bad_flags", dup3(f, 51, 1));
            t.sys("b.dup2.bad_fd", dup2(9999, 52));
            t.sys("b.fcntl.dupfd_60", fcntl(f, F_DUPFD, 60));
            t.sys("b.fcntl.dupfd_cloexec_70", fcntl(f, F_DUPFD_CLOEXEC, 70));
            t.sys("b.fcntl.dupfd_cloexec_flag", fcntl(70, F_GETFD, 0));
            t.sys("b.fcntl.bad_fd", fcntl(9999, F_GETFD, 0));
            for fd in [50, 60, 70, f] {
                let _ = close(fd);
            }
        }
        Err(e) => t.say("b.open.dev_null", e),
    }

    // --- pipes
    match pipe2(O_CLOEXEC) {
        Ok((r, w)) => {
            t.sys("b.pipe2.cloexec_flag", fcntl(r, F_GETFD, 0));
            t.sys("b.fcntl.getfl_read_end", fcntl(r, F_GETFL, 0));
            t.sys("b.fcntl.getfl_write_end", fcntl(w, F_GETFL, 0));
            let mut pf = [PollFd { fd: r as i32, events: POLLIN, revents: 0 }];
            t.sys("b.poll.empty_pipe", poll(&mut pf, 0));
            t.sys("b.write.pipe", write(w, b"hello"));
            t.say("b.ioctl.fionread", ioctl_fionread(r).map(|n| n.to_string()).unwrap_or_else(|e| e.to_string()));
            let mut pf = [PollFd { fd: r as i32, events: POLLIN, revents: 0 }];
            t.sys("b.poll.pipe_readable", poll(&mut pf, 0));
            t.say("b.poll.pipe_readable_revents", pf[0].revents);
            let mut pf = [PollFd { fd: r as i32, events: POLLIN, revents: 0 }];
            t.sys("b.ppoll.pipe_readable", ppoll(&mut pf, Some((0, 0))));
            let mut b = [0u8; 16];
            t.sys("b.read.pipe", read(r, &mut b));
            t.sys("b.fcntl.setfl_nonblock_pipe", fcntl(r, F_SETFL, O_NONBLOCK));
            t.sys("b.read.empty_nonblocking", read(r, &mut b));
            // epoll on the pipe
            match epoll_create1(0) {
                Ok(ep) => {
                    t.sys("b.epoll_ctl.add", epoll_ctl(ep, EPOLL_CTL_ADD, r, EPOLLIN, 7));
                    t.sys("b.epoll_ctl.add_again", epoll_ctl(ep, EPOLL_CTL_ADD, r, EPOLLIN, 7));
                    t.say("b.epoll_wait.nothing", format!("{:?}", epoll_wait(ep, 4, 0, false).map_err(|e| e.to_string())));
                    let _ = write(w, b"x");
                    t.say("b.epoll_wait.readable", format!("{:?}", epoll_wait(ep, 4, 0, false).map_err(|e| e.to_string())));
                    t.say("b.epoll_pwait.readable", format!("{:?}", epoll_wait(ep, 4, 0, true).map_err(|e| e.to_string())));
                    t.sys("b.epoll_ctl.mod", epoll_ctl(ep, EPOLL_CTL_MOD, r, EPOLLIN | EPOLLOUT, 8));
                    t.say("b.epoll_wait.after_mod", format!("{:?}", epoll_wait(ep, 4, 0, false).map_err(|e| e.to_string())));
                    t.sys("b.epoll_ctl.del", epoll_ctl(ep, EPOLL_CTL_DEL, r, 0, 0));
                    t.sys("b.epoll_ctl.del_again", epoll_ctl(ep, EPOLL_CTL_DEL, r, 0, 0));
                    t.sys("b.epoll_ctl.self", epoll_ctl(ep, EPOLL_CTL_ADD, ep, EPOLLIN, 0));
                    t.sys("b.epoll_ctl.bad_op", epoll_ctl(ep, 9, r, EPOLLIN, 0));
                    t.sys("b.epoll_wait.zero_max", epoll_wait(ep, 0, 0, false).map(|v| v.len()));
                    let _ = close(ep);
                }
                Err(e) => t.say("b.epoll_create1", e),
            }
            t.sys("b.epoll_create1.bad_flags", epoll_create1(1));
            let mut b = [0u8; 16];
            let _ = read(r, &mut b);
            t.sys("b.close.write_end", close(w));
            let mut pf = [PollFd { fd: r as i32, events: POLLIN, revents: 0 }];
            t.sys("b.poll.writer_gone", poll(&mut pf, 0));
            t.say("b.poll.writer_gone_revents", pf[0].revents);
            t.sys("b.read.writer_gone_eof", read(r, &mut b));
            let _ = close(r);
        }
        Err(e) => t.say("b.pipe2", e),
    }
    match pipe() {
        Ok((r, w)) => {
            let _ = close(r);
            // Rust's runtime ignores SIGPIPE, so a write without a reader answers EPIPE.
            t.sys("b.write.no_reader", write(w, b"x"));
            let _ = close(w);
        }
        Err(e) => t.say("b.pipe", e),
    }
    t.kind("b.pipe2.bad_flags", &pipe2(1));

    // --- eventfd
    match eventfd2(3, O_NONBLOCK) {
        Ok(e) => {
            let mut b = [0u8; 8];
            t.sys("b.eventfd.read", read(e, &mut b));
            t.say("b.eventfd.value", u64::from_le_bytes(b));
            t.sys("b.eventfd.read_empty", read(e, &mut b));
            t.sys("b.eventfd.write", write(e, &5u64.to_le_bytes()));
            t.sys("b.eventfd.write_max", write(e, &u64::MAX.to_le_bytes()));
            let mut small = [0u8; 4];
            t.sys("b.eventfd.read_short_buffer", read(e, &mut small));
            let _ = read(e, &mut b);
            t.say("b.eventfd.value_after_write", u64::from_le_bytes(b));
            let _ = close(e);
        }
        Err(e) => t.say("b.eventfd2", e),
    }
    // 1 would be EFD_SEMAPHORE (valid); 2 is no eventfd flag.
    t.sys("b.eventfd2.bad_flags", eventfd2(0, 2));

    // --- memory: madvise, mremap, mincore, msync
    match Mapping::anon(4 * PAGE, PROT_READ | PROT_WRITE, 0) {
        Ok(mut m) => {
            m.set(0, 9);
            t.say("b.mincore.first_page_touched", format!("{:?}", m.mincore(0, 4 * PAGE).map(|v| v.iter().map(|b| b & 1).collect::<Vec<u8>>()).map_err(|e| e.to_string())));
            t.sys("b.madvise.dontneed", m.advise(0, PAGE, 4));
            t.say("b.madvise.dontneed_zeroes", m.get(0).unwrap_or(9));
            t.sys("b.madvise.bad_advice", m.advise(0, PAGE, 9999));
            t.sys("b.madvise.unaligned", madvise_probe(0x10001, PAGE, 4));
            t.sys("b.madvise.willneed", m.advise(0, 4 * PAGE, 3));
            t.sys("b.msync.sync", m.sync(0, PAGE, 4));
            t.sys("b.msync.bad_flags", m.sync(0, PAGE, 5));
            m.set(100, 77);
            t.kind("b.mremap.grow_maymove", &m.remap(16 * PAGE, true));
            t.say("b.mremap.content_kept", m.get(100).unwrap_or(0));
            t.say("b.mremap.new_part_zero", m.get(15 * PAGE).unwrap_or(9));
            let before = m.addr();
            t.kind("b.mremap.shrink", &m.remap(2 * PAGE, false));
            t.check("b.mremap.shrink_in_place", m.addr() == before);
            t.say("b.mremap.content_after_shrink", m.get(100).unwrap_or(0));
        }
        Err(e) => t.say("b.mmap", e),
    }

    // --- limits, sysinfo, prctl, random
    match prlimit64(0, 7, None) {
        Ok((cur, max)) => {
            t.sys("b.prlimit64.set_nofile_64", prlimit64(0, 7, Some((64, max))).map(|_| 0));
            t.say("b.prlimit64.nofile_now", prlimit64(0, 7, None).map(|(c, _)| c.to_string()).unwrap_or_else(|e| e.to_string()));
            t.sys("b.dup2.above_limit", dup2(0, 64));
            t.sys("b.prlimit64.cur_above_max", prlimit64(0, 7, Some((max.saturating_add(1), max))).map(|_| 0));
            let _ = prlimit64(0, 7, Some((cur, max)));
        }
        Err(e) => t.say("b.prlimit64", e),
    }
    t.sys("b.prlimit64.bad_resource", prlimit64(0, 99, None).map(|_| 0));
    match sysinfo() {
        Ok(s) => {
            t.check("b.sysinfo.uptime_nonnegative", s.uptime >= 0);
            t.check("b.sysinfo.totalram_positive", s.totalram > 0);
            t.check("b.sysinfo.freeram_le_total", s.freeram <= s.totalram);
            t.check("b.sysinfo.procs_positive", s.procs >= 1);
            t.say("b.sysinfo.mem_unit", s.mem_unit);
        }
        Err(e) => t.say("b.sysinfo", e),
    }
    t.sys("b.prctl.set_name", prctl_set_name("abi-b-name"));
    t.say("b.prctl.get_name", prctl_get_name().map(|n| String::from_utf8_lossy(&n).into_owned()).unwrap_or_else(|e| e.to_string()));
    t.sys("b.prctl.get_dumpable", prctl_int(3, 0));
    t.sys("b.prctl.unknown_option", prctl_int(0x5a5a_5a5a, 0));
    let mut b = [0u8; 32];
    t.sys("b.getrandom.zero", getrandom(&mut b[..0], 0));
    t.sys("b.getrandom.nonblock_32", getrandom(&mut b, 1));
    t.sys("b.getrandom.bad_flags", getrandom(&mut b, 0x100));

    // --- stricter checks of wave-A calls
    t.say("b.clock_getres.monotonic", clock_getres(CLOCK_MONOTONIC).map(|v| format!("{v:?}")).unwrap_or_else(|e| e.to_string()));
    t.sys("b.clock_gettime.bad_clock", clock_gettime(99).map(|_| 0));
    t.kind("b.clock_gettime.boottime", &clock_gettime(CLOCK_BOOTTIME));
    t.kind("b.clock_gettime.process_cputime", &clock_gettime(CLOCK_PROCESS_CPUTIME_ID));
    t.check("b.clock_gettime.realtime_after_2024", clock_gettime(CLOCK_REALTIME).is_ok_and(|(s, _)| s > 1_704_067_200));
    t.check("b.gettimeofday.after_2024", gettimeofday().is_ok_and(|(s, _)| s > 1_704_067_200));
    t.sys("b.clock_nanosleep.1ms", clock_nanosleep(CLOCK_MONOTONIC, 0, 0, 1_000_000));
    t.sys("b.clock_nanosleep.abs_past", clock_nanosleep(CLOCK_MONOTONIC, 1, 0, 1));
    t.sys("b.clock_nanosleep.bad_clock", clock_nanosleep(99, 0, 0, 1));
    // Rust std's thread::sleep goes through clock_nanosleep; without it std panics (caught here).
    t.check("b.std.thread_sleep_1ms", std::panic::catch_unwind(|| std::thread::sleep(std::time::Duration::from_millis(1))).is_ok());
    t.say("b.rt_sigprocmask.old_written", format!("{:?}", rt_sigprocmask(SIG_BLOCK, None, true, 0xdead, 8).map_err(|e| e.to_string())));
    t.sys("b.rt_sigprocmask.block_usr1", rt_sigprocmask(SIG_BLOCK, Some(1 << 9), false, 0, 8).map(|_| 0));
    t.say("b.rt_sigprocmask.usr1_blocked", format!("{:?}", rt_sigprocmask(SIG_BLOCK, None, true, 0, 8).map_err(|e| e.to_string())));
    t.sys("b.rt_sigprocmask.unblock_usr1", rt_sigprocmask(SIG_UNBLOCK, Some(1 << 9), false, 0, 8).map(|_| 0));
    t.sys("b.rt_sigprocmask.bad_how", rt_sigprocmask(99, Some(0), false, 0, 8).map(|_| 0));
    t.sys("b.rt_sigprocmask.bad_size", rt_sigprocmask(SIG_BLOCK, None, true, 0, 16).map(|_| 0));
    t.say("b.rt_sigaction.usr1_default", rt_sigaction_query(10).map(|a| a[0].to_string()).unwrap_or_else(|e| e.to_string()));
    t.sys("b.rt_sigaction.usr1_ignore", rt_sigaction_simple(10, true));
    t.say("b.rt_sigaction.usr1_now_ignored", rt_sigaction_query(10).map(|a| a[0].to_string()).unwrap_or_else(|e| e.to_string()));
    t.sys("b.rt_sigaction.usr1_default_again", rt_sigaction_simple(10, false));
    t.sys("b.rt_sigaction.sigkill", rt_sigaction_simple(9, true));
    t.sys("b.rt_sigaction.bad_signal", rt_sigaction_simple(99, true));
    t.sys("b.rt_sigaction.bad_size", rt_sigaction_bad_size(10, 16));
    t.sys("b.sigaltstack.too_small", sigaltstack_try(0x10000, 0, 1024));
    t.sys("b.sigaltstack.bad_flags", sigaltstack_try(0x10000, 99, 65536));
    t.say("b.sigaltstack.std_installed_one", sigaltstack_query().map(|(_, f, s)| format!("flags {f} size>=8192 {}", s >= 8192)).unwrap_or_else(|e| e.to_string()));

    t.end();
}

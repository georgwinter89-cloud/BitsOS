//! abi-a: wave A ("hello", spec/rust-ecosystem.md §3.5): start/exit, read/write/writev/open/openat/close/
//! fstat/newfstatat/lseek/ioctl(TIOCGWINSZ), brk/mmap/munmap/mprotect, arch_prctl/set_tid_address,
//! clock_gettime/nanosleep, uname/getpid/getuid..., rt_sigprocmask/rt_sigaction (as no-ops), plus what Rust std
//! needs at start (poll, sigaltstack, getrandom, prlimit64). Run with stdin and stdout as pipes (empty stdin), cwd
//! anywhere; prints a transcript equal to CachyOS's.
#![forbid(unsafe_code)]

use abitest::{T, scratch};
use std::sync::atomic::AtomicU32;
use usys::*;

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o200;
const O_TRUNC: usize = 0o1000;
const O_CLOEXEC: usize = 0o2000000;
const AT_FDCWD: isize = -100;
const AT_EMPTY_PATH: usize = 0x1000;
const SEEK_SET: usize = 0;
const SEEK_CUR: usize = 1;
const SEEK_END: usize = 2;
const PAGE: usize = 4096;

static TID_SLOT: AtomicU32 = AtomicU32::new(0);

fn pattern(i: usize) -> u8 {
    (i * 7 % 251) as u8
}

fn main() {
    let mut t = T::begin('A');

    // --- identity
    let pid = getpid();
    t.check("a.getpid.positive", pid.is_ok_and(|p| p > 0));
    t.check("a.gettid.equals_pid", gettid() == pid);
    t.check("a.set_tid_address.returns_tid", set_tid_address(&TID_SLOT) == pid);
    t.check("a.getuid.equals_geteuid", getuid() == geteuid());
    t.check("a.getgid.equals_getegid", getgid() == getegid());
    match uname() {
        Ok(u) => {
            t.say("a.uname.sysname", &u.sysname);
            t.say("a.uname.machine", &u.machine);
            t.check("a.uname.nodename_nonempty", !u.nodename.is_empty());
            t.check("a.uname.release_starts_with_digit", u.release.starts_with(|c: char| c.is_ascii_digit()));
        }
        Err(e) => t.say("a.uname", e),
    }

    // --- stdio (stdin: empty pipe, stdout: pipe)
    let line = b"a.write.stdout_line_length: ";
    let r = write(1, line);
    t.raw_result(r, line.len());
    let parts: [&[u8]; 3] = [b"a.writev", b".three_parts", b": "];
    let r = writev(1, &parts);
    t.raw_result(r, 22);
    t.sys("a.write.bad_fd", write(9999, b"x"));
    t.sys("a.write.negative_fd", write(usize::MAX, b"x"));
    t.sys("a.write.bad_buffer", write_from_bad(1, 0x1000, 5));
    t.sys("a.write.zero_length", write(1, b""));
    let mut b = [0u8; 16];
    t.sys("a.read.stdin_eof", read(0, &mut b));
    t.sys("a.read.bad_fd", read(9999, &mut b));
    t.sys("a.read.bad_buffer_on_stdin", read_to_bad(0, 0x1000, 16));
    t.say("a.fstat.stdin_type", fstat(0).map(|s| s.file_type().to_string()).unwrap_or_else(|e| e.to_string()));
    t.say("a.fstat.stdout_type", fstat(1).map(|s| s.file_type().to_string()).unwrap_or_else(|e| e.to_string()));
    t.kind("a.fstat.bad_fd", &fstat(9999));
    t.sys("a.fstat.bad_buffer", fstat_bad(1, 0x1000));
    t.kind("a.ioctl.tiocgwinsz_on_pipe", &ioctl_winsize(1));
    t.kind("a.ioctl.tiocgwinsz_bad_fd", &ioctl_winsize(9999));
    t.sys("a.lseek.on_pipe", lseek(1, 0, SEEK_CUR));

    // --- files
    let dir = scratch('A');
    t.sys("a.mkdir.scratch", mkdir(&dir, 0o755));
    let f = format!("{dir}/file");
    t.sys("a.open.missing", open(&format!("{dir}/missing"), O_RDONLY, 0));
    t.sys("a.open.missing_dir", open("/nonexistent-ul-abi/x", O_RDONLY, 0));
    t.sys("a.open.bad_path", open_bad_path(0x1000));
    t.sys("a.open.dir_for_writing", open("/", O_WRONLY, 0));
    // The lowest free descriptor (a starter may have passed more than 0-2 on).
    let lowest = dup(0);
    if let Ok(l) = lowest {
        let _ = close(l);
    }
    let fd = open(&f, O_CREAT | O_WRONLY | O_TRUNC | O_CLOEXEC, 0o644);
    t.kind("a.open.create", &fd);
    t.check("a.open.create_gives_lowest_fd", fd.is_ok() && fd == lowest);
    if let Ok(fd) = fd {
        let data: Vec<u8> = (0..10000).map(pattern).collect();
        t.sys("a.write.file_10000", write(fd, &data));
        t.sys("a.lseek.cur", lseek(fd, 0, SEEK_CUR));
        t.sys("a.lseek.end", lseek(fd, 0, SEEK_END));
        t.sys("a.lseek.set_100", lseek(fd, 100, SEEK_SET));
        t.sys("a.lseek.negative", lseek(fd, -1, SEEK_SET));
        t.sys("a.lseek.bad_whence", lseek(fd, 0, 7));
        t.sys("a.lseek.beyond_end", lseek(fd, 20000, SEEK_SET));
        t.sys("a.read.write_only_fd", read(fd, &mut b));
        match fstat(fd) {
            Ok(s) => {
                t.say("a.fstat.file_type", s.file_type());
                t.say("a.fstat.file_size", s.size);
                t.say("a.fstat.file_mode", format!("{:o}", s.mode & 0o7777));
                t.say("a.fstat.file_nlink", s.nlink);
                t.check("a.fstat.uid_is_euid", geteuid() == Ok(s.uid as usize));
            }
            Err(e) => t.say("a.fstat.file", e),
        }
        t.sys("a.close.file", close(fd));
        t.sys("a.close.again", close(fd));
    }
    t.sys("a.open.excl_existing", open(&f, O_CREAT | O_EXCL | O_WRONLY, 0o644));
    t.sys("a.open.file_as_dir", open(&format!("{f}/x"), O_RDONLY, 0));
    let fd = openat(AT_FDCWD, &f, O_RDONLY, 0);
    t.kind("a.openat.cwd_relative_absolute", &fd);
    if let Ok(fd) = fd {
        let mut buf = vec![0u8; 4096];
        t.sys("a.read.first_4096", read(fd, &mut buf));
        t.check("a.read.first_4096_content", buf.iter().enumerate().all(|(i, &v)| v == pattern(i)));
        let mut rest = vec![0u8; 8192];
        t.sys("a.read.rest", read(fd, &mut rest));
        t.check("a.read.rest_content", rest[..5904].iter().enumerate().all(|(i, &v)| v == pattern(i + 4096)));
        t.sys("a.read.at_eof", read(fd, &mut rest));
        t.sys("a.write.read_only_fd", write(fd, b"x"));
        t.say("a.newfstatat.empty_path_on_fd", newfstatat(fd as isize, "", AT_EMPTY_PATH).map(|s| s.size.to_string()).unwrap_or_else(|e| e.to_string()));
        let _ = close(fd);
    }
    t.say("a.newfstatat.size", newfstatat(AT_FDCWD, &f, 0).map(|s| s.size.to_string()).unwrap_or_else(|e| e.to_string()));
    t.kind("a.newfstatat.empty_path", &newfstatat(AT_FDCWD, "", 0));
    t.kind("a.newfstatat.missing", &newfstatat(AT_FDCWD, &format!("{dir}/missing"), 0));
    t.say("a.newfstatat.dir_type", newfstatat(AT_FDCWD, &dir, 0).map(|s| s.file_type().to_string()).unwrap_or_else(|e| e.to_string()));
    let fd = open(&f, O_RDWR, 0);
    t.check("a.open.rdwr_reuses_lowest_fd", fd.is_ok() && fd == lowest);
    if let Ok(fd) = fd {
        let _ = close(fd);
    }
    t.sys("a.unlink.file", unlink(&f));
    t.sys("a.rmdir.scratch", rmdir(&dir));

    // --- memory
    let (before, after, seen, restored) = brk_grow_restore(3 * PAGE);
    t.check("a.brk.initial_nonzero", before != 0);
    t.check("a.brk.grow_3_pages", after == before + 3 * PAGE);
    t.say("a.brk.new_memory_rw", format!("{seen:?}"));
    t.check("a.brk.restore", restored == before);
    match Mapping::anon(3 * PAGE, PROT_READ | PROT_WRITE, 0) {
        Ok(m) => {
            t.check("a.mmap.anon_aligned", m.addr() % PAGE == 0);
            t.check("a.mmap.anon_zeroed", (0..3 * PAGE).step_by(512).all(|o| m.get(o) == Some(0)));
            m.set(5000, 42);
            t.say("a.mmap.anon_rw", m.get(5000).unwrap_or(0));
            t.sys("a.mprotect.read_only", m.protect(0, PAGE, PROT_READ));
            t.say("a.mprotect.still_readable", m.get(10).unwrap_or(1));
            t.sys("a.mprotect.bad_prot", m.protect(0, PAGE, 0x100));
            t.sys("a.mprotect.none_then_rw", m.protect(PAGE, PAGE, PROT_NONE).and_then(|_| m.protect(PAGE, PAGE, PROT_READ | PROT_WRITE)));
            t.say("a.mprotect.content_kept", m.get(5000).unwrap_or(0));
            t.sys("a.munmap.middle_page", m.unmap_part(PAGE, PAGE));
            t.say("a.munmap.neighbours_kept", format!("{:?} {:?}", m.get(10), m.get(2 * PAGE + 10)));
            t.sys("a.mmap.fixed_into_hole", m.map_fixed_part(PAGE, PAGE, PROT_READ | PROT_WRITE, false).map(|a| a - m.addr()));
            t.say("a.mmap.fixed_page_zeroed", m.get(5000).unwrap_or(9));
        }
        Err(e) => t.say("a.mmap.anon", e),
    }
    t.sys("a.mmap.zero_length", mmap_probe(0, 0, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, usize::MAX, 0));
    t.sys("a.mmap.neither_private_nor_shared", mmap_probe(0, PAGE, PROT_READ, MAP_ANONYMOUS, usize::MAX, 0));
    t.sys("a.mmap.fixed_unaligned", mmap_probe(0x10001, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, usize::MAX, 0));
    t.sys("a.mmap.fixed_below_min_addr", mmap_probe(0x1000, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, usize::MAX, 0));
    t.kind("a.mmap.huge", &mmap_probe(0, 1 << 47, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, usize::MAX, 0));
    t.kind("a.mmap.noreserve_1tib", &mmap_probe(0, 1 << 40, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, usize::MAX, 0));
    t.sys("a.munmap.unaligned", munmap_probe(0x10001, PAGE));
    t.sys("a.munmap.zero_length", munmap_probe(0x10000, 0));
    t.sys("a.munmap.unmapped_low", munmap_probe(0x1000, PAGE));
    t.sys("a.mprotect.unaligned", mprotect_probe(0x10001, PAGE, PROT_READ));

    // --- arch, time, signals, misc
    t.check("a.arch_prctl.get_fs_nonzero", arch_prctl_get(0x1003).is_ok_and(|v| v != 0));
    t.kind("a.arch_prctl.get_gs", &arch_prctl_get(0x1004));
    t.sys("a.arch_prctl.unknown_code", arch_prctl_get(0x9999).map(|v| v as usize));
    let t1 = clock_gettime(CLOCK_MONOTONIC);
    let t2 = clock_gettime(CLOCK_MONOTONIC);
    t.kind("a.clock_gettime.monotonic", &t1);
    t.check("a.clock_gettime.nsec_in_range", t1.is_ok_and(|(_, n)| (0..1_000_000_000).contains(&n)));
    t.check("a.clock_gettime.non_decreasing", matches!((t1, t2), (Ok(a), Ok(b)) if b >= a));
    t.kind("a.clock_gettime.realtime", &clock_gettime(CLOCK_REALTIME));
    t.sys("a.clock_gettime.bad_buffer", clock_gettime_bad(CLOCK_MONOTONIC, 0x1000));
    let s0 = mono_ns();
    t.sys("a.nanosleep.20ms", nanosleep(0, 20_000_000));
    t.check("a.nanosleep.slept_at_least_20ms", mono_ns().saturating_sub(s0) >= 20_000_000);
    t.sys("a.nanosleep.zero", nanosleep(0, 0));
    t.sys("a.nanosleep.nsec_too_big", nanosleep(0, 1_000_000_000));
    t.sys("a.nanosleep.negative", nanosleep(-1, 0));
    t.kind("a.rt_sigprocmask.query", &rt_sigprocmask(SIG_BLOCK, None, false, 0, 8));
    t.kind("a.rt_sigaction.query_sigusr1", &rt_sigaction_query(10));
    t.kind("a.sigaltstack.query", &sigaltstack_query());
    let mut rnd = [0u8; 16];
    t.sys("a.getrandom.16", getrandom(&mut rnd, 0));
    let mut pf = [PollFd { fd: 1, events: POLLOUT, revents: 0 }, PollFd { fd: 9999, events: POLLIN, revents: 0 }];
    t.sys("a.poll.stdout_and_bad_fd", poll(&mut pf, 0));
    t.say("a.poll.revents", format!("{} {}", pf[0].revents, pf[1].revents));
    match prlimit64(0, 7, None) {
        Ok((cur, max)) => t.check("a.prlimit64.nofile_cur_le_max", cur <= max && cur >= 3),
        Err(e) => t.say("a.prlimit64.nofile", e),
    }

    t.end();
}

trait RawResult {
    fn raw_result(&mut self, r: SysResult, want: usize);
}

impl RawResult for T {
    /// The value of a write whose own bytes start the line (so the transcript stays one line per check).
    fn raw_result(&mut self, r: SysResult, want: usize) {
        let s = match r {
            Ok(n) if n == want => "= length".to_string(),
            other => show(other),
        };
        // Completes the line the write itself began.
        let _ = write(1, format!("{s}\n").as_bytes());
    }
}

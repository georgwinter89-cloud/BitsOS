//! The file syscalls of `vfs::sys` registered with `syscall!` (owner: FS).
//!
//! `vfs` sits below `proc` and `syscall` (both use its types), so it cannot register handlers itself; this
//! crate on top maps a `SysCtx` onto `vfs::sys::Sys` (the process's memory, fd table, mount view, credentials
//! and B3 subject) and registers one thin handler per number. The calls themselves, with Linux's argument
//! checks and error order, live in `vfs::sys` where host tests run them unchanged.
//!
//! FS-2: PCI's view under `/sys` (`sysinfo`); the framework is `vfs::synth`. The process files (AB-7a) and
//! cpuinfo/meminfo/uptime (AB-7b) moved to AB's `syscall::procfs`.
//! FS-5: the background write-back thread (`vfs::writeback`, class Background) and the ktest that runs
//! `/bin/fs-mmap` against Linux's transcript.
//! FS-10: memfd_create (the seal commands go through fcntl) and the ktests of `/bin/fs-memfd` (`fs10_ktests`).
//! FS-14: inotify's four calls; the ktests of `/bin/fs-memfd-lazy` and `/bin/fs-inotify` (`fs14_ktests`).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod bitsos;
mod fs2_ktests;
mod fs3_ktests;
mod fs4_ktests;
mod fs5_ktests;
mod fs10_ktests;
mod fs13_ktests;
mod fs14_inotify_ktests;
mod fs14_ktests;
mod kmsg;
pub mod preload;
mod preload_ktests;
mod preload_reclaim_ktest;
mod preload_tool;
mod ktests;
mod sysinfo;

use alloc::vec::Vec;
use proc::Process;
use syscall::SysCtx;
use uapi::errno::*;
use vfs::Errno;
use vfs::sys::{Args, Sys, SysFn, UserMem};

/// A process's memory as `vfs::sys` sees it (MM-5a's checked copies; lazily filled pages are filled).
struct ProcMem<'a>(&'a Process);

impl UserMem for ProcMem<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        self.0.mm.copy_from_user(buf, addr).is_ok()
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.0.mm.copy_to_user(addr, data).is_ok()
    }
    /// FS-4: the calling program is being ended (a kill, a fatal signal: Linux `fatal_signal_pending`).
    fn interrupted(&self) -> bool {
        self.0.has_exited()
    }
    /// FS-4 (AB-6): a signal waits for the calling thread's handler (Linux `signal_pending`): a wait with no
    /// event ends, and the syscall exit delivers the handler. False in a kernel thread.
    fn signal_pending(&self) -> bool {
        proc::current_signal_pending()
    }
    /// FS-4 (`set_user_sigmask`, AB-6): the calling thread's temporary mask; a default-fatal signal it
    /// unblocks ends the program at once, so the wait ends with EINTR.
    fn set_sigmask(&self, mask: u64) -> Option<(u64, u64)> {
        self.0.group().sigmask_for_wait(mask)
    }
    /// FS-4 (`restore_saved_sigmask_unless`, AB-6): puts `old` back while `temp` is still in place; when the
    /// call ends with EINTR for a signal, `old` is kept as the saved mask until the handler ran.
    fn restore_sigmask(&self, old: u64, temp: u64, interrupted: bool) {
        self.0.group().sigmask_after_wait(old, temp, interrupted);
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno> {
        let mut v = Vec::new();
        match hal::usercopy::strncpy_from_user(self.0.mm.root(), addr, &mut v, max) {
            Ok(_) => Ok(v),
            Err(hal::usercopy::StrError::Fault) => Err(EFAULT),
            Err(hal::usercopy::StrError::TooLong) => Err(ENAMETOOLONG),
        }
    }
}

/// Whether the calling thread's program was told to end (kill, exit of another thread, fault).
fn killed() -> bool {
    proc::current().is_some_and(|p| p.has_exited())
}

/// FS-4 (AB-6): whether the calling thread has a signal to handle (an unblocked pending one, or the end).
fn signal_pending() -> bool {
    proc::current_signal_pending()
}

/// Runs `f` for the calling process. Credentials: the process's uid; the group is the same number (wave A
/// answers getgid with it too) until AB's credentials item gives processes their own gid.
/// FS-3: a write to a pipe without readers sends SIGPIPE to the caller (`send_sig(SIGPIPE, current, 0)`).
/// Its default action ends the program; ignored it is dropped and the write answers EPIPE; with a handler it
/// stays pending until AB-6 delivers it.
fn sigpipe() {
    proc::signal_current_thread(uapi::ab::signal::SIGPIPE);
}

fn current_fs_cred() -> Option<vfs::Cred> {
    proc::current().map(|p| p.fs_cred())
}

fn run(c: &mut SysCtx<'_>, f: SysFn) -> i64 {
    // B1/B4: a writer held back by a frozen file system gives up with EINTR once its program is killed, and
    // (AB-6) with a handled signal, which the syscall exit then restarts per SA_RESTART.
    vfs::dentry::set_interrupt_check(killed);
    vfs::dentry::set_signal_check(signal_pending);
    vfs::pipe::set_sigpipe(sigpipe);
    vfs::inode::set_group_check(proc::cred::current_in_group);
    vfs::inode::set_current_cred(current_fs_cred);
    let p = c.proc;
    let mem = ProcMem(p);
    let s = Sys { mem: &mem, fds: &p.fds, ns: &p.ns, cred: p.fs_cred(), subject: p.subject() };
    let a: Args = c.args;
    f(&s, a)
}

/// One handler per call. Numbers `uapi::nr` names go through `syscall!`; the few it does not name yet
/// (change request: add them to `uapi::nr`) are registered into the same registry with their number.
macro_rules! fs_call {
    ($NR:ident, $f:path) => {
        const _: () = {
            fn h(c: &mut SysCtx<'_>) -> i64 {
                run(c, $f)
            }
            syscall::syscall!($NR, h);
        };
    };
    ($nr:literal, $name:literal, $f:path) => {
        const _: () = {
            fn h(c: &mut SysCtx<'_>) -> i64 {
                run(c, $f)
            }
            syscall::__kreg::register!("syscall", syscall::SyscallEntry, syscall::SyscallEntry { nr: $nr, name: $name, handler: h });
        };
    };
}

use vfs::sys as v;

/// NW-5: a socket fd's read/write/readv/writev are one recvmsg/sendmsg each (a datagram stays one datagram;
/// the generic path splits writes into 64 KiB pieces, fine only for byte streams).
fn on_socket(s: &Sys<'_>, fd: u64) -> bool {
    let fd = fd as u32 as i32;
    fd >= 0 && s.fds.get(fd as usize).is_ok_and(|f| net::sock::SockFile::of(&f.file).is_some())
}

fn read_any(s: &Sys<'_>, a: Args) -> i64 {
    if on_socket(s, a[0]) { net::sock::sys::read(s, a) } else { v::read(s, a) }
}

fn write_any(s: &Sys<'_>, a: Args) -> i64 {
    if on_socket(s, a[0]) { net::sock::sys::write(s, a) } else { v::write(s, a) }
}

fn readv_any(s: &Sys<'_>, a: Args) -> i64 {
    if on_socket(s, a[0]) { net::sock::sys::readv(s, a) } else { v::readv(s, a) }
}

fn writev_any(s: &Sys<'_>, a: Args) -> i64 {
    if on_socket(s, a[0]) { net::sock::sys::writev(s, a) } else { v::writev(s, a) }
}

fs_call!(READ, read_any);
fs_call!(WRITE, write_any);
fs_call!(OPEN, v::open);
// AB-5 (integration patch): close, dup2, dup3 and fcntl are AB's `syscall::fcntl::{sys_close, sys_dup2, sys_dup3,
// sys_fcntl}` (FS's calls plus the POSIX lock release of `filp_close` and AB's fcntl commands).
fs_call!(STAT, v::stat);
fs_call!(FSTAT, v::fstat);
fs_call!(LSTAT, v::lstat);
fs_call!(LSEEK, v::lseek);
fs_call!(PREAD64, v::pread64);
fs_call!(PWRITE64, v::pwrite64);
fs_call!(READV, readv_any);
fs_call!(WRITEV, writev_any);
fs_call!(ACCESS, v::access);
fs_call!(DUP, v::dup);
fs_call!(FSYNC, v::fsync);
fs_call!(FDATASYNC, v::fsync);
fs_call!(TRUNCATE, v::truncate);
fs_call!(FTRUNCATE, v::ftruncate);
fs_call!(GETCWD, v::getcwd);
fs_call!(CHDIR, v::chdir);
fs_call!(81, "fchdir", v::fchdir);
fs_call!(RENAME, v::rename);
fs_call!(MKDIR, v::mkdir);
fs_call!(RMDIR, v::rmdir);
fs_call!(85, "creat", v::creat);
fs_call!(86, "link", v::link);
fs_call!(UNLINK, v::unlink);
fs_call!(SYMLINK, v::symlink);
fs_call!(READLINK, v::readlink);
fs_call!(UMASK, v::umask);
fs_call!(GETDENTS64, v::getdents64);
fs_call!(OPENAT, v::openat);
fs_call!(MKDIRAT, v::mkdirat);
fs_call!(NEWFSTATAT, v::newfstatat);
fs_call!(UNLINKAT, v::unlinkat);
fs_call!(RENAMEAT, v::renameat);
fs_call!(265, "linkat", v::linkat);
fs_call!(266, "symlinkat", v::symlinkat);
fs_call!(READLINKAT, v::readlinkat);
fs_call!(FACCESSAT, v::faccessat);
fs_call!(RENAMEAT2, v::renameat2);
fs_call!(STATX, v::statx);
fs_call!(FACCESSAT2, v::faccessat2);
// FS-3 (the brief grants these entry points; they live with the other file calls here, see the notes)
fs_call!(PIPE, v::pipe);
fs_call!(PIPE2, v::pipe2);
fs_call!(133, "mknod", v::mknod);
fs_call!(259, "mknodat", v::mknodat);
fs_call!(284, "eventfd", v::eventfd);
fs_call!(285, "fallocate", v::fallocate);
fs_call!(EVENTFD2, v::eventfd2);
// FS-4 (the brief grants these entry points; AB-3's minimal poll/ppoll in `crates/syscall` are replaced)
use vfs::pollsys as ps;
fs_call!(POLL, ps::poll);
fs_call!(PPOLL, ps::ppoll);
fs_call!(23, "select", ps::select);
fs_call!(270, "pselect6", ps::pselect6);
fs_call!(213, "epoll_create", ps::epoll_create);
fs_call!(EPOLL_CREATE1, ps::epoll_create1);
fs_call!(EPOLL_CTL, ps::epoll_ctl);
fs_call!(EPOLL_WAIT, ps::epoll_wait);
fs_call!(EPOLL_PWAIT, ps::epoll_pwait);
fs_call!(441, "epoll_pwait2", ps::epoll_pwait2);
// FS-10 (the brief grants this entry point; the seal commands go through fcntl above)
fs_call!(MEMFD_CREATE, vfs::memfd::memfd_create);
// FS-14 (the brief grants these entry points): inotify.
use vfs::inotify::sys as vi;
fs_call!(253, "inotify_init", vi::inotify_init);
fs_call!(254, "inotify_add_watch", vi::inotify_add_watch);
fs_call!(255, "inotify_rm_watch", vi::inotify_rm_watch);
fs_call!(INOTIFY_INIT1, vi::inotify_init1);
// FS-1b (the brief grants these entry points): attributes, statfs, sync, vectored and copying I/O.
use vfs::sys_attr as va;
use vfs::sys_copy as vc;
fs_call!(CHMOD, va::chmod);
fs_call!(FCHMOD, va::fchmod);
fs_call!(92, "chown", va::chown);
fs_call!(93, "fchown", va::fchown);
fs_call!(94, "lchown", va::lchown);
fs_call!(132, "utime", va::utime);
fs_call!(STATFS, va::statfs);
fs_call!(FSTATFS, va::fstatfs);
fs_call!(162, "sync", va::sync);
fs_call!(235, "utimes", va::utimes);
fs_call!(260, "fchownat", va::fchownat);
fs_call!(261, "futimesat", va::futimesat);
fs_call!(268, "fchmodat", va::fchmodat);
fs_call!(UTIMENSAT, va::utimensat);
fs_call!(306, "syncfs", va::syncfs);
fs_call!(452, "fchmodat2", va::fchmodat2);
fs_call!(40, "sendfile", vc::sendfile);
fs_call!(221, "fadvise64", vc::fadvise64);
fs_call!(295, "preadv", vc::preadv);
fs_call!(296, "pwritev", vc::pwritev);
fs_call!(326, "copy_file_range", vc::copy_file_range);
fs_call!(327, "preadv2", preadv2_any);
fs_call!(328, "pwritev2", pwritev2_any);

/// preadv2/pwritev2 at the current position (offset -1) on a socket are its readv/writev (one datagram stays
/// one); flags other than 6.6's RWF_SUPPORTED are EOPNOTSUPP, the known ones change nothing for a socket here.
fn rwf_ok(a: &Args) -> bool {
    a[5] as u32 as u64 & !0x1f == 0
}

fn preadv2_any(s: &Sys<'_>, a: Args) -> i64 {
    if a[3] as i64 == -1 && on_socket(s, a[0]) {
        return if rwf_ok(&a) { net::sock::sys::readv(s, a) } else { -EOPNOTSUPP };
    }
    vc::preadv2(s, a)
}

fn pwritev2_any(s: &Sys<'_>, a: Args) -> i64 {
    if a[3] as i64 == -1 && on_socket(s, a[0]) {
        return if rwf_ok(&a) { net::sock::sys::writev(s, a) } else { -EOPNOTSUPP };
    }
    vc::pwritev2(s, a)
}

/// FS-5: the background write-back of files whose page cache has a backend (none before FS-6: then it only
/// sleeps). Class Background (B4, FS-12); it never waits on a program, each pass is bounded by the backends.
const WRITEBACK_PERIOD_NS: u64 = 5_000_000_000;

fn writeback_loop() {
    loop {
        sched::sleep_ns(WRITEBACK_PERIOD_NS);
        if vfs::writeback::registered() > 0 {
            let p = vfs::writeback::flush_all(vfs::pagecache::SyncKind::Background);
            if p.failed > 0 {
                klog::event!(Warn, "vfs", "writeback.pass", flushed = p.flushed, failed = p.failed);
            }
        }
    }
}

fn start_writeback() -> Result<(), &'static str> {
    match sched::spawn("vfs.writeback", sched::Class::Background, writeback_loop) {
        Ok(_) => {
            klog::event!(Info, "vfs", "writeback.started", period_ms = WRITEBACK_PERIOD_NS / 1_000_000);
            Ok(())
        }
        Err(_) => {
            klog::event!(Warn, "vfs", "writeback.no_thread"; "dirty pages are written back by fsync/msync only");
            Err("vfs.writeback: no thread")
        }
    }
}
kreg::initcall!(kreg::Level::Service, "vfs.writeback", start_writeback);

/// FS-12: the read-ahead thread (`vfs::readahead`). Background, so its disk requests never go before a reader's
/// own (`block::prio::current`). Without it read-ahead is off and readers read every page themselves.
fn start_readahead() -> Result<(), &'static str> {
    match sched::spawn("vfs.readahead", sched::Class::Background, || vfs::readahead::reader_loop()) {
        Ok(_) => Ok(()),
        Err(_) => {
            klog::event!(Warn, "vfs", "readahead.no_thread"; "read-ahead off");
            Err("vfs.readahead: no thread")
        }
    }
}
kreg::initcall!(kreg::Level::Service, "vfs.readahead", start_readahead);

/// FS-6: the block layer's write rules need the CONFIG switches (`stick_write`, `usb_msd_write`,
/// `nvme_write`) and the boot stick's identity (BootInfo `Stick`: the first 96 bytes of the boot volume's
/// block 0). The kernel hands BootInfo to kexec before the init levels; this runs at Subsys, before PCI binds
/// the xHCI and usb-msd registers any disk. Without it the block layer keeps every switch off and treats every
/// USB disk as the stick (the safe side).
fn block_boot_facts() -> Result<(), &'static str> {
    let stick = kexec::boot_info().and_then(|(bi, _)| bi.stick()).map(|s| block::StickIdentity {
        block0: (s.flags & bootinfo::stick_flags::BLOCK0_VALID != 0).then_some(s.block0),
        usb: (s.flags & bootinfo::stick_flags::USB_VALID != 0).then_some((s.usb_vid, s.usb_pid)),
        // FS-7: only `\BITSOS2\STICK.ID` counts (V1's legacy file means the stick was not prepared for V2).
        id: (s.flags & bootinfo::stick_flags::STICK_ID_VALID != 0 && s.flags & bootinfo::stick_flags::STICK_ID_LEGACY == 0).then_some(s.stick_id),
    });
    block::configure(kexec::boot::config, stick);
    Ok(())
}
kreg::initcall!(kreg::Level::Subsys, "block.boot_facts", block_boot_facts);

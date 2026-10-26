//! AB's constants (signals, clone flags, ...). Lane file.

pub mod clone {
    pub const CLONE_VM: u64 = 0x100;
    pub const CLONE_FS: u64 = 0x200;
    pub const CLONE_FILES: u64 = 0x400;
    pub const CLONE_SIGHAND: u64 = 0x800;
    pub const CLONE_VFORK: u64 = 0x4000;
    pub const CLONE_THREAD: u64 = 0x10000;
    pub const CLONE_SYSVSEM: u64 = 0x40000;
    pub const CLONE_SETTLS: u64 = 0x80000;
    pub const CLONE_PARENT_SETTID: u64 = 0x100000;
    pub const CLONE_CHILD_CLEARTID: u64 = 0x200000;
    pub const CLONE_CHILD_SETTID: u64 = 0x1000000;
    /// The exit signal in clone's low byte (clone3 has its own field).
    pub const CSIGNAL: u64 = 0xff;
    pub const CLONE_NEWTIME: u64 = 0x80;
    pub const CLONE_PIDFD: u64 = 0x1000;
    pub const CLONE_PTRACE: u64 = 0x2000;
    pub const CLONE_PARENT: u64 = 0x8000;
    pub const CLONE_NEWNS: u64 = 0x20000;
    pub const CLONE_DETACHED: u64 = 0x400000;
    pub const CLONE_UNTRACED: u64 = 0x800000;
    pub const CLONE_NEWCGROUP: u64 = 0x200_0000;
    pub const CLONE_NEWUTS: u64 = 0x400_0000;
    pub const CLONE_NEWIPC: u64 = 0x800_0000;
    pub const CLONE_NEWUSER: u64 = 0x1000_0000;
    pub const CLONE_NEWPID: u64 = 0x2000_0000;
    pub const CLONE_NEWNET: u64 = 0x4000_0000;
    pub const CLONE_IO: u64 = 0x8000_0000;
    /// clone3 only.
    pub const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;
    pub const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;
    /// Every namespace flag (V2 has no namespaces yet: EINVAL, as a kernel built without them).
    pub const CLONE_NAMESPACES: u64 = CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWTIME;
    /// Linux `CLONE_LEGACY_FLAGS`: the 32 bits clone(2) can carry.
    pub const CLONE_LEGACY_FLAGS: u64 = 0xffff_ffff;
    /// `struct clone_args` sizes (include/uapi/linux/sched.h).
    pub const CLONE_ARGS_SIZE_VER0: u64 = 64;
    pub const CLONE_ARGS_SIZE_VER1: u64 = 80;
    pub const CLONE_ARGS_SIZE_VER2: u64 = 88;
    /// `MAX_PID_NS_LEVEL`: the most set_tid entries clone3 takes.
    pub const MAX_PID_NS_LEVEL: u64 = 32;
}

pub mod signal {
    pub const SIGHUP: u32 = 1;
    pub const SIGINT: u32 = 2;
    pub const SIGQUIT: u32 = 3;
    pub const SIGILL: u32 = 4;
    pub const SIGTRAP: u32 = 5;
    pub const SIGABRT: u32 = 6;
    pub const SIGBUS: u32 = 7;
    pub const SIGFPE: u32 = 8;
    pub const SIGKILL: u32 = 9;
    pub const SIGSEGV: u32 = 11;
    pub const SIGPIPE: u32 = 13;
    pub const SIGTERM: u32 = 15;
    pub const SIGCHLD: u32 = 17;
    pub const SIGCONT: u32 = 18;
    pub const SIGSTOP: u32 = 19;
    /// AB-6: the rest of the standard signals and the first real-time one.
    pub const SIGUSR1: u32 = 10;
    pub const SIGUSR2: u32 = 12;
    pub const SIGALRM: u32 = 14;
    pub const SIGSTKFLT: u32 = 16;
    pub const SIGTSTP: u32 = 20;
    pub const SIGTTIN: u32 = 21;
    pub const SIGTTOU: u32 = 22;
    pub const SIGURG: u32 = 23;
    pub const SIGXCPU: u32 = 24;
    pub const SIGXFSZ: u32 = 25;
    pub const SIGVTALRM: u32 = 26;
    pub const SIGPROF: u32 = 27;
    pub const SIGWINCH: u32 = 28;
    pub const SIGIO: u32 = 29;
    pub const SIGPWR: u32 = 30;
    pub const SIGSYS: u32 = 31;
    /// The kernel's first real-time signal (`SIGRTMIN`, 32; libcs keep 32..34 for themselves).
    pub const SIGRTMIN: u32 = 32;
}

/// AB-6: `si_code` values (include/uapi/asm-generic/siginfo.h) and the restart codes the syscall exit turns
/// into a restart or EINTR (include/linux/errno.h; never seen by programs).
pub mod siginfo {
    pub const SI_USER: i32 = 0;
    pub const SI_KERNEL: i32 = 0x80;
    pub const SI_QUEUE: i32 = -1;
    pub const SI_TIMER: i32 = -2;
    pub const SI_MESGQ: i32 = -3;
    pub const SI_ASYNCIO: i32 = -4;
    pub const SI_SIGIO: i32 = -5;
    pub const SI_TKILL: i32 = -6;
    pub const SI_DETHREAD: i32 = -7;
    pub const SI_ASYNCNL: i32 = -60;
    pub const ILL_ILLOPN: i32 = 2;
    pub const FPE_INTDIV: i32 = 1;
    pub const FPE_FLTDIV: i32 = 3;
    pub const FPE_FLTOVF: i32 = 4;
    pub const FPE_FLTUND: i32 = 5;
    pub const FPE_FLTRES: i32 = 6;
    pub const FPE_FLTINV: i32 = 7;
    pub const SEGV_MAPERR: i32 = 1;
    pub const SEGV_ACCERR: i32 = 2;
    pub const SEGV_CPERR: i32 = 10;
    pub const BUS_ADRALN: i32 = 1;
    pub const BUS_ADRERR: i32 = 2;
    pub const TRAP_BRKPT: i32 = 1;
    pub const TRAP_TRACE: i32 = 2;
    pub const CLD_EXITED: i32 = 1;
    pub const CLD_KILLED: i32 = 2;
    pub const CLD_DUMPED: i32 = 3;
    pub const CLD_TRAPPED: i32 = 4;
    pub const CLD_STOPPED: i32 = 5;
    pub const CLD_CONTINUED: i32 = 6;
    /// The highest code per signal-specific layout (`NSIGILL` ...; `sig_sicodes` in kernel/signal.c).
    pub const NSIGILL: i32 = 11;
    pub const NSIGFPE: i32 = 15;
    pub const NSIGSEGV: i32 = 10;
    pub const NSIGBUS: i32 = 5;
    pub const NSIGTRAP: i32 = 6;
    pub const NSIGCHLD: i32 = 6;
    pub const NSIGPOLL: i32 = 6;
    pub const NSIGSYS: i32 = 2;
    /// Kernel-internal restart answers (linux-6.6/include/linux/errno.h:13).
    pub const ERESTARTSYS: i64 = 512;
    pub const ERESTARTNOINTR: i64 = 513;
    pub const ERESTARTNOHAND: i64 = 514;
    pub const ERESTART_RESTARTBLOCK: i64 = 516;
    /// `sizeof(struct signalfd_siginfo)`.
    pub const SIGNALFD_SIGINFO_SIZE: usize = 128;
    /// signalfd4's flags (O_CLOEXEC, O_NONBLOCK).
    pub const SFD_CLOEXEC: u64 = 0o2000000;
    pub const SFD_NONBLOCK: u64 = 0o4000;
}

/// `wait4`/`waitpid` options and status encoding (Linux `include/uapi/linux/wait.h`).
pub mod wait {
    pub const WNOHANG: u64 = 0x1;
    pub const WUNTRACED: u64 = 0x2;
    /// waitid only: wait4 refuses it with EINVAL.
    pub const WEXITED: u64 = 0x4;
    pub const WCONTINUED: u64 = 0x8;
    pub const WNOTHREAD: u64 = 0x2000_0000;
    pub const WALL: u64 = 0x4000_0000;
    pub const WCLONE: u64 = 0x8000_0000;
    /// Every bit `wait4` accepts (`kernel_wait4`).
    pub const WAIT4_VALID: u64 = WNOHANG | WUNTRACED | WCONTINUED | WNOTHREAD | WALL | WCLONE;
    /// waitid's name for WUNTRACED.
    pub const WSTOPPED: u64 = 0x2;
    pub const WNOWAIT: u64 = 0x0100_0000;
    /// Every bit `waitid` accepts (`kernel_waitid`).
    pub const WAITID_VALID: u64 = WNOHANG | WNOWAIT | WEXITED | WSTOPPED | WCONTINUED | WNOTHREAD | WCLONE | WALL;
    /// waitid's id types.
    pub const P_ALL: u32 = 0;
    pub const P_PID: u32 = 1;
    pub const P_PGID: u32 = 2;
    pub const P_PIDFD: u32 = 3;
    /// `si_code` of a SIGCHLD siginfo.
    pub const CLD_EXITED: i32 = 1;
    pub const CLD_KILLED: i32 = 2;
    /// AB-6: job control.
    pub const CLD_STOPPED: i32 = 5;
    pub const CLD_CONTINUED: i32 = 6;
    /// `sizeof(siginfo_t)`.
    pub const SIGINFO_SIZE: usize = 128;
    /// Size of `struct rusage` on x86-64 (wait4's fourth argument).
    pub const RUSAGE_SIZE: usize = 144;
}

/// Linux's limits on exec arguments (`include/uapi/linux/binfmts.h`, `fs/exec.c`).
pub mod exec {
    /// One argument or environment string, NUL included (`MAX_ARG_STRLEN`, 32 pages).
    pub const MAX_ARG_STRLEN: usize = 32 * 4096;
    /// Linux's `RLIMIT_STACK` default (8 MiB): the main thread's stack; argv + envp may use a quarter of it.
    pub const STACK_RLIMIT: u64 = 8 << 20;
    /// `ARG_MAX` (32 pages): the argument space is never below it.
    pub const ARG_MAX: u64 = 32 * 4096;
    /// Three quarters of `_STK_LIM` (8 MiB): the argument space is never above it (`bprm_stack_limits`).
    pub const STK_LIM_3_4: u64 = (8 << 20) / 4 * 3;
    /// `bprm_stack_limits` (linux-6.6/fs/exec.c:487): the bytes argv + envp pointers and strings may take for a
    /// soft RLIMIT_STACK of `rlim`.
    pub const fn arg_limit(rlim: u64) -> u64 {
        let l = if STK_LIM_3_4 < rlim / 4 { STK_LIM_3_4 } else { rlim / 4 };
        if l > ARG_MAX { l } else { ARG_MAX }
    }
}

/// Auxiliary vector entries the `uapi::auxv` module does not name yet (Linux `include/uapi/linux/auxvec.h`).
pub mod auxv_more {
    pub const AT_IGNORE: u64 = 1;
    pub const AT_EXECFD: u64 = 2;
    pub const AT_NOTELF: u64 = 10;
    pub const AT_BASE_PLATFORM: u64 = 24;
    pub const AT_RSEQ_FEATURE_SIZE: u64 = 27;
    pub const AT_RSEQ_ALIGN: u64 = 28;
    pub const AT_HWCAP3: u64 = 29;
    pub const AT_HWCAP4: u64 = 30;
}

/// Signal ABI values (linux-6.6/arch/x86/include/uapi/asm/signal.h, include/uapi/asm-generic/signal-defs.h,
/// include/linux/signal_types.h `UAPI_SA_FLAGS`, include/uapi/linux/signal.h).
pub mod sigabi {
    /// Signals 1..=64 (`_NSIG`); a sigset is one u64 on x86-64.
    pub const NSIG: u32 = 64;
    pub const SIGSET_SIZE: u64 = 8;
    pub const SIG_DFL: u64 = 0;
    pub const SIG_IGN: u64 = 1;
    pub const SA_NOCLDSTOP: u64 = 0x1;
    pub const SA_NOCLDWAIT: u64 = 0x2;
    pub const SA_SIGINFO: u64 = 0x4;
    pub const SA_EXPOSE_TAGBITS: u64 = 0x800;
    pub const SA_RESTORER: u64 = 0x0400_0000;
    pub const SA_ONSTACK: u64 = 0x0800_0000;
    pub const SA_RESTART: u64 = 0x1000_0000;
    pub const SA_NODEFER: u64 = 0x4000_0000;
    pub const SA_RESETHAND: u64 = 0x8000_0000;
    /// The flag bits Linux keeps (and reports); others are cleared so programs can detect missing support.
    pub const UAPI_SA_FLAGS: u64 =
        SA_NOCLDSTOP | SA_NOCLDWAIT | SA_SIGINFO | SA_ONSTACK | SA_RESTART | SA_NODEFER | SA_RESETHAND | SA_EXPOSE_TAGBITS | SA_RESTORER;
    pub const SIG_BLOCK: u64 = 0;
    pub const SIG_UNBLOCK: u64 = 1;
    pub const SIG_SETMASK: u64 = 2;
    pub const SS_ONSTACK: u32 = 1;
    pub const SS_DISABLE: u32 = 2;
    pub const SS_AUTODISARM: u32 = 1 << 31;
    pub const SS_FLAG_BITS: u32 = SS_AUTODISARM;
    pub const MINSIGSTKSZ: u64 = 2048;
}

/// Resource limits (linux-6.6/include/uapi/asm-generic/resource.h, include/asm-generic/resource.h).
pub mod rlimit {
    pub const RLIMIT_CPU: u32 = 0;
    pub const RLIMIT_FSIZE: u32 = 1;
    pub const RLIMIT_DATA: u32 = 2;
    pub const RLIMIT_STACK: u32 = 3;
    pub const RLIMIT_CORE: u32 = 4;
    pub const RLIMIT_RSS: u32 = 5;
    pub const RLIMIT_NPROC: u32 = 6;
    pub const RLIMIT_NOFILE: u32 = 7;
    pub const RLIMIT_MEMLOCK: u32 = 8;
    pub const RLIMIT_AS: u32 = 9;
    pub const RLIMIT_LOCKS: u32 = 10;
    pub const RLIMIT_SIGPENDING: u32 = 11;
    pub const RLIMIT_MSGQUEUE: u32 = 12;
    pub const RLIMIT_NICE: u32 = 13;
    pub const RLIMIT_RTPRIO: u32 = 14;
    pub const RLIMIT_RTTIME: u32 = 15;
    /// The number of resources (`RLIM_NLIMITS`): an ABI constant, not a count of things.
    pub const RLIM_NLIMITS: u32 = 16;
    pub const RLIM_INFINITY: u64 = u64::MAX;
    /// `INR_OPEN_CUR` / `INR_OPEN_MAX` (include/uapi/linux/fs.h).
    pub const INR_OPEN_CUR: u64 = 1024;
    pub const INR_OPEN_MAX: u64 = 4096;
    /// `MLOCK_LIMIT`, `MQ_BYTES_MAX`.
    pub const MLOCK_LIMIT: u64 = 8 << 20;
    pub const MQ_BYTES_MAX: u64 = 819_200;
}

/// Clocks and timer flags (linux-6.6/include/uapi/linux/time.h).
pub mod clock {
    pub const CLOCK_REALTIME: i32 = 0;
    pub const CLOCK_MONOTONIC: i32 = 1;
    pub const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
    pub const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
    pub const CLOCK_MONOTONIC_RAW: i32 = 4;
    pub const CLOCK_REALTIME_COARSE: i32 = 5;
    pub const CLOCK_MONOTONIC_COARSE: i32 = 6;
    pub const CLOCK_BOOTTIME: i32 = 7;
    pub const CLOCK_REALTIME_ALARM: i32 = 8;
    pub const CLOCK_BOOTTIME_ALARM: i32 = 9;
    pub const CLOCK_TAI: i32 = 11;
    pub const TIMER_ABSTIME: u64 = 1;
}

/// poll(2) bits (linux-6.6/include/uapi/asm-generic/poll.h).
pub mod poll {
    pub const POLLIN: u16 = 0x001;
    pub const POLLPRI: u16 = 0x002;
    pub const POLLOUT: u16 = 0x004;
    pub const POLLERR: u16 = 0x008;
    pub const POLLHUP: u16 = 0x010;
    pub const POLLNVAL: u16 = 0x020;
    pub const POLLRDNORM: u16 = 0x040;
    pub const POLLRDBAND: u16 = 0x080;
    pub const POLLWRNORM: u16 = 0x100;
    pub const POLLWRBAND: u16 = 0x200;
    pub const POLLRDHUP: u16 = 0x2000;
}

/// getrandom(2) flags (include/uapi/linux/random.h).
pub mod random {
    pub const GRND_NONBLOCK: u64 = 1;
    pub const GRND_RANDOM: u64 = 2;
    pub const GRND_INSECURE: u64 = 4;
}

/// The generic ioctls of `do_vfs_ioctl` (fs/ioctl.c) and termios sizes (arch x86: asm-generic/termbits.h).
pub mod ioctl_more {
    pub const FIONBIO: u64 = 0x5421;
    pub const FIONCLEX: u64 = 0x5450;
    pub const FIOCLEX: u64 = 0x5451;
    pub const FIOASYNC: u64 = 0x5452;
    /// The kernel's `struct termios` (TCGETS): 4 flags words, c_line, 19 control characters.
    pub const TERMIOS_SIZE: usize = 36;
    pub const WINSIZE_SIZE: usize = 8;
}

/// Syscall numbers `uapi::nr` (desk) does not name yet; AB registers them by number.
pub mod nr_more {
    pub const RT_SIGPENDING: usize = 127;
    pub const GETRESUID: usize = 118;
    pub const GETRESGID: usize = 120;
    pub const GETGROUPS: usize = 115;
    /// AB-13: credentials (linux-6.6/arch/x86/entry/syscalls/syscall_64.tbl).
    pub const SETREUID: usize = 113;
    pub const SETREGID: usize = 114;
    pub const SETGROUPS: usize = 116;
    pub const SETRESUID: usize = 117;
    pub const SETRESGID: usize = 119;
    pub const SETFSUID: usize = 122;
    pub const SETFSGID: usize = 123;
    pub const CAPGET: usize = 125;
    pub const CAPSET: usize = 126;
    /// AB-7b: the host and domain name (also /proc/sys/kernel/{hostname,domainname}).
    pub const SETHOSTNAME: usize = 170;
    pub const SETDOMAINNAME: usize = 171;
    pub const TIME: usize = 201;
    pub const GETSID: usize = 124;
    pub const WAITID: usize = 247;
    pub const EXECVEAT: usize = 322;
    /// AB-5.
    pub const FLOCK: usize = 73;
    pub const SYSINFO: usize = 99;
    pub const PRCTL: usize = 157;
    pub const TIMERFD_CREATE: usize = 283;
    pub const TIMERFD_SETTIME: usize = 286;
    pub const TIMERFD_GETTIME: usize = 287;
    /// AB-5 fixer.
    pub const CLOSE_RANGE: usize = 436;
    /// AB-6: the signal calls not named in `uapi::nr`.
    pub const PAUSE: usize = 34;
    pub const RT_SIGTIMEDWAIT: usize = 128;
    pub const RT_SIGQUEUEINFO: usize = 129;
    pub const RT_SIGSUSPEND: usize = 130;
    pub const RESTART_SYSCALL: usize = 219;
    pub const SIGNALFD: usize = 282;
    pub const SIGNALFD4: usize = 289;
    pub const RT_TGSIGQUEUEINFO: usize = 297;
    /// AB-6 fixer: interval timers and POSIX timers (the senders of SIGALRM/SIGVTALRM/SIGPROF/SI_TIMER).
    pub const GETITIMER: usize = 36;
    pub const ALARM: usize = 37;
    pub const SETITIMER: usize = 38;
    pub const TIMER_CREATE: usize = 222;
    pub const TIMER_SETTIME: usize = 223;
    pub const TIMER_GETTIME: usize = 224;
    pub const TIMER_GETOVERRUN: usize = 225;
    pub const TIMER_DELETE: usize = 226;
    /// AB-12: process handles (pidfds).
    pub const PIDFD_SEND_SIGNAL: usize = 424;
    pub const PIDFD_OPEN: usize = 434;
    pub const PIDFD_GETFD: usize = 438;
}

/// AB-12: pidfd flags (linux-6.6/include/uapi/linux/pidfd.h:10).
pub mod pidfd {
    /// `PIDFD_NONBLOCK` = O_NONBLOCK: pidfd_open's only flag; waitid on such a pidfd does not wait (EAGAIN).
    pub const PIDFD_NONBLOCK: u64 = 0o4000;
}

/// Interval and POSIX timers (linux-6.6/include/uapi/linux/time.h, asm-generic/siginfo.h `sigevent`).
pub mod timers {
    pub const ITIMER_REAL: i32 = 0;
    pub const ITIMER_VIRTUAL: i32 = 1;
    pub const ITIMER_PROF: i32 = 2;
    pub const SIGEV_SIGNAL: i32 = 0;
    pub const SIGEV_NONE: i32 = 1;
    pub const SIGEV_THREAD: i32 = 2;
    pub const SIGEV_THREAD_ID: i32 = 4;
    /// `struct sigevent`: sigev_value 0 (8 bytes), sigev_signo 8, sigev_notify 12, the union from 16
    /// (`sigev_notify_thread_id` at 16); 64 bytes in all.
    pub const SIGEVENT_SIZE: usize = 64;
    /// `struct __kernel_old_itimerval`: it_interval then it_value, each a timeval (sec, usec).
    pub const ITIMERVAL_SIZE: usize = 32;
}

/// Short names of the signals, for messages ("SIGSEGV").
pub mod signal_names {
    pub fn name(sig: u32) -> &'static str {
        match sig {
            1 => "SIGHUP",
            2 => "SIGINT",
            3 => "SIGQUIT",
            4 => "SIGILL",
            5 => "SIGTRAP",
            6 => "SIGABRT",
            7 => "SIGBUS",
            8 => "SIGFPE",
            9 => "SIGKILL",
            10 => "SIGUSR1",
            11 => "SIGSEGV",
            12 => "SIGUSR2",
            13 => "SIGPIPE",
            14 => "SIGALRM",
            15 => "SIGTERM",
            16 => "SIGSTKFLT",
            17 => "SIGCHLD",
            18 => "SIGCONT",
            19 => "SIGSTOP",
            20 => "SIGTSTP",
            21 => "SIGTTIN",
            22 => "SIGTTOU",
            23 => "SIGURG",
            24 => "SIGXCPU",
            25 => "SIGXFSZ",
            26 => "SIGVTALRM",
            27 => "SIGPROF",
            28 => "SIGWINCH",
            29 => "SIGIO",
            30 => "SIGPWR",
            31 => "SIGSYS",
            _ => "SIG?",
        }
    }
}

/// AB-5: errno values `uapi::errno` does not name yet.
pub mod errno_more {
    pub const ENOLCK: i64 = 37;
    /// A PT_INTERP file that is not an x86-64 ELF (`load_elf_binary`).
    pub const ELIBBAD: i64 = 80;
    pub const ECANCELED: i64 = 125;
}

/// AB-5: timerfd (include/uapi/linux/timerfd.h).
pub mod timerfd {
    pub const TFD_TIMER_ABSTIME: u64 = 1;
    pub const TFD_TIMER_CANCEL_ON_SET: u64 = 2;
    pub const TFD_SETTIME_FLAGS: u64 = TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET;
    pub const TFD_CLOEXEC: u64 = 0o2000000;
    pub const TFD_NONBLOCK: u64 = 0o4000;
    pub const TFD_CREATE_FLAGS: u64 = TFD_CLOEXEC | TFD_NONBLOCK;
    /// `_IOW('T', 0, u64)` (CONFIG_CHECKPOINT_RESTORE).
    pub const TFD_IOC_SET_TICKS: u64 = 0x4008_5400;
}

/// AB-5: fcntl commands and flock(2) (include/uapi/asm-generic/fcntl.h, linux/fcntl.h).
pub mod fcntl {
    pub const F_DUPFD: u64 = 0;
    pub const F_GETFD: u64 = 1;
    pub const F_SETFD: u64 = 2;
    pub const F_GETFL: u64 = 3;
    pub const F_SETFL: u64 = 4;
    pub const F_GETLK: u64 = 5;
    pub const F_SETLK: u64 = 6;
    pub const F_SETLKW: u64 = 7;
    pub const F_SETOWN: u64 = 8;
    pub const F_GETOWN: u64 = 9;
    pub const F_SETSIG: u64 = 10;
    pub const F_GETSIG: u64 = 11;
    pub const F_SETOWN_EX: u64 = 15;
    pub const F_GETOWN_EX: u64 = 16;
    pub const F_GETOWNER_UIDS: u64 = 17;
    pub const F_OFD_GETLK: u64 = 36;
    pub const F_OFD_SETLK: u64 = 37;
    pub const F_OFD_SETLKW: u64 = 38;
    pub const F_SETLEASE: u64 = 1024;
    pub const F_GETLEASE: u64 = 1025;
    pub const F_NOTIFY: u64 = 1026;
    pub const F_DUPFD_QUERY: u64 = 1027;
    pub const F_CANCELLK: u64 = 1029;
    pub const F_DUPFD_CLOEXEC: u64 = 1030;
    pub const F_SETPIPE_SZ: u64 = 1031;
    pub const F_GETPIPE_SZ: u64 = 1032;
    pub const F_ADD_SEALS: u64 = 1033;
    pub const F_GET_SEALS: u64 = 1034;
    pub const F_GET_RW_HINT: u64 = 1035;
    pub const F_SET_RW_HINT: u64 = 1036;
    pub const F_GET_FILE_RW_HINT: u64 = 1037;
    pub const F_SET_FILE_RW_HINT: u64 = 1038;
    pub const F_OWNER_TID: i32 = 0;
    pub const F_OWNER_PID: i32 = 1;
    pub const F_OWNER_PGRP: i32 = 2;
    pub const FD_CLOEXEC: u64 = 1;
    pub const LOCK_SH: u64 = 1;
    pub const LOCK_EX: u64 = 2;
    pub const LOCK_NB: u64 = 4;
    pub const LOCK_UN: u64 = 8;
    pub const LOCK_MAND: u64 = 32;
    /// `RWH_WRITE_LIFE_EXTREME`: the last valid write-life hint.
    pub const RWH_WRITE_LIFE_EXTREME: u64 = 5;
    /// `struct flock` (x86-64): l_type, l_whence, l_start, l_len, l_pid.
    pub const FLOCK_SIZE: usize = 32;
}

/// AB-5: prctl options (include/uapi/linux/prctl.h).
pub mod prctl {
    pub const PR_SET_PDEATHSIG: i32 = 1;
    pub const PR_GET_PDEATHSIG: i32 = 2;
    pub const PR_GET_DUMPABLE: i32 = 3;
    pub const PR_SET_DUMPABLE: i32 = 4;
    pub const PR_GET_UNALIGN: i32 = 5;
    pub const PR_SET_UNALIGN: i32 = 6;
    pub const PR_GET_KEEPCAPS: i32 = 7;
    pub const PR_SET_KEEPCAPS: i32 = 8;
    pub const PR_GET_FPEMU: i32 = 9;
    pub const PR_SET_FPEMU: i32 = 10;
    pub const PR_GET_FPEXC: i32 = 11;
    pub const PR_SET_FPEXC: i32 = 12;
    pub const PR_GET_TIMING: i32 = 13;
    pub const PR_SET_TIMING: i32 = 14;
    pub const PR_SET_NAME: i32 = 15;
    pub const PR_GET_NAME: i32 = 16;
    pub const PR_GET_ENDIAN: i32 = 19;
    pub const PR_SET_ENDIAN: i32 = 20;
    pub const PR_GET_SECCOMP: i32 = 21;
    pub const PR_SET_SECCOMP: i32 = 22;
    pub const PR_CAPBSET_READ: i32 = 23;
    pub const PR_CAPBSET_DROP: i32 = 24;
    pub const PR_GET_TSC: i32 = 25;
    pub const PR_SET_TSC: i32 = 26;
    pub const PR_GET_SECUREBITS: i32 = 27;
    pub const PR_SET_SECUREBITS: i32 = 28;
    pub const PR_SET_TIMERSLACK: i32 = 29;
    pub const PR_GET_TIMERSLACK: i32 = 30;
    pub const PR_TASK_PERF_EVENTS_DISABLE: i32 = 31;
    pub const PR_TASK_PERF_EVENTS_ENABLE: i32 = 32;
    pub const PR_MCE_KILL: i32 = 33;
    pub const PR_MCE_KILL_GET: i32 = 34;
    pub const PR_SET_MM: i32 = 35;
    pub const PR_SET_CHILD_SUBREAPER: i32 = 36;
    pub const PR_GET_CHILD_SUBREAPER: i32 = 37;
    pub const PR_SET_NO_NEW_PRIVS: i32 = 38;
    pub const PR_GET_NO_NEW_PRIVS: i32 = 39;
    pub const PR_GET_TID_ADDRESS: i32 = 40;
    pub const PR_SET_THP_DISABLE: i32 = 41;
    pub const PR_GET_THP_DISABLE: i32 = 42;
    pub const PR_MPX_ENABLE_MANAGEMENT: i32 = 43;
    pub const PR_MPX_DISABLE_MANAGEMENT: i32 = 44;
    pub const PR_SET_FP_MODE: i32 = 45;
    pub const PR_GET_FP_MODE: i32 = 46;
    pub const PR_CAP_AMBIENT: i32 = 47;
    pub const PR_SVE_SET_VL: i32 = 50;
    pub const PR_SVE_GET_VL: i32 = 51;
    pub const PR_GET_SPECULATION_CTRL: i32 = 52;
    pub const PR_SET_SPECULATION_CTRL: i32 = 53;
    pub const PR_PAC_RESET_KEYS: i32 = 54;
    pub const PR_SET_TAGGED_ADDR_CTRL: i32 = 55;
    pub const PR_GET_TAGGED_ADDR_CTRL: i32 = 56;
    pub const PR_SET_IO_FLUSHER: i32 = 57;
    pub const PR_GET_IO_FLUSHER: i32 = 58;
    pub const PR_SET_SYSCALL_USER_DISPATCH: i32 = 59;
    pub const PR_PAC_SET_ENABLED_KEYS: i32 = 60;
    pub const PR_PAC_GET_ENABLED_KEYS: i32 = 61;
    pub const PR_SCHED_CORE: i32 = 62;
    pub const PR_SME_SET_VL: i32 = 63;
    pub const PR_SME_GET_VL: i32 = 64;
    pub const PR_SET_MDWE: i32 = 65;
    pub const PR_GET_MDWE: i32 = 66;
    pub const PR_SET_MEMORY_MERGE: i32 = 67;
    pub const PR_GET_MEMORY_MERGE: i32 = 68;
    pub const PR_RISCV_V_SET_CONTROL: i32 = 69;
    pub const PR_RISCV_V_GET_CONTROL: i32 = 70;
    pub const PR_SET_VMA: i32 = 0x5356_4d41;
    pub const PR_GET_AUXV: i32 = 0x4155_5856;
    pub const PR_SET_PTRACER: i32 = 0x5961_6d61;
    pub const PR_SET_MM_MAP: u64 = 14;
    pub const PR_SET_MM_MAP_SIZE: u64 = 15;
    pub const PR_MCE_KILL_CLEAR: u64 = 0;
    pub const PR_MCE_KILL_SET: u64 = 1;
    pub const PR_MCE_KILL_LATE: u64 = 0;
    pub const PR_MCE_KILL_EARLY: u64 = 1;
    pub const PR_MCE_KILL_DEFAULT: u64 = 2;
    pub const PR_TSC_ENABLE: i32 = 1;
    pub const PR_TSC_SIGSEGV: i32 = 2;
    pub const PR_CAP_AMBIENT_IS_SET: u64 = 1;
    pub const PR_CAP_AMBIENT_RAISE: u64 = 2;
    pub const PR_CAP_AMBIENT_LOWER: u64 = 3;
    pub const PR_CAP_AMBIENT_CLEAR_ALL: u64 = 4;
    pub const PR_SPEC_STORE_BYPASS: u64 = 0;
    pub const PR_SPEC_INDIRECT_BRANCH: u64 = 1;
    pub const PR_SPEC_L1D_FLUSH: u64 = 2;
    pub const PR_SYS_DISPATCH_OFF: u64 = 0;
    pub const PR_SYS_DISPATCH_ON: u64 = 1;
}

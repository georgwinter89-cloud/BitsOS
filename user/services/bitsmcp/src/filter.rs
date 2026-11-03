//! The seccomp program bitsmcp runs under once its setup is done (classic BPF, linux-6.6/kernel/seccomp.c's
//! rules, SX-1 on V2): x86-64 only (another arch or an x32 number ends the process), the calls a std program
//! needs to serve accepted sockets with threads, `clone` only for threads (CLONE_THREAD), `clone3` ENOSYS (libc
//! falls back), program start and tracing end the process, everything else EPERM: no open, no new socket, no
//! bind, no ioctl, no kill. Together with the gate's root-only descriptors opened before, the service cannot
//! reach the file system or other programs.

/// One classic BPF instruction (`struct sock_filter`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JGE_K: u16 = 0x35;
const JSET_K: u16 = 0x45;
const RET_K: u16 = 0x06;

pub const RET_KILL_PROCESS: u32 = 0x8000_0000;
pub const RET_ERRNO: u32 = 0x0005_0000;
pub const RET_ALLOW: u32 = 0x7fff_0000;
pub const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const X32_BIT: u32 = 0x4000_0000;
const CLONE_THREAD: u32 = 0x0001_0000;
const EPERM: u32 = 1;
const ENOSYS: u32 = 38;

/// `struct seccomp_data` offsets.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16;

pub const NR_CLONE: u32 = 56;
pub const NR_CLONE3: u32 = 435;

/// Calls the service makes after its setup (x86-64 numbers): memory, threads, time, signals of its own, the
/// accepted sockets and the gate's descriptors.
pub const ALLOWED: &[u32] = &[
    0,   // read
    1,   // write
    3,   // close
    5,   // fstat
    7,   // poll
    8,   // lseek
    9,   // mmap
    10,  // mprotect
    11,  // munmap
    12,  // brk
    13,  // rt_sigaction
    14,  // rt_sigprocmask
    15,  // rt_sigreturn
    19,  // readv
    20,  // writev
    24,  // sched_yield
    25,  // mremap
    28,  // madvise
    35,  // nanosleep
    39,  // getpid
    43,  // accept
    44,  // sendto
    45,  // recvfrom
    48,  // shutdown
    51,  // getsockname
    52,  // getpeername
    54,  // setsockopt
    55,  // getsockopt
    60,  // exit
    72,  // fcntl
    74,  // fsync (MC-8b: the one-time pairing's file, opened before the filter)
    131, // sigaltstack
    157, // prctl (thread names)
    186, // gettid
    202, // futex
    204, // sched_getaffinity
    218, // set_tid_address
    219, // restart_syscall
    228, // clock_gettime
    230, // clock_nanosleep
    231, // exit_group
    234, // tgkill (abort of its own threads)
    262, // newfstatat
    271, // ppoll
    273, // set_robust_list
    288, // accept4
    318, // getrandom
    332, // statx
    334, // rseq
];

/// Calls that end the process: starting a program, tracing, another process's memory, kernel images and modules.
pub const KILLED: &[u32] = &[
    57,  // fork
    58,  // vfork
    59,  // execve
    101, // ptrace
    175, // init_module
    246, // kexec_load
    310, // process_vm_readv
    311, // process_vm_writev
    313, // finit_module
    320, // kexec_file_load
    322, // execveat
];

fn ld(off: u32) -> Insn {
    Insn { code: LD_W_ABS, jt: 0, jf: 0, k: off }
}
fn ret(k: u32) -> Insn {
    Insn { code: RET_K, jt: 0, jf: 0, k }
}
fn jeq(k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code: JEQ_K, jt, jf, k }
}

/// The program, in order.
pub fn program() -> Vec<Insn> {
    let mut p = vec![
        ld(OFF_ARCH),
        jeq(AUDIT_ARCH_X86_64, 1, 0),
        ret(RET_KILL_PROCESS),
        ld(OFF_NR),
        Insn { code: JGE_K, jt: 0, jf: 1, k: X32_BIT },
        ret(RET_KILL_PROCESS),
        // clone: threads only (A is the number again on the path that does not return).
        jeq(NR_CLONE, 0, 4),
        ld(OFF_ARG0),
        Insn { code: JSET_K, jt: 0, jf: 1, k: CLONE_THREAD },
        ret(RET_ALLOW),
        ret(RET_KILL_PROCESS),
        jeq(NR_CLONE3, 0, 1),
        ret(RET_ERRNO | ENOSYS),
    ];
    for &nr in KILLED {
        p.push(jeq(nr, 0, 1));
        p.push(ret(RET_KILL_PROCESS));
    }
    for &nr in ALLOWED {
        p.push(jeq(nr, 0, 1));
        p.push(ret(RET_ALLOW));
    }
    p.push(ret(RET_ERRNO | EPERM));
    p
}

/// The program's verdict for a call (a tiny interpreter of exactly the instructions `program` uses), for tests.
pub fn verdict(p: &[Insn], arch: u32, nr: u32, arg0: u64) -> u32 {
    let mut a: u32 = 0;
    let mut pc = 0usize;
    while pc < p.len() {
        let i = p[pc];
        match i.code {
            LD_W_ABS => {
                a = match i.k {
                    OFF_NR => nr,
                    OFF_ARCH => arch,
                    OFF_ARG0 => arg0 as u32,
                    _ => 0,
                }
            }
            RET_K => return i.k,
            JEQ_K | JGE_K | JSET_K => {
                let t = match i.code {
                    JEQ_K => a == i.k,
                    JGE_K => a >= i.k,
                    _ => a & i.k != 0,
                };
                pc += if t { i.jt as usize } else { i.jf as usize };
            }
            _ => return RET_KILL_PROCESS,
        }
        pc += 1;
    }
    RET_KILL_PROCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts() {
        let p = program();
        assert!(p.len() <= 4096, "BPF_MAXINSNS");
        let x86 = AUDIT_ARCH_X86_64;
        // read, write, fsync (the pairing's file), futex, accept4, getrandom (rustls/ring), clock_gettime.
        for nr in [0, 1, 74, 202, 228, 288, 318] {
            assert_eq!(verdict(&p, x86, nr, 0), RET_ALLOW, "{nr}");
        }
        assert_eq!(verdict(&p, x86, 59, 0), RET_KILL_PROCESS, "execve");
        assert_eq!(verdict(&p, x86, 257, 0), RET_ERRNO | 1, "openat is EPERM");
        assert_eq!(verdict(&p, x86, 41, 0), RET_ERRNO | 1, "socket is EPERM");
        assert_eq!(verdict(&p, x86, 49, 0), RET_ERRNO | 1, "bind is EPERM");
        assert_eq!(verdict(&p, x86, 62, 0), RET_ERRNO | 1, "kill is EPERM");
        assert_eq!(verdict(&p, x86, 16, 0), RET_ERRNO | 1, "ioctl is EPERM");
        assert_eq!(verdict(&p, x86, NR_CLONE, 0x003d_0f00), RET_ALLOW, "musl's thread clone");
        assert_eq!(verdict(&p, x86, NR_CLONE, 0x11), RET_KILL_PROCESS, "a fork-like clone");
        assert_eq!(verdict(&p, x86, NR_CLONE3, 0), RET_ERRNO | 38);
        assert_eq!(verdict(&p, 0x4000_0003, 0, 0), RET_KILL_PROCESS, "i386");
        assert_eq!(verdict(&p, x86, 0x4000_0000, 0), RET_KILL_PROCESS, "x32");
        // Every jump lands inside the program.
        for (i, ins) in p.iter().enumerate() {
            if ins.code != RET_K && ins.code != LD_W_ABS {
                assert!(i + 1 + ins.jt.max(ins.jf) as usize <= p.len() - 1, "jump out at {i}");
            }
        }
        assert!(ALLOWED.iter().all(|n| !KILLED.contains(n)));
    }
}

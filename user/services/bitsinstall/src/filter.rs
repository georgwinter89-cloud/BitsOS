//! The seccomp program bitsinstall runs under after its setup (classic BPF, linux-6.6/kernel/seccomp.c; SX-1 on
//! V2). Unlike bitsmcp it keeps the file system and its sockets (the grants file, /proc/<pid>/exe of a
//! confirmer, the confirmer's socket), so the program is a deny list: another arch or an x32 number, starting a
//! program, a fork (clone without CLONE_THREAD), tracing, another process's memory, kernel images and modules,
//! mounts and the raw reboot call end the process; `clone3` is ENOSYS (libc falls back to clone). The kernel
//! acts for it only through the gate, after a decision.

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
const ENOSYS: u32 = 38;

const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0: u32 = 16;

pub const NR_CLONE: u32 = 56;
pub const NR_CLONE3: u32 = 435;

/// Calls that end the process (x86-64 numbers).
pub const KILLED: &[u32] = &[
    57,  // fork
    58,  // vfork
    59,  // execve
    101, // ptrace
    155, // pivot_root
    165, // mount
    166, // umount2
    167, // swapon
    168, // swapoff
    169, // reboot (the installer's reboot is the kernel's tool, after a decision)
    172, // iopl
    173, // ioperm
    175, // init_module
    176, // delete_module
    246, // kexec_load
    272, // unshare
    298, // perf_event_open
    308, // setns
    310, // process_vm_readv
    311, // process_vm_writev
    313, // finit_module
    320, // kexec_file_load
    321, // bpf
    322, // execveat
    428, // open_tree
    429, // move_mount
    432, // fsmount
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

pub fn program() -> Vec<Insn> {
    let mut p = vec![
        ld(OFF_ARCH),
        jeq(AUDIT_ARCH_X86_64, 1, 0),
        ret(RET_KILL_PROCESS),
        ld(OFF_NR),
        Insn { code: JGE_K, jt: 0, jf: 1, k: X32_BIT },
        ret(RET_KILL_PROCESS),
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
    p.push(ret(RET_ALLOW));
    p
}

/// The program's verdict for a call (an interpreter of exactly the instructions `program` uses), for tests.
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
        assert!(p.len() <= 4096);
        let x86 = AUDIT_ARCH_X86_64;
        // read, write, openat, socket, connect, accept4, getsockopt (SO_PEERCRED), readlinkat, renameat, fsync.
        for nr in [0, 1, 257, 41, 42, 288, 55, 267, 264, 74] {
            assert_eq!(verdict(&p, x86, nr, 0), RET_ALLOW, "{nr}");
        }
        // MC-11, the text console: ioctl, poll, close, getdents64, getrandom, nanosleep, clock_nanosleep.
        for nr in [16, 7, 3, 217, 318, 35, 230] {
            assert_eq!(verdict(&p, x86, nr, 0), RET_ALLOW, "{nr}");
        }
        for nr in [59, 322, 57, 58, 101, 169, 246, 320, 165, 272, 310, 311] {
            assert_eq!(verdict(&p, x86, nr, 0), RET_KILL_PROCESS, "{nr}");
        }
        assert_eq!(verdict(&p, x86, NR_CLONE, 0x003d_0f00), RET_ALLOW, "a thread");
        assert_eq!(verdict(&p, x86, NR_CLONE, 0x11), RET_KILL_PROCESS, "a fork-like clone");
        assert_eq!(verdict(&p, x86, NR_CLONE3, 0), RET_ERRNO | ENOSYS);
        assert_eq!(verdict(&p, 0x4000_0003, 0, 0), RET_KILL_PROCESS, "i386");
        assert_eq!(verdict(&p, x86, 0x4000_0001, 0), RET_KILL_PROCESS, "x32");
        for (i, ins) in p.iter().enumerate() {
            if ins.code != RET_K && ins.code != LD_W_ABS {
                assert!(i + 1 + ins.jt.max(ins.jf) as usize <= p.len() - 1, "jump out at {i}");
            }
        }
    }
}

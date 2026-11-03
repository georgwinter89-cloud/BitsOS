//! The user ABI numbers of seccomp and classic BPF (linux-6.6/include/uapi/linux/seccomp.h, filter.h,
//! bpf_common.h, audit.h). Kept here because `uapi` does not name them yet (change request to the desk: move
//! them under `uapi::ab` when another crate needs them).

/// `__NR_seccomp` on x86-64.
pub const NR_SECCOMP: usize = 317;

// seccomp(2) operations.
pub const SET_MODE_STRICT: u32 = 0;
pub const SET_MODE_FILTER: u32 = 1;
pub const GET_ACTION_AVAIL: u32 = 2;
pub const GET_NOTIF_SIZES: u32 = 3;

// prctl(PR_SET_SECCOMP) modes and the modes /proc shows.
pub const MODE_DISABLED: u8 = 0;
pub const MODE_STRICT: u8 = 1;
pub const MODE_FILTER: u8 = 2;
/// After a kill action: the thread never runs another call (`SECCOMP_MODE_DEAD`).
pub const MODE_DEAD: u8 = 3;

// SECCOMP_SET_MODE_FILTER flags.
pub const FLAG_TSYNC: u32 = 1;
pub const FLAG_LOG: u32 = 2;
pub const FLAG_SPEC_ALLOW: u32 = 4;
pub const FLAG_NEW_LISTENER: u32 = 8;
pub const FLAG_TSYNC_ESRCH: u32 = 16;
pub const FLAG_WAIT_KILLABLE_RECV: u32 = 32;
/// The flags V2 builds: Linux's `SECCOMP_FILTER_FLAG_MASK` without the user-notification pair (Chromium never
/// asks for a listener; a program that does gets EINVAL, as from a kernel without it).
pub const FLAGS_BUILT: u32 = FLAG_TSYNC | FLAG_LOG | FLAG_SPEC_ALLOW | FLAG_TSYNC_ESRCH;

// Return values: action in the high 16 bits (the top one signed), data in the low 16.
pub const RET_KILL_PROCESS: u32 = 0x8000_0000;
pub const RET_KILL_THREAD: u32 = 0x0000_0000;
pub const RET_TRAP: u32 = 0x0003_0000;
pub const RET_ERRNO: u32 = 0x0005_0000;
pub const RET_USER_NOTIF: u32 = 0x7fc0_0000;
pub const RET_TRACE: u32 = 0x7ff0_0000;
pub const RET_LOG: u32 = 0x7ffc_0000;
pub const RET_ALLOW: u32 = 0x7fff_0000;
pub const RET_ACTION_FULL: u32 = 0xffff_0000;
pub const RET_DATA: u32 = 0x0000_ffff;

/// `AUDIT_ARCH_X86_64` (EM_X86_64 | __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE).
pub const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
/// `__X32_SYSCALL_BIT`: x32 calls carry it in their number (with the x86-64 arch value).
pub const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// `sizeof(struct seccomp_data)`: nr, arch, instruction_pointer, args[6].
pub const DATA_SIZE: u32 = 64;
/// `sizeof(struct sock_fprog)` on x86-64 (u16 len, 6 bytes padding, pointer).
pub const FPROG_SIZE: usize = 16;
/// `sizeof(struct sock_filter)`.
pub const INSN_SIZE: usize = 8;
/// `BPF_MAXINSNS`: one filter's bound (hostile-input bound from Linux, R1).
pub const MAX_INSNS: usize = 4096;
/// `MAX_INSNS_PER_PATH`: the whole stack's bound, every filter costing its length plus 4.
pub const MAX_INSNS_PER_PATH: usize = (1 << 18) / INSN_SIZE;
/// `MAX_ERRNO`: an ERRNO action's data is capped here.
pub const MAX_ERRNO: u32 = 4095;
/// `struct seccomp_notif_sizes` answers: seccomp_notif, seccomp_notif_resp, seccomp_data.
pub const NOTIF_SIZES: [u16; 3] = [80, 24, 64];

/// `SYS_SECCOMP`: si_code of the SIGSYS a TRAP raises.
pub const SYS_SECCOMP: i32 = 1;

/// Strict mode's calls on x86-64 (`mode1_syscalls`): read, write, exit, rt_sigreturn.
pub const STRICT_ALLOWED: [u32; 4] = [0, 1, 60, 15];

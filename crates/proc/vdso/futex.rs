// L7-SYNC: the robust-futex try-unlock helpers of Linux 7.3.1's vDSO (linux-7.3.1/arch/x86/entry/vdso/common/
// vfutex.c, include/vdso/futex.h; own code). `__vdso_futex_robust_list64_try_unlock(lock, tid, pop)` swaps the
// lock word from `tid` to 0 and, when that worked, stores 0 into the robust list's `list_op_pending` at `pop`;
// it returns the word it found (== `tid` on success). A C library calls the futex syscall with
// FUTEX_ROBUST_UNLOCK when the answer is not `tid` (FUTEX_WAITERS set).
//
// The window between the successful cmpxchg and the store is the critical section the kernel knows by the
// hidden labels `__futex_list{64,32}_try_unlock_cs_{start,end}` (build.rs turns them into offsets for
// `proc::vdso`): a thread interrupted there with ZF set (the cmpxchg succeeded) gets `*pop` cleared by the
// kernel before a signal is delivered, so its exit can never walk a pending pointer to a lock it no longer
// holds. Hand-written so the registers are fixed: the lock in rdi, the TID in esi then eax, `pop` in rdx (the
// kernel reads it from there), 0 in ecx/rcx.

use core::arch::global_asm;

global_asm!(
    ".pushsection .text.vdso_futex, \"ax\", @progbits",
    ".globl __vdso_futex_robust_list64_try_unlock",
    ".type __vdso_futex_robust_list64_try_unlock, @function",
    "__vdso_futex_robust_list64_try_unlock:",
    ".cfi_startproc",
    "mov eax, esi",
    "xor ecx, ecx",
    "lock cmpxchg dword ptr [rdi], ecx",
    ".globl __futex_list64_try_unlock_cs_start",
    ".hidden __futex_list64_try_unlock_cs_start",
    "__futex_list64_try_unlock_cs_start:",
    "jnz 2f",
    "mov qword ptr [rdx], rcx",
    ".globl __futex_list64_try_unlock_cs_end",
    ".hidden __futex_list64_try_unlock_cs_end",
    "__futex_list64_try_unlock_cs_end:",
    "2:",
    "ret",
    ".cfi_endproc",
    ".size __vdso_futex_robust_list64_try_unlock, . - __vdso_futex_robust_list64_try_unlock",
    "",
    ".globl __vdso_futex_robust_list32_try_unlock",
    ".type __vdso_futex_robust_list32_try_unlock, @function",
    "__vdso_futex_robust_list32_try_unlock:",
    ".cfi_startproc",
    "mov eax, esi",
    "xor ecx, ecx",
    "lock cmpxchg dword ptr [rdi], ecx",
    ".globl __futex_list32_try_unlock_cs_start",
    ".hidden __futex_list32_try_unlock_cs_start",
    "__futex_list32_try_unlock_cs_start:",
    "jnz 2f",
    "mov dword ptr [rdx], ecx",
    ".globl __futex_list32_try_unlock_cs_end",
    ".hidden __futex_list32_try_unlock_cs_end",
    "__futex_list32_try_unlock_cs_end:",
    "2:",
    "ret",
    ".cfi_endproc",
    ".size __vdso_futex_robust_list32_try_unlock, . - __vdso_futex_robust_list32_try_unlock",
    ".popsection",
);

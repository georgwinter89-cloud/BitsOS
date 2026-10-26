//! Memory and arch calls: brk/mmap/munmap/mprotect on MM-5a's Linux semantics, file mappings (FS-5), and MM-5b's
//! shared anonymous memory, device mappings, madvise, mremap, mincore (the entry points MM-5b was granted);
//! arch_prctl.

use crate::SysCtx;
use uapi::errno::*;

fn sys_brk(c: &mut SysCtx<'_>) -> i64 {
    c.proc.mm.sys_brk(c.args[0]) as i64
}
crate::syscall!(BRK, sys_brk);

/// MM-5a's Linux semantics (hints, atomic MAP_FIXED, NOREPLACE, EPERM below mmap_min_addr, overflow checks).
/// FS-5: file mappings in `ksys_mmap_pgoff`'s order: the offset (EINVAL, arch `sys_mmap`), the fd (`fget` of
/// an `unsigned int`: EBADF, O_PATH too), MAP_HUGETLB on a file that is not hugetlbfs (EINVAL), then MM's
/// address checks and FS's file checks (`vfs::mmap::check`).
fn sys_mmap(c: &mut SysCtx<'_>) -> i64 {
    let [addr, len, prot, flags, fd, off] = c.args;
    if flags & uapi::mman::MAP_ANONYMOUS == 0 {
        if off & 0xfff != 0 {
            return -EINVAL;
        }
        let f = match vfs::mmap::fd_for_mmap(c.proc.fds.get(fd as u32 as usize).ok()) {
            Ok(f) => f,
            Err(e) => return -e,
        };
        if flags & 0x4_0000 != 0 {
            return -EINVAL; // MAP_HUGETLB
        }
        // MM-5b: a registered device file (fbdev, GPU surfaces) maps through its fault hook.
        if let Some(map) = mm::device::lookup(&f) {
            return c.proc.mm.sys_mmap_device(addr, len, prot, flags, off, &f, &map).map(|a| a as i64).unwrap_or_else(|e| -e);
        }
        let r = c.proc.mm.sys_mmap_file(addr, len, prot, flags, off, &mut |len| vfs::mmap::check(&f, prot, flags, off >> 12, len));
        return r.map(|a| a as i64).unwrap_or_else(|e| -e);
    }
    c.proc.mm.sys_mmap(addr, len, prot, flags, off).map(|a| a as i64).unwrap_or_else(|e| -e.code())
}
crate::syscall!(MMAP, sys_mmap);

/// FS-5: msync with linux-6.6/mm/msync.c's rules (MM's `sys_msync`; the write-back is the file's).
fn sys_msync(c: &mut SysCtx<'_>) -> i64 {
    c.proc.mm.sys_msync(c.args[0], c.args[1], c.args[2] as u32 as u64).map(|_| 0).unwrap_or_else(|e| -e)
}
crate::syscall!(MSYNC, sys_msync);

fn sys_munmap(c: &mut SysCtx<'_>) -> i64 {
    c.proc.mm.sys_munmap(c.args[0], c.args[1]).map(|_| 0).unwrap_or_else(|e| -e.code())
}
crate::syscall!(MUNMAP, sys_munmap);

fn sys_mprotect(c: &mut SysCtx<'_>) -> i64 {
    c.proc.mm.sys_mprotect(c.args[0], c.args[1], c.args[2]).map(|_| 0).unwrap_or_else(|e| -e.code())
}
crate::syscall!(MPROTECT, sys_mprotect);

// Protection keys as Linux 6.6 answers without OSPKE (V2 never turns PKU on): arch_max_pkey() is 1 and no key is
// allocated, not even 0 (arch/x86/include/asm/pkeys.h:10, :81-97; the allocation map is only set up with OSPKE).

/// `do_mprotect_pkey` (linux-6.6/mm/mprotect.c:690-731): pkey -1 is mprotect; any other key fails the
/// "allocated" check with EINVAL, after the argument checks that come before it.
fn sys_pkey_mprotect(c: &mut SysCtx<'_>) -> i64 {
    const PROT_GROWSDOWN: u64 = 0x0100_0000;
    const PROT_GROWSUP: u64 = 0x0200_0000;
    // PROT_READ | PROT_WRITE | PROT_EXEC | PROT_SEM: the generic arch_validate_prot (include/linux/mman.h).
    const PROT_VALID: u64 = 0xf;
    let (start, len, prot, pkey) = (c.args[0], c.args[1], c.args[2], c.args[3] as u32 as i32);
    if pkey == -1 {
        return sys_mprotect(c);
    }
    let grows = prot & (PROT_GROWSDOWN | PROT_GROWSUP);
    if grows == PROT_GROWSDOWN | PROT_GROWSUP || start & 0xfff != 0 {
        return -EINVAL;
    }
    if len == 0 {
        return 0;
    }
    let end = start.wrapping_add(len.wrapping_add(0xfff) & !0xfff);
    if end <= start {
        return -ENOMEM;
    }
    if prot & !(PROT_GROWSDOWN | PROT_GROWSUP) & !PROT_VALID != 0 {
        return -EINVAL;
    }
    -EINVAL
}
crate::syscall!(PKEY_MPROTECT, sys_pkey_mprotect);

/// `pkey_alloc` (mm/mprotect.c:851-867): EINVAL for flags or an init_val outside PKEY_ACCESS_MASK (3), else
/// ENOSPC (no key left; BR-3 CR 4 accepts ENOSPC also for the first call).
fn sys_pkey_alloc(c: &mut SysCtx<'_>) -> i64 {
    if c.args[0] != 0 || c.args[1] & !3 != 0 {
        return -EINVAL;
    }
    -ENOSPC
}
crate::syscall!(PKEY_ALLOC, sys_pkey_alloc);

/// `pkey_free` (mm/mprotect.c:869-880, `mm_pkey_free`): no key is allocated, so EINVAL.
fn sys_pkey_free(_c: &mut SysCtx<'_>) -> i64 {
    -EINVAL
}
crate::syscall!(PKEY_FREE, sys_pkey_free);

/// MM-5b: madvise with Linux 6.6's semantics (`mm::aspace` advise.rs; the advice is the `int` argument).
fn sys_madvise(c: &mut SysCtx<'_>) -> i64 {
    c.proc.mm.sys_madvise(c.args[0], c.args[1], c.args[2] as u32 as i32 as i64 as u64).map(|_| 0).unwrap_or_else(|e| -e.code())
}
crate::syscall!(MADVISE, sys_madvise);

/// MM-5b: mremap(addr, old_len, new_len, flags, new_addr) (mm/mremap.c's checks and order).
fn sys_mremap(c: &mut SysCtx<'_>) -> i64 {
    let [addr, old_len, new_len, flags, new_addr, _] = c.args;
    c.proc.mm.sys_mremap(addr, old_len, new_len, flags, new_addr).map(|a| a as i64).unwrap_or_else(|e| -e.code())
}
crate::syscall!(MREMAP, sys_mremap);

/// MM-5b: mincore(start, len, vec).
fn sys_mincore(c: &mut SysCtx<'_>) -> i64 {
    c.proc.mm.sys_mincore(c.args[0], c.args[1], c.args[2]).map(|_| 0).unwrap_or_else(|e| -e.code())
}
crate::syscall!(MINCORE, sys_mincore);

const ARCH_GET_CPUID: u64 = 0x1011;
const ARCH_SET_CPUID: u64 = 0x1012;

/// arch_prctl (linux-6.6/arch/x86/kernel/process_64.c `do_arch_prctl_64` + process.c `do_arch_prctl_common`).
fn sys_arch_prctl(c: &mut SysCtx<'_>) -> i64 {
    use uapi::arch_prctl::*;
    let option = c.args[0] as u32 as u64;
    match option {
        // Linux refuses bases at or above TASK_SIZE_MAX with EPERM. The entry code reads both bases back on
        // every return to the kernel, so values set by wrfsbase/wrgsbase are what GET returns (AB-1).
        ARCH_SET_FS | ARCH_SET_GS => {
            if c.args[1] >= hal::USER_TOP {
                return -EPERM;
            }
            if option == ARCH_SET_FS {
                c.user.fs_base = c.args[1];
            } else {
                c.user.gs_base = c.args[1];
            }
            0
        }
        ARCH_GET_FS => c.proc.mm.write_u64(c.args[1], c.user.fs_base).map(|_| 0).unwrap_or(-EFAULT),
        ARCH_GET_GS => c.proc.mm.write_u64(c.args[1], c.user.gs_base).map(|_| 0).unwrap_or(-EFAULT),
        // CPUID is never made to fault in V2 (`get_cpuid_mode` = 1); switching it needs CPUID faulting, which
        // V2 does not use (`set_cpuid_mode`: ENODEV without X86_FEATURE_CPUID_FAULT).
        ARCH_GET_CPUID => 1,
        ARCH_SET_CPUID => -ENODEV,
        _ => -EINVAL,
    }
}
crate::syscall!(ARCH_PRCTL, sys_arch_prctl);

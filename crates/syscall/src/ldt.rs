//! AB-10a: the TLS and LDT calls Wine's WoW64 makes (`set_thread_area`, `get_thread_area`, `modify_ldt`), as
//! 64-bit syscalls (205, 211, 154) and through `int 0x80` with the 32-bit ABI (243, 244, 123). Every other
//! `int 0x80` number answers -ENOSYS (Georg 27.10.: no full 32-bit support). Linux semantics from
//! linux-6.6/arch/x86/kernel/tls.c (do_set_thread_area, do_get_thread_area, tls_desc_okay), ldt.c (read_ldt,
//! write_ldt, read_default_ldt, modify_ldt), include/asm/desc.h (fill_ldt, LDT_empty, LDT_zero) and
//! entry/common.c (do_int80_syscall_32). The descriptors themselves live in hal (`entry::segs`).

use crate::{SysCtx, syscall_nr};
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::entry::UserContext;
use hal::entry::segs::{self, LdtError};
use proc::{Process, Thread};
use seccomp::{Data, Verdict};
use uapi::ab::nr_more::{GET_THREAD_AREA, MODIFY_LDT, SET_THREAD_AREA};
use uapi::errno::*;

klog::counter!(INT80_CALLS, "syscall.int80");
klog::counter!(INT80_ENOSYS, "syscall.int80.enosys");

/// `struct user_desc` (asm/ldt.h): 16 bytes, the same in both ABIs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserDesc {
    pub entry_number: u32,
    pub base_addr: u32,
    pub limit: u32,
    /// seg_32bit:1 contents:2 read_exec_only:1 limit_in_pages:1 seg_not_present:1 useable:1 lm:1
    pub flags: u32,
}

const F_32BIT: u32 = 1;
const F_READ_EXEC_ONLY: u32 = 1 << 3;
const F_PAGES: u32 = 1 << 4;
const F_NOT_PRESENT: u32 = 1 << 5;
const F_USEABLE: u32 = 1 << 6;
const F_LM: u32 = 1 << 7;

/// GDT index of the first TLS slot (`GDT_ENTRY_TLS_MIN`).
const TLS_MIN: u32 = hal::gdt::TLS_FIRST as u32;
const TLS_MAX: u32 = TLS_MIN + hal::gdt::TLS_ENTRIES as u32 - 1;
/// Bytes `modify_ldt(2)` reads back on x86-64 (`read_default_ldt`).
const DEFAULT_LDT_BYTES: u64 = 128;
const LDT_BYTES: u64 = (segs::LDT_ENTRIES * 8) as u64;
/// 32-bit numbers (syscall_32.tbl).
const NR32_MODIFY_LDT: u32 = 123;
const NR32_SET_THREAD_AREA: u32 = 243;
const NR32_GET_THREAD_AREA: u32 = 244;
/// `AUDIT_ARCH_I386` (EM_386 | __AUDIT_ARCH_LE): what seccomp filters see for `int 0x80`.
pub const AUDIT_ARCH_I386: u32 = 0x4000_0003;

impl UserDesc {
    pub fn from_bytes(b: &[u8; 16]) -> UserDesc {
        let w = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        UserDesc { entry_number: w(0), base_addr: w(4), limit: w(8), flags: w(12) }
    }
    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        for (i, v) in [self.entry_number, self.base_addr, self.limit, self.flags].into_iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
    fn contents(&self) -> u32 {
        self.flags >> 1 & 3
    }
    fn bit(&self, f: u32) -> bool {
        self.flags & f != 0
    }
    /// The fields `LDT_empty` and `LDT_zero` look at, lm left out.
    fn shape(&self) -> (u32, u32, u32) {
        (self.base_addr, self.limit, self.flags & 0x7F)
    }
    /// `LDT_empty`: the documented "no segment".
    pub fn is_empty(&self) -> bool {
        self.shape() == (0, 0, F_READ_EXEC_ONLY | F_NOT_PRESENT)
    }
    /// `LDT_zero`: all zero but the number, which programs use for "no segment" too.
    pub fn is_zero(&self) -> bool {
        self.shape() == (0, 0, 0)
    }

    /// The descriptor (`fill_ldt`): S=1, DPL 3, accessed set, L always 0 (a TLS or LDT entry never names
    /// 64-bit code; SYSRET would override it anyway).
    pub fn descriptor(&self) -> u64 {
        let (base, limit) = (self.base_addr as u64, self.limit as u64);
        let typ = ((!self.flags >> 3 & 1) as u64) << 1 | (self.contents() as u64) << 2 | 1;
        (limit & 0xFFFF)
            | (base & 0xFF_FFFF) << 16
            | typ << 40
            | 1 << 44
            | 3 << 45
            | (!self.bit(F_NOT_PRESENT) as u64) << 47
            | (limit >> 16 & 0xF) << 48
            | (self.bit(F_USEABLE) as u64) << 52
            | (self.bit(F_32BIT) as u64) << 54
            | (self.bit(F_PAGES) as u64) << 55
            | (base >> 24 & 0xFF) << 56
    }

    /// `fill_user_desc`: what get_thread_area reports for descriptor `d` of slot `idx`.
    pub fn of_descriptor(idx: u32, d: u64) -> UserDesc {
        let typ = (d >> 40 & 0xF) as u32;
        let b = |bit: u32, f: u32| if d >> bit & 1 != 0 { f } else { 0 };
        let flags = b(54, F_32BIT)
            | (typ >> 2 & 3) << 1
            | if typ & 2 == 0 { F_READ_EXEC_ONLY } else { 0 }
            | b(55, F_PAGES)
            | if d >> 47 & 1 == 0 { F_NOT_PRESENT } else { 0 }
            | b(52, F_USEABLE)
            | b(53, F_LM);
        UserDesc { entry_number: idx, base_addr: segs::desc_base(d) as u32, limit: ((d & 0xFFFF) | (d >> 48 & 0xF) << 16) as u32, flags }
    }
}

/// `tls_desc_okay`: a TLS slot holds a present 32-bit data segment, or nothing.
pub fn tls_ok(u: &UserDesc) -> bool {
    if u.is_empty() || u.is_zero() {
        return true;
    }
    u.bit(F_32BIT) && u.contents() <= 1 && !u.bit(F_NOT_PRESENT)
}

/// `write_ldt`'s checks and descriptor for entry `u` (`oldmode`: func 1). CachyOS has CONFIG_X86_16BIT, so
/// 16-bit segments are accepted as Linux does (V2 never returns to one: `refuse_entry`).
pub fn ldt_descriptor(u: &UserDesc, oldmode: bool) -> Result<u64, i64> {
    if u.entry_number as usize >= segs::LDT_ENTRIES {
        return Err(-EINVAL);
    }
    if u.contents() == 3 && (oldmode || !u.bit(F_NOT_PRESENT)) {
        return Err(-EINVAL);
    }
    if (oldmode && u.base_addr == 0 && u.limit == 0) || u.is_empty() {
        return Ok(0);
    }
    let d = u.descriptor();
    Ok(if oldmode { d & !(1 << 52) } else { d })
}

fn read_desc(p: &Process, at: u64) -> Result<UserDesc, i64> {
    let mut b = [0u8; 16];
    p.mm.copy_from_user(&mut b, at).map_err(|_| -EFAULT)?;
    Ok(UserDesc::from_bytes(&b))
}

/// `set_thread_area(u_info)` (`do_set_thread_area(current, -1, u_info, 1)`).
fn set_thread_area(p: &Process, ctx: &mut UserContext, at: u64) -> i64 {
    let u = match read_desc(p, at) {
        Ok(u) => u,
        Err(e) => return e,
    };
    if !tls_ok(&u) {
        return -EINVAL;
    }
    let mut idx = u.entry_number;
    if idx == u32::MAX {
        let Some(free) = (0..hal::gdt::TLS_ENTRIES).find(|&i| segs::tls(i) == 0) else { return -ESRCH };
        idx = TLS_MIN + free as u32;
        if p.mm.copy_to_user(at, &idx.to_le_bytes()).is_err() {
            return -EFAULT;
        }
    }
    if !(TLS_MIN..=TLS_MAX).contains(&idx) {
        return -EINVAL;
    }
    let d = if u.is_empty() || u.is_zero() { 0 } else { u.descriptor() };
    if !segs::set_tls((idx - TLS_MIN) as usize, d, ctx) {
        return -EINVAL;
    }
    klog::event!(Debug, "syscall", "tls.set", pid = p.pid, entry = idx, base = klog::Hex(u.base_addr as u64), clear = d == 0);
    0
}

/// `get_thread_area(u_info)`: the slot named by `u_info->entry_number`.
fn get_thread_area(p: &Process, at: u64) -> i64 {
    let mut n = [0u8; 4];
    if p.mm.copy_from_user(&mut n, at).is_err() {
        return -EFAULT;
    }
    let idx = u32::from_le_bytes(n);
    if !(TLS_MIN..=TLS_MAX).contains(&idx) {
        return -EINVAL;
    }
    let u = UserDesc::of_descriptor(idx, segs::tls((idx - TLS_MIN) as usize));
    if p.mm.copy_to_user(at, &u.to_bytes()).is_err() {
        return -EFAULT;
    }
    0
}

/// Writes `n` zero bytes at `at` (`clear_user`).
fn clear_user(p: &Process, at: u64, n: u64) -> Result<(), i64> {
    const Z: [u8; 512] = [0; 512];
    let mut done = 0;
    while done < n {
        let k = (n - done).min(Z.len() as u64);
        p.mm.copy_to_user(at + done, &Z[..k as usize]).map_err(|_| -EFAULT)?;
        done += k;
    }
    Ok(())
}

/// `read_ldt`: the entries written so far, zero-filled up to `bytecount` (at most the whole table).
fn read_ldt(p: &Process, at: u64, bytecount: u64) -> i64 {
    let slot = &p.ldt;
    let nr = slot.entries() as u64;
    if nr == 0 {
        return 0;
    }
    let bytecount = bytecount.min(LDT_BYTES);
    let size = (nr * 8).min(bytecount);
    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve_exact(size as usize).is_err() {
        return -ENOMEM;
    }
    for i in 0..size.div_ceil(8) as usize {
        buf.extend_from_slice(&slot.entry(i).to_le_bytes());
    }
    buf.truncate(size as usize);
    if p.mm.copy_to_user(at, &buf).is_err() {
        return -EFAULT;
    }
    if let Err(e) = clear_user(p, at + size, bytecount - size) {
        return e;
    }
    bytecount as i64
}

/// `write_ldt`: one entry of the process's LDT; the first write creates the table; every write reaches the
/// CPUs of the other threads before it returns (`flush_siblings`).
fn write_ldt(p: &Arc<Process>, at: u64, bytecount: u64, oldmode: bool) -> i64 {
    if bytecount != 16 {
        return -EINVAL;
    }
    let u = match read_desc(p, at) {
        Ok(u) => u,
        Err(e) => return e,
    };
    let d = match ldt_descriptor(&u, oldmode) {
        Ok(d) => d,
        Err(e) => return e,
    };
    match p.ldt.write(u.entry_number as usize, d) {
        Ok(created) => {
            segs::ldt_written(&p.ldt);
            if created {
                klog::event!(Info, "syscall", "ldt.created", pid = p.pid, program = p.name.as_str());
            }
            flush_siblings(p);
            0
        }
        Err(LdtError::NoMemory) => {
            klog::event!(Warn, "syscall", "ldt.no_memory", pid = p.pid);
            -ENOMEM
        }
        Err(LdtError::BadIndex | LdtError::BadDescriptor) => -EINVAL,
    }
}

/// How long one CPU may take to run the LDT refresh (it needs interrupts on there at some point).
const FLUSH_NS: u64 = 10_000_000;
/// How often a CPU whose cross-CPU mailbox another caller holds is asked again.
const FLUSH_BUSY_TRIES: u32 = 64;

/// Linux `install_ldt` → `on_each_cpu_mm(flush_ldt)`: after a write, every CPU a sibling thread runs on has the
/// table in its LDTR and DS/ES reloaded before `modify_ldt` returns (`segs::refresh_cpu_ldt`, synchronous).
/// sched has no public "CPU this thread runs on" yet, so every online CPU is asked; the call is a no-op on a CPU
/// not running a thread of this process. Single-threaded processes need none (their own CPU did it already).
/// A CPU that does not answer in time gets its sibling kicked instead, the generation check before the return
/// to user then catching it up.
fn flush_siblings(p: &Arc<Process>) {
    if p.group().thread_count() <= 1 {
        return;
    }
    let id = segs::slot_id(&p.ldt);
    let mut failed = 0u32;
    for cpu in (0..hal::percpu::cpu_count()).filter(|&c| hal::smp::is_online(c)) {
        let mut tries = 0;
        let r = loop {
            match hal::smp::call_on(cpu, segs::refresh_cpu_ldt, id, time::now_ns, FLUSH_NS) {
                Err(hal::smp::CallError::Busy) if tries < FLUSH_BUSY_TRIES => {
                    tries += 1;
                    sched::yield_now();
                }
                r => break r,
            }
        };
        if let Err(e) = r {
            failed += 1;
            klog::event!(Warn, "syscall", "ldt.flush_failed", pid = p.pid, cpu = cpu, error = match e {
                hal::smp::CallError::NotOnline => "not_online",
                hal::smp::CallError::Busy => "busy",
                hal::smp::CallError::Ipi(_) => "ipi",
                hal::smp::CallError::Timeout => "timeout",
                hal::smp::CallError::Late => "late",
            });
        }
    }
    if failed != 0 {
        let me = sched::current_id();
        for t in p.group().threads() {
            if t.kthread() != me && t.kthread() != 0 {
                sched::kick_thread(t.kthread());
            }
        }
        klog::event!(Info, "syscall", "ldt.flush_fallback", pid = p.pid, cpus = failed);
    }
}

/// `modify_ldt(func, ptr, bytecount)`; the answer is an `int` zero-extended into rax, as Linux's
/// `(unsigned int)ret`.
fn modify_ldt(p: &Arc<Process>, func: i32, at: u64, bytecount: u64) -> i64 {
    let r = match func {
        0 => read_ldt(p, at, bytecount),
        1 => write_ldt(p, at, bytecount, true),
        2 => {
            let n = bytecount.min(DEFAULT_LDT_BYTES);
            clear_user(p, at, n).map_or_else(|e| e, |_| n as i64)
        }
        0x11 => write_ldt(p, at, bytecount, false),
        _ => -ENOSYS,
    };
    r as i32 as u32 as i64
}

// Linux wires `modify_ldt` (154) as a 64-bit syscall, but NOT `set_thread_area` (205) or `get_thread_area`
// (211): their syscall_64.tbl rows have no handler, so a 64-bit caller gets -ENOSYS (verified on CachyOS
// 7.3.0). V2 matches: only modify_ldt is registered here; all three are reachable through int 0x80.
fn sys_modify_ldt(c: &mut SysCtx<'_>) -> i64 {
    modify_ldt(c.proc, c.args[0] as i32, c.args[1], c.args[2])
}
syscall_nr!(MODIFY_LDT, "modify_ldt", sys_modify_ldt);
const _: () = assert!(SET_THREAD_AREA == 205 && GET_THREAD_AREA == 211);

/// The int 0x80 arguments (ebx, ecx, edx, esi, edi, ebp), zero-extended as `compat_ptr` and the 32-bit ABI give
/// them.
fn args32(u: &UserContext) -> [u64; 6] {
    let r = &u.regs;
    [r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rbp].map(|v| v as u32 as u64)
}

/// seccomp for an int 0x80 call (`populate_seccomp_data` with the i386 arch): true when the call must not run
/// (its answer is set or the thread is ending). The actions are those of `seccomp_sys::entry`.
fn seccomp_stops(p: &Arc<Process>, t: &Arc<Thread>, user: &mut UserContext, nr: u32, args: &[u64; 6], nr64: usize) -> bool {
    let d = Data { nr, arch: AUDIT_ARCH_I386, ip: user.regs.rip, args: *args };
    let s = t.prctl().seccomp();
    match s.check(&d) {
        Verdict::Allow { .. } => false,
        Verdict::Errno { errno, .. } => {
            let ret = -(errno as i64);
            user.set_syscall_return(ret);
            proc::deliver::syscall_exit(t, user, nr64, ret);
            true
        }
        Verdict::Trap { data, .. } => {
            proc::deliver::force_seccomp_trap(t, data, d.ip, d.nr, d.arch);
            let rax = user.regs.rax as i64;
            proc::deliver::syscall_exit(t, user, nr64, rax);
            true
        }
        v @ (Verdict::KillThread { .. } | Verdict::KillProcess { .. }) => {
            let (data, whole) = match v {
                Verdict::KillProcess { data } => (data, true),
                Verdict::KillThread { data } => (data, t.group().thread_count() <= 1),
                _ => (0, true),
            };
            s.set_dead();
            klog::event!(Warn, "seccomp", "kill", pid = p.pid, tid = t.tid(), nr = nr, arch = klog::Hex(AUDIT_ARCH_I386 as u64), data = data, process = whole);
            if whole {
                t.group().kill(uapi::ab::signal::SIGSYS);
            } else {
                t.exit_thread(uapi::ab::signal::SIGSYS);
            }
            true
        }
        Verdict::StrictKill => {
            s.set_dead();
            if t.group().thread_count() > 1 {
                t.exit_thread(uapi::ab::signal::SIGKILL);
            } else {
                t.group().kill(uapi::ab::signal::SIGKILL);
            }
            true
        }
    }
}

/// `int 0x80` (`do_int80_syscall_32`): number in eax, arguments in ebx..ebp, the answer in rax. Only the three
/// TLS/LDT calls; every other number is -ENOSYS, logged once per number as unknown 64-bit numbers are.
pub(crate) fn int80(p: &Arc<Process>, t: &Arc<Thread>, user: &mut UserContext) {
    INT80_CALLS.inc();
    let nr = user.regs.rax as u32;
    let a = args32(user);
    let nr64 = match nr {
        NR32_SET_THREAD_AREA => SET_THREAD_AREA,
        NR32_GET_THREAD_AREA => GET_THREAD_AREA,
        NR32_MODIFY_LDT => MODIFY_LDT,
        _ => usize::MAX,
    };
    if t.prctl().seccomp().mode() != 0 && seccomp_stops(p, t, user, nr, &a, nr64) {
        return;
    }
    let ret = match nr64 {
        SET_THREAD_AREA => set_thread_area(p, user, a[0]),
        GET_THREAD_AREA => get_thread_area(p, a[0]),
        MODIFY_LDT => modify_ldt(p, a[0] as i32, a[1], a[2]),
        _ => {
            INT80_ENOSYS.inc();
            note_enosys(p, nr);
            -ENOSYS
        }
    };
    user.set_syscall_return(ret);
    proc::deliver::syscall_exit(t, user, nr64, ret);
}

/// One Info event per 32-bit number that answered -ENOSYS (numbers above 1023 share one).
fn note_enosys(p: &Process, nr: u32) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static SEEN: [AtomicU64; 17] = [const { AtomicU64::new(0) }; 17];
    let (w, bit) = ((nr.min(1024) / 64) as usize, 1u64 << (nr.min(1024) % 64));
    if SEEN[w].fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        klog::event!(Info, "syscall", "int80.enosys", nr = nr, program = p.name.as_str(); "V2 answers int 0x80 only for set_thread_area, get_thread_area and modify_ldt");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ud(base: u32, limit: u32, flags: u32) -> UserDesc {
        UserDesc { entry_number: u32::MAX, base_addr: base, limit, flags }
    }

    #[test]
    fn descriptors_as_fill_ldt_builds_them() {
        // Wine's fs selector: 32-bit, 4 GiB in pages, writable data.
        let u = ud(0x1234_5678, 0xFFFFF, F_32BIT | F_PAGES);
        let d = u.descriptor();
        assert_eq!(d, 0x12CF_F334_5678_FFFF);
        assert!(segs::desc_loadable(d, 3));
        assert_eq!(UserDesc::of_descriptor(12, d), UserDesc { entry_number: 12, ..u });
        // read_exec_only data, expand-down, useable.
        let v = ud(0x10, 0x20, F_32BIT | F_READ_EXEC_ONLY | 1 << 1 | F_USEABLE);
        let e = v.descriptor();
        assert_eq!(e >> 40 & 0xF, 0b0101, "read-only, expand-down, accessed");
        assert_eq!(UserDesc::of_descriptor(13, e).flags, v.flags);
        // An empty slot reads back as LDT_empty.
        assert!(UserDesc::of_descriptor(14, 0).is_empty());
        // lm never reaches a descriptor.
        assert_eq!(ud(0, 0xFFFFF, F_32BIT | F_LM).descriptor() >> 53 & 1, 0);
        let b = u.to_bytes();
        assert_eq!(UserDesc::from_bytes(&b), u);
    }

    #[test]
    fn tls_rules() {
        assert!(tls_ok(&ud(0, 0, F_READ_EXEC_ONLY | F_NOT_PRESENT)), "LDT_empty");
        assert!(tls_ok(&ud(0, 0, 0)), "LDT_zero");
        assert!(tls_ok(&ud(0, 0, F_LM)), "lm is not looked at");
        assert!(tls_ok(&ud(0x1000, 0xFFFFF, F_32BIT | F_PAGES)));
        assert!(!tls_ok(&ud(0x1000, 0xFFFF, 0)), "16-bit");
        assert!(!tls_ok(&ud(0x1000, 0xFFFF, F_32BIT | 2 << 1)), "code");
        assert!(!tls_ok(&ud(0x1000, 0xFFFF, F_32BIT | F_NOT_PRESENT)), "not present");
    }

    #[test]
    fn ldt_rules() {
        let mut u = ud(0x1000, 0xFFF, F_32BIT);
        u.entry_number = 8192;
        assert_eq!(ldt_descriptor(&u, false), Err(-EINVAL));
        u.entry_number = 3;
        assert_eq!(ldt_descriptor(&u, false), Ok(u.descriptor()));
        let mut c = ud(0x1000, 0xFFF, F_32BIT | 3 << 1);
        c.entry_number = 1;
        assert_eq!(ldt_descriptor(&c, false), Err(-EINVAL), "conforming code must be not present");
        c.flags |= F_NOT_PRESENT;
        assert!(ldt_descriptor(&c, false).is_ok());
        assert_eq!(ldt_descriptor(&c, true), Err(-EINVAL), "never in the old mode");
        let mut z = ud(0, 0, F_32BIT | F_USEABLE);
        z.entry_number = 0;
        assert_eq!(ldt_descriptor(&z, true), Ok(0), "oldmode: base 0 and limit 0 clear");
        assert_ne!(ldt_descriptor(&z, false), Ok(0), "mode 0x11: a real (empty-sized) segment");
        let mut a = ud(0x1000, 0xFFF, F_32BIT | F_USEABLE);
        a.entry_number = 0;
        assert_eq!(ldt_descriptor(&a, true).unwrap() >> 52 & 1, 0, "oldmode drops avl");
        assert_eq!(ldt_descriptor(&a, false).unwrap() >> 52 & 1, 1);
        let at0 = |u: UserDesc| UserDesc { entry_number: 0, ..u };
        assert!(ldt_descriptor(&at0(ud(0x1000, 0xFFFF, 0)), false).is_ok(), "16-bit with CONFIG_X86_16BIT");
        assert_eq!(ldt_descriptor(&at0(ud(0, 0, F_READ_EXEC_ONLY | F_NOT_PRESENT)), false), Ok(0), "LDT_empty clears");
    }

    #[test]
    fn int80_arguments_are_32_bit() {
        let mut u = UserContext::default();
        u.regs.rbx = 0xFFFF_FFFF_0000_1234;
        u.regs.rbp = 0x1_0000_0002;
        assert_eq!(args32(&u), [0x1234, 0, 0, 0, 0, 2]);
    }
}

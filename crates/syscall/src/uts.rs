//! AB-7b: the host and domain name (linux-6.6/kernel/sys.c `sethostname` :1343, `setdomainname` :1395,
//! kernel/utsname_sysctl.c): read by uname(2) and `/proc/sys/kernel/{hostname,domainname}`, changed by root
//! through the two calls or those files. SX-2: the pair is the calling process's UTS namespace's
//! (`proc::ns::misc::UtsNs`; kernel threads and the boot use the initial one).

use crate::SysCtx;
use uapi::errno::*;

/// `__NEW_UTS_LEN`: the ABI's field size (65 bytes with the NUL).
pub(crate) const UTS_LEN: usize = 64;

const fn field(s: &str) -> [u8; UTS_LEN + 1] {
    let mut f = [0u8; UTS_LEN + 1];
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && i < UTS_LEN {
        f[i] = b[i];
        i += 1;
    }
    f
}

/// The UTS namespace of the calling process (the initial one outside a process).
fn current_uts() -> alloc::sync::Arc<proc::ns::misc::UtsNs> {
    proc::current().map_or_else(|| proc::ns::misc::init_uts().clone(), |p| p.nsset().uts.clone())
}

pub(crate) const HOST: usize = 0;
pub(crate) const DOMAIN: usize = 1;

/// One name up to its first NUL.
pub(crate) fn name(which: usize) -> alloc::vec::Vec<u8> {
    let f = current_uts().names()[which & 1];
    let n = f.iter().position(|&b| b == 0).unwrap_or(UTS_LEN);
    f[..n].to_vec()
}

/// Sets a name (the caller checked privilege and length): the bytes, the rest zeroed (`memcpy` + `memset`).
pub(crate) fn set(which: usize, v: &[u8]) {
    let n = v.len().min(UTS_LEN);
    let ns = current_uts();
    ns.set(which, &v[..n]);
    klog::event!(Info, "syscall", "uts.set", which = which, len = n, ns = ns.inum);
}

/// `struct new_utsname`: sysname, nodename, release, version, machine, domainname (65 bytes each).
pub(crate) fn bytes() -> [u8; 390] {
    let names = current_uts().names();
    let mut b = [0u8; 390];
    let fixed = [field("Linux"), names[HOST], field(crate::info::RELEASE), field(crate::info::VERSION), field("x86_64"), names[DOMAIN]];
    for (i, f) in fixed.iter().enumerate() {
        b[i * 65..i * 65 + 65].copy_from_slice(f);
    }
    b
}

/// `sethostname`/`setdomainname`: EPERM without CAP_SYS_ADMIN (root), EINVAL for a length above 64 (or
/// negative), EFAULT for an unreadable buffer; the name may hold any bytes.
fn set_from_user(c: &mut SysCtx<'_>, which: usize) -> i64 {
    if !sys_admin(c) {
        klog::event!(Info, "syscall", "uts.set_denied", which = which, uid = c.proc.cred().euid);
        return -EPERM;
    }
    // `int len`: the upper half of the register is not looked at.
    let len = c.args[1] as u32 as i32;
    if !(0..=UTS_LEN as i32).contains(&len) {
        klog::event!(Info, "syscall", "uts.set_bad_len", which = which, len = len);
        return -EINVAL;
    }
    let mut v = alloc::vec![0u8; len as usize];
    if c.proc.mm.copy_from_user(&mut v, c.args[0]).is_err() {
        klog::event!(Info, "syscall", "uts.set_fault", which = which);
        return -EFAULT;
    }
    set(which, &v);
    0
}

/// `ns_capable(uts_ns->user_ns, CAP_SYS_ADMIN)` within the thread's bounding set (as `/proc/<pid>/status`
/// shows it, AB-7a).
pub(crate) fn sys_admin(c: &SysCtx<'_>) -> bool {
    use proc::cred::Cap;
    let uts = c.proc.nsset().uts.clone();
    proc::cred::ns_capable(&c.proc.cred(), &uts.user_ns, Cap::SysAdmin) && c.thread.prctl().cap_bset() & (1u64 << Cap::SysAdmin as u32) != 0
}

fn sys_sethostname(c: &mut SysCtx<'_>) -> i64 {
    set_from_user(c, HOST)
}
crate::syscall_nr!(uapi::ab::nr_more::SETHOSTNAME, "sethostname", sys_sethostname);

fn sys_setdomainname(c: &mut SysCtx<'_>) -> i64 {
    set_from_user(c, DOMAIN)
}
crate::syscall_nr!(uapi::ab::nr_more::SETDOMAINNAME, "setdomainname", sys_setdomainname);

//! The socket syscalls (NW-5, glue granted to NW): each number maps a `SysCtx` onto `vfs::sys::Sys` and calls
//! `net::sock::sys`, where the calls live with Linux's argument checks and their host tests. The kernel side
//! the socket layer needs from above it (kill check, SIGPIPE, pid lookup, the calling program) is installed as
//! `net::sock::Hooks` on the first call.

use crate::SysCtx;
use alloc::vec::Vec;
use net::sock::sys as s;
use proc::Process;
use uapi::errno::*;
use vfs::Errno;
use vfs::sys::{Args, Sys, SysFn, UserMem};

/// The calling process's memory for `vfs::sys` (also SX-2's chroot path argument).
pub(crate) struct ProcMem<'a>(pub(crate) &'a Process);

impl UserMem for ProcMem<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        self.0.mm.copy_from_user(buf, addr).is_ok()
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.0.mm.copy_to_user(addr, data).is_ok()
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

/// A blocking socket call gives up: the program was killed, or (AB-6 CR7, INT-A2a) a signal it handles is
/// pending. The EINTR becomes -ERESTARTSYS at the syscall exit (`proc::deliver::restart_class`), so SA_RESTART
/// restarts it; NW's own -EINTR for calls with SO_RCVTIMEO/SO_SNDTIMEO (Linux `sock_intr_errno`) is still open.
fn killed() -> bool {
    proc::current().is_some_and(|p| p.has_exited()) || proc::current_signal_pending()
}

/// SIGPIPE to the calling thread (Linux `send_sig(SIGPIPE, current, 0)`; INT-A2a, as FS-3's pipes do).
fn sigpipe() {
    proc::signal_current_thread(net::sock::abi::SIGPIPE);
}

fn pid_exists(pid: u32) -> bool {
    proc::find(pid).is_some()
}

fn current() -> Option<net::sock::Caller> {
    // AB-13: the effective ids (`cred_to_ucred`; root = the CAP_NET_* checks of NW).
    proc::current().map(|p| {
        let k = p.cred();
        net::sock::Caller::new(p.pid, k.euid, k.egid, p.subject())
    })
}

static HOOKS: net::sock::Hooks = net::sock::Hooks { interrupted: killed, sigpipe, pid_exists, current };

/// NW-11: a calling program's pid and user namespaces as the socket layer asks for them (`pid_vnr`,
/// `find_vpid`, `make_kuid`, `from_kuid_munged`, `ns_capable`).
struct ProcIds {
    cred: alloc::sync::Arc<proc::cred::Cred>,
    pid_ns: alloc::sync::Arc<proc::ns::pid::PidNs>,
    /// The user namespace that owns the caller's network namespace (CAP_NET_* count there).
    net_owner: alloc::sync::Arc<proc::ns::user::UserNs>,
}

impl net::sock::Ids for ProcIds {
    fn capable(&self, cap: u32) -> bool {
        use net::sock::cap;
        let ns: &proc::ns::user::UserNs = match cap {
            cap::NET_ADMIN | cap::NET_BIND_SERVICE | cap::NET_RAW => &self.net_owner,
            cap::SYS_ADMIN => &self.pid_ns.user_ns,
            _ => &self.cred.user_ns,
        };
        proc::cred::ns_capable_raw(&self.cred, ns, cap)
    }
    fn capable_initial(&self, cap: u32) -> bool {
        // NW-12: `capable` (SIOCSIFTXQLEN): over the initial user namespace whatever the caller's are.
        proc::cred::ns_capable_raw(&self.cred, proc::ns::user::init_user_ns(), cap)
    }
    fn pid_to_kernel(&self, pid: u32) -> Option<u32> {
        self.pid_ns.id(pid)
    }
    fn pid_from_kernel(&self, kpid: u32) -> u32 {
        self.pid_ns.nr(kpid).unwrap_or(0)
    }
    fn uid_to_kernel(&self, uid: u32) -> Option<u32> {
        self.cred.user_ns.to_kernel(proc::ns::user::MapKind::Uid, uid)
    }
    fn gid_to_kernel(&self, gid: u32) -> Option<u32> {
        self.cred.user_ns.to_kernel(proc::ns::user::MapKind::Gid, gid)
    }
    fn uid_from_kernel(&self, kuid: u32) -> u32 {
        self.cred.user_ns.uid(kuid)
    }
    fn gid_from_kernel(&self, kgid: u32) -> u32 {
        self.cred.user_ns.gid(kgid)
    }
}

/// NW-11: the calling program's network namespace view, ids and real/effective/saved uid and gid (for
/// SCM_CREDENTIALS, `scm_check_creds`); no ids for a program whose namespaces are all the initial ones (the
/// socket layer then works exactly as without namespaces).
fn ns_current() -> net::sock::NsNow {
    let Some(p) = proc::current() else { return net::sock::NsNow::default() };
    let cred = p.cred();
    let ids = net::sock::CredIds { uid: [cred.uid, cred.euid, cred.suid], gid: [cred.gid, cred.egid, cred.sgid] };
    // INT-W20 (NW-11 CR 4, in short): while no namespace besides the initial ones exists, the caller is in the
    // initial ones; three atomic loads instead of the namespace set's lock.
    if proc::ns::user::live() == 0 && proc::ns::pid::live() == 0 && proc::ns::misc::live().2 == 0 {
        return net::sock::NsNow { net: None, ids: None, cred: Some(ids) };
    }
    let set = p.nsset();
    let pid_ns = &p.group().pid_ns;
    let mut now = net::sock::NsNow { net: set.net.view.clone(), ids: None, cred: Some(ids) };
    if !(cred.user_ns.is_init() && pid_ns.is_init() && set.net.user_ns.is_init()) {
        let ids = ProcIds { cred, pid_ns: pid_ns.clone(), net_owner: set.net.user_ns.clone() };
        now.ids = Some(alloc::sync::Arc::new(ids));
    }
    now
}

static NS_HOOKS: net::sock::NsHooks = net::sock::NsHooks { current: ns_current };


fn run(c: &mut SysCtx<'_>, f: SysFn) -> i64 {
    net::sock::set_hooks(&HOOKS);
    net::sock::set_ns_hooks(&NS_HOOKS);
    vfs::dentry::set_interrupt_check(killed);
    let p = c.proc;
    let mem = ProcMem(p);
    let fs = c.thread.fs(p);
    let ns = p.mount_ns();
    let sys = Sys { mem: &mem, fds: &p.fds, fs: &fs, ns: &ns, cred: p.fs_cred(), subject: p.subject() };
    let a: Args = c.args;
    f(&sys, a)
}

/// NW-6: the interface ioctls of a socket fd, asked by `ioctl.rs` before the file's own ioctl: Some(result) when
/// `fd` is a socket and `cmd` one of them.
pub(crate) fn dev_ioctl(c: &mut SysCtx<'_>, fd: usize, cmd: u64, arg: u64) -> Option<i64> {
    net::sock::set_ns_hooks(&NS_HOOKS);
    let p = c.proc;
    let mem = ProcMem(p);
    let fs = c.thread.fs(p);
    let ns = p.mount_ns();
    let sys = Sys { mem: &mem, fds: &p.fds, fs: &fs, ns: &ns, cred: p.fs_cred(), subject: p.subject() };
    s::dev_ioctl(&sys, [fd as u64, cmd, arg, 0, 0, 0])
}

macro_rules! sock_call {
    ($NR:ident, $f:path) => {
        const _: () = {
            fn h(c: &mut SysCtx<'_>) -> i64 {
                run(c, $f)
            }
            crate::syscall!($NR, h);
        };
    };
    ($nr:literal, $name:literal, $f:path) => {
        const _: () = {
            fn h(c: &mut SysCtx<'_>) -> i64 {
                run(c, $f)
            }
            crate::syscall_nr!($nr, $name, h);
        };
    };
}

sock_call!(SOCKET, s::socket);
sock_call!(CONNECT, s::connect);
sock_call!(ACCEPT, s::accept);
sock_call!(SENDTO, s::sendto);
sock_call!(RECVFROM, s::recvfrom);
sock_call!(SENDMSG, s::sendmsg);
sock_call!(RECVMSG, s::recvmsg);
sock_call!(SHUTDOWN, s::shutdown);
sock_call!(BIND, s::bind);
sock_call!(LISTEN, s::listen);
sock_call!(GETSOCKNAME, s::getsockname);
sock_call!(GETPEERNAME, s::getpeername);
sock_call!(SOCKETPAIR, s::socketpair);
/// setsockopt/getsockopt: an option the stack does not know is logged once (YT-4a CR 11).
fn sockopt(c: &mut SysCtx<'_>, f: SysFn) -> i64 {
    let r = run(c, f);
    crate::note_gap(1, (c.args[1] as u32 as u64) << 32 | c.args[2] as u32 as u64, r, c.proc.name.as_str());
    r
}
fn setsockopt(c: &mut SysCtx<'_>) -> i64 {
    sockopt(c, s::setsockopt)
}
fn getsockopt(c: &mut SysCtx<'_>) -> i64 {
    sockopt(c, s::getsockopt)
}
crate::syscall!(SETSOCKOPT, setsockopt);
crate::syscall!(GETSOCKOPT, getsockopt);
sock_call!(288, "accept4", s::accept4);
sock_call!(299, "recvmmsg", s::recvmmsg);
sock_call!(307, "sendmmsg", s::sendmmsg);

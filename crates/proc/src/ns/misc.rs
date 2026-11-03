//! SX-2: the UTS, IPC and network namespaces. Each belongs to the user namespace that was current when it was
//! made (its capabilities decide `sethostname`, `setns`), and lives while a group's set names it.
//!
//! - UTS (linux-6.6/kernel/utsname.c `copy_utsname`): a copy of the creator's host and domain name; the
//!   syscall crate's `uts` reads and writes the caller's.
//! - IPC (ipc/namespace.c): V2 has no SysV IPC or POSIX message queues (Chromium shares memory through
//!   memfds), so an IPC namespace has nothing in it but works for unshare, clone, setns and `/proc`.
//! - Network (net/core/net_namespace.c `copy_net_ns`): a new namespace has only `lo`, down and without an
//!   address, which is where Linux leaves it. `is_init` tells the socket glue which rules apply.

use super::count::{self, Charge, Counted};
use super::user::UserNs;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::{Once, SpinLock};
use vfs::Errno;

/// `__NEW_UTS_LEN` + the NUL: one field of `struct new_utsname`.
pub const UTS_FIELD: usize = 65;
pub type UtsNames = [[u8; UTS_FIELD]; 2];

const fn field(s: &str) -> [u8; UTS_FIELD] {
    let mut f = [0u8; UTS_FIELD];
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && i < UTS_FIELD - 1 {
        f[i] = b[i];
        i += 1;
    }
    f
}

/// Namespaces besides the initial ones that exist now, by kind (the lifetime ktest).
static LIVE_UTS: AtomicU64 = AtomicU64::new(0);
static LIVE_IPC: AtomicU64 = AtomicU64::new(0);
static LIVE_NET: AtomicU64 = AtomicU64::new(0);

pub struct UtsNs {
    pub inum: u32,
    pub user_ns: Arc<UserNs>,
    init: bool,
    /// (nodename, domainname), NUL-padded.
    names: SpinLock<UtsNames>,
    _charge: Option<Charge>,
}

impl UtsNs {
    pub fn names(&self) -> UtsNames {
        *self.names.lock()
    }
    /// One of the two names (0 host, 1 domain) set to `v` (the caller checked length and privilege).
    pub fn set(&self, which: usize, v: &[u8]) {
        let mut f = [0u8; UTS_FIELD];
        let n = v.len().min(UTS_FIELD - 1);
        f[..n].copy_from_slice(&v[..n]);
        self.names.lock()[which & 1] = f;
    }
    pub fn is_init(&self) -> bool {
        self.init
    }
}

impl Drop for UtsNs {
    fn drop(&mut self) {
        if !self.init {
            LIVE_UTS.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

pub struct IpcNs {
    pub inum: u32,
    pub user_ns: Arc<UserNs>,
    init: bool,
    _charge: Option<Charge>,
}

impl Drop for IpcNs {
    fn drop(&mut self) {
        if !self.init {
            LIVE_IPC.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

pub struct NetNs {
    pub inum: u32,
    pub user_ns: Arc<UserNs>,
    init: bool,
    _charge: Option<Charge>,
    /// NW-11: the socket layer's view of this namespace (its lo, ports, abstract names, netlink ids); None for
    /// the initial namespace, whose sockets keep the socket layer's own tables.
    pub view: net::netns::Net,
}

impl NetNs {
    pub fn is_init(&self) -> bool {
        self.init
    }
}

impl Drop for NetNs {
    fn drop(&mut self) {
        if !self.init {
            LIVE_NET.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

static INIT_UTS: Once<Arc<UtsNs>> = Once::new();
static INIT_IPC: Once<Arc<IpcNs>> = Once::new();
static INIT_NET: Once<Arc<NetNs>> = Once::new();

pub fn init_uts() -> &'static Arc<UtsNs> {
    INIT_UTS.get_or_init(|| {
        Arc::new(UtsNs { inum: super::INIT_UTS_INUM, user_ns: super::user::init_user_ns().clone(), init: true, names: SpinLock::named("proc.uts", [field("bitsos"), field("(none)")]), _charge: None })
    })
}

pub fn init_ipc() -> &'static Arc<IpcNs> {
    INIT_IPC.get_or_init(|| Arc::new(IpcNs { inum: super::INIT_IPC_INUM, user_ns: super::user::init_user_ns().clone(), init: true, _charge: None }))
}

pub fn init_net() -> &'static Arc<NetNs> {
    INIT_NET.get_or_init(|| Arc::new(NetNs { inum: super::alloc_inum(), user_ns: super::user::init_user_ns().clone(), init: true, _charge: None, view: None }))
}

/// `copy_utsname`: the names of `from`, owned by `user_ns`, made by `euid` (ENOSPC past its
/// `max_uts_namespaces`, `count`).
pub(crate) fn new_uts(from: &UtsNs, user_ns: &Arc<UserNs>, euid: u32) -> Result<Arc<UtsNs>, Errno> {
    let c = count::charge(Counted::Uts, user_ns, euid)?;
    LIVE_UTS.fetch_add(1, Ordering::Relaxed);
    Ok(Arc::new(UtsNs { inum: super::alloc_inum(), user_ns: user_ns.clone(), init: false, names: SpinLock::named("proc.uts", from.names()), _charge: Some(c) }))
}

/// `create_ipc_ns` (ENOSPC past `max_ipc_namespaces`).
pub(crate) fn new_ipc(user_ns: &Arc<UserNs>, euid: u32) -> Result<Arc<IpcNs>, Errno> {
    let c = count::charge(Counted::Ipc, user_ns, euid)?;
    LIVE_IPC.fetch_add(1, Ordering::Relaxed);
    Ok(Arc::new(IpcNs { inum: super::alloc_inum(), user_ns: user_ns.clone(), init: false, _charge: Some(c) }))
}

/// `copy_net_ns` (ENOSPC past `max_net_namespaces`).
pub(crate) fn new_net(user_ns: &Arc<UserNs>, euid: u32) -> Result<Arc<NetNs>, Errno> {
    let c = count::charge(Counted::Net, user_ns, euid)?;
    LIVE_NET.fetch_add(1, Ordering::Relaxed);
    let inum = super::alloc_inum();
    Ok(Arc::new(NetNs { inum, user_ns: user_ns.clone(), init: false, _charge: Some(c), view: Some(net::netns::NetView::new(inum)) }))
}

/// (UTS, IPC, network) namespaces besides the initial ones that exist now.
pub fn live() -> (u64, u64, u64) {
    (LIVE_UTS.load(Ordering::Relaxed), LIVE_IPC.load(Ordering::Relaxed), LIVE_NET.load(Ordering::Relaxed))
}

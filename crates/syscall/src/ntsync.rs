//! SC-8: `/dev/ntsync` as Linux's `drivers/misc/ntsync.c` (6.14, with 7.3's wait-owner rule; misc device, mode
//! 0666; no time namespaces in V2, so 7.2's `timens_ktime_to_host` changes nothing): each open is a
//! device; CREATE_SEM/CREATE_MUTEX/CREATE_EVENT on it answer a new close-on-exec O_RDWR fd
//! (`anon_inode:ntsync`) for the object; WAIT_ANY/WAIT_ALL on it wait for objects given by fd; the object
//! ioctls go to the object's fd. The objects and waits are `sched::ntsync`; this file is the ABI: ioctl
//! numbers, argument structs (`include/uapi/linux/ntsync.h`), user memory, fds, and errno order.
//!
//! The VFS has no downcast for `dyn File`: the registry below finds the device or object behind an fd by its
//! address (compared only, as `drm::sync_file`).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use ksync::SpinLock;
use sched::futex::Deadline;
use sched::ntsync::{self as nt, Device, EOWNERDEAD, MAX_WAIT_COUNT, Obj, State};
use uapi::errno::*;
use vfs::fd::{FdTable, OpenFile};
use vfs::sys::UserMem;
use vfs::{Errno, File};

const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | ((b'N' as u32) << 8) | nr
}
const W: u32 = 1;
const R: u32 = 2;
const WR: u32 = 3;
pub const NTSYNC_IOC_CREATE_SEM: u32 = ioc(W, 0x80, 8);
pub const NTSYNC_IOC_SEM_RELEASE: u32 = ioc(WR, 0x81, 4);
pub const NTSYNC_IOC_WAIT_ANY: u32 = ioc(WR, 0x82, WAIT_ARGS as u32);
pub const NTSYNC_IOC_WAIT_ALL: u32 = ioc(WR, 0x83, WAIT_ARGS as u32);
pub const NTSYNC_IOC_CREATE_MUTEX: u32 = ioc(W, 0x84, 8);
pub const NTSYNC_IOC_MUTEX_UNLOCK: u32 = ioc(WR, 0x85, 8);
pub const NTSYNC_IOC_MUTEX_KILL: u32 = ioc(W, 0x86, 4);
pub const NTSYNC_IOC_CREATE_EVENT: u32 = ioc(W, 0x87, 8);
pub const NTSYNC_IOC_EVENT_SET: u32 = ioc(R, 0x88, 4);
pub const NTSYNC_IOC_EVENT_RESET: u32 = ioc(R, 0x89, 4);
pub const NTSYNC_IOC_EVENT_PULSE: u32 = ioc(R, 0x8a, 4);
pub const NTSYNC_IOC_SEM_READ: u32 = ioc(R, 0x8b, 8);
pub const NTSYNC_IOC_MUTEX_READ: u32 = ioc(R, 0x8c, 8);
pub const NTSYNC_IOC_EVENT_READ: u32 = ioc(R, 0x8d, 8);
pub const NTSYNC_WAIT_REALTIME: u32 = 1;
/// `struct ntsync_wait_args`: timeout u64, objs u64, count, index, flags, owner, alert, pad (u32 each).
pub const WAIT_ARGS: usize = 40;
const WAIT_INDEX: u64 = 20;

/// The misc device's number as CachyOS shows it (`MISC_DYNAMIC_MINOR`: Linux picks it at load; 10:261 there).
pub const NTSYNC_DEV: (u32, u32) = (10, 261);
const S_IFCHR: u32 = 0o020000;

klog::counter!(DEVICES, "ntsync.devices.opened");
klog::counter!(OBJECTS, "ntsync.objects.created");

enum Entry {
    Dev(Weak<DevFile>),
    Obj(Weak<ObjFile>),
}

static FILES: SpinLock<BTreeMap<usize, Entry>> = SpinLock::named("syscall.ntsync.registry", BTreeMap::new());

fn addr_of(f: &Arc<dyn File>) -> usize {
    Arc::as_ptr(f) as *const () as usize
}
fn addr<T>(t: &T) -> usize {
    t as *const T as *const () as usize
}

/// The ntsync object behind an open file (`get_obj`'s `f_op == &ntsync_obj_fops`).
/// The registry entry of `f`, upgraded. Bound outside the lock: dropping the last reference of a file takes the
/// registry lock in its `Drop`.
fn entry(f: &Arc<dyn File>) -> (Option<Arc<DevFile>>, Option<Arc<ObjFile>>) {
    let reg = FILES.lock();
    let e = match reg.get(&addr_of(f)) {
        Some(Entry::Dev(w)) => (w.upgrade(), None),
        Some(Entry::Obj(w)) => (None, w.upgrade()),
        None => (None, None),
    };
    drop(reg);
    e
}

fn as_obj(f: &Arc<dyn File>) -> Option<Arc<Obj>> {
    entry(f).1.map(|o| o.obj.clone())
}

/// The node in /dev: every open makes a new device (`ntsync_char_open`).
struct Node;

fn make_node() -> Arc<dyn File> {
    Arc::new(Node)
}
vfs::devnode!("/dev/ntsync", make_node);

impl File for Node {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(NTSYNC_DEV.0, NTSYNC_DEV.1)
    }
    fn open_node(&self, _flags: u64, _who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        Some(Ok(DevFile::open() as Arc<dyn File>))
    }
}

/// One open of /dev/ntsync.
pub(crate) struct DevFile {
    dev: Arc<Device>,
}

impl DevFile {
    pub(crate) fn open() -> Arc<DevFile> {
        let f = Arc::new(DevFile { dev: Device::new() });
        FILES.lock().insert(addr(&*f), Entry::Dev(Arc::downgrade(&f)));
        DEVICES.inc();
        f
    }
}

impl Drop for DevFile {
    fn drop(&mut self) {
        FILES.lock().remove(&addr(self));
    }
}

/// An object's fd.
struct ObjFile {
    obj: Arc<Obj>,
}

impl Drop for ObjFile {
    fn drop(&mut self) {
        FILES.lock().remove(&addr(self));
    }
}

/// What one ioctl works with: the caller's memory, its fd table (None in kernel threads: EMFILE for a new
/// object), and what ends a wait early.
pub(crate) struct Ctx<'a> {
    pub mem: &'a dyn UserMem,
    pub fds: Option<&'a FdTable>,
    pub intr: nt::Interrupted<'a>,
}

/// A program's wait ends for a kill (EINTR) or a signal to handle (ERESTARTSYS, as `ntsync_schedule`).
fn program_interrupted() -> Option<i64> {
    if vfs::dentry::killed() {
        Some(EINTR)
    } else if proc::current_signal_pending() {
        Some(uapi::ab::siginfo::ERESTARTSYS)
    } else {
        None
    }
}

fn with_caller<R>(mem: &dyn UserMem, f: impl FnOnce(&Ctx<'_>) -> R) -> R {
    let fds = proc::current().map(|p| p.fds.clone());
    f(&Ctx { mem, fds: fds.as_deref(), intr: &program_interrupted })
}

impl File for DevFile {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(NTSYNC_DEV.0, NTSYNC_DEV.1)
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        with_caller(mem, |c| dev_ioctl(&self.dev, cmd as u32, arg, c))
    }
}

impl File for ObjFile {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        0o600
    }
    fn attr(&self) -> Option<vfs::abi::Attr> {
        Some(vfs::eventfd::anon_attr())
    }
    fn link_name(&self) -> Option<String> {
        Some(String::from("anon_inode:ntsync"))
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        obj_ioctl(&self.obj, cmd as u32, arg, mem)
    }
}

/// The ioctl on an open file the way the dispatcher makes it, with an explicit context (ktests: a kernel
/// thread for a process). None: not an ntsync file or not its command (ENOTTY).
pub(crate) fn ioctl_with(f: &Arc<dyn File>, cmd: u32, arg: u64, c: &Ctx<'_>) -> Option<Result<i64, Errno>> {
    match entry(f) {
        (Some(d), _) => dev_ioctl(&d.dev, cmd, arg, c),
        (_, Some(o)) => obj_ioctl(&o.obj, cmd, arg, c.mem),
        _ => None,
    }
}

fn get<const N: usize>(mem: &dyn UserMem, at: u64) -> Result<[u8; N], Errno> {
    let mut b = [0u8; N];
    if mem.read(at, &mut b) { Ok(b) } else { Err(EFAULT) }
}
fn put(mem: &dyn UserMem, at: u64, data: &[u8]) -> Result<(), Errno> {
    if mem.write(at, data) { Ok(()) } else { Err(EFAULT) }
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn pair(a: u32, b: u32) -> [u8; 8] {
    let mut o = [0u8; 8];
    o[..4].copy_from_slice(&a.to_le_bytes());
    o[4..].copy_from_slice(&b.to_le_bytes());
    o
}

/// `ntsync_obj_get_fd`: `get_unused_fd_flags(O_CLOEXEC)` + an O_RDWR anonymous file.
fn install(fds: Option<&FdTable>, obj: Arc<Obj>) -> Result<i64, Errno> {
    let fds = fds.ok_or(EMFILE)?;
    let f = Arc::new(ObjFile { obj });
    FILES.lock().insert(addr(&*f), Entry::Obj(Arc::downgrade(&f)));
    let n = fds.install_flags(OpenFile::anon(f as Arc<dyn File>, vfs::abi::O_RDWR), 0, true)?;
    OBJECTS.inc();
    Ok(n as i64)
}

fn dev_ioctl(dev: &Arc<Device>, cmd: u32, arg: u64, c: &Ctx<'_>) -> Option<Result<i64, Errno>> {
    let create = |st: Result<State, Errno>| -> Result<i64, Errno> { install(c.fds, dev.create(st?)?) };
    Some(match cmd {
        NTSYNC_IOC_CREATE_SEM => create(get::<8>(c.mem, arg).map(|b| State::Sem { count: u32_at(&b, 0), max: u32_at(&b, 4) })),
        NTSYNC_IOC_CREATE_MUTEX => create(get::<8>(c.mem, arg).map(|b| State::Mutex { owner: u32_at(&b, 0), count: u32_at(&b, 4), dead: false })),
        NTSYNC_IOC_CREATE_EVENT => create(get::<8>(c.mem, arg).map(|b| State::Event { manual: u32_at(&b, 0) != 0, signaled: u32_at(&b, 4) != 0 })),
        NTSYNC_IOC_WAIT_ANY => wait(dev, arg, false, c),
        NTSYNC_IOC_WAIT_ALL => wait(dev, arg, true, c),
        _ => return None,
    })
}

/// `setup_wait` + `ntsync_wait_any/all`: the argument checks in Linux's order, then the wait; the index of
/// the signaled object is written back (also with EOWNERDEAD).
fn wait(dev: &Arc<Device>, arg: u64, all: bool, c: &Ctx<'_>) -> Result<i64, Errno> {
    let a = get::<WAIT_ARGS>(c.mem, arg)?;
    let timeout = u64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
    let objs = u64::from_le_bytes([a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15]]);
    let (count, flags, owner, alert, pad) = (u32_at(&a, 16), u32_at(&a, 24), u32_at(&a, 28), u32_at(&a, 32), u32_at(&a, 36));
    // Owner 0 is EINVAL since Linux 7.3 (6.14 to 7.2 let a wait take a mutex for owner 0); CachyOS has 7.3.
    if pad != 0 || flags & !NTSYNC_WAIT_REALTIME != 0 || owner == 0 || count as usize > MAX_WAIT_COUNT {
        return Err(EINVAL);
    }
    let mut raw = Vec::new();
    raw.resize(count as usize * 4, 0u8);
    if !raw.is_empty() && !c.mem.read(objs, &mut raw) {
        return Err(EFAULT);
    }
    let mut fdl: Vec<u32> = raw.chunks_exact(4).map(|b| u32_at(b, 0)).collect();
    if alert != 0 {
        fdl.push(alert);
    }
    let fds = c.fds.ok_or(EINVAL)?;
    let mut got: Vec<Arc<Obj>> = Vec::with_capacity(fdl.len());
    for fd in fdl {
        // fget: an O_PATH file or a closed fd is no object; an object of another device neither.
        let f = fds.get(fd as usize).ok().filter(|f| !f.is_path_only()).ok_or(EINVAL)?;
        let o = as_obj(&f.file).filter(|o| Arc::ptr_eq(o.device(), dev)).ok_or(EINVAL)?;
        if all && got.iter().any(|g| Arc::ptr_eq(g, &o)) {
            return Err(EINVAL);
        }
        got.push(o);
    }
    let alert_obj = if alert != 0 { got.pop() } else { None };
    let deadline = match timeout {
        u64::MAX => None,
        t if flags & NTSYNC_WAIT_REALTIME != 0 => Some(Deadline::Realtime(t)),
        t => Some(Deadline::Monotonic(t)),
    };
    let req = nt::Wait { objs: &got, alert: alert_obj.as_ref(), owner, all, deadline };
    let (index, dead) = nt::wait(dev, &req, c.intr)?;
    put(c.mem, arg + WAIT_INDEX, &index.to_le_bytes())?;
    if dead { Err(EOWNERDEAD) } else { Ok(0) }
}

fn obj_ioctl(obj: &Obj, cmd: u32, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
    use sched::ntsync::Kind;
    let kind = |k: Kind| if obj.kind() == k { Ok(()) } else { Err(EINVAL) };
    Some(match cmd {
        NTSYNC_IOC_SEM_RELEASE => get::<4>(mem, arg).and_then(|b| obj.sem_release(u32_at(&b, 0))).and_then(|prev| put(mem, arg, &prev.to_le_bytes())).map(|_| 0),
        NTSYNC_IOC_MUTEX_UNLOCK => get::<8>(mem, arg).and_then(|b| obj.mutex_unlock(u32_at(&b, 0))).and_then(|prev| put(mem, arg + 4, &prev.to_le_bytes())).map(|_| 0),
        NTSYNC_IOC_MUTEX_KILL => get::<4>(mem, arg).and_then(|b| obj.mutex_kill(u32_at(&b, 0))).map(|_| 0),
        NTSYNC_IOC_EVENT_SET | NTSYNC_IOC_EVENT_PULSE => obj.event_set(cmd == NTSYNC_IOC_EVENT_PULSE).and_then(|prev| put(mem, arg, &(prev as u32).to_le_bytes())).map(|_| 0),
        NTSYNC_IOC_EVENT_RESET => obj.event_reset().and_then(|prev| put(mem, arg, &(prev as u32).to_le_bytes())).map(|_| 0),
        NTSYNC_IOC_SEM_READ | NTSYNC_IOC_MUTEX_READ | NTSYNC_IOC_EVENT_READ => {
            let want = match cmd {
                NTSYNC_IOC_SEM_READ => Kind::Sem,
                NTSYNC_IOC_MUTEX_READ => Kind::Mutex,
                _ => Kind::Event,
            };
            kind(want).and_then(|_| {
                let (out, ret) = match obj.read() {
                    State::Sem { count, max } => (pair(count, max), Ok(0)),
                    State::Mutex { count, owner, dead } => (pair(owner, count), if dead { Err(EOWNERDEAD) } else { Ok(0) }),
                    State::Event { manual, signaled } => (pair(manual as u32, signaled as u32), Ok(0)),
                };
                put(mem, arg, &out).and(ret)
            })
        }
        _ => return None,
    })
}

/// The objects the device behind `f` holds (ktests: closed objects are gone); None for another file.
pub(crate) fn objects_of(f: &Arc<dyn File>) -> Option<usize> {
    entry(f).0.map(|d| d.dev.objects())
}

/// The waits under way on the device behind `f` (ktests); None for another file.
pub(crate) fn waiters_of(f: &Arc<dyn File>) -> Option<usize> {
    entry(f).0.map(|d| d.dev.waiters())
}

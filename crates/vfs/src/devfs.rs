//! devfs, procfs and sysfs at boot (FS-2): `/dev` is a tmpfs of its own ("devtmpfs") with the device nodes,
//! `/proc` and `/sys` are synthetic file systems whose names only the kernel makes (`dentry::Fixed`).
//!
//! Nodes come from three registries (link-time, no shared table): `devnode!` (a device's `File`), `procfile!`
//! (a text made per open, at any path under `/proc` or `/sys`) and `procdir!` (a computed directory attached at
//! a path: the process directories, PCI's sysfs view). FS's own devices are built in here and created first, so
//! a second registration of the same path elsewhere is refused deterministically (`vfs devnode.duplicate`);
//! the terminals take a registration of their path as their fallback (`tty`).
//!
//! The memory devices follow drivers/char/mem.c and drivers/char/random.c of Linux 6.6: null (1:3), zero
//! (1:5), full (1:7), random (1:8), urandom (1:9), all 0666, lseek answers 0 and leaves nothing to seek.

use crate::dentry::{Dentry, Fixed, SuperBlock};
use crate::inode::{Cred, Data, Inode};
use crate::mount_ns::{Mount, MountNs};
use crate::node::File;
use crate::synth::SynthDir;
use crate::tty::{TtyDev, Which};
use crate::{abi, Errno};
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mem {
    Null,
    Zero,
    Full,
    Random,
    Urandom,
}

/// A memory device (drivers/char/mem.c, random.c).
pub struct MemDev(pub Mem);

impl File for MemDev {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        match self.0 {
            Mem::Null => Ok(0),
            Mem::Zero | Mem::Full => {
                buf.fill(0);
                Ok(buf.len())
            }
            Mem::Random | Mem::Urandom => {
                crate::random::fill(buf);
                Ok(buf.len())
            }
        }
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        match self.0 {
            Mem::Null | Mem::Zero => Ok(buf.len()),
            Mem::Full => Err(ENOSPC),
            Mem::Random | Mem::Urandom => {
                crate::random::absorb(buf);
                Ok(buf.len())
            }
        }
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        let minor = match self.0 {
            Mem::Null => 3,
            Mem::Zero => 5,
            Mem::Full => 7,
            Mem::Random => 8,
            Mem::Urandom => 9,
        };
        abi::encode_dev(1, minor)
    }
    /// Positional: pread/pwrite work on them as on Linux (they are not stream files there); the data does not
    /// depend on the position.
    fn seekable(&self) -> bool {
        true
    }
    /// `null_lseek`/`zero_lseek`/`full_lseek` set the position to 0; random's `noop_llseek` leaves it at 0.
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        Some(Ok(0))
    }
    fn fsync(&self) -> Result<(), Errno> {
        Err(EINVAL)
    }
    /// FS-4: only /dev/random has a poll operation (`random_poll`); null, zero, full and urandom have none
    /// (epoll_ctl EPERM, poll sees DEFAULT_POLLMASK).
    fn can_poll(&self) -> bool {
        self.0 == Mem::Random
    }
    /// `random_poll`: EPOLLIN|EPOLLRDNORM once the generator is ready (always here: seeded at boot), no OUT.
    fn events(&self) -> crate::poll::Events {
        use crate::poll::Events;
        match self.0 {
            Mem::Random => Events(Events::IN),
            _ => Events(Events::IN | Events::OUT),
        }
    }
}

/// FS's memory devices: (path, object). All root:root 0666 as on Linux.
pub(crate) fn mem_devices() -> Vec<(&'static str, Arc<dyn File>)> {
    alloc::vec![
        ("/dev/null", Arc::new(MemDev(Mem::Null)) as Arc<dyn File>),
        ("/dev/zero", Arc::new(MemDev(Mem::Zero))),
        ("/dev/full", Arc::new(MemDev(Mem::Full))),
        ("/dev/random", Arc::new(MemDev(Mem::Random))),
        ("/dev/urandom", Arc::new(MemDev(Mem::Urandom))),
    ]
}

/// The terminal nodes and their owners. The console belongs to the user programs run as (BitsOS has no login:
/// Georg's terminal, uid 1000, group tty 5, like a Linux tty after login); `/dev/tty` is root:tty 0666.
pub const TTYS: [(Which, u32, u32); 3] = [(Which::Console, 1000, 5), (Which::Tty, 0, 5), (Which::Tty0, 1000, 5)];

/// The built-in paths (tests compare the registries against them).
pub fn builtin_paths() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = mem_devices().into_iter().map(|(p, _)| p).collect();
    v.extend(TTYS.iter().map(|t| t.0.path()));
    v
}

fn synthetic_fs(fstype: &'static str, fixed: Fixed) -> (Arc<SuperBlock>, Arc<Dentry>) {
    let sb = SuperBlock::new_fixed(fstype, fixed);
    let root = Inode::new(sb.next_ino(), uapi::S_IFDIR | 0o555, Cred::ROOT, Data::SynthDir(Arc::new(SynthDir::new())));
    let d = Dentry::new_root(sb.clone(), root);
    (sb, d)
}

/// Mounts `sb`/`root` on `path` of `ns` (the directory is made when missing).
fn mount_at(ns: &MountNs, path: &str, sb: Arc<SuperBlock>, root: Arc<Dentry>) -> Result<Arc<Mount>, Errno> {
    ns.mkdir_p(path)?;
    let at = ns.lookup_path(path.as_bytes(), true)?;
    if at.is_mount_root() && at.mnt.sb.fstype == sb.fstype {
        return Err(EBUSY);
    }
    ns.mount(&at, sb, root, false)
}

/// What `populate` made (for the `vfs root.ready` event and tests).
#[derive(Default, Debug, Clone, Copy)]
pub struct Populated {
    pub devnodes: usize,
    pub procfiles: usize,
    pub procdirs: usize,
    /// Terminal nodes that use a registered device as their fallback.
    pub adopted: usize,
    pub failed: usize,
}

/// Mounts devtmpfs at `/dev`, proc at `/proc`, sysfs at `/sys` in `ns` and makes every registered node. Each
/// failure is an event and the rest goes on.
pub fn populate(ns: &MountNs) -> Populated {
    populate_from(ns, crate::DEVNODES.entries(), crate::PROCFILES.entries(), crate::PROCDIRS.entries())
}

/// `populate` with the registries given (host tests pass their own lists).
pub fn populate_from(ns: &MountNs, devnodes: &[crate::DevNodeEntry], procfiles: &[crate::ProcFileEntry], procdirs: &[crate::ProcDirEntry]) -> Populated {
    let mut st = Populated::default();
    let (dsb, droot) = crate::tmpfs::new_fs_as("devtmpfs", 0o755);
    let (psb, proot) = synthetic_fs("proc", Fixed::Proc);
    let (ssb, sroot) = synthetic_fs("sysfs", Fixed::Sys);
    // AB-7c: the flags Linux's init gives them (/dev: devtmpfs, nosuid only; proc and sysfs nosuid,nodev,noexec).
    let pseudo = crate::MNT_NOSUID | crate::MNT_NODEV | crate::MNT_NOEXEC | crate::MNT_RELATIME;
    for (path, sb, root, flags) in [("/dev", dsb, droot, crate::MNT_NOSUID), ("/proc", psb, proot, pseudo), ("/sys", ssb, sroot, pseudo)] {
        let fstype = sb.fstype;
        if let Err(e) = mount_at(ns, path, sb, root).map(|m| m.set_flags(flags)) {
            st.failed += 1;
            klog::event!(Error, "vfs", "mount.failed", path = path, fs = fstype, errno = e);
        }
    }
    // IN-7 CR3: /dev/pts is a file system of its own, so statfs and /proc/mounts say devpts (a tmpfs with that
    // name: IN's pty layer makes and removes the slave nodes in it).
    let (ptsb, ptroot) = crate::tmpfs::new_fs_as("devpts", 0o755);
    // `devpts_show_options` without mount options: the slaves' mode (console's SLAVE_PERM 0600) and ptmxmode.
    ptsb.set_options(",mode=600,ptmxmode=000");
    if let Err(e) = mount_at(ns, "/dev/pts", ptsb, ptroot).map(|m| m.set_flags(crate::MNT_NOSUID | crate::MNT_NOEXEC | crate::MNT_RELATIME)) {
        st.failed += 1;
        klog::event!(Error, "vfs", "mount.failed", path = "/dev/pts", fs = "devpts", errno = e);
    }
    make_devices(ns, devnodes, &mut st);
    for p in procfiles {
        match ns.create_proc(p.path, p.render) {
            Ok(_) => st.procfiles += 1,
            Err(e) => {
                st.failed += 1;
                klog::event!(Warn, "vfs", "procfile.failed", path = p.path, errno = e);
            }
        }
    }
    for p in procdirs {
        match attach(ns, p.path, (p.make)()) {
            Ok(()) => st.procdirs += 1,
            Err(e) => {
                st.failed += 1;
                klog::event!(Warn, "vfs", "procdir.failed", path = p.path, errno = e);
            }
        }
    }
    st
}

/// FS's own devices, then every registered one. A terminal path registered by `devnode!` (IN's G0 console
/// device) becomes that terminal node's fallback until IN's VT sets the `tty` backend; any other registration
/// of a path that exists already is refused (`vfs devnode.duplicate`), the first one wins.
pub fn make_devices(ns: &MountNs, registered: &[crate::DevNodeEntry], st: &mut Populated) {
    for (path, f) in mem_devices() {
        match ns.create_device(path, f) {
            Ok(_) => st.devnodes += 1,
            Err(e) => {
                st.failed += 1;
                klog::event!(Warn, "vfs", "devnode.failed", path = path, errno = e);
            }
        }
    }
    for (which, uid, gid) in TTYS {
        let path = which.path();
        let fallback = registered.iter().find(|d| d.path == path).map(|d| (d.make)());
        if fallback.is_some() {
            st.adopted += 1;
            klog::event!(Info, "vfs", "devnode.adopted", path = path; "the registered device serves this terminal until a console backend is set");
        }
        match ns.create_device(path, Arc::new(TtyDev::new(which, fallback))) {
            Ok(d) => {
                d.inode.meta.set_owner(uid, gid);
                st.devnodes += 1;
            }
            Err(e) => {
                st.failed += 1;
                klog::event!(Warn, "vfs", "devnode.failed", path = path, errno = e);
            }
        }
    }
    let builtin = builtin_paths();
    let mut seen: Vec<&'static str> = Vec::new();
    for d in registered {
        let tty = TTYS.iter().any(|t| t.0.path() == d.path);
        if tty && !seen.contains(&d.path) {
            // Adopted above (the first registration of the path).
            seen.push(d.path);
            continue;
        }
        if builtin.contains(&d.path) || seen.contains(&d.path) {
            st.failed += 1;
            klog::event!(Warn, "vfs", "devnode.duplicate", path = d.path; "a node of this path exists already; this registration is not used");
            continue;
        }
        seen.push(d.path);
        match ns.create_device(d.path, (d.make)()) {
            Ok(n) => {
                // IN-7 CR3: /dev/ptmx is root:tty as on CachyOS (crw-rw-rw- root tty).
                if d.path == "/dev/ptmx" {
                    n.inode.meta.set_owner(0, 5);
                }
                st.devnodes += 1
            }
            Err(e) => {
                st.failed += 1;
                klog::event!(Warn, "vfs", "devnode.failed", path = d.path, errno = e);
            }
        }
    }
}

/// `/proc/sys/fs`: the VFS's own limits as programs read them (`nr_open`: the RLIMIT_NOFILE ceiling, Linux's
/// default 1048576; `file-max`: no system-wide limit of open files (R1), shown as LONG_MAX like a systemd
/// system; FS-3's pipe limits). Also keeps the `procdir` registry section present when no other crate registers one.
struct ProcSysFs;

impl crate::synth::DirSource for ProcSysFs {
    fn entries(&self) -> Result<Vec<crate::synth::SynthEntry>, Errno> {
        use crate::synth::{SynthEntry, Text};
        Ok(alloc::vec![
            SynthEntry::file("file-max", 0o444, Arc::new(Text(alloc::format!("{}\n", i64::MAX)))).cookie(1),
            SynthEntry::file("nr_open", 0o444, Arc::new(Text(alloc::format!("{}\n", abi::NR_OPEN)))).cookie(2),
            // FS-3: the pipe limits, root-writable as on Linux (0644). pipe-max-size rules F_SETPIPE_SZ and new
            // pipes; the user-pages numbers are kept but not enforced (R1).
            SynthEntry::file("pipe-max-size", 0o644, Arc::new(crate::sysctl::Knob { get: crate::pipe::max_size, set: crate::pipe::set_max_size, uint: true })).cookie(3),
            SynthEntry::file("pipe-user-pages-hard", 0o644, Arc::new(crate::pipe::user_pages_knob(true))).cookie(4),
            SynthEntry::file("pipe-user-pages-soft", 0o644, Arc::new(crate::pipe::user_pages_knob(false))).cookie(5),
        ])
    }
}

pub(crate) fn proc_sys_fs() -> Arc<dyn crate::synth::DirSource> {
    Arc::new(ProcSysFs)
}
crate::procdir!("/proc/sys/fs", proc_sys_fs);

/// Attaches a computed directory at `path` (made when missing). The directory must be on proc or sysfs.
pub fn attach(ns: &MountNs, path: &str, src: Arc<dyn crate::synth::DirSource>) -> Result<(), Errno> {
    let d = ns.mkdir_p(path)?;
    match &d.inode.data {
        Data::SynthDir(sd) => {
            sd.add(src);
            Ok(())
        }
        _ => Err(ENOTDIR),
    }
}

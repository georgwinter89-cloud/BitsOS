//! IN-PTY ktests in the kernel: two devpts instances mounted on a scratch directory of the boot view, a pty in
//! each through the VFS's open path, their numbers and nodes, the wrong instance refused, TIOCGPTPEER's slave in
//! its own instance, the node gone with the master, and an unmount with a pty open. Everything opened is closed
//! and unmounted again (Safe).

use crate::ktests_in4::{BASE, KMem};
use crate::pty;
use crate::termios::ioc::*;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;
use vfs::mount_fs::{self, Mounter};

const O_RDWR: u64 = 2;
const O_NOCTTY: u64 = 0o400;
const DIR: &str = "/tmp/ktest-inpty";

fn mount(dir: &str, opts: &[u8]) -> Result<Arc<vfs::Mount>, String> {
    let ns = vfs::root_ns();
    ns.mkdir_p(dir).map_err(|e| format!("mkdir {dir}: errno {e}"))?;
    let at = ns.lookup_path(dir.as_bytes(), true).map_err(|e| format!("{dir}: errno {e}"))?;
    let o = mount_fs::split(opts).map_err(|e| format!("options: errno {e}"))?;
    let same = |n: u32| Some(n);
    let who = Mounter { fsuid: 0, fsgid: 0, uid_in: &same, gid_in: &same, init_ns: true, ns_root: None };
    let d = mount_fs::devpts_opts(&o, &who).map_err(|e| format!("devpts options: errno {e}"))?;
    let (sb, root) = mount_fs::devpts(&d, &who, &|i| Some(pty::instance_ptmx(i)));
    ns.mount(&at, sb, root, false).map_err(|e| format!("mount {dir}: errno {e}"))
}

fn umount(m: &Arc<vfs::Mount>) -> Result<(), String> {
    vfs::root_ns().umount(&vfs::Path { mnt: m.clone(), dentry: m.root.clone() }).map_err(|e| format!("umount: errno {e}"))
}

fn open(path: &str) -> Result<Arc<vfs::OpenFile>, vfs::Errno> {
    vfs::root_ns().open_flags(path.as_bytes(), O_RDWR | O_NOCTTY, 0)
}

fn master(ptmx: &str, mem: &KMem) -> Result<(Arc<vfs::OpenFile>, i32), String> {
    let m = open(ptmx).map_err(|e| format!("open {ptmx}: errno {e}"))?;
    kassert!(m.file.ioctl_user(TIOCGPTN, BASE, mem) == Some(Ok(0)), "TIOCGPTN {ptmx}");
    let n = mem.i32_at(0);
    mem.put(0, &0i32.to_le_bytes());
    kassert!(m.file.ioctl_user(TIOCSPTLCK, BASE, mem) == Some(Ok(0)), "unlockpt {ptmx}");
    Ok((m, n))
}

fn read_nb(f: &vfs::OpenFile) -> String {
    let mut b = vec![0u8; 256];
    let n = f.file.read_stream(&mut b, true).unwrap_or(0);
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// The mounts and scratch directories of `instances`, undone however its body ends (an early `kassert!` or `?`
/// must not leave mounts or pairs behind for later ktests: Safe). The body's open files drop when it returns.
struct Scratch {
    mounts: alloc::vec::Vec<Arc<vfs::Mount>>,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let ns = vfs::root_ns();
        for m in self.mounts.drain(..).rev() {
            let _ = umount(&m); // an instance the body unmounted already answers EINVAL: nothing left to undo
        }
        for d in [format!("{DIR}/a"), format!("{DIR}/b"), String::from(DIR)] {
            let _ = ns.remove(&d);
        }
    }
}

fn instances() -> TestResult {
    let before = pty::count();
    let r = {
        let mut sc = Scratch { mounts: alloc::vec::Vec::new() };
        body(&mut sc)
    };
    r?;
    kassert!(pty::count() == before, "pairs {} after, {before} before", pty::count());
    Ok(())
}

fn body(sc: &mut Scratch) -> TestResult {
    let ns = vfs::root_ns();
    let (a, b) = (format!("{DIR}/a"), format!("{DIR}/b"));
    sc.mounts.push(mount(&a, b"ptmxmode=666")?);
    sc.mounts.push(mount(&b, b"ptmxmode=666,mode=620,gid=5")?);
    let ma_ = sc.mounts[0].clone();
    let mem = KMem::new();
    let (ma, na) = master(&format!("{a}/ptmx"), &mem)?;
    let (mb, nb) = master(&format!("{b}/ptmx"), &mem)?;
    kassert!(na == 0 && nb == 0, "numbers {na} and {nb}: each instance counts from 0");
    let (ma2, na2) = master(&format!("{a}/ptmx"), &mem)?;
    kassert!(na2 == 1, "a's second number {na2}");
    kassert!(ns.lookup(&format!("{a}/1")).is_ok() && ns.lookup(&format!("{b}/1")).is_err(), "a's second slave is not in a only");
    let b0 = ns.lookup(&format!("{b}/0")).map_err(|e| format!("{b}/0: errno {e}"))?;
    kassert!(b0.inode.meta.mode() == uapi::S_IFCHR | 0o620 && b0.inode.meta.gid() == 5, "b/0 mode {:o} gid {}", b0.inode.meta.mode(), b0.inode.meta.gid());
    kassert!(open(&format!("{b}/1")).err() == Some(ENOENT), "a's slave opened through b");
    let sa = open(&format!("{a}/0")).map_err(|e| format!("open a/0: errno {e}"))?;
    let sb = open(&format!("{b}/0")).map_err(|e| format!("open b/0: errno {e}"))?;
    kassert!(ma.file.write_stream(b"to-a\r", false) == Ok(5) && mb.file.write_stream(b"to-b\r", false) == Ok(5), "master writes");
    kassert!(read_nb(&sa) == "to-a\n" && read_nb(&sb) == "to-b\n", "each slave reads its own master");
    let peer = pty::open_peer(&mb, O_RDWR | O_NOCTTY).ok_or("b's master is no master")?.map_err(|e| format!("TIOCGPTPEER: errno {e}"))?;
    kassert!(peer.path.as_ref().is_some_and(|p| Arc::ptr_eq(&p.dentry, &b0)), "TIOCGPTPEER's slave is not b/0");
    drop((peer, sa, sb, ma2));
    drop((ma, mb));
    kassert!(ns.lookup(&format!("{a}/0")).is_err() && ns.lookup(&format!("{b}/0")).is_err(), "slave nodes after the masters closed");
    // Unmount with a pty open: it keeps working, its node leaves the unmounted tree with the master.
    let root = ma_.root.clone();
    let (m, n) = master(&format!("{a}/ptmx"), &mem)?;
    let s = open(&format!("{a}/{n}")).map_err(|e| format!("open a/{n}: errno {e}"))?;
    umount(&ma_)?;
    kassert!(m.file.write_stream(b"u\r", false) == Ok(2) && read_nb(&s) == "u\n", "a pty of an unmounted instance stopped working");
    drop(m);
    kassert!(root.child(format!("{n}").as_bytes()).is_none(), "a stale node in the unmounted tree");
    kassert!(read_nb(&s).is_empty(), "the slave after the master closed");
    drop(s);
    Ok(())
}
ktest!("in.pty.instances", Safe, instances);

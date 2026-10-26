//! FS-13 host tests: the mount flags noexec and nodev are enforced (open, access, mmap), the flags survive
//! `copy_view`, and mounts without them behave as before.

use crate::abi::{O_NONBLOCK, O_PATH, O_RDONLY, X_OK};
use crate::cases_test::{program_view, SUBJECT, USER};
use crate::inode::Cred;
use crate::mmap::{check, MAP_PRIVATE, MAP_SHARED, PROT_EXEC};
use crate::mount_ns::{MountNs, MNT_NODEV, MNT_NOEXEC, MNT_NOSUID, MNT_RELATIME};
use crate::ops::Ctx;
use uapi::errno::*;

const PROT_READ: u64 = 1;

/// A view with /nx (tmpfs nosuid,nodev,noexec) and /ok (plain tmpfs), each holding a 0755 file `f`, the
/// /dev/zero node `z` (1:5) and a FIFO `p`.
fn view() -> alloc::sync::Arc<MountNs> {
    let (ns, _) = program_view();
    let (_, zero) = crate::devfs::mem_devices().into_iter().find(|d| d.0 == "/dev/zero").unwrap();
    ns.create_device("/zero-node", zero).unwrap();
    crate::tmpfs::mount_new(&ns, "/nx", 0o777, None).unwrap().set_flags(MNT_NOSUID | MNT_NODEV | MNT_NOEXEC | MNT_RELATIME);
    crate::tmpfs::mount_new(&ns, "/ok", 0o777, None).unwrap();
    let root = Ctx { cred: Cred::ROOT, ..Ctx::kernel(&ns) };
    for d in ["/nx", "/ok"] {
        let f = root.open(None, alloc::format!("{d}/f").as_bytes(), crate::abi::O_CREAT | crate::abi::O_WRONLY, 0o755).unwrap();
        f.write(b"\x7fELF not really").unwrap();
        root.mknod(None, alloc::format!("{d}/z").as_bytes(), uapi::S_IFCHR | 0o666, (1 << 8) | 5).unwrap();
        root.mknod(None, alloc::format!("{d}/p").as_bytes(), uapi::S_IFIFO | 0o666, 0).unwrap();
    }
    ns
}

fn checks(ns: &MountNs) {
    let user = Ctx { cred: USER, subject: SUBJECT, ..Ctx::kernel(ns) };
    // nodev: a device node does not open (EACCES, also for root), O_PATH does; FIFOs are not devices.
    assert_eq!(user.open(None, b"/nx/z", O_RDONLY, 0).err(), Some(EACCES));
    let root = Ctx { cred: Cred::ROOT, ..Ctx::kernel(ns) };
    assert_eq!(root.open(None, b"/nx/z", O_RDONLY, 0).err(), Some(EACCES));
    assert!(user.open(None, b"/nx/z", O_PATH, 0).is_ok());
    assert!(user.open(None, b"/nx/p", O_RDONLY | O_NONBLOCK, 0).is_ok());
    assert!(user.open(None, b"/ok/z", O_RDONLY, 0).is_ok());
    // noexec: access(X_OK) of a regular file is EACCES; directories stay searchable.
    assert_eq!(user.access(None, b"/nx/f", X_OK, true).err(), Some(EACCES));
    assert_eq!(user.access(None, b"/nx", X_OK, true), Ok(()));
    assert_eq!(user.access(None, b"/ok/f", X_OK, true), Ok(()));
    // noexec: mmap PROT_EXEC is EPERM (private and shared), without it VM_MAYEXEC goes.
    let nx = user.open(None, b"/nx/f", O_RDONLY, 0).unwrap();
    assert_eq!(check(&nx, PROT_READ | PROT_EXEC, MAP_PRIVATE, 0, 4096).err(), Some(EPERM));
    assert_eq!(check(&nx, PROT_READ | PROT_EXEC, MAP_SHARED, 0, 4096).err(), Some(EPERM));
    assert!(!check(&nx, PROT_READ, MAP_PRIVATE, 0, 4096).unwrap().may_exec);
    let ok = user.open(None, b"/ok/f", O_RDONLY, 0).unwrap();
    assert!(check(&ok, PROT_READ | PROT_EXEC, MAP_PRIVATE, 0, 4096).unwrap().may_exec);
    // EACCES for an fd not open for reading comes first (linux-6.6/mm/mmap.c:1321).
    let wo = user.open(None, b"/nx/f", crate::abi::O_WRONLY, 0);
    if let Ok(wo) = wo {
        assert_eq!(check(&wo, PROT_READ | PROT_EXEC, MAP_PRIVATE, 0, 4096).err(), Some(EACCES));
    }
}

#[test]
fn noexec_and_nodev_are_enforced() {
    let ns = view();
    checks(&ns);
    // A copied view keeps the flags (FS-11's unshare starts there).
    checks(&ns.copy_view());
}

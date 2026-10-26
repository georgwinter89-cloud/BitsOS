//! FS-1b host tests beyond the case list: mknod by device number opens the device registered under it
//! (`special`, the answer to fs-FS-2.md open point 3).
use crate::cases_test::{program_view, SUBJECT, USER};
use crate::inode::Cred;
use crate::ops::Ctx;
use uapi::errno::*;

#[test]
fn mknod_by_number_opens_the_registered_device() {
    let (ns, _) = program_view();
    let (_, zero) = crate::devfs::mem_devices().into_iter().find(|d| d.0 == "/dev/zero").unwrap();
    ns.create_device("/zero-node", zero).unwrap();
    let root = Ctx { cred: Cred::ROOT, ..Ctx::kernel(&ns) };
    let user = Ctx { cred: USER, subject: SUBJECT, ..Ctx::kernel(&ns) };
    // makedev(1, 5) is /dev/zero's number: a node made by root opens it; a user may not make one.
    root.mknod(None, b"/tmp/z", uapi::S_IFCHR | 0o666, (1 << 8) | 5).unwrap();
    assert_eq!(user.mknod(None, b"/tmp/z2", uapi::S_IFCHR | 0o666, (1 << 8) | 5).err(), Some(EPERM));
    let f = user.open(None, b"/tmp/z", crate::abi::O_RDONLY, 0).unwrap();
    let mut b = [7u8; 4];
    assert_eq!(f.read(&mut b).unwrap(), 4);
    assert_eq!(b, [0; 4]);
    assert_eq!(f.stat().rdev, crate::abi::encode_dev(1, 5));
    // A number nobody registered: the node exists, opening it is ENXIO.
    root.mknod(None, b"/tmp/nothing", uapi::S_IFCHR | 0o666, (1 << 8) | 250).unwrap();
    assert_eq!(user.open(None, b"/tmp/nothing", crate::abi::O_RDONLY, 0).err(), Some(ENXIO));
}

/// /proc answers a mode change under a process directory or /proc/sys with EPERM, also to root (`proc_setattr`,
/// `proc_sys_setattr`); its other entries take one. The rule goes by the file system type and the first name.
#[test]
fn proc_refuses_mode_changes_like_linux() {
    let (ns, _) = program_view();
    let (sb, root) = crate::tmpfs::new_fs_as("proc", 0o555);
    ns.mkdir_p("/p").unwrap();
    let at = ns.lookup_path(b"/p", true).unwrap();
    ns.mount(&at, sb, root, false).unwrap();
    for d in ["/p/123", "/p/sys/kernel", "/p/self"] {
        ns.mkdir_p(d).unwrap();
    }
    for f in ["/p/123/status", "/p/sys/kernel/x", "/p/meminfo", "/p/self/stat"] {
        ns.create_file(f, 0o444, alloc::vec::Vec::new()).unwrap();
    }
    let root = Ctx { cred: Cred::ROOT, ..Ctx::kernel(&ns) };
    let set = |p: &str, a: crate::setattr::SetAttr| {
        let path = root.lookup(None, p.as_bytes(), false)?;
        root.setattr(&path, |_| a)
    };
    let chmod = crate::setattr::chmod(0o400);
    let own = crate::setattr::SetAttr { valid: crate::setattr::ATTR_UID | crate::setattr::ATTR_CTIME, uid: 5, ..Default::default() };
    assert_eq!(set("/p/123/status", chmod), Err(EPERM));
    assert_eq!(set("/p/123", chmod), Err(EPERM));
    assert_eq!(set("/p/self/stat", chmod), Err(EPERM));
    assert_eq!(set("/p/sys/kernel/x", chmod), Err(EPERM));
    assert_eq!(set("/p/sys/kernel/x", own), Err(EPERM));
    set("/p/123/status", own).unwrap();
    set("/p/meminfo", chmod).unwrap();
    set("/p", chmod).unwrap();
}

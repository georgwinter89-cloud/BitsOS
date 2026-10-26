//! In-kernel tests of FS-8a. All Safe (rerunnable, nothing of the machine's disks is touched): they mount
//! `testdata/ktest.img.z` (made by `host/tests/fs/mk-ext4-ktest-image.sh` with CachyOS's mkfs.ext4 and an
//! e2fsck -D htree) from memory, from a RAM block disk and as a loop image file on tmpfs, in views of their
//! own.
//!
//! - `fs.ext4.read`: the tree, contents, links, the sparse file and the htree directory read right.
//! - `fs.ext4.write`: names, data, links, unlink while open, truncate, insert into the htree directory,
//!   fsync and syncfs; a second mount of the bytes reads it all back with the same inode numbers.
//! - `fs.ext4.loop`: the image as a file of tmpfs mounted as a loop image, written, synced, read back.
//! - `fs.ext4.ramdisk`: the image on a `block::RamDisk` through the block layer (`DiskDev`).
//! - `fs.ext4.big`: a 5 GiB sparse file with data past 4 GiB.
//! - `fs.ext4.hostile`: damaged copies of the image mount or fail cleanly, never panic.
//! - `fs.ext4.limits`: RENAME_EXCHANGE, a FIFO with two names, a set-group-ID directory, a second mount of
//!   one image (EBUSY), the volume filled to ENOSPC with statfs agreeing; all of it e2fsck-shaped on a remount.
//! The writing tests also check that they added no lock-order violation (`ksync::lockdep`).

use crate::mount;
use crate::{MemDev, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

static IMAGE: &[u8] = include_bytes!("../testdata/ktest.img.z");

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

/// Runs `f` and fails when it added lock-order violations.
fn no_new_violations(f: impl FnOnce() -> TestResult) -> TestResult {
    let before = ksync::lockdep::counts().unexpected;
    f()?;
    let after = ksync::lockdep::counts().unexpected;
    kassert!(after == before, "{} new lock-order violations: {:?}", after - before, ksync::lockdep::list());
    Ok(())
}

fn image() -> Result<Vec<u8>, String> {
    miniz_oxide::inflate::decompress_to_vec_zlib(IMAGE).map_err(|x| format!("inflate: {x:?}"))
}

fn view(img: Vec<u8>) -> Result<(Arc<MemDev>, vfs::MountNs, Arc<crate::Volume>), String> {
    let dev = Arc::new(MemDev::new(img));
    let ns = vfs::MountNs::new_empty();
    let (v, _) = mount::mount_dev(dev.clone(), None, &ns, "/x", Options::default()).map_err(e)?;
    Ok((dev, ns, v))
}

fn check_tree(ns: &vfs::MountNs) -> TestResult {
    kassert!(ns.read_file("/x/hello.txt", 100).map_err(e)? == b"hello from ext4\n");
    let d = ns.read_file("/x/dir/sub/data.bin", 1 << 20).map_err(e)?;
    kassert!(d.len() == 200_000);
    kassert!(d.iter().enumerate().all(|(i, &x)| x == ((i * 31) % 251) as u8), "data.bin content");
    kassert!(ns.read_link("/x/fast-link").map_err(e)? == b"hello.txt");
    kassert!(ns.read_link("/x/slow-link").map_err(e)? == "x/".repeat(60).as_bytes());
    let c = Ctx::kernel(ns);
    let a = c.stat(None, b"/x/hello.txt", true).map_err(e)?;
    let h = c.stat(None, b"/x/dir/hard", true).map_err(e)?;
    kassert!(a.ino == h.ino && a.nlink == 2, "hard link: {} {} nlink {}", a.ino, h.ino, a.nlink);
    let s = c.stat(None, b"/x/sparse", true).map_err(e)?;
    kassert!(s.size == 20 << 20 && s.blocks < 64, "sparse: size {} blocks {}", s.size, s.blocks);
    let f = c.open(None, b"/x/sparse", O_RDWR, 0).map_err(e)?;
    let mut b = [0u8; 3];
    f.pread((20 << 20) - 3, &mut b).map_err(e)?;
    kassert!(&b == b"end");
    kassert!(ns.list_dir("/x/many").map_err(e)?.len() == 400, "htree directory");
    Ok(())
}

fn fs_ext4_read() -> TestResult {
    let (_d, ns, v) = view(image()?)?;
    kassert!(v.writable());
    check_tree(&ns)?;
    let st = vfs::statfs::of(&ns.lookup("/x").map_err(e)?.sb, false).map_err(e)?;
    kassert!(st.ftype == 0xEF53 && st.bsize == 4096, "statfs {:?}", st.ftype);
    Ok(())
}
ktest!("fs.ext4.read", Safe, fs_ext4_read);

pub(crate) fn sync_tree(ns: &vfs::MountNs, p: &str) -> TestResult {
    for (n, a) in ns.list_dir(p).map_err(e)? {
        let path = format!("{p}/{}", String::from_utf8_lossy(&n));
        if a.mode & uapi::S_IFMT == uapi::S_IFDIR {
            sync_tree(ns, &path)?;
        } else if a.mode & uapi::S_IFMT == uapi::S_IFREG {
            ns.lookup(&path).map_err(e)?.inode.sync(SyncKind::Fsync).map_err(e)?;
        }
    }
    Ok(())
}

/// Changes through the VFS; returns the inode number of the file it checks.
fn work(ns: &vfs::MountNs) -> Result<u64, String> {
    let c = Ctx::kernel(ns);
    c.mkdir(None, b"/x/w", 0o755).map_err(e)?;
    for i in 0..200usize {
        ns.create_file_bytes(format!("/x/w/f{i}").as_bytes(), 0o644, &vec![i as u8; 50 + i * 41]).map_err(e)?;
    }
    let f = c.open(None, b"/x/w/log", O_RDWR | O_CREAT, 0o600).map_err(e)?;
    f.pwrite(0, &vec![0x5A; 100_000]).map_err(e)?;
    f.pwrite(3 << 20, b"far").map_err(e)?;
    let ino = c.stat(None, b"/x/w/log", true).map_err(e)?.ino;
    c.symlink(b"log", None, b"/x/w/link").map_err(e)?;
    c.link(None, b"/x/w/log", None, b"/x/w/log2", false).map_err(e)?;
    c.rename(None, b"/x/w/f1", None, b"/x/moved", 0).map_err(e)?;
    c.rename(None, b"/x/w/f2", None, b"/x/moved", 0).map_err(e)?;
    let g = c.open(None, b"/x/w/f3", O_RDWR, 0).map_err(e)?;
    c.unlink(None, b"/x/w/f3").map_err(e)?;
    let mut b = vec![0u8; 20];
    kassert!(g.pread(0, &mut b).map_err(e)? == 20 && b.iter().all(|&x| x == 3), "unlinked while open");
    drop(g);
    c.truncate(None, b"/x/w/f4", 7).map_err(e)?;
    ns.create_file_bytes(b"/x/many/new-in-htree", 0o644, b"new").map_err(e)?;
    c.unlink(None, b"/x/many/entry-with-a-long-name-007").map_err(e)?;
    sync_tree(ns, "/x")?;
    Ok(ino)
}

fn check_work(ns: &vfs::MountNs, ino: u64) -> TestResult {
    let c = Ctx::kernel(ns);
    let a = c.stat(None, b"/x/w/log", true).map_err(e)?;
    kassert!(a.ino == ino && a.nlink == 2 && a.size == (3 << 20) + 3, "log: ino {} nlink {} size {}", a.ino, a.nlink, a.size);
    let d = ns.read_file("/x/w/log", 4 << 20).map_err(e)?;
    kassert!(d[..100_000].iter().all(|&x| x == 0x5A) && d[100_000] == 0 && &d[3 << 20..] == b"far");
    kassert!(ns.read_link("/x/w/link").map_err(e)? == b"log");
    kassert!(ns.read_file("/x/moved", 1000).map_err(e)? == vec![2u8; 50 + 2 * 41]);
    kassert!(ns.read_file("/x/w/f4", 100).map_err(e)? == vec![4u8; 7]);
    kassert!(ns.list_dir("/x/w").map_err(e)?.len() == 200 - 3 + 3);
    kassert!(ns.list_dir("/x/many").map_err(e)?.len() == 400);
    kassert!(ns.read_file("/x/many/new-in-htree", 10).map_err(e)? == b"new");
    check_tree(ns)
}

fn fs_ext4_write() -> TestResult {
    no_new_violations(write_inner)
}
fn write_inner() -> TestResult {
    let (dev, ns, v) = view(image()?)?;
    let ino = work(&ns)?;
    check_work(&ns, ino)?;
    v.sync_fs().map_err(e)?;
    let (_d2, ns2, _v2) = view(dev.image())?;
    check_work(&ns2, ino)
}
ktest!("fs.ext4.write", Safe, fs_ext4_write);

fn fs_ext4_loop() -> TestResult {
    no_new_violations(loop_inner)
}
fn loop_inner() -> TestResult {
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/img").map_err(e)?;
    ns.create_file_bytes(b"/img/data.img", 0o644, &image()?).map_err(e)?;
    mount::mount_image(&ns, "/img/data.img", "/x", Options::default()).map_err(e)?;
    check_tree(&ns)?;
    let ino = work(&ns)?;
    mount::sync_all().map_err(e)?;
    let bytes = ns.read_file("/img/data.img", 64 << 20).map_err(e)?;
    let (_d, ns2, _v) = view(bytes)?;
    check_work(&ns2, ino)
}
ktest!("fs.ext4.loop", Safe, fs_ext4_loop);

fn fs_ext4_ramdisk() -> TestResult {
    no_new_violations(ramdisk_inner)
}
fn ramdisk_inner() -> TestResult {
    let name = "ext4test";
    block::unregister(name);
    let img = image()?;
    let rd = Arc::new(block::RamDisk::from_image(name, 512, img.clone(), false));
    let disk = block::register(rd.clone(), block::DiskKind::Ram).map_err(e)?;
    let r = (|| -> TestResult {
        let ns = vfs::MountNs::new_empty();
        let blocks = img.len() as u64 / 512;
        let (v, _) = mount::mount_disk(disk.clone(), 0, blocks, &ns, "/x", Options::default()).map_err(e)?;
        check_tree(&ns)?;
        let ino = work(&ns)?;
        v.sync_fs().map_err(e)?;
        let (_d, ns2, _v) = view(rd.snapshot())?;
        check_work(&ns2, ino)
    })();
    block::unregister(name);
    r
}
ktest!("fs.ext4.ramdisk", Safe, fs_ext4_ramdisk);

fn fs_ext4_big() -> TestResult {
    let (dev, ns, v) = view(image()?)?;
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/x/five", O_RDWR | O_CREAT, 0o644).map_err(e)?;
    let five = 5u64 << 30;
    f.pwrite((4 << 30) + 12345, b"past four GiB").map_err(e)?;
    f.pwrite(five - 4, b"last").map_err(e)?;
    ns.lookup("/x/five").map_err(e)?.inode.sync(SyncKind::Fsync).map_err(e)?;
    v.sync_fs().map_err(e)?;
    let (_d, ns2, _v) = view(dev.image())?;
    let c2 = Ctx::kernel(&ns2);
    let a = c2.stat(None, b"/x/five", true).map_err(e)?;
    kassert!(a.size == five && a.blocks <= 16, "size {} blocks {}", a.size, a.blocks);
    let g = c2.open(None, b"/x/five", O_RDWR, 0).map_err(e)?;
    let mut b = [0u8; 13];
    g.pread((4 << 30) + 12345, &mut b).map_err(e)?;
    kassert!(&b == b"past four GiB");
    Ok(())
}
ktest!("fs.ext4.big", Safe, fs_ext4_big);

fn fs_ext4_hostile() -> TestResult {
    let base = image()?;
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut mounted = 0;
    for _ in 0..60 {
        let mut img = base.clone();
        for _ in 0..3 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let at = (x % (1 << 20)) as usize;
            img[at] ^= 1 << (x % 8);
        }
        if let Ok((_d, ns, _v)) = view(img) {
            mounted += 1;
            let mut work = vec![String::from("/x")];
            let mut seen = 0;
            while let Some(d) = work.pop() {
                let Ok(l) = ns.list_dir(&d) else { continue };
                for (n, a) in l {
                    seen += 1;
                    let p = format!("{d}/{}", String::from_utf8_lossy(&n));
                    if a.mode & uapi::S_IFMT == uapi::S_IFDIR && seen < 2000 {
                        work.push(p);
                    } else if a.mode & uapi::S_IFMT == uapi::S_IFREG {
                        let _ = ns.read_file(&p, 1 << 20);
                    }
                }
            }
            let _ = ns.create_file_bytes(b"/x/after-damage", 0o644, b"x");
        }
    }
    klog::event!(Info, "ext4", "ktest.hostile", mounted = mounted);
    Ok(())
}
ktest!("fs.ext4.hostile", Safe, fs_ext4_hostile);


fn fs_ext4_limits() -> TestResult {
    no_new_violations(limits_inner)
}
fn limits_inner() -> TestResult {
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/img").map_err(e)?;
    ns.create_file_bytes(b"/img/data.img", 0o644, &image()?).map_err(e)?;
    mount::mount_image(&ns, "/img/data.img", "/x", Options::default()).map_err(e)?;
    let again = mount::mount_image(&ns, "/img/data.img", "/y", Options::default());
    kassert!(again.as_ref().err() == Some(&crate::errno::EBUSY), "second mount: {:?}", again.err());
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/x/l", 0o755).map_err(e)?;
    ns.create_file_bytes(b"/x/l/a", 0o644, b"aaa").map_err(e)?;
    c.mkdir(None, b"/x/l/b", 0o755).map_err(e)?;
    let (ia, ib) = (c.stat(None, b"/x/l/a", true).map_err(e)?.ino, c.stat(None, b"/x/l/b", true).map_err(e)?.ino);
    c.rename(None, b"/x/l/a", None, b"/x/l/b", vfs::abi::RENAME_EXCHANGE).map_err(e)?;
    kassert!(c.stat(None, b"/x/l/a", true).map_err(e)?.ino == ib && c.stat(None, b"/x/l/b", true).map_err(e)?.ino == ia, "exchange");
    // A FIFO with two names keeps one inode when one name goes.
    c.mknod(None, b"/x/l/fifo", uapi::S_IFIFO | 0o600, 0).map_err(e)?;
    c.link(None, b"/x/l/fifo", None, b"/x/l/fifo2", false).map_err(e)?;
    let before = ns.lookup("/x/l/fifo2").map_err(e)?.inode.clone();
    c.unlink(None, b"/x/l/fifo").map_err(e)?;
    let after = ns.lookup("/x/l/fifo2").map_err(e)?.inode.clone();
    kassert!(Arc::ptr_eq(&before, &after) && after.meta.nlink() == 1, "fifo: one inode, nlink {}", after.meta.nlink());
    // set-group-ID directory
    c.mkdir(None, b"/x/l/g", 0o755).map_err(e)?;
    let p = ns.lookup_path(b"/x/l/g", true).map_err(e)?;
    c.setattr(&p, |m| vfs::setattr::chown(m, vfs::inode::Cred::ROOT, u32::MAX, 77)).map_err(e)?;
    c.setattr(&p, |_| vfs::setattr::chmod(0o2777)).map_err(e)?;
    let mut u = Ctx::kernel(&ns);
    u.cred = vfs::inode::Cred { uid: 1000, gid: 1000 };
    u.mkdir(None, b"/x/l/g/sub", 0o755).map_err(e)?;
    let st = u.stat(None, b"/x/l/g/sub", true).map_err(e)?;
    kassert!(st.gid == 77 && st.mode & 0o2000 != 0, "setgid: gid {} mode {:o}", st.gid, st.mode);
    // Fill the volume: ENOSPC at write(), statfs then has (almost) nothing available.
    let f = c.open(None, b"/x/l/fill", O_RDWR | O_CREAT, 0o644).map_err(e)?;
    let chunk = vec![0xF1u8; 1 << 20];
    let mut at = 0u64;
    let full = loop {
        match f.pwrite(at, &chunk) {
            Ok(n) => at += n as u64,
            Err(x) => break x,
        }
        kassert!(at < 1 << 30, "no ENOSPC after 1 GiB");
    };
    kassert!(full == uapi::errno::ENOSPC, "filling ended with {full}");
    let sf = vfs::statfs::of(&ns.lookup("/x").map_err(e)?.sb, false).map_err(e)?;
    kassert!(sf.bavail < 256, "statfs bavail {} after ENOSPC", sf.bavail);
    sync_tree(&ns, "/x")?;
    mount::sync_all().map_err(e)?;
    drop(f);
    c.unlink(None, b"/x/l/fill").map_err(e)?;
    mount::sync_all().map_err(e)?;
    let bytes = ns.read_file("/img/data.img", 64 << 20).map_err(e)?;
    let (_d, ns2, _v) = view(bytes)?;
    let c2 = Ctx::kernel(&ns2);
    kassert!(c2.stat(None, b"/x/l/fifo2", false).map_err(e)?.nlink == 1);
    kassert!(c2.stat(None, b"/x/l/g/sub", true).map_err(e)?.gid == 77);
    kassert!(c2.stat(None, b"/x/l/fill", true).is_err(), "fill is gone");
    check_tree(&ns2)
}
ktest!("fs.ext4.limits", Safe, fs_ext4_limits);

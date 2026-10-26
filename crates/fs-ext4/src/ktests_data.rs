//! FS-8c ktests (prefix fs.ext4.):
//! - `mkfs` (Safe, RAM): V2's mkfs, mounted in a private view, written, synced, mounted again and read back.
//! - `pool` (Safe, RAM): a pool image in a tmpfs file of a private view, made like data.img, its volume
//!   registered as a stacked disk and mounted, the user folders, Brave-like work, then a second open of the
//!   same image reads it back; the disk is unregistered at the end (rerunnable).
//! - `data_boot` (Safe, RAM): the boot steps (`data::bring_up`) on a tmpfs "stick" of a private view: a
//!   leftover `.new` removed before the room check, the image made and mounted, mounted again (not remade),
//!   then mounted read-only as on a read-only stick without a byte of the image changing (rerunnable).
//! - `data` (Safe, reads only): the boot mount of `\BITSOS2\data.img` at /data (skip when there is none):
//!   ext4 type, rw/ro as the stick, the user folders' owners, the mount's source (/proc/mounts' first field).
//! - `data_write` (QemuOnly): files, a symlink lock, rename over and fsync in /data/ktest-fs8c; a counter
//!   file there counts the boots that wrote it (host/tests/fs/50-data.sh checks 1, then 2), then
//!   `mount::sync_all`.

use crate::data::{self, Phase};
use crate::imgblk::ImageBlock;
use crate::mkfs::{self, MkfsOptions};
use crate::mount;
use crate::{MemDev, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ktest::{kassert, ktest, TestResult};
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn opts() -> MkfsOptions {
    let mut u = [0u8; 16];
    vfs::random::fill(&mut u);
    MkfsOptions::new(b"ktest", u, u, vfs::inode::now())
}

/// Brave-like work below `root` (a mounted ext4): returns the inode of the big file.
fn work(ns: &vfs::MountNs, root: &str) -> Result<u64, String> {
    let c = Ctx::kernel(ns);
    let d = format!("{root}/w");
    c.mkdir(None, d.as_bytes(), 0o700).map_err(e)?;
    for i in 0..120usize {
        ns.create_file_bytes(format!("{d}/f{i}").as_bytes(), 0o600, &vec![i as u8; 30 + i * 53]).map_err(e)?;
    }
    let f = c.open(None, format!("{d}/History").as_bytes(), O_RDWR | O_CREAT, 0o600).map_err(e)?;
    f.pwrite(0, &vec![0xC3; 200_000]).map_err(e)?;
    f.pwrite(5 << 20, b"tail").map_err(e)?;
    f.file.fsync().map_err(e)?;
    let ino = c.stat(None, format!("{d}/History").as_bytes(), true).map_err(e)?.ino;
    c.symlink(b"bitsos-1", None, format!("{d}/SingletonLock").as_bytes()).map_err(e)?;
    ns.create_file_bytes(format!("{d}/Local State.tmp").as_bytes(), 0o600, b"state 2").map_err(e)?;
    ns.create_file_bytes(format!("{d}/Local State").as_bytes(), 0o600, b"state 1").map_err(e)?;
    c.rename(None, format!("{d}/Local State.tmp").as_bytes(), None, format!("{d}/Local State").as_bytes(), 0).map_err(e)?;
    crate::ktests::sync_tree(ns, root)?;
    Ok(ino)
}

fn check(ns: &vfs::MountNs, root: &str, ino: u64) -> TestResult {
    let c = Ctx::kernel(ns);
    let d = format!("{root}/w");
    let a = c.stat(None, format!("{d}/History").as_bytes(), true).map_err(e)?;
    kassert!(a.ino == ino && a.size == (5 << 20) + 4, "History: ino {} size {}", a.ino, a.size);
    let h = ns.read_file(&format!("{d}/History"), 6 << 20).map_err(e)?;
    kassert!(h[..200_000].iter().all(|&x| x == 0xC3) && h[200_000] == 0 && &h[5 << 20..] == b"tail", "History content");
    kassert!(ns.read_link(&format!("{d}/SingletonLock")).map_err(e)? == b"bitsos-1");
    kassert!(ns.read_file(&format!("{d}/Local State"), 100).map_err(e)? == b"state 2", "rename over");
    kassert!(ns.read_file(&format!("{d}/f77"), 10_000).map_err(e)? == vec![77u8; 30 + 77 * 53]);
    kassert!(ns.list_dir(&d).map_err(e)?.len() == 120 + 3, "names in w");
    Ok(())
}

fn fs_ext4_mkfs() -> TestResult {
    let dev = Arc::new(MemDev::new(vec![0u8; 48 << 20]));
    let made = mkfs::format(&*dev, &{
        let mut o = opts();
        o.zeroed = true;
        o
    })
    .map_err(e)?;
    let ns = vfs::MountNs::new_empty();
    let (v, _) = mount::mount_dev(dev.clone(), None, &ns, "/x", Options::default()).map_err(e)?;
    kassert!(v.statfs().map_err(e)?.bfree == made.free_blocks, "free blocks as made");
    let ino = work(&ns, "/x")?;
    v.sync_fs().map_err(e)?;
    drop(ns);
    drop(v);
    let ns2 = vfs::MountNs::new_empty();
    mount::mount_dev(Arc::new(MemDev::new(dev.image())), None, &ns2, "/x", Options::default()).map_err(e)?;
    check(&ns2, "/x", ino)?;
    klog::event!(Info, "ext4", "ktest.mkfs", blocks = made.blocks, groups = made.groups, journal = made.journal_blocks);
    Ok(())
}
ktest!("fs.ext4.mkfs", Safe, fs_ext4_mkfs);

static RUN: AtomicU32 = AtomicU32::new(0);

fn fs_ext4_pool() -> TestResult {
    let name = format!("ktpool{}", RUN.fetch_add(1, Ordering::Relaxed));
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/img").map_err(e)?;
    ns.create_file_bytes(b"/img/data.img", 0o644, &[]).map_err(e)?;
    let file = ns.lookup("/img/data.img").map_err(e)?.inode.clone();
    file.truncate(64 << 20).map_err(e)?;
    data::format_pool(Arc::new(ImageBlock::new(&name, file, true).map_err(e)?), b"ktest").map_err(e)?;
    let r = (|| -> TestResult {
        let ino = {
            let m = data::mount(&ns, "/img/data.img", "/x", true, &name).map_err(e)?;
            kassert!(m.pool, "the image holds a pool");
            data::user_dirs(&ns, "/x").map_err(e)?;
            let st = Ctx::kernel(&ns).stat(None, b"/x/home/1000", true).map_err(e)?;
            kassert!(st.uid == data::UID && st.mode & 0o7777 == 0o700, "home/1000 uid {} mode {:o}", st.uid, st.mode);
            let ino = work(&ns, "/x")?;
            vfs::writeback::Flush::stop(&*m.vol).map_err(e)?;
            ns.umount(&ns.lookup_path(b"/x", true).map_err(e)?).map_err(e)?;
            if let Some(d) = &m.disk {
                block::unregister(d);
            }
            ino
        };
        let m = data::mount(&ns, "/img/data.img", "/y", false, &name).map_err(e)?;
        let r = check(&ns, "/y", ino);
        if let Ok(p) = ns.lookup_path(b"/y", true) {
            let _ = ns.umount(&p);
        }
        if let Some(d) = &m.disk {
            block::unregister(d);
        }
        r
    })();
    block::unregister(&format!("{name}.v1"));
    r
}
ktest!("fs.ext4.pool", Safe, fs_ext4_pool);

fn unmount(ns: &vfs::MountNs, at: &str, m: &data::Mounted) -> TestResult {
    ns.umount(&ns.lookup_path(at.as_bytes(), true).map_err(e)?).map_err(e)?;
    if let Some(d) = &m.disk {
        block::unregister(d);
    }
    Ok(())
}

fn fs_ext4_data_boot() -> TestResult {
    let name = format!("ktboot{}", RUN.fetch_add(1, Ordering::Relaxed));
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/stick").map_err(e)?;
    let p = data::Place { image: "/stick/data.img", temp: "/stick/data.img.new", at: "/d", pool_name: &name, on_stick: false };
    ns.create_file_bytes(p.temp.as_bytes(), 0o644, &[]).map_err(e)?;
    ns.lookup(p.temp).map_err(e)?.inode.truncate(24 << 20).map_err(e)?;
    let free = || if ns.lookup_path(p.temp.as_bytes(), false).is_ok() { 100 << 20 } else { 1 << 40 };
    let r = (|| -> TestResult {
        let (ph, m) = data::bring_up(&ns, &p, true, 32, &free, &|_| {});
        kassert!(ph == Phase::Mounted { pool: true, writable: true, created: true }, "first boot: {ph:?}");
        kassert!(ns.lookup_path(p.temp.as_bytes(), false).is_err(), "the leftover is gone");
        let m = m.ok_or("first boot: nothing mounted")?;
        let ino = work(&ns, "/d")?;
        vfs::writeback::Flush::stop(&*m.vol).map_err(e)?;
        unmount(&ns, "/d", &m)?;
        let (ph, m) = data::bring_up(&ns, &p, true, 32, &|| 0, &|_| {});
        kassert!(ph == Phase::Mounted { pool: true, writable: true, created: false }, "second boot: {ph:?}");
        let m = m.ok_or("second boot: nothing mounted")?;
        check(&ns, "/d", ino)?;
        vfs::writeback::Flush::stop(&*m.vol).map_err(e)?;
        unmount(&ns, "/d", &m)?;
        let before = ns.read_file(p.image, 64 << 20).map_err(e)?;
        let (ph, m) = data::bring_up(&ns, &p, false, 32, &|| 0, &|_| {});
        kassert!(ph == Phase::Mounted { pool: true, writable: false, created: false }, "read-only stick: {ph:?}");
        let m = m.ok_or("read-only stick: nothing mounted")?;
        let path = ns.lookup_path(b"/d", true).map_err(e)?;
        kassert!(path.mnt.readonly, "/d is read-only");
        check(&ns, "/d", ino)?;
        vfs::writeback::Flush::stop(&*m.vol).map_err(e)?;
        unmount(&ns, "/d", &m)?;
        kassert!(ns.read_file(p.image, 64 << 20).map_err(e)? == before, "the read-only mount changed the image");
        Ok(())
    })();
    block::unregister(&format!("{name}.v1"));
    r
}
ktest!("fs.ext4.data_boot", Safe, fs_ext4_data_boot);

/// The boot mount, once it finished (a first boot may still be making the image).
fn boot_mount() -> Result<(bool, bool, bool), TestResult> {
    match data::wait_done(120_000) {
        Phase::Mounted { pool, writable, created } => Ok((pool, writable, created)),
        Phase::Skipped(why) => Err(ktest::skip(why)),
        Phase::Failed(why) => Err(Err(format!("/data not mounted: {why}"))),
        // A first boot on a real stick zero-fills 2 GiB for minutes: not a failure of this test.
        Phase::Creating => Err(ktest::skip("data.img is still being made (see the ext4 data.grow events)")),
        Phase::Waiting => Err(Err(String::from("/data: still waiting for the boot stick after 120 s"))),
    }
}

fn fs_ext4_data() -> TestResult {
    let (pool, writable, created) = match boot_mount() {
        Ok(x) => x,
        Err(r) => return r,
    };
    let ns = vfs::root_ns();
    let st = vfs::statfs::of(&ns.lookup(data::AT).map_err(e)?.sb, !writable).map_err(e)?;
    kassert!(st.ftype == 0xEF53, "/data type {:#x}", st.ftype);
    // What /proc/mounts shows for it (procfs renders the same fields): the mount's source and rw/ro.
    let p = ns.lookup_path(data::AT.as_bytes(), true).map_err(e)?;
    let source = p.dentry.sb.source();
    kassert!(p.is_mount_root() && p.mnt.readonly == !writable, "/data mount root {} readonly {}", p.is_mount_root(), p.mnt.readonly);
    kassert!(source == if pool { "/dev/mapper/data" } else { "/dev/loop0" }, "/data source {source}");
    if writable {
        for (p, uid) in [("/data/appdata", data::UID), ("/data/home/1000", data::UID)] {
            let a = Ctx::kernel(&ns).stat(None, p.as_bytes(), true).map_err(e)?;
            kassert!(a.uid == uid, "{p} uid {}", a.uid);
        }
    }
    let m = data::mounted().ok_or("data::mounted() is empty while /data is mounted")?;
    kassert!(m.pool == pool && m.handle.is_some() == pool, "data::mounted() pool {} handle {}", m.pool, m.handle.is_some());
    if let Some((h, id)) = &m.handle {
        let info = h.lock().map_err(e)?.info(*id).map_err(e)?;
        kassert!(info.label == data::VOLUME && !info.snapshot, "pool volume {} label {}", id, info.label);
    }
    klog::event!(Info, "ext4", "ktest.data", pool = pool, writable = writable, created = created, free_blocks = st.bfree, source = source.as_str());
    Ok(())
}
ktest!("fs.ext4.data", Safe, fs_ext4_data);

fn fs_ext4_data_write() -> TestResult {
    let (_, writable, created) = match boot_mount() {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !writable {
        return ktest::skip("/data is read-only (the stick is)");
    }
    let ns = vfs::root_ns();
    let c = Ctx::kernel(&ns);
    let dir = "/data/ktest-fs8c";
    let boots = match ns.read_file(&format!("{dir}/boots"), 64) {
        Ok(b) => core::str::from_utf8(&b).ok().and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(0),
        Err(_) => 0,
    } + 1;
    let _ = ns.remove_tree(&format!("{dir}/x"));
    let _ = c.mkdir(None, dir.as_bytes(), 0o755);
    c.mkdir(None, format!("{dir}/x").as_bytes(), 0o755).map_err(e)?;
    let ino = work(&ns, &format!("{dir}/x"))?;
    check(&ns, &format!("{dir}/x"), ino)?;
    ns.create_file_bytes(format!("{dir}/boots.tmp").as_bytes(), 0o644, format!("{boots}\n").as_bytes()).map_err(e)?;
    let f = c.open(None, format!("{dir}/boots.tmp").as_bytes(), O_RDWR, 0).map_err(e)?;
    f.file.fsync().map_err(e)?;
    c.rename(None, format!("{dir}/boots.tmp").as_bytes(), None, format!("{dir}/boots").as_bytes(), 0).map_err(e)?;
    mount::sync_all().map_err(e)?;
    klog::event!(Info, "ext4", "ktest.data_write", boots = boots, created = created);
    Ok(())
}
ktest!("fs.ext4.data_write", QemuOnly, fs_ext4_data_write);

//! FS-8c on the host: the data image as the kernel makes it (a bitsvol pool in a file of another file system,
//! one volume "data", V2's mkfs inside), driven through the VFS like Brave's profile, e2fsck-clean on the
//! volume taken out of the pool; out of space on the thin volume reaches ext4 as ENOSPC. `extract_volume`
//! (ignored) takes the volume out of a pool image for host/tests/fs/50-data.sh.

use super::util::*;
use crate::data;
use crate::imgblk::{BlockDev, ImageBlock};
use crate::mount;
use bitsvol::{Config, Pool, PoolHandle};
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use devmodel::DevError;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::vec::Vec;
use std::{format, vec};
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;

/// A pool image file on the host, read only.
struct HostFile(std::fs::File, u64);

impl BlockDevice for HostFile {
    fn name(&self) -> &str {
        "host"
    }
    fn info(&self) -> BlockInfo {
        BlockInfo { block_size: 4096, blocks: self.1 / 4096, read_only: true, max_blocks_per_request: 256, supports_discard: false, has_volatile_cache: false }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], _p: IoPriority) -> Result<(), DevError> {
        self.0.read_exact_at(buf, lba * 4096).map_err(|_| DevError::Io)
    }
    fn write_blocks(&self, _: u64, _: &[u8], _: bool, _: IoPriority) -> Result<(), DevError> {
        Err(DevError::Refused)
    }
    fn flush(&self) -> Result<(), DevError> {
        Ok(())
    }
}

/// The bytes of volume "data" in the pool on `dev`.
fn volume_bytes(dev: Arc<dyn BlockDevice>) -> Vec<u8> {
    let pool = Pool::open(dev, Config::default()).expect("pool opens");
    let id = pool.list().unwrap().into_iter().find(|v| v.label == data::VOLUME && !v.snapshot).expect("volume data").id;
    let h = PoolHandle::new(pool, "x");
    let vd = h.open_volume(id).unwrap();
    let n = vd.info().blocks * 4096;
    let mut out = vec![0u8; n as usize];
    for (i, c) in out.chunks_mut(1 << 20).enumerate() {
        vd.read_blocks((i as u64) << 8, c, IoPriority::Normal).unwrap();
    }
    out
}

/// BITSVOL_POOL_IMG=<pool image> BITSVOL_OUT=<file>: writes volume "data" of the pool to the file.
#[test]
#[ignore]
fn extract_volume() {
    let src = std::env::var("BITSVOL_POOL_IMG").expect("BITSVOL_POOL_IMG");
    let out = std::env::var("BITSVOL_OUT").expect("BITSVOL_OUT");
    let f = std::fs::File::open(&src).unwrap();
    let len = f.metadata().unwrap().len();
    let bytes = volume_bytes(Arc::new(HostFile(f, len)));
    std::fs::write(&out, &bytes).unwrap();
    std::println!("extracted {} bytes of volume data from {src}", bytes.len());
}

fn image_ns(mib: u64) -> (vfs::MountNs, Arc<vfs::inode::Inode>) {
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/boot/BITSOS2").unwrap();
    ns.create_file_bytes(data::IMAGE.as_bytes(), 0o644, &[]).unwrap();
    let file = ns.lookup(data::IMAGE).unwrap().inode.clone();
    file.truncate(mib << 20).unwrap();
    (ns, file)
}

fn open_data(file: &Arc<vfs::inode::Inode>) -> Arc<dyn crate::Dev> {
    let ib: Arc<dyn BlockDevice> = Arc::new(ImageBlock::new("data.img", file.clone(), true).unwrap());
    let pool = Pool::open(ib, Config::default()).unwrap();
    let id = pool.list().unwrap().into_iter().find(|v| v.label == data::VOLUME).unwrap().id;
    let h = PoolHandle::new(pool, data::VOLUME);
    Arc::new(BlockDev::new(Arc::new(h.open_volume(id).unwrap())))
}

/// What Brave does in its profile (plan/notes/br-BR-1.md): locks, a symlink lock, rename over, fsync.
fn profile_work(ns: &vfs::MountNs) {
    let c = Ctx::kernel(ns);
    c.mkdir(None, b"/data/appdata/brave", 0o700).unwrap();
    let f = c.open(None, b"/data/appdata/brave/Local State.tmp", O_RDWR | O_CREAT, 0o600).unwrap();
    f.pwrite(0, &vec![b'{'; 9000]).unwrap();
    f.file.fsync().unwrap();
    c.rename(None, b"/data/appdata/brave/Local State.tmp", None, b"/data/appdata/brave/Local State", 0).unwrap();
    c.symlink(b"bitsos-1234", None, b"/data/appdata/brave/SingletonLock").unwrap();
    c.mkdir(None, b"/data/appdata/brave/Cache", 0o700).unwrap();
    for i in 0..200 {
        ns.create_file_bytes(format!("/data/appdata/brave/Cache/f_{i:04}").as_bytes(), 0o600, &vec![i as u8; 1000 + i * 91]).unwrap();
    }
    let n = ns.create_file_bytes(b"/data/appdata/brave/new", 0o600, b"replacement").unwrap();
    drop(n);
    c.rename(None, b"/data/appdata/brave/new", None, b"/data/appdata/brave/Local State", 0).unwrap();
}

#[test]
fn pool_image_on_another_file_system_through_the_vfs() {
    let s = Scratch::new();
    let (ns, file) = image_ns(96);
    data::format_pool(Arc::new(ImageBlock::new("data.img", file.clone(), true).unwrap()), b"bitsdata").unwrap();
    {
        let (vol, _m) = mount::mount_dev(open_data(&file), None, &ns, data::AT, opts()).unwrap();
        data::user_dirs(&ns, data::AT).unwrap();
        // A second call keeps what is there.
        data::user_dirs(&ns, data::AT).unwrap();
        let st = Ctx::kernel(&ns).stat(None, b"/data/home/1000", true).unwrap();
        assert_eq!((st.uid, st.gid, st.mode & 0o7777), (data::UID, data::UID, 0o700));
        assert_eq!(Ctx::kernel(&ns).stat(None, b"/data/appdata", true).unwrap().uid, data::UID);
        profile_work(&ns);
        let sf = vfs::statfs::of(&ns.lookup("/data").unwrap().sb, false).unwrap();
        assert_eq!(sf.ftype, 0xEF53);
        assert!(vfs::writeback::sync_all().failed == 0);
        assert!(!vol.st.lock().dirty, "the stop pass leaves the volume clean");
    }
    let mut img = vec![0u8; 96 << 20];
    assert_eq!(file.read_at(0, &mut img).unwrap(), img.len());
    let md = bitsvol::memdev::MemDev::new("copy", 4096, (96 << 20) / 4096, 256).unwrap();
    for (i, c) in img.chunks(1 << 20).enumerate() {
        md.write_blocks((i as u64) << 8, c, false, IoPriority::Normal).unwrap();
    }
    let vol = volume_bytes(Arc::new(md));
    assert_clean(&s, &vol, "Brave-like work on the pool volume in a loop image");
    let dump = debugfs(&s, &vol, "stat /appdata/brave/SingletonLock");
    assert!(dump.contains("Fast link dest: \"bitsos-1234\""), "{dump}");
    let ls = debugfs(&s, &vol, "cat \"/appdata/brave/Local State\"");
    assert_eq!(ls, "replacement");
}

#[test]
fn thin_volume_full_is_enospc_not_eio() {
    // A pool far smaller than its volume claims (thin): writes beyond the pool's room fail with ENOSPC and the
    // volume stays consistent.
    let s = Scratch::new();
    let md = Arc::new(bitsvol::memdev::MemDev::new("small", 4096, (48 << 20) / 4096, 256).unwrap());
    let mut pool = Pool::format(md.clone(), Config::default()).unwrap();
    let id = pool.create_volume(data::VOLUME, 128 << 20, 1).unwrap();
    let h = PoolHandle::new(pool, "small");
    let vd: Arc<dyn BlockDevice> = Arc::new(h.open_volume(id).unwrap());
    let mut o = super::mkfs::opts_for(true);
    o.journal_blocks = Some(1024);
    crate::mkfs::format(&BlockDev::new(vd.clone()), &o).unwrap();
    let ns = vfs::MountNs::new_empty();
    let (v, _m) = mount::mount_dev(Arc::new(BlockDev::new(vd.clone())), None, &ns, "/data", opts()).unwrap();
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/data/fill", O_RDWR | O_CREAT, 0o600).unwrap();
    let chunk = vec![0x5Au8; 1 << 20];
    let mut err = None;
    for i in 0..200u64 {
        if let Err(e) = f.pwrite(i << 20, &chunk).and_then(|_| f.file.fsync().map(|_| 0)) {
            err = Some(e);
            break;
        }
    }
    assert_eq!(err, Some(crate::errno::ENOSPC), "a full thin pool must say ENOSPC");
    drop(f);
    // The volume is still consistent: e2fsck on the volume taken out of the pool (after the device write
    // failed, ext4 kept the metadata of the blocks it could not write; what ENOSPC cost is only that data).
    let synced = v.sync_fs();
    let vol = volume_bytes(md.clone());
    let (code, out) = fsck(&s, &vol);
    assert!(code == 0, "after ENOSPC on the thin volume (sync_fs {synced:?}): {out}");
}

// FS-8c fixer: the boot steps (`bring_up`) on a private view, the hostile images, the user folders' repair,
// the stop pass on a frozen volume, CONFIG and the state line.

fn file_bytes(ns: &vfs::MountNs, path: &str) -> Vec<u8> {
    let f = ns.lookup(path).unwrap().inode.clone();
    let mut b = vec![0u8; f.size() as usize];
    assert_eq!(f.read_at(0, &mut b).unwrap(), b.len());
    b
}

fn exists(ns: &vfs::MountNs, path: &str) -> bool {
    ns.lookup_path(path.as_bytes(), false).is_ok()
}

/// Unmounts what `bring_up` mounted at `at` and takes its disk away (the next step mounts the image again).
fn take_down(ns: &vfs::MountNs, at: &str, m: Option<data::Mounted>) {
    let m = m.expect("mounted");
    ns.umount(&ns.lookup_path(at.as_bytes(), true).unwrap()).unwrap();
    if let Some(d) = &m.disk {
        block::unregister(d);
    }
}

fn place<'a>(image: &'a str, temp: &'a str, at: &'a str, pool: &'a str) -> data::Place<'a> {
    data::Place { image, temp, at, pool_name: pool, on_stick: false }
}

#[test]
fn bring_up_removes_a_leftover_before_the_room_check_then_makes_mounts_and_mounts_read_only() {
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/stick").unwrap();
    let p = place("/stick/data.img", "/stick/data.img.new", "/d", "hostbring");
    // A creation cut half-way: its room would make the check fail if it still counted.
    ns.create_file_bytes(p.temp.as_bytes(), 0o644, &[]).unwrap();
    ns.lookup(p.temp).unwrap().inode.truncate(40 << 20).unwrap();
    let free = || if exists(&ns, p.temp) { 300 << 20 } else { 1 << 40 };
    let creating = std::cell::Cell::new(false);
    let (ph, m) = data::bring_up(&ns, &p, true, 32, &free, &|x| creating.set(creating.get() || x == data::Phase::Creating));
    assert_eq!(ph, data::Phase::Mounted { pool: true, writable: true, created: true });
    assert!(creating.get(), "Creating was reported");
    assert!(!exists(&ns, p.temp), "the leftover and the temp are gone");
    let c = Ctx::kernel(&ns);
    assert_eq!(c.stat(None, b"/d/appdata", true).unwrap().uid, data::UID);
    ns.create_file_bytes(b"/d/home/1000/kept", 0o600, b"boot 1").unwrap();
    // What Reload does: the files' pages, then the stop pass.
    assert!(vfs::writeback::sync_all().failed == 0);
    assert!(!m.as_ref().unwrap().vol.st.lock().dirty);
    take_down(&ns, "/d", m);
    // The second boot mounts what is there.
    let (ph, m) = data::bring_up(&ns, &p, true, 32, &|| 0, &|_| panic!("no creation on the second boot"));
    assert_eq!(ph, data::Phase::Mounted { pool: true, writable: true, created: false });
    assert_eq!(ns.read_file("/d/home/1000/kept", 64).unwrap(), b"boot 1");
    // What Reload does: the files' pages, then the stop pass.
    assert!(vfs::writeback::sync_all().failed == 0);
    assert!(!m.as_ref().unwrap().vol.st.lock().dirty);
    take_down(&ns, "/d", m);
    // A read-only stick: /data read-only, nothing written to the image (also not by the stop pass).
    let before = file_bytes(&ns, p.image);
    let (ph, m) = data::bring_up(&ns, &p, false, 32, &|| 0, &|_| panic!("no creation on a read-only stick"));
    assert_eq!(ph, data::Phase::Mounted { pool: true, writable: false, created: false });
    let path = ns.lookup_path(b"/d", true).unwrap();
    assert!(path.is_mount_root() && path.mnt.readonly, "/d is a read-only mount");
    assert_eq!(ns.read_file("/d/home/1000/kept", 64).unwrap(), b"boot 1");
    assert_eq!(c.mkdir(None, b"/d/x", 0o755).err(), Some(crate::errno::EROFS));
    assert_eq!(vfs::writeback::Flush::stop(&*m.as_ref().unwrap().vol), Ok(true));
    vfs::writeback::sync_all();
    take_down(&ns, "/d", m);
    assert!(file_bytes(&ns, p.image) == before, "the read-only mount wrote to the image");
}

#[test]
fn bring_up_skips_without_room_and_a_read_only_stick_never_creates() {
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/stick").unwrap();
    let p = place("/stick/data.img", "/stick/data.img.new", "/d", "hostskip");
    // 32 MiB + 512 MiB kept free: 500 MiB is not enough.
    let (ph, m) = data::bring_up(&ns, &p, true, 32, &|| 500 << 20, &|_| panic!("no creation without room"));
    assert!(matches!(ph, data::Phase::Skipped(_)) && m.is_none(), "{ph:?}");
    // A leftover on a read-only stick stays (nothing is written there), and nothing is made.
    ns.create_file_bytes(p.temp.as_bytes(), 0o644, b"half").unwrap();
    let (ph, _) = data::bring_up(&ns, &p, false, 32, &|| 1 << 40, &|_| panic!("no creation on a read-only stick"));
    assert!(matches!(ph, data::Phase::Skipped(_)), "{ph:?}");
    assert!(exists(&ns, p.temp) && !exists(&ns, p.image));
}

#[test]
fn hostile_images_are_left_as_they_are_and_not_mounted() {
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/stick").unwrap();
    let p = place("/stick/data.img", "/stick/data.img.new", "/d", "hosthostile");
    let mut garbage = vec![0u8; 16 << 20];
    fill_pattern(&mut garbage);
    let truncated = vec![0u8; 5000];
    for (what, bytes) in [("garbage", garbage), ("zeros", vec![0u8; 16 << 20]), ("not a block multiple", truncated)] {
        ns.create_file_bytes(p.image.as_bytes(), 0o644, &bytes).unwrap();
        let (ph, m) = data::bring_up(&ns, &p, true, 32, &|| 1 << 40, &|_| panic!("an existing image is never remade"));
        assert!(matches!(ph, data::Phase::Failed(_)) && m.is_none(), "{what}: {ph:?}");
        assert!(file_bytes(&ns, p.image) == bytes, "{what}: the image was changed");
        assert!(!exists(&ns, "/d/appdata"), "{what}: nothing mounted at /d");
    }
    // A pool whose metadata is damaged after its superblocks: not mounted, not reformatted.
    ns.create_file_bytes(p.image.as_bytes(), 0o644, &[]).unwrap();
    let f = ns.lookup(p.image).unwrap().inode.clone();
    f.truncate(32 << 20).unwrap();
    data::format_pool(Arc::new(ImageBlock::new("x", f.clone(), true).unwrap()), b"h").unwrap();
    let mut b = file_bytes(&ns, p.image);
    for x in &mut b[2 * 4096..64 * 4096] {
        *x ^= 0xA5;
    }
    f.write_at(0, &b).unwrap();
    drop(f);
    let (ph, m) = data::bring_up(&ns, &p, true, 32, &|| 1 << 40, &|_| panic!("never remade"));
    assert!(matches!(ph, data::Phase::Failed(_)) && m.is_none(), "damaged pool: {ph:?}");
    assert!(file_bytes(&ns, p.image) == b, "damaged pool: the image was changed");
}

fn fill_pattern(b: &mut [u8]) {
    for (i, x) in b.iter_mut().enumerate() {
        *x = ((i as u32).wrapping_mul(2654435761) >> 11) as u8;
    }
}

#[test]
fn a_plain_ext4_image_mounts_as_a_loop_image() {
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/stick").unwrap();
    let p = place("/stick/data.img", "/stick/data.img.new", "/d", "hostplain");
    ns.create_file_bytes(p.image.as_bytes(), 0o644, &[]).unwrap();
    let f = ns.lookup(p.image).unwrap().inode.clone();
    f.truncate(24 << 20).unwrap();
    let dev = BlockDev::new(Arc::new(ImageBlock::new("x", f.clone(), true).unwrap()));
    crate::mkfs::format(&dev, &super::mkfs::opts_for(true)).unwrap();
    drop((dev, f));
    let (ph, m) = data::bring_up(&ns, &p, true, 32, &|| 0, &|_| panic!("never remade"));
    assert_eq!(ph, data::Phase::Mounted { pool: false, writable: true, created: false });
    let m = m.unwrap();
    assert!(m.disk.is_none() && m.handle.is_none());
    assert_eq!(ns.lookup_path(b"/d", true).unwrap().dentry.sb.source(), "/dev/loop0");
    assert_eq!(Ctx::kernel(&ns).stat(None, b"/d/home/1000", true).unwrap().uid, data::UID);
}

#[test]
fn user_dirs_gives_an_empty_root_owned_folder_to_the_user_and_keeps_a_used_one() {
    let ns = vfs::MountNs::new_empty();
    let dev = Arc::new(crate::MemDev::new(vec![0u8; 16 << 20]));
    crate::mkfs::format(&*dev, &super::mkfs::opts_for(true)).unwrap();
    let (_v, _m) = mount::mount_dev(dev, None, &ns, "/d", opts()).unwrap();
    let c = Ctx::kernel(&ns);
    // A cut after the mkdir, before the chown: root's and empty.
    c.mkdir(None, b"/d/appdata", 0o755).unwrap();
    // Root's and used: left alone (someone put it there on purpose).
    c.mkdir(None, b"/d/home", 0o755).unwrap();
    c.mkdir(None, b"/d/home/1000", 0o755).unwrap();
    ns.create_file_bytes(b"/d/home/1000/root-file", 0o600, b"x").unwrap();
    data::user_dirs(&ns, "/d").unwrap();
    let a = c.stat(None, b"/d/appdata", true).unwrap();
    assert_eq!((a.uid, a.gid, a.mode & 0o7777), (data::UID, data::UID, 0o755));
    let h = c.stat(None, b"/d/home/1000", true).unwrap();
    assert_eq!((h.uid, h.mode & 0o7777), (0, 0o755), "a used folder is kept as it is");
}

#[test]
fn stop_on_a_frozen_volume_returns_at_once_and_does_nothing() {
    let ns = vfs::MountNs::new_empty();
    let dev = Arc::new(crate::MemDev::new(vec![0u8; 16 << 20]));
    crate::mkfs::format(&*dev, &super::mkfs::opts_for(true)).unwrap();
    let (v, _m) = mount::mount_dev(dev, None, &ns, "/d", opts()).unwrap();
    ns.create_file_bytes(b"/d/a", 0o600, b"before the freeze").unwrap();
    Ctx::kernel(&ns).open(None, b"/d/a", O_RDWR, 0).unwrap().file.fsync().unwrap();
    v.freeze().unwrap();
    let t0 = std::time::Instant::now();
    assert_eq!(vfs::writeback::Flush::stop(&*v), Ok(false), "a frozen volume stays as its freeze left it");
    vfs::writeback::Flush::flush(&*v, vfs::pagecache::SyncKind::Fsync).unwrap();
    assert!(t0.elapsed() < std::time::Duration::from_secs(2), "{:?}", t0.elapsed());
    v.thaw();
    ns.create_file_bytes(b"/d/b", 0o600, b"after").unwrap();
    assert_eq!(vfs::writeback::Flush::stop(&*v), Ok(true));
    assert!(!v.st.lock().dirty, "clean after the stop");
}

#[test]
fn config_size_and_state_line() {
    assert_eq!(data::image_mb(None), Some(2048));
    assert_eq!(data::image_mb(Some("")), Some(2048));
    assert_eq!(data::image_mb(Some("0")), None);
    assert_eq!(data::image_mb(Some(" 64 ")), Some(64));
    assert_eq!(data::image_mb(Some("31")), Some(32));
    assert_eq!(data::image_mb(Some("4096")), Some(4095));
    assert_eq!(data::image_mb(Some("2G")), Some(2048));
    assert_eq!(data::describe(data::Phase::Waiting), "waiting\n");
    assert_eq!(data::describe(data::Phase::Creating), "creating\n");
    assert_eq!(data::describe(data::Phase::Mounted { pool: true, writable: true, created: true }), "mounted pool rw created\n");
    assert_eq!(data::describe(data::Phase::Mounted { pool: false, writable: false, created: false }), "mounted plain ro\n");
    assert_eq!(data::describe(data::Phase::Skipped("no boot stick")), "skipped no boot stick\n");
    assert_eq!(data::describe(data::Phase::Failed("x")), "failed x\n");
}

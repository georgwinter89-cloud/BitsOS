//! `/data` at boot (FS-8c, V2-PLAN §8a.8): the ext4 data volume in the loop image `\BITSOS2\data.img` on the
//! boot stick until the BitsOS SSD exists. The image holds a bitsvol thin pool (VS-1) with one volume "data"
//! (snapshots for VS-2/3); the volume is registered as a stacked disk and mounted at `/data`. An image made
//! elsewhere with a plain ext4 (`mkfs.ext4` on CachyOS) is mounted as a plain loop image.
//!
//! A background thread waits for the stick's write decision. A missing image is made only on a writable stick
//! with room for it: grown as `data.img.new` (zero-filled by FAT, in steps so `/boot` stays usable), pool and
//! ext4 made inside, then renamed to `data.img` (a cut before the rename leaves only the `.new` file, removed
//! at the next boot). An existing `data.img` is never shrunk, reformatted or deleted: one V2 cannot read stays
//! as it is and `/data` is not mounted. A read-only stick mounts `/data` read-only.
//!
//! The phase is in `phase()` and, for user space, one line in `/run/bitsos/data-state` (`describe`).
//!
//! NV-2p (Georg 5.11.): with a checked window into the Windows SSD (`block::gpt::window`, CONFIG `nvme_write=on` and
//! `nvme_data=`) the bitsvol pool on that partition is /data, the same stack as ever; the stick's `data.img` is
//! then mounted read-only at /stickdata, never made, changed or removed. No window, a refused one or a partition
//! without a usable pool: everything as below, with a Warn. /data never formats anything at boot (`bitsdisk` does).
//!
//! CONFIG `data_img_mb=` sets the size of a new image (default 2048, 32..4095: FAT32 files stay below 4 GiB);
//! `data_img_mb=0` turns `/data` off.

use crate::errno::*;
use crate::imgblk::{BlockDev, ImageBlock};
use crate::mkfs::{self, MkfsOptions};
use crate::vol::Options;
use alloc::sync::Arc;
use bitsvol::{Config, Pool, PoolHandle, VolError};
use devmodel::block::BlockDevice;
use ksync::SpinLock;
use vfs::inode::Cred;
use vfs::mount_ns::MountNs;
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

pub const IMAGE: &str = "/boot/BITSOS2/data.img";
const TEMP: &str = "/boot/BITSOS2/data.img.new";
pub const AT: &str = "/data";
/// Where the stick's image is mounted read-only while /data lies on the SSD.
pub const STICK_AT: &str = "/stickdata";
pub const VOLUME: &str = "data";
const DEFAULT_MB: u64 = 2048;
const MIN_MB: u64 = 32;
const MAX_MB: u64 = 4095;
/// Room the stick keeps after a new image (kernel generations, programs below `\BITSOS2\`).
const KEEP_FREE: u64 = 512 << 20;
/// The image grows in steps of this size: FAT holds its volume lock while it zero-fills one step.
const GROW_STEP: u64 = 16 << 20;
/// How long the thread waits for the stick (USB enumerates and settles after the kernel starts).
const STICK_WAIT_MS: u64 = 120_000;
/// How long /data waits for the NVMe driver's word on a window before it goes on without (controllers bring their
/// namespaces up within seconds; CAP.TO may allow more, the machine must still come up).
const WINDOW_WAIT_MS: u64 = 60_000;
pub const UID: u32 = 1000;

/// Where the boot mount stands (ktests, MCP checks through the events).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Waiting,
    Creating,
    /// Mounted at /data.
    Mounted { pool: bool, writable: bool, created: bool },
    /// Not mounted, on purpose (no stick, turned off, no room on a read-only or full stick).
    Skipped(&'static str),
    /// Not mounted after an error.
    Failed(&'static str),
}

static PHASE: SpinLock<Phase> = SpinLock::named("ext4.data.phase", Phase::Waiting);

pub fn phase() -> Phase {
    *PHASE.lock()
}

fn set(p: Phase) {
    *PHASE.lock() = p;
}

/// Waits (sleeping) up to `ms` until the boot mount has finished one way or the other.
pub fn wait_done(ms: u64) -> Phase {
    let end = ksync::now_ns().unwrap_or(0).saturating_add(ms * 1_000_000);
    loop {
        let p = phase();
        if !matches!(p, Phase::Waiting | Phase::Creating) || ksync::now_ns().unwrap_or(u64::MAX) >= end {
            return p;
        }
        sched::sleep_ns(50_000_000);
    }
}

/// The size of a new image from CONFIG `data_img_mb=` (None: turned off).
pub fn image_mb(v: Option<&str>) -> Option<u64> {
    match v.map(str::trim) {
        None | Some("") => Some(DEFAULT_MB),
        Some(s) => match s.parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n.clamp(MIN_MB, MAX_MB)),
            Err(_) => {
                klog::event!(Warn, "ext4", "data.config_bad", value = s; "data_img_mb is not a number: the default is used");
                Some(DEFAULT_MB)
            }
        },
    }
}

fn now_ns() -> u64 {
    let t = vfs::inode::now();
    (t.sec.max(0) as u64).saturating_mul(1_000_000_000).saturating_add(t.nsec as u64)
}

fn random16() -> [u8; 16] {
    let mut b = [0u8; 16];
    vfs::random::fill(&mut b);
    b
}

/// A new pool with one volume "data" over all of `dev`, and ext4 inside it.
pub fn format_pool(dev: Arc<dyn BlockDevice>, label: &[u8]) -> Result<(), i64> {
    let mut pool = Pool::format(dev, Config::default()).map_err(|e| e.errno() as i64)?;
    let s = pool.stats();
    let chunks = s.free.saturating_sub(s.reserve);
    let size = chunks.saturating_mul(s.chunk_size as u64) & !((4 << 20) - 1);
    if size < 8 << 20 {
        return Err(ENOSPC);
    }
    let id = pool.create_volume(VOLUME, size, now_ns()).map_err(|e| e.errno() as i64)?;
    let h = PoolHandle::new(pool, "data.mkfs");
    let vd: Arc<dyn BlockDevice> = Arc::new(h.open_volume(id).map_err(|e| e.errno() as i64)?);
    let mut o = MkfsOptions::new(label, random16(), random16(), vfs::inode::now());
    // A single-user volume: 1 % for root (the launcher's mkdir/chown, repairs) instead of mke2fs's 5 %.
    o.reserved_percent = 1;
    // A fresh thin volume reads zero wherever nothing was written.
    o.zeroed = true;
    let made = mkfs::format(&BlockDev::new(vd.clone()), &o)?;
    vd.flush().map_err(|e| block::BlockError::from_dev(e).errno())?;
    klog::event!(Info, "ext4", "data.pool_made", volume_bytes = size, blocks = made.blocks, chunk = s.chunk_size, chunks = s.chunks);
    Ok(())
}

/// What a device with this BitsOS pool's signature holds (bitsdisk asks before it would make one again).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolLook {
    /// A pool with the volume "data": never made again.
    Data,
    /// No superblock decodes, or a good pool without that volume: a cut in `format_pool` left it, nothing to lose.
    HalfMade,
    /// A read error, no memory, damaged metadata: it may hold Georg's data, so it is never taken for empty.
    Unclear(alloc::string::String),
}

pub fn look_at_pool(dev: Arc<dyn BlockDevice>) -> PoolLook {
    let pool = match Pool::open(dev, Config::default()) {
        Ok(p) => p,
        Err(VolError::NotFound) => return PoolLook::HalfMade,
        Err(e) => return PoolLook::Unclear(alloc::format!("{e:?}")),
    };
    match pool.list() {
        Ok(v) if v.iter().any(|v| v.label == VOLUME && !v.snapshot) => PoolLook::Data,
        Ok(_) => PoolLook::HalfMade,
        Err(e) => PoolLook::Unclear(alloc::format!("{e:?}")),
    }
}

/// Grows `place.temp` to `bytes` (zero-filled), makes the pool and ext4 inside, renames it to `place.image`.
fn create(ns: &MountNs, place: &Place, bytes: u64) -> Result<(), i64> {
    let c = Ctx::kernel(ns);
    ns.create_file_bytes(place.temp.as_bytes(), 0o644, &[])?;
    let file = ns.lookup_path(place.temp.as_bytes(), false)?.dentry.inode.clone();
    let t0 = ksync::now_ns().unwrap_or(0);
    let mut size = 0;
    while size < bytes {
        size = (size + GROW_STEP).min(bytes);
        file.truncate(size)?;
        if size % (256 << 20) == 0 || size == bytes {
            klog::event!(Info, "ext4", "data.grow", bytes = size, of = bytes, ms = ksync::now_ns().unwrap_or(0).saturating_sub(t0) / 1_000_000);
        }
    }
    file.sync(SyncKind::Fsync)?;
    format_pool(Arc::new(ImageBlock::new("data.img", file.clone(), true)?), b"bitsdata")?;
    file.sync(SyncKind::Fsync)?;
    drop(file);
    c.rename(None, place.temp.as_bytes(), None, place.image.as_bytes(), 0)?;
    if place.on_stick
        && let Some(fat) = fs_fat::stick::volume()
    {
        vfs::writeback::Flush::flush(&*fat, SyncKind::Fsync)?;
    }
    klog::event!(Info, "ext4", "data.created", path = place.image, bytes = bytes, ms = ksync::now_ns().unwrap_or(0).saturating_sub(t0) / 1_000_000);
    Ok(())
}

/// A half-made image (a creation cut before its rename, or one that failed) is removed, so its room on the
/// stick counts as free again. Only `temp`, never the image.
fn remove_leftover(ns: &MountNs, temp: &str) {
    if ns.lookup_path(temp.as_bytes(), false).is_err() {
        return;
    }
    match ns.remove(temp) {
        Ok(()) => klog::event!(Info, "ext4", "data.leftover_removed", path = temp; "a creation was cut or failed before its rename"),
        Err(e) => klog::event!(Warn, "ext4", "data.leftover_stays", path = temp, errno = e),
    }
}

/// A mounted data image.
#[derive(Clone)]
pub struct Mounted {
    /// On a bitsvol pool (else a plain ext4 image).
    pub pool: bool,
    pub vol: Arc<crate::Volume>,
    /// The stacked disk of the pool's volume (`<pool name>.v<id>`).
    pub disk: Option<alloc::string::String>,
    /// The pool and the volume's id (VS-2 snapshots it: `PoolHandle::lock()?.snapshot(id, ..)`, or
    /// `open_volume(id)` for `Snapshots`).
    pub handle: Option<(Arc<PoolHandle>, u64)>,
}

/// The boot mount at /data (None until it is mounted, and when it is not).
static DATA: SpinLock<Option<Mounted>> = SpinLock::named("ext4.data.mounted", None);

/// The volume, pool and disk behind /data (VS-2's freeze and snapshots, VS-3's restore).
pub fn mounted() -> Option<Mounted> {
    DATA.lock().clone()
}

/// Mounts the image at `at`: the pool's volume "data" (registered as the stacked disk `<pool_name>.v<id>`),
/// else a plain ext4 image. Never writes anything before the mount decides it may.
pub fn mount(ns: &MountNs, image: &str, at: &str, writable: bool, pool_name: &str) -> Result<Mounted, &'static str> {
    let file = ns.lookup_path(image.as_bytes(), true).map_err(|_| "the image cannot be looked up")?.dentry.inode.clone();
    let opts = Options { writable, ..Options::default() };
    let dev: Arc<dyn BlockDevice> = Arc::new(ImageBlock::new(pool_name, file, writable).map_err(|_| "the image cannot be opened")?);
    match Pool::open(dev.clone(), Config::default()) {
        Ok(pool) => mount_pool(pool, ns, at, opts, pool_name),
        Err(VolError::NotFound) => {
            drop(dev);
            match crate::mount::mount_image(ns, image, at, opts) {
                Ok((vol, m)) => {
                    m.sb.set_source("/dev/loop0");
                    Ok(Mounted { pool: false, vol, disk: None, handle: None })
                }
                Err(e) => {
                    klog::event!(Warn, "ext4", "data.mount_failed", errno = e, pool = false);
                    Err("neither a bitsvol pool nor an ext4 volume")
                }
            }
        }
        Err(e) => {
            klog::event!(Error, "ext4", "data.pool_unusable", error = alloc::format!("{e:?}").as_str(); "the image is left as it is");
            Err("the pool in the image cannot be opened")
        }
    }
}

/// The pool's volume "data" at `at`, registered as the stacked disk `<pool_name>.v<id>`.
fn mount_pool(pool: Pool, ns: &MountNs, at: &str, opts: Options, pool_name: &str) -> Result<Mounted, &'static str> {
    let vols = pool.list().map_err(|_| "the pool's volume list cannot be read")?;
    let id = vols.iter().find(|v| v.label == VOLUME && !v.snapshot).map(|v| v.id).ok_or("the pool has no volume \"data\"")?;
    let h = PoolHandle::new(pool, pool_name);
    let vd = h.open_volume(id).map_err(|_| "the volume cannot be opened")?;
    let handle = Some((h.clone(), id));
    let disk = block::register(Arc::new(vd), block::DiskKind::Stacked).map_err(|_| "the volume cannot be registered as a disk")?;
    let blocks = disk.dev_info().blocks;
    match crate::mount::mount_disk(disk.clone(), 0, blocks, ns, at, opts) {
        Ok((vol, m)) => {
            m.sb.set_source(&alloc::format!("/dev/mapper/{pool_name}"));
            Ok(Mounted { pool: true, vol, disk: Some(disk.name().into()), handle })
        }
        Err(e) => {
            klog::event!(Warn, "ext4", "data.mount_failed", errno = e, pool = true);
            block::unregister(disk.name());
            Err("ext4 on the pool's volume does not mount")
        }
    }
}

/// `/data/appdata` and `/data/home/1000` for the user (uid 1000), made when missing. An existing one is kept,
/// except one still empty and owned by root where the user's is wanted: a cut between the mkdir and the chown
/// left it so, and nothing else makes it.
pub fn user_dirs(ns: &MountNs, at: &str) -> Result<(), i64> {
    let c = Ctx::kernel(ns);
    for (rel, mode, owner) in [("appdata", 0o755, UID), ("home", 0o755, 0), ("home/1000", 0o700, UID)] {
        let p = alloc::format!("{at}/{rel}");
        match c.mkdir(None, p.as_bytes(), mode) {
            Ok(path) => {
                if owner != 0 {
                    c.setattr(&path, |m| vfs::setattr::chown(m, Cred::ROOT, owner, owner))?;
                }
                klog::event!(Info, "ext4", "data.dir_made", path = p.as_str(), uid = owner);
            }
            Err(EEXIST) if owner != 0 => fix_owner(ns, &p, mode, owner)?,
            Err(EEXIST) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn fix_owner(ns: &MountNs, p: &str, mode: u32, owner: u32) -> Result<(), i64> {
    let c = Ctx::kernel(ns);
    let a = c.stat(None, p.as_bytes(), false)?;
    if a.uid != 0 || a.mode & uapi::S_IFMT != uapi::S_IFDIR || !dir_empty(ns, p)? {
        return Ok(());
    }
    let path = ns.lookup_path(p.as_bytes(), false)?;
    // The mode first: the owner is the mark of a finished folder.
    c.setattr(&path, |_| vfs::setattr::chmod(mode))?;
    c.setattr(&path, |m| vfs::setattr::chown(m, Cred::ROOT, owner, owner))?;
    klog::event!(Warn, "ext4", "data.dir_fixed", path = p, uid = owner; "an empty root-owned user folder (a cut after its mkdir) given to its user");
    Ok(())
}

/// Only "." and ".." (one getdents64 read: three records of short names fit in 512 bytes).
fn dir_empty(ns: &MountNs, p: &str) -> Result<bool, i64> {
    let f = ns.open_flags(p.as_bytes(), vfs::abi::O_RDONLY | vfs::abi::O_DIRECTORY, 0)?;
    let b = vfs::ops::read_dir(&f, 512)?;
    let (mut at, mut n) = (0usize, 0usize);
    while at + 19 <= b.len() {
        let len = u16::from_le_bytes([b[at + 16], b[at + 17]]) as usize;
        if len == 0 {
            break;
        }
        n += 1;
        at += len;
    }
    Ok(n <= 2)
}

/// Where a data image lives and where it is mounted (the boot one is `BOOT`; tests use their own).
#[derive(Clone, Copy)]
pub struct Place<'a> {
    pub image: &'a str,
    /// The image while it is being made.
    pub temp: &'a str,
    pub at: &'a str,
    /// The stacked disk's name prefix (`<pool_name>.v<id>`) and `/dev/mapper/<pool_name>`.
    pub pool_name: &'a str,
    /// The image is on the boot stick: the stick's FAT is flushed after the rename.
    pub on_stick: bool,
}

pub const BOOT: Place<'static> = Place { image: IMAGE, temp: TEMP, at: AT, pool_name: VOLUME, on_stick: true };

/// The boot mount once the stick decided (`rw`: writable): a leftover `temp` is removed first (so its room
/// counts as free), a missing image of `mb` MiB is made when `free()` (the free bytes where it goes, read after
/// that removal) leaves `KEEP_FREE`, then the image is mounted and the user folders made. `report` hears
/// `Creating` before a creation starts.
pub fn bring_up(ns: &MountNs, place: &Place, rw: bool, mb: u64, free: &dyn Fn() -> u64, report: &dyn Fn(Phase)) -> (Phase, Option<Mounted>) {
    if rw {
        remove_leftover(ns, place.temp);
    }
    let mut created = false;
    match ns.lookup_path(place.image.as_bytes(), true) {
        Ok(_) => {}
        Err(ENOENT) if !rw => {
            klog::event!(Info, "ext4", "data.skipped", reason = "no data.img and the stick is read-only");
            return (Phase::Skipped("stick read-only: no data.img"), None);
        }
        Err(ENOENT) => {
            let bytes = mb << 20;
            let free = free();
            if free < bytes.saturating_add(KEEP_FREE) {
                klog::event!(Info, "ext4", "data.skipped", reason = "no room on the stick", free = free, want = bytes, keep = KEEP_FREE);
                return (Phase::Skipped("no room on the stick for data.img"), None);
            }
            report(Phase::Creating);
            klog::event!(Info, "ext4", "data.creating", path = place.image, bytes = bytes, free = free);
            if let Err(e) = create(ns, place, bytes) {
                klog::event!(Error, "ext4", "data.create_failed", errno = e; "no /data this boot; the half-made image is removed");
                remove_leftover(ns, place.temp);
                return (Phase::Failed("making data.img failed"), None);
            }
            created = true;
        }
        Err(e) => {
            klog::event!(Error, "ext4", "data.lookup_failed", errno = e);
            return (Phase::Failed("data.img cannot be looked up"), None);
        }
    }
    let m = match mount(ns, place.image, place.at, rw, place.pool_name) {
        Ok(m) => m,
        Err(why) => {
            klog::event!(Error, "ext4", "data.not_mounted", reason = why);
            return (Phase::Failed(why), None);
        }
    };
    if rw && let Err(e) = user_dirs(ns, place.at) {
        klog::event!(Warn, "ext4", "data.dirs_failed", errno = e);
    }
    klog::event!(Info, "ext4", "data.mounted", at = place.at, pool = m.pool, writable = rw, created = created, image = place.image);
    (Phase::Mounted { pool: m.pool, writable: rw, created }, Some(m))
}

/// Where user space reads the boot mount's phase (`describe`): the Brave launcher and init wait on it instead
/// of guessing from /proc/mounts while the stick settles or the first boot makes the image.
pub const STATE_FILE: &str = "/run/bitsos/data-state";
const STATE_NEW: &str = "/run/bitsos/data-state.new";

/// One line: `waiting`, `creating`, `mounted pool|plain rw|ro [created]`, `skipped <why>`, `failed <why>`.
pub fn describe(p: Phase) -> alloc::string::String {
    match p {
        Phase::Waiting => "waiting\n".into(),
        Phase::Creating => "creating\n".into(),
        Phase::Mounted { pool, writable, created } => alloc::format!(
            "mounted {} {}{}\n",
            if pool { "pool" } else { "plain" },
            if writable { "rw" } else { "ro" },
            if created { " created" } else { "" }
        ),
        Phase::Skipped(why) => alloc::format!("skipped {why}\n"),
        Phase::Failed(why) => alloc::format!("failed {why}\n"),
    }
}

/// `phase()` and `STATE_FILE` (replaced by a rename: a reader sees the old line or the new one).
fn publish(ns: &MountNs, p: Phase) {
    set(p);
    let r = ns
        .create_file_bytes(STATE_NEW.as_bytes(), 0o644, describe(p).as_bytes())
        .and_then(|_| Ctx::kernel(ns).rename(None, STATE_NEW.as_bytes(), None, STATE_FILE.as_bytes(), 0));
    if let Err(e) = r {
        klog::event!(Warn, "ext4", "data.state_unwritten", path = STATE_FILE, errno = e);
    }
}

/// The stacked disk of the boot's window partition, if there is one to use. Waits (bounded) for the driver's word;
/// every refusal has been said by the driver or is said here, and /data goes on as if there were no keys.
fn window_partition() -> Option<alloc::string::String> {
    use block::gpt::window::{self, Boot, Why};
    match window::boot_keys() {
        Ok(_) => {}
        Err(Why::NoDataKey) => return None,
        Err(why) => {
            klog::event!(Warn, "ext4", "data.window_refused", why = why.word(), text = why.text(); "every disk stays read-only, /data stays on the stick's data.img");
            return None;
        }
    }
    let t0 = ksync::now_ns().unwrap_or(0);
    let waited = || ksync::now_ns().unwrap_or(u64::MAX).saturating_sub(t0) / 1_000_000;
    loop {
        // No controller at all: nothing else tells the driver's verdict.
        window::look();
        match window::boot_state() {
            Boot::Armed { part, .. } => return Some(part),
            Boot::Refused { word, .. } => {
                klog::event!(Info, "ext4", "data.window_refused", why = word; "/data stays on the stick's data.img");
                window::data_went_on();
                return None;
            }
            Boot::Looking => {}
        }
        if waited() >= WINDOW_WAIT_MS {
            klog::event!(Warn, "ext4", "data.window_refused", why = "timeout"; "no word from the NVMe driver: /data stays on the stick's data.img");
            window::data_went_on();
            return None;
        }
        sched::sleep_ns(20_000_000);
    }
}

/// /data on the window's partition, writable, and the user folders. Err: why not (the partition stays as it is; no
/// repair and no format here, only `bitsdisk` makes a pool).
pub fn mount_window(ns: &MountNs, part: &str, at: &str, pool_name: &str) -> Result<Mounted, &'static str> {
    let d = block::find(part).ok_or("the partition is not registered")?;
    let dev: Arc<dyn BlockDevice> = Arc::new(block::DiskDevice(d));
    let pool = Pool::open(dev, Config::default()).map_err(|e| {
        klog::event!(Warn, "ext4", "data.window_pool", part = part, error = alloc::format!("{e:?}").as_str());
        "the partition holds no usable pool"
    })?;
    let m = mount_pool(pool, ns, at, Options { writable: true, ..Options::default() }, pool_name)?;
    if let Err(e) = user_dirs(ns, at) {
        klog::event!(Warn, "ext4", "data.dirs_failed", errno = e);
    }
    Ok(m)
}

/// The stick's `data.img` at /stickdata, read-only, for the one move of its contents (host/tests/minipc/data-move.sh).
/// Its absence or a failure costs nothing but the Warn; nothing is made and nothing removed.
fn stick_data(ns: &MountNs) {
    if fs_fat::stick::wait_mounted(STICK_WAIT_MS).is_none() {
        klog::event!(Info, "ext4", "data.stickdata", state = "no stick");
        return;
    }
    if ns.lookup_path(IMAGE.as_bytes(), true).is_err() {
        klog::event!(Info, "ext4", "data.stickdata", state = "no data.img");
        return;
    }
    match mount(ns, IMAGE, STICK_AT, false, "stickdata") {
        Ok(m) => {
            klog::event!(Info, "ext4", "data.stickdata", state = "mounted", pool = m.pool; "read-only");
            *STICK_DATA.lock() = Some(m);
        }
        Err(why) => klog::event!(Warn, "ext4", "data.stickdata", state = "not mounted", reason = why),
    }
}

/// /stickdata is there.
pub fn stick_data_mounted() -> bool {
    STICK_DATA.lock().is_some()
}

/// Keeps /stickdata's volume and pool alive.
static STICK_DATA: SpinLock<Option<Mounted>> = SpinLock::named("ext4.data.stick", None);

fn run() {
    let ns = vfs::root_ns();
    publish(&ns, Phase::Waiting);
    if let Some(part) = window_partition() {
        match mount_window(&ns, &part, AT, VOLUME) {
            Ok(m) => {
                let pool = m.pool;
                *DATA.lock() = Some(m);
                klog::event!(Info, "ext4", "data.mounted", at = AT, pool = pool, writable = true, created = false, image = part.as_str(); "/data lies on the window partition");
                publish(&ns, Phase::Mounted { pool, writable: true, created: false });
                stick_data(&ns);
                return;
            }
            Err(why) => klog::event!(Warn, "ext4", "data.window_unused", part = part.as_str(), reason = why; "/data stays on the stick's data.img"),
        }
    }
    let Some(mb) = image_mb(kexec::boot::config("data_img_mb").as_deref()) else {
        klog::event!(Info, "ext4", "data.off"; "data_img_mb=0: no /data");
        publish(&ns, Phase::Skipped("data_img_mb=0"));
        return;
    };
    let Some(fat) = fs_fat::stick::wait_mounted(STICK_WAIT_MS) else {
        klog::event!(Info, "ext4", "data.skipped", reason = "no boot stick mounted");
        publish(&ns, Phase::Skipped("no boot stick"));
        return;
    };
    // data.img is the stick's fallback only: /boot on the NVMe beside a refused or unused window makes none there.
    if fs_fat::stick::on_nvme() {
        klog::event!(Warn, "ext4", "data.skipped", reason = "the boot volume is on the NVMe"; "no data.img there: /data needs the nvme_data= window");
        publish(&ns, Phase::Skipped("boot volume on the NVMe, no window"));
        return;
    }
    let rw = fs_fat::stick::last_reason().is_none();
    let free = || fat.statfs().map(|(cb, _, f)| cb * f).unwrap_or(0);
    let (p, m) = bring_up(&ns, &BOOT, rw, mb, &free, &|p| publish(&ns, p));
    if let Some(m) = m {
        *DATA.lock() = Some(m);
    }
    publish(&ns, p);
}

fn init() -> Result<(), &'static str> {
    sched::spawn("ext4.data", sched::Class::Background, run).map(|_| ()).map_err(|_| "the /data thread could not start")
}
kreg::initcall!(kreg::Level::Service, "ext4.data", init);

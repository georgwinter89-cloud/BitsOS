//! The boot stick at `/boot` (kernel side).
//!
//! When the block layer lists a USB disk it recognised as the boot stick (`StickMatch::Yes`: a partition start
//! or block 0 holds the 96 bytes bitsboot read from the boot volume), this module plugs its write gate into
//! that disk, mounts the FAT32 volume there at `/boot` and decides whether it may be written. **All** of these
//! must hold, else the volume is mounted read-only and the first reason that failed is logged (R8b: a doubt
//! refuses the write; V1 `fs/stickid.asm`):
//! 1. bitsboot handed over `\BITSOS2\STICK.ID` and the volume's own `\BITSOS2\STICK.ID` holds the same 32 hex
//!    digits;
//! 2. the volume label is `BITSOS` (the stick tool's rule, BT-3);
//! 3. exactly one USB disk matches the stick and no USB disk is a doubt (`StickMatch::Unknown`): a cloned
//!    stick or one whose partition starts could not be read turns writing off for both;
//! 4. the device is not write-protected, its partition table is clean (no Windows/EFI partition, R8a), and
//!    the volume has two mirrored FATs.
//!
//! The volume is mounted read-only at once and writing is decided only after no disk came or went for 2 s, so a
//! clone plugged in with the stick is seen first. A USB disk that might be the stick appearing later turns
//! writing off for good (`set_read_only`).
//!
//! The write gate (`StickGate`) is the block layer's last word on this disk: it allows exactly the sectors the
//! FAT code announces while it writes them (`DiskDev::write`), and only inside the FATs, the data area and the
//! FSInfo sector; never the partition table, the boot sector or its backup, and nothing at all unless armed.

use crate::vol::{Clock, Options, Volume};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use block::{Disk, DiskKind, StickMatch};
pub use crate::gate::{DiskDev, StickGate};
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{Mutex, SpinLock};
use uapi::errno::*;

/// Where the stick is mounted in the boot view.
pub const MOUNT: &str = "/boot";

/// `\BITSOS2\STICK.ID`'s text as 16 bytes (bitsboot's rule: 32 hex digits, white space around ignored).
pub fn parse_stick_id(text: &[u8]) -> Option<[u8; 16]> {
    let t = core::str::from_utf8(text).ok()?.trim();
    if t.len() != 32 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(id)
}

/// What the write decision looks at.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub loader_id: Option<[u8; 16]>,
    pub volume_id: Option<[u8; 16]>,
    pub label: Vec<u8>,
    /// USB disks recognised as the stick, and USB disks that might be it.
    pub sticks: usize,
    pub doubts: usize,
    pub device_read_only: bool,
    pub table_clean: bool,
    pub layout_ok: bool,
}

/// Ok: the volume may be written (below `\BITSOS2\`); Err: why not (pure; host-tested).
pub fn verdict(f: &Facts) -> Result<(), &'static str> {
    let Some(want) = f.loader_id else { return Err("bitsboot handed over no \\BITSOS2\\STICK.ID") };
    match f.volume_id {
        None => return Err("the volume has no readable \\BITSOS2\\STICK.ID"),
        Some(id) if id != want => return Err("the volume's STICK.ID differs from the boot stick's"),
        _ => {}
    }
    if !f.label.eq_ignore_ascii_case(b"BITSOS") {
        return Err("the volume label is not BITSOS");
    }
    if f.sticks != 1 {
        return Err("more than one USB disk looks like the boot stick");
    }
    if f.doubts != 0 {
        return Err("a USB disk could not be told apart from the boot stick");
    }
    if f.device_read_only {
        return Err("the stick is write-protected or its unit refuses writes");
    }
    if !f.table_clean {
        return Err("its partition table is protected (R8a)");
    }
    if !f.layout_ok {
        return Err("the volume does not keep two mirrored FATs");
    }
    Ok(())
}

struct Mounted {
    disk: String,
    /// The disk itself (a name can come back with a new disk after a re-plug).
    dref: Weak<Disk>,
    vol: Arc<Volume>,
    gate: Arc<StickGate>,
}

static MOUNTED: Mutex<Option<Mounted>> = Mutex::named("fat.stick", None);
/// A mount is under way (two registrations of the stick never mount twice).
static MOUNTING: AtomicBool = AtomicBool::new(false);
/// Why the last mount attempt left the stick read-only or unmounted (for the ktests and MCP checks).
static LAST: SpinLock<Option<&'static str>> = SpinLock::named("fat.stick.last", None);
static GATES: SpinLock<Vec<(Weak<Disk>, Arc<StickGate>)>> = SpinLock::named("fat.stick.gates", Vec::new());
/// A stick whose mount thread could not be started: tried again at the next disk event and by `wait_mounted`
/// (never mounted inline in the block layer's listener: that is USB's enumeration thread, B4).
static PENDING: SpinLock<Option<Arc<Disk>>> = SpinLock::named("fat.stick.pending", None);

/// The mounted boot stick's volume, if any.
pub fn volume() -> Option<Arc<Volume>> {
    MOUNTED.lock().as_ref().map(|m| m.vol.clone())
}

/// The SYS API on the mounted stick (`sysapi::Sys` at `/boot` of the boot view).
pub fn with_sys<R>(f: impl FnOnce(&crate::sysapi::Sys<'_>) -> R) -> Result<R, i64> {
    let vol = volume().ok_or(ENODEV)?;
    let ns = vfs::root_ns();
    Ok(f(&crate::sysapi::Sys { vol: &vol, ns: &ns, mount: MOUNT }))
}

fn usb_counts() -> (usize, usize) {
    let mut yes = 0;
    let mut unknown = 0;
    for d in block::disks() {
        if d.kind() == DiskKind::Usb {
            match d.stick() {
                StickMatch::Yes => yes += 1,
                StickMatch::Unknown => unknown += 1,
                StickMatch::No => {}
            }
        }
    }
    (yes, unknown)
}

fn utc_now() -> i64 {
    time::wall::realtime_ns().map_or(0, |n| (n / 1_000_000_000) as i64)
}
fn local_offset(utc: i64) -> i32 {
    time::wall::tz().offset_at(utc)
}
pub const CLOCK: Clock = Clock { now: utc_now, offset: local_offset };

fn is_listed(d: &Arc<Disk>) -> bool {
    block::disks().iter().any(|x| Arc::ptr_eq(x, d))
}

fn gate_for(d: &Arc<Disk>) -> Arc<StickGate> {
    let mut g = GATES.lock();
    g.retain(|(w, _)| w.strong_count() > 0);
    let me = Arc::downgrade(d);
    if let Some((_, gate)) = g.iter().find(|(w, _)| Weak::ptr_eq(w, &me)) {
        return gate.clone();
    }
    let gate = StickGate::new();
    g.push((me, gate.clone()));
    drop(g);
    d.add_gate(gate.clone());
    gate
}

static START: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The disk LBA where the mounted stick's volume starts (0 for a superfloppy).
pub fn start_lba() -> u64 {
    START.load(Ordering::Relaxed)
}

/// Why the mounted stick is read-only (or was not mounted); None: read-write.
pub fn last_reason() -> Option<&'static str> {
    *LAST.lock()
}

/// Waits (sleeping) up to `ms` for the boot stick to be mounted and the write decision made (USB enumerates
/// after the kernel starts; writing is decided once USB settled).
pub fn wait_mounted(ms: u64) -> Option<Arc<Volume>> {
    let end = ksync::now_ns().unwrap_or(0) + ms * 1_000_000;
    loop {
        if let Some(v) = volume()
            && !MOUNTING.load(Ordering::Acquire)
        {
            return Some(v);
        }
        if ksync::now_ns().unwrap_or(u64::MAX) >= end {
            return None;
        }
        retry_pending();
        sched::sleep_ns(20_000_000);
    }
}

struct Busy;
impl Drop for Busy {
    fn drop(&mut self) {
        MOUNTING.store(false, Ordering::Release);
    }
}

/// Mounts the boot stick `d` at `/boot` (read-write when every identity rule holds).
pub fn mount_stick(d: Arc<Disk>) -> Result<(), &'static str> {
    if MOUNTING.swap(true, Ordering::AcqRel) {
        return Err("a mount of the boot stick is under way");
    }
    let _busy = Busy;
    let r = mount_inner(d);
    *LAST.lock() = match r {
        Ok(rw) => rw,
        Err(e) => Some(e),
    };
    r.map(|_| ())
}

/// Waits until no disk came or went for 2 s (at most 30 s): a second stick plugged with the first is seen
/// before writing is allowed.
fn settle() {
    let now = || ksync::now_ns().unwrap_or(u64::MAX);
    let end = now().saturating_add(30_000_000_000);
    let (mut last, mut quiet) = (block::generation(), now());
    while now() < end {
        sched::sleep_ns(50_000_000);
        let g = block::generation();
        if g != last {
            last = g;
            quiet = now();
        } else if now().saturating_sub(quiet) >= 2_000_000_000 {
            return;
        }
    }
}

/// `/dev/<disk>p<n>` for the partition starting at `start` (n is Linux's partition number from the scan: the
/// table slot, logical partitions from 5; Linux's `disk_name` puts a `p` after a disk name ending in a digit),
/// `/dev/<disk>` for a stick without a table.
pub fn source_name(disk: &str, start: u64, parts: &[(u64, u64, String, u32)]) -> String {
    match parts.iter().find(|p| p.0 == start) {
        Some(p) if start != 0 => {
            let sep = if disk.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
            alloc::format!("/dev/{disk}{sep}{}", p.3)
        }
        _ => alloc::format!("/dev/{disk}"),
    }
}

/// Mounts read-only at once (the tree is readable while USB settles), then decides about writing.
fn mount_inner(d: Arc<Disk>) -> Result<Option<&'static str>, &'static str> {
    if MOUNTED.lock().is_some() {
        return Err("a boot stick is mounted already");
    }
    let identity = block::policy::stick_identity().ok_or("no boot stick identity")?;
    let block0 = identity.block0.ok_or("bitsboot could not read the stick's block 0")?;
    let scan = d.scan();
    let start = scan.firsts.iter().find(|(_, b)| *b == block0).map(|(lba, _)| *lba).ok_or("no partition start holds the boot volume")?;
    let blocks = d.dev_info().blocks;
    let sectors = if start == 0 { blocks } else { scan.parts.iter().find(|p| p.0 == start).map(|p| p.1).ok_or("the boot volume's partition is not listed")? };
    let gate = gate_for(&d);
    let dev = Arc::new(DiskDev::new(d.clone(), start, sectors, Some(gate.clone())));
    let vol = Volume::mount(dev, Options { writable: false, stick: true }, CLOCK)?;
    let ns = vfs::root_ns();
    match crate::glue::mount(&vol, &ns, MOUNT, false) {
        // FS-13: the device as Linux names it (`mnt_devname`), so /proc/mounts shows /boot's real source.
        Ok(m) => m.sb.set_source(&source_name(d.name(), start, &scan.parts)),
        Err(e) => {
            klog::event!(Warn, "fat", "stick.mount_failed", disk = String::from(d.name()), errno = e);
            return Err("mounting /boot failed");
        }
    }
    START.store(start, Ordering::Relaxed);
    let w: alloc::sync::Weak<dyn vfs::writeback::Flush> = Arc::downgrade(&vol) as alloc::sync::Weak<dyn vfs::writeback::Flush>;
    if vfs::writeback::register_fs(w).is_err() {
        klog::event!(Warn, "fat", "stick.no_sync_hook"; "vfs::writeback::sync_all will not reach the stick's FSInfo and cache flush");
    }
    *MOUNTED.lock() = Some(Mounted { disk: String::from(d.name()), dref: Arc::downgrade(&d), vol: vol.clone(), gate: gate.clone() });
    // The disk may have gone between the scan and the line above (its `unregistered` found nothing to undo):
    // block::unregister takes it off the list before it tells the listeners, so a check after the store
    // sees it.
    if !is_listed(&d) {
        if let Some(m) = take_mounted(&d) {
            teardown(m);
        }
        return Err("the stick went away while it was mounted");
    }
    klog::event!(Info, "fat", "stick.mounted", disk = String::from(d.name()), start_lba = start, sectors = sectors,
        cluster_bytes = vol.geo.cluster_bytes(), clusters = vol.geo.clusters,
        label = String::from_utf8_lossy(&vol.label_text()).into_owned(); "the boot stick is at /boot (read-only until its identity is decided)");
    settle();
    let volume_id = vol.peek(&[b"BITSOS2", b"STICK.ID"]).ok().flatten().and_then(|e| vol.peek_read(&e, 64).ok()).and_then(|t| parse_stick_id(&t));
    let (sticks, doubts) = usb_counts();
    let facts = Facts {
        loader_id: identity.id,
        volume_id,
        label: vol.label_text(),
        sticks,
        doubts,
        device_read_only: d.dev_info().read_only,
        table_clean: d.scan().verdict() == block::table::Verdict::Clean,
        layout_ok: vol.geo.writable_layout(),
    };
    let mut decision = verdict(&facts);
    if decision.is_ok() && vol.is_gone() {
        decision = Err("the stick went away while USB settled");
    }
    if decision.is_ok() {
        let g = &vol.geo;
        gate.arm(start + g.reserved, start + g.total, g.fsinfo.map(|s| start + s));
        if !vol.allow_writes() {
            gate.disarm();
            decision = Err("writing was turned off meanwhile");
        }
    }
    let free = vol.statfs().map(|s| s.2).unwrap_or(0);
    klog::event!(Info, "fat", "stick.decided", disk = String::from(d.name()), rw = decision.is_ok(), sticks = sticks, doubts = doubts,
        free_clusters = free, reason = decision.err().unwrap_or("identity confirmed: writes below \\BITSOS2\\ only"));
    Ok(decision.err())
}

/// Takes the mount record of disk `d`, if the mounted stick is that disk.
fn take_mounted(d: &Arc<Disk>) -> Option<Mounted> {
    let me = Arc::downgrade(d);
    let mut m = MOUNTED.lock();
    match m.as_ref() {
        Some(x) if Weak::ptr_eq(&x.dref, &me) => m.take(),
        _ => None,
    }
}

/// The stick went away: no more writes, every call EIO, `/boot` unmounted.
fn teardown(m: Mounted) {
    m.gate.disarm();
    m.vol.set_gone();
    let ns = vfs::root_ns();
    let r = ns.lookup_path(MOUNT.as_bytes(), true).and_then(|p| ns.umount(&p));
    klog::event!(Warn, "fat", "stick.gone", disk = m.disk, umount = r.err().unwrap_or(0); "the boot stick went away: /boot unmounted");
}

/// Mounts `d` on a thread of its own (it sleeps while USB settles); when no thread can be started, it is kept
/// for `retry_pending`.
fn start_mount(d: Arc<Disk>) {
    let disk = d.clone();
    let run = move || {
        if let Err(why) = mount_stick(disk.clone()) {
            klog::event!(Warn, "fat", "stick.not_mounted", disk = String::from(disk.name()), reason = why);
        }
    };
    if let Err(e) = sched::spawn("fat.mount", sched::Class::Normal, run) {
        klog::event!(Warn, "fat", "stick.spawn_failed", disk = String::from(d.name()), error = alloc::format!("{e:?}");
            "no thread for mounting the boot stick: tried again at the next disk event");
        *PENDING.lock() = Some(d);
    }
}

/// A mount whose thread could not be started, once more (if its disk is still listed and nothing is mounted).
fn retry_pending() {
    let Some(d) = PENDING.lock().take() else { return };
    if !is_listed(&d) || MOUNTED.lock().is_some() {
        return;
    }
    if MOUNTING.load(Ordering::Acquire) {
        // Another mount is under way: keep it for the next chance.
        *PENDING.lock() = Some(d);
        return;
    }
    start_mount(d);
}

/// The block layer's news about disks.
struct Listener;

impl block::DiskListener for Listener {
    fn registered(&self, d: &Arc<Disk>) {
        retry_pending();
        if d.kind() != DiskKind::Usb {
            return;
        }
        match d.stick() {
            StickMatch::Yes if MOUNTED.lock().is_none() => start_mount(d.clone()),
            StickMatch::No => {}
            _ => {
                // Another disk that is (or might be) the boot stick: writing stops until the next boot.
                if let Some(m) = MOUNTED.lock().as_ref()
                    && !Weak::ptr_eq(&m.dref, &Arc::downgrade(d))
                {
                    let was = m.vol.writable();
                    m.vol.set_read_only();
                    m.gate.disarm();
                    klog::event!(Warn, "fat", "stick.read_only", disk = m.disk.clone(), other = String::from(d.name()), was_writable = was;
                        "a second disk looks like the boot stick: /boot is read-only from now on");
                }
            }
        }
    }
    fn unregistered(&self, d: &Arc<Disk>) {
        let gone = take_mounted(d);
        let me = Arc::downgrade(d);
        GATES.lock().retain(|(w, _)| w.strong_count() > 0 && !Weak::ptr_eq(w, &me));
        {
            let mut p = PENDING.lock();
            if p.as_ref().is_some_and(|x| Arc::ptr_eq(x, d)) {
                *p = None;
            }
        }
        if let Some(m) = gone {
            teardown(m);
        }
        retry_pending();
    }
}

fn init() -> Result<(), &'static str> {
    block::add_listener(Arc::new(Listener));
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "fat.stick", init);

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Facts {
        Facts { loader_id: Some([7; 16]), volume_id: Some([7; 16]), label: b"BITSOS".to_vec(), sticks: 1, doubts: 0, device_read_only: false, table_clean: true, layout_ok: true }
    }

    #[test]
    fn source_names_like_linux() {
        // Slot 2 is empty: the second partition is number 3, its slot, not its position (sdb3, not sdb2).
        let parts = alloc::vec![(2048u64, 100u64, String::from("FAT32"), 1u32), (4096, 100, String::from("Linux"), 3)];
        assert_eq!(source_name("msd0", 2048, &parts), "/dev/msd0p1");
        assert_eq!(source_name("msd0", 4096, &parts), "/dev/msd0p3");
        assert_eq!(source_name("sdb", 4096, &parts), "/dev/sdb3");
        assert_eq!(source_name("msd1", 0, &[]), "/dev/msd1");
    }

    #[test]
    fn identity_rules() {
        assert_eq!(verdict(&good()), Ok(()));
        let cases: Vec<(Facts, &str)> = alloc::vec![
            (Facts { loader_id: None, ..good() }, "bitsboot handed over no \\BITSOS2\\STICK.ID"),
            (Facts { volume_id: None, ..good() }, "the volume has no readable \\BITSOS2\\STICK.ID"),
            (Facts { volume_id: Some([8; 16]), ..good() }, "the volume's STICK.ID differs from the boot stick's"),
            (Facts { label: b"ZORINOS".to_vec(), ..good() }, "the volume label is not BITSOS"),
            (Facts { sticks: 2, ..good() }, "more than one USB disk looks like the boot stick"),
            (Facts { doubts: 1, ..good() }, "a USB disk could not be told apart from the boot stick"),
            (Facts { device_read_only: true, ..good() }, "the stick is write-protected or its unit refuses writes"),
            (Facts { table_clean: false, ..good() }, "its partition table is protected (R8a)"),
            (Facts { layout_ok: false, ..good() }, "the volume does not keep two mirrored FATs"),
        ];
        for (f, why) in cases {
            assert_eq!(verdict(&f), Err(why));
        }
        assert_eq!(verdict(&Facts { label: b"bitsos".to_vec(), ..good() }), Ok(()));
    }

    #[test]
    fn stick_id_text() {
        assert_eq!(parse_stick_id(b"00112233445566778899aabbccddeeff\r\n").unwrap()[15], 0xff);
        assert!(parse_stick_id(b"0011").is_none());
        assert!(parse_stick_id(b"zz112233445566778899aabbccddeeff").is_none());
    }
}

//! RB-14: `rb.stop.reboot_order` (QemuOnly: it stops QEMU's stick and /data for a moment, then resumes them; on
//! the Mini-PC that would fail the journal's writes). The MCP `reboot`'s steps before the reset, in the order the
//! RB-14 change request gives `mcp/src/tools/reboot.rs` `reset()` (write-back, storage stop, park, quiesce),
//! with a slow disk of its own (a registered RAM disk, 20 ms per call) that a writer keeps writing to, as an
//! install's last writes and the 5 s flusher did on 27.10. Checks: the stop finds every disk quiet and every
//! layer idle (usb-msd's stick, xHCI's bulk rings), the slow disk's last device call is the flush after its last
//! write, the writer's next write is held back (no error, nothing reaches the device after the stop up to and
//! after the Quiesce), and the Quiesce comes after all of it. Then everything is resumed: the held write lands
//! and the stick reads again. Run with `-- -smp 4` or more, so the park has CPUs to park.
//!
//! `rb.stop.stick_install` (QemuOnly, writes QEMU's stick and removes what it wrote): the install case through
//! the real stack. A writer writes a 16 MiB KERNEL of a scratch generation through the stick's FAT volume
//! (`Sys::write_file`, as `install_kernel` does; QEMU's stick needs seconds for it) and the stop comes 30 ms
//! into it, as when reboot.rs's write-back bound ran out with its helper still writing, or the 5 s flusher was
//! under way. Checks: the writer was still writing at the stop; every USB disk
//! drained and flushed at the device (`Flushed::Done`, SYNCHRONIZE CACHE through usb-msd and xHCI), the usb-msd
//! and xHCI layers idle (no Bulk-Only command, no bulk TD in flight), every xHCI controller storage-idle after
//! the stop; after `resume_all` the writer's file completes without an error and reads back byte for byte.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::ramdisk::{CallOp, RamDisk};
use block::{BlockError, DiskKind};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::block::IoPriority::Normal;
use ktest::{kassert, ktest, TestResult};

const MS: u64 = 1_000_000;
const NAME: &str = "rb14-quiesce";
const DISK: &str = "rb14-reboot";

static ARMED: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicU64 = AtomicU64::new(0);
/// The sequence number the Quiesce saw (0 = not called while armed).
static AT_QUIESCE: AtomicU64 = AtomicU64::new(0);
static REGISTERED: AtomicBool = AtomicBool::new(false);

struct Fake;

impl devmodel::Quiesce for Fake {
    fn name(&self) -> &str {
        NAME
    }
    /// Lock-free: only notes when it ran (the others are parked).
    fn quiesce(&self) {
        if ARMED.load(Ordering::Acquire) {
            AT_QUIESCE.store(SEQ.fetch_add(1, Ordering::AcqRel) + 1, Ordering::Release);
        }
    }
}

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn until(ms: u64, f: impl Fn() -> bool) -> bool {
    let end = time::now_ns() + ms * MS;
    while time::now_ns() < end {
        if f() {
            return true;
        }
        sched::sleep_ns(MS);
    }
    f()
}

fn reboot_order() -> TestResult {
    if !REGISTERED.swap(true, Ordering::AcqRel) {
        devmodel::register_quiesce(Arc::new(Fake));
    }
    // The stick first (QEMU's usb-storage): the stop must find it registered, not racing its scan.
    let _ = until(5000, || !usb_msd_absent());
    let ram = Arc::new(RamDisk::try_new(DISK, 512, 4096).map_err(e)?.with_max_blocks(64));
    let disk = block::register(ram.clone(), DiskKind::Ram).map_err(e)?;
    let stop_writer = Arc::new(AtomicBool::new(false));
    let first_err: Arc<ksync::SpinLock<Option<BlockError>>> = Arc::new(ksync::SpinLock::new(None));
    let ok = Arc::new(AtomicU64::new(0));
    let res = (|| -> TestResult {
        ram.set_delay_ns(20 * MS);
        ram.record();
        let (d, s, f, n) = (disk.clone(), stop_writer.clone(), first_err.clone(), ok.clone());
        sched::spawn("rb14-reboot-writer", sched::Class::Normal, move || {
            let mut lba = 0u64;
            while !s.load(Ordering::Acquire) {
                match d.write(lba % 4000, &[0xC3; 4096], false, Normal) {
                    Ok(()) => {
                        n.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(x) => {
                        *f.lock() = Some(x);
                        break;
                    }
                }
                lba += 8;
            }
        })
        .map_err(e)?;
        kassert!(until(2000, || ok.load(Ordering::Acquire) >= 3), "the writer did not get going");
        // reset(): the write-back (bounded in reboot.rs), then the storage stop.
        let synced = vfs::writeback::sync_all();
        let sum = block::shutdown::stop_all("ktest-reboot", block::shutdown::STOP_NS);
        let after_stop = ram.calls().len();
        let stop_seq = SEQ.fetch_add(1, Ordering::AcqRel) + 1;
        // Everything the stop promised, before the park.
        let not_quiet: Vec<String> = block::disks().iter().filter(|d| !d.is_closed() || d.pending() != (0, 0)).map(|d| String::from(d.name())).collect();
        let busy: Vec<String> = xhci::controllers().iter().map(|c| c.activity()).filter(|a| !a.storage_idle()).map(|a| a.describe()).collect();
        ARMED.store(true, Ordering::Release);
        {
            let _park = platform::power::park_others("rb14-ktest");
            devmodel::quiesce_matching(NAME);
        }
        ARMED.store(false, Ordering::Release);
        let q = AT_QUIESCE.swap(0, Ordering::AcqRel);
        sched::sleep_ns(100 * MS);
        let calls = ram.calls();
        klog::event!(Info, "rb", "ktest.reboot_order", synced = synced.flushed, sync_failed = synced.failed, quiet = sum.quiet, ms = sum.ms,
            disks = sum.disks.len(), layers = sum.layers.len(), writes_ok = ok.load(Ordering::Acquire), calls = calls.len());
        kassert!(sum.quiet, "the stop was not quiet: {:?} {:?}", sum.disks, sum.layers);
        kassert!(not_quiet.is_empty(), "disks open or busy after the stop: {not_quiet:?}");
        kassert!(busy.is_empty(), "xHCI storage transfers after the stop: {busy:?}");
        kassert!(sum.layers.iter().any(|(n, _)| n == "usb-msd") || usb_msd_absent(), "usb-msd's layer did not run: {:?}", sum.layers);
        kassert!(sum.layers.iter().any(|(n, _)| n == "xhci"), "xHCI's layer did not run: {:?}", sum.layers);
        let mine = sum.disks.iter().find(|s| s.name == DISK).ok_or("the slow disk was not stopped")?;
        kassert!(mine.flush == block::shutdown::Flushed::Done && mine.drained, "{mine:?}");
        kassert!(q > stop_seq, "the Quiesce ran before the stop had ended (quiesce {q}, stop {stop_seq})");
        kassert!(until(2000, || disk.held() == 1), "the writer's next write was not held back (held {})", disk.held());
        kassert!(first_err.lock().is_none(), "the writer saw an error: {:?}", *first_err.lock());
        kassert!(calls.len() == after_stop, "the device was called after the stop ({} -> {})", after_stop, calls.len());
        kassert!(calls.last().map(|c| c.op) == Some(CallOp::Flush), "the slow disk's last call is not the flush");
        Ok(())
    })();
    let ok_at_resume = ok.load(Ordering::Acquire);
    ARMED.store(false, Ordering::Release);
    block::shutdown::resume_all("ktest-reboot");
    let landed = until(2000, || ok.load(Ordering::Acquire) > ok_at_resume);
    // Read before the writer is told to end: its write under way when the disk is unregistered below fails
    // with `Gone`, as it should.
    let err_at_landing = *first_err.lock();
    stop_writer.store(true, Ordering::Release);
    ram.set_delay_ns(0);
    block::unregister(DISK);
    res?;
    kassert!(landed && err_at_landing.is_none(), "the held write did not land after the resume ({err_at_landing:?})");
    // Resumed: the stick reads again.
    for d in block::disks().into_iter().filter(|d| d.kind() == DiskKind::Usb) {
        d.read_vec(0, d.block_size() as usize, Normal).map_err(|x| format!("{} after the resume: {x:?}", d.name()))?;
    }
    Ok(())
}
ktest!("rb.stop.reboot_order", QemuOnly, reboot_order);

fn usb_msd_absent() -> bool {
    !block::disks().iter().any(|d| d.kind() == DiskKind::Usb)
}

const SCRATCH_BYTES: usize = 16 << 20;

fn stick_install() -> TestResult {
    let Some(_vol) = fs_fat::stick::wait_mounted(40_000) else {
        return match fs_fat::stick::last_reason() {
            Some(r) => Err(format!("the boot stick was not mounted: {r}")),
            None => ktest::fail("no boot stick (no USB disk recognised as the stick)"),
        };
    };
    let _l = crate::install::LOCK.lock_timeout(60_000 * MS).ok_or("the install lock is held")?;
    let gens = fs_fat::stick::with_sys(|s| s.generations()).map_err(e)?.map_err(e)?;
    let n = gens.iter().map(|g| g.n).max().unwrap_or(0) + 500;
    fs_fat::stick::with_sys(|s| s.create_generation(n)).map_err(e)?.map_err(e)?;
    let mut data = Vec::new();
    data.try_reserve_exact(SCRATCH_BYTES).map_err(e)?;
    data.extend((0..SCRATCH_BYTES).map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[1]));
    let data = Arc::new(data);
    let started = Arc::new(AtomicBool::new(false));
    let result: Arc<ksync::SpinLock<Option<Result<(), i64>>>> = Arc::new(ksync::SpinLock::new(None));
    let res = (|| -> TestResult {
        let (d2, st, r2) = (data.clone(), started.clone(), result.clone());
        sched::spawn("rb14-stick-writer", sched::Class::Normal, move || {
            st.store(true, Ordering::Release);
            let r = fs_fat::stick::with_sys(|s| s.write_file(n, "KERNEL", &d2)).unwrap_or_else(Err);
            *r2.lock() = Some(r);
        })
        .map_err(e)?;
        kassert!(until(2000, || started.load(Ordering::Acquire)), "the writer did not start");
        // Bounded; if nothing shows, `caught` below says so.
        let _ = until(2000, || xhci::controllers().iter().any(|c| !c.activity().storage_idle()));
        let sum = block::shutdown::stop_all("ktest-stick", block::shutdown::STOP_NS);
        let caught = result.lock().is_none();
        let busy: Vec<String> = xhci::controllers().iter().map(|c| c.activity()).filter(|a| !a.storage_idle()).map(|a| a.describe()).collect();
        let usb: Vec<String> = block::disks().iter().filter(|d| d.kind() == DiskKind::Usb).map(|d| String::from(d.name())).collect();
        let held: usize = block::disks().iter().filter(|d| d.kind() == DiskKind::Usb).map(|d| d.held()).sum();
        klog::event!(Info, "rb", "ktest.stick_install", caught = caught, held = held, quiet = sum.quiet, ms = sum.ms, disks = sum.disks.len());
        kassert!(!usb.is_empty(), "no USB disk although the stick is mounted");
        kassert!(caught, "the 16 MiB file was written before the stop came: nothing was caught under way");
        for name in &usb {
            let s = sum.disks.iter().find(|s| &s.name == name).ok_or_else(|| format!("{name} was not stopped"))?;
            kassert!(s.drained && s.failed_queued == 0 && s.flush == block::shutdown::Flushed::Done, "{s:?}");
        }
        for l in ["usb-msd", "xhci"] {
            let note = sum.layers.iter().find(|(n, _)| n == l).ok_or_else(|| format!("the {l} layer did not run: {:?}", sum.layers))?;
            kassert!(note.1.idle, "{l}: {:?}", note.1);
        }
        kassert!(busy.is_empty(), "xHCI storage transfers after the stop: {busy:?}");
        kassert!(sum.quiet, "the stop was not quiet: {:?} {:?}", sum.disks, sum.layers);
        Ok(())
    })();
    block::shutdown::resume_all("ktest-stick");
    let written = until(120_000, || result.lock().is_some());
    let wr = result.lock().take();
    let back = (|| -> TestResult {
        kassert!(written, "the writer did not finish after the resume");
        kassert!(wr == Some(Ok(())), "the writer's file: {wr:?}");
        let mut buf = Vec::new();
        buf.try_reserve_exact(1 << 20).map_err(e)?;
        buf.resize(1 << 20, 0);
        let mut off = 0usize;
        while off < SCRATCH_BYTES {
            let got = fs_fat::stick::with_sys(|s| s.read_at(n, "KERNEL", off as u64, &mut buf)).map_err(e)?.map_err(e)?;
            kassert!(got > 0, "KERNEL ends at {off} of {SCRATCH_BYTES}");
            kassert!(buf[..got] == data[off..off + got], "KERNEL differs in the MiB at {off}");
            off += got;
        }
        Ok(())
    })();
    let removed = if written { fs_fat::stick::with_sys(|s| s.remove_generation(n)).map_err(e).and_then(|r| r.map_err(e)) } else { Ok(()) };
    res?;
    back?;
    removed.map_err(|x| format!("scratch generation {n} not removed: {x}"))?;
    Ok(())
}
ktest!("rb.stop.stick_install", QemuOnly, stick_install);

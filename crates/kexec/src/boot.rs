//! What this kernel was started with: the BootInfo the kernel hands over (`set_boot_info`), its CONFIG keys for
//! kexec, and the hand-over facts the `handover` event and `reload_info` report (counter, TSC, initramfs hash).

use crate::Fnv;
use alloc::string::{String, ToString};
use bootinfo::{BootInfo, module_kind};
use ksync::Once;

static BOOT: Once<(BootInfo<'static>, u64)> = Once::new();
/// The kernel's own page-table root, taken when the kernel hands its BootInfo over (before any program ran and
/// left its own root in CR3).
static KERNEL_ROOT: Once<u64> = Once::new();
static FACTS: Once<Facts> = Once::new();

/// The kernel calls this once, before the init calls, with its BootInfo and the physical address it got in rsi.
/// Later calls change nothing.
pub fn set_boot_info(bi: BootInfo<'static>, phys: u64) {
    KERNEL_ROOT.get_or_init(hal::paging::current_root);
    let first = BOOT.get_or_init(|| (bi, phys));
    if first.1 != phys {
        klog::event!(Warn, "kexec", "boot_info.again"; "BootInfo was set already; kept");
    }
}

/// INT-A1 (CONTRACTS 6a: the initramfs module is released once FS copied it into tmpfs): set by the kernel
/// after `release_module`. From then on the running initramfs's bytes are gone, so a Reload must bring its own
/// (`stage` refuses "keep the running one").
static INITRD_RELEASED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The kernel calls this right *before* it gives the initramfs module's frames back to the allocator (so no
/// reader sees the flag clear while the frames are already free). It stays set if the release then fails: the
/// conservative side (a Reload must bring an initrd URL, the module is no longer read).
pub fn note_initrd_released() {
    INITRD_RELEASED.store(true, core::sync::atomic::Ordering::Release);
}

/// Whether this kernel released its initramfs module (see `note_initrd_released`).
pub fn initrd_released() -> bool {
    INITRD_RELEASED.load(core::sync::atomic::Ordering::Acquire)
}

/// Whether a BootInfo module's bytes may still be read through `hal::kexec_stub::read_phys` (its module rule):
/// every module except the initramfs once it was released to the frame allocator.
pub fn module_readable(m: &bootinfo::Module<'_>) -> bool {
    m.kind != module_kind::INITRAMFS || !initrd_released()
}

pub fn boot_info() -> Option<(BootInfo<'static>, u64)> {
    BOOT.get().copied()
}

/// The monitor's EDID as bitsboot read it from the firmware (BootInfo tag 21, carried on by every Reload);
/// `None` without one. For GX-4's mode list and SN-5's sink.
pub fn edid() -> Option<&'static [u8]> {
    boot_info().and_then(|(bi, _)| bi.edid())
}

/// The kernel's own root (see `KERNEL_ROOT`); `None` before `set_boot_info`.
pub fn kernel_root() -> Option<u64> {
    KERNEL_ROOT.get().copied()
}

/// A CONFIG.TXT value from the BootInfo (the kernel's rules: `name=value`, `#` comment, last line wins).
pub fn config(key: &str) -> Option<String> {
    let (bi, _) = boot_info()?;
    let text = core::str::from_utf8(bi.config()?).ok()?;
    let mut v = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, val)) = line.split_once('=')
            && k.trim() == key
        {
            v = Some(val.trim().to_string());
        }
    }
    v
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub counter: u32,
    pub tsc_hz_handed: u64,
    pub tsc_calibrated_at_reload: u32,
    pub initrd_phys: u64,
    pub initrd_bytes: u64,
    pub initrd_fnv: u64,
    pub config_bytes: u64,
    pub bootinfo_bytes: u64,
}

/// FNV-1a 64 of a physical range, read in pieces (smaller ones when memory is short; `None` without any).
pub fn fnv_phys(phys: u64, len: u64) -> Option<u64> {
    let mut h = Fnv::new();
    let mut buf = alloc::vec::Vec::new();
    for size in [64 * 1024, 4096, 256] {
        if buf.try_reserve_exact(size).is_ok() {
            buf.resize(size, 0u8);
            break;
        }
    }
    if buf.is_empty() {
        return None;
    }
    let mut off = 0;
    while off < len {
        let n = (len - off).min(buf.len() as u64) as usize;
        hal::kexec_stub::read_phys(phys + off, &mut buf[..n]);
        h.update(&buf[..n]);
        off += n as u64;
    }
    Some(h.finish())
}

fn collect() -> Option<Facts> {
    let (bi, _) = boot_info()?;
    let tsc = bi.tsc().unwrap_or_default();
    let initrd = bi.modules().find(|m| m.kind == module_kind::INITRAMFS);
    // The hash only while the module is readable (`module_readable`; collect runs in an initcall, before the
    // release, so in practice always).
    let readable = initrd.filter(module_readable);
    Some(Facts {
        counter: bi.reload().map(|r| r.counter).unwrap_or(0),
        tsc_hz_handed: tsc.calibrated_hz,
        tsc_calibrated_at_reload: tsc.calibrated_at_reload,
        initrd_phys: initrd.map(|m| m.phys).unwrap_or(0),
        initrd_bytes: initrd.map(|m| m.size).unwrap_or(0),
        // The lean stick kernel skips hashing ~60 MB at every boot for one log line (desk 24.10.).
        initrd_fnv: if cfg!(bitsos_no_ktests) { 0 } else { readable.and_then(|m| fnv_phys(m.phys, m.size)).unwrap_or(0) },
        config_bytes: bi.config().map(|c| c.len() as u64).unwrap_or(0),
        bootinfo_bytes: bi.total_size() as u64,
    })
}

pub fn facts() -> Option<Facts> {
    FACTS.get().copied()
}

/// Init call: one event with what this kernel was handed (B6), so a Reload round reads the counter, the TSC
/// rate and the initramfs hash from the log.
fn handover_event() -> Result<(), &'static str> {
    let Some(f) = collect() else {
        klog::event!(Warn, "kexec", "boot_info.missing"; "the kernel did not call kexec::set_boot_info; reload refuses until it does");
        return Ok(());
    };
    let f = *FACTS.get_or_init(|| f);
    klog::event!(
        Info,
        "kexec",
        "handover",
        reload = f.counter,
        tsc_hz = f.tsc_hz_handed,
        tsc_from_reload = f.counter > 0 && f.tsc_hz_handed != 0,
        initrd_bytes = f.initrd_bytes,
        initrd_fnv = klog::Hex(f.initrd_fnv),
        config_bytes = f.config_bytes,
        bootinfo_bytes = f.bootinfo_bytes
    );
    Ok(())
}
kreg::initcall!(kreg::Level::Subsys, "kexec.handover", handover_event);

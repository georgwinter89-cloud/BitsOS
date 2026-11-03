//! DRM devices: what a GPU backend tells the core (`Driver`), the registry with Linux's minor numbers
//! (linux-6.6/drivers/gpu/drm/drm_drv.c:119 `drm_minor_alloc`: 64 per kind, render minors 128..191), and the
//! CONFIG switch `drm=` that decides whether the fake GPU stands in for a missing hardware backend.

use crate::dispatch::{Ctx, Desc};
use crate::file::DrmFile;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use ksync::SpinLock;
use linux::errno::*;
use vfs::Errno;

/// The char major of every DRM node (Linux DRM_MAJOR, drm_drv.c:1090 `register_chrdev(DRM_MAJOR, ..)`).
pub const DRM_MAJOR: u32 = 226;
/// Render minors start here (`64 * DRM_MINOR_RENDER`).
pub const RENDER_BASE: u32 = 128;
/// Minors per kind (Linux's ABI: libdrm derives the kind from `minor >> 6`).
pub const MINORS_PER_KIND: u32 = 64;
/// How long `unplug` waits for calls still inside the device (Linux `synchronize_srcu` waits without a bound;
/// B4 wants one). After it the teardown goes on with a Warn: the driver must tolerate a late call.
const UNPLUG_WAIT_NS: u64 = 1_000_000_000;

/// `drm_driver.driver_features` that the core looks at.
#[derive(Clone, Copy, Debug)]
pub struct Features {
    pub gem: bool,
    pub modeset: bool,
    pub syncobj: bool,
    pub syncobj_timeline: bool,
}

/// The `mode_config` values GET_CAP reports for a KMS driver.
#[derive(Clone, Copy, Debug)]
pub struct ModeConfig {
    pub dumb_create: bool,
    pub preferred_depth: u32,
    pub prefer_shadow: bool,
    pub async_page_flip: bool,
    /// Every CRTC has `page_flip_target` (amdgpu's DM has none: 0).
    pub page_flip_target: bool,
    pub cursor_width: u32,
    pub cursor_height: u32,
    /// `!fb_modifiers_not_supported`.
    pub fb_modifiers: bool,
}

/// DRM_IOCTL_VERSION's answer and the features.
#[derive(Clone, Copy, Debug)]
pub struct DriverInfo {
    pub name: &'static str,
    pub date: &'static str,
    pub desc: &'static str,
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
    pub features: Features,
    pub mode: ModeConfig,
}

/// Where the device's PCI function lives in /sys.
#[derive(Clone, Debug)]
pub enum Place {
    /// The DRM view makes the whole function directory `devices/pci<domain>:<bus>/<slot name>` with its files
    /// (the fake: no PCI function behind it).
    Own,
    /// The PCI view's directory of a real function (path below /sys, e.g. `devices/pci0000:00/0000:00:08.1/0000:c7:00.0`);
    /// PC provides its files, the DRM view adds `drm/` below it. No backend registers one yet: GX's hardware
    /// backend takes the path from pci's sysfs view when it does (C1 removed the unused `pci::sysfs::function_dir`).
    Pci(String),
}

/// The PCI identity libdrm reads (vendor, device, subsystem, revision, the slot from `uevent`).
#[derive(Clone, Debug)]
pub struct PciIdentity {
    pub domain: u16,
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub subsystem_vendor: u16,
    pub subsystem_device: u16,
    pub revision: u8,
    /// 24-bit class code (0x030000 = VGA).
    pub class: u32,
    /// The bound driver's name (uevent DRIVER=).
    pub driver: &'static str,
    pub place: Place,
}

impl PciIdentity {
    /// `pci_name`: `%04x:%02x:%02x.%u`.
    pub fn slot_name(&self) -> String {
        format!("{:04x}:{:02x}:{:02x}.{}", self.domain, self.bus, self.slot, self.func)
    }
    /// The function's directory below /sys.
    pub fn sysfs_dir(&self) -> String {
        match &self.place {
            Place::Own => format!("devices/pci{:04x}:{:02x}/{}", self.domain, 0, self.slot_name()),
            Place::Pci(p) => p.clone(),
        }
    }
}

/// A GPU backend behind the DRM core: the fake now, the amdgpu hardware backend later (GX).
pub trait Driver: Send + Sync {
    fn info(&self) -> &DriverInfo;
    fn pci(&self) -> &PciIdentity;
    /// The driver ioctls, indexed by `nr - DRM_COMMAND_BASE` (None = a hole).
    fn ioctls(&self) -> &'static [Option<Desc>];
    /// A driver ioctl of the table; None = not done yet (EINVAL and an event). An amdgpu backend with a
    /// `GpuDevice` gets the uAPI's INFO and CTX from here (DR-4).
    fn ioctl(&self, ctx: &Ctx, nr: u32, data: &mut [u8]) -> Option<Result<i64, Errno>> {
        crate::gpu_ioctls::ioctl(ctx, self.gpu()?, nr, data)
    }
    /// The GPU behind the amdgpu uAPI (DR-4); None for a driver without one.
    fn gpu(&self) -> Option<&dyn crate::gpu::GpuDevice> {
        None
    }
    /// `drm_driver.open`: a new file (per-file state of the backend comes with DR-3).
    fn open(&self, _file: &DrmFile) -> Result<(), Errno> {
        Ok(())
    }
    /// `drm_driver.postclose`: the last close of a file.
    fn postclose(&self, _file: &DrmFile) {}
    /// Lines after the core's in /proc/<pid>/fdinfo (amdgpu's memory stats later).
    fn fdinfo(&self, _file: &DrmFile, _out: &mut String) {}
    /// The fake GPU behind this driver (its ring signals fences in tests); None for a hardware backend.
    fn as_fake(&self) -> Option<&crate::fake::FakeGpu> {
        None
    }
}

/// How many program-chosen unknown keys (INFO queries, CTX ops, refused registers) one device remembers for the
/// Warn-once rule. A log dedup, not a resource limit (R1): past it the Warn becomes Debug, nothing is refused.
pub const UNKNOWN_REMEMBERED: usize = 64;

#[derive(Default)]
struct Warned {
    ioctls: BTreeSet<u64>,
    unknown: BTreeSet<u64>,
    unknown_full: bool,
}

pub struct Device {
    index: u32,
    driver: Arc<dyn Driver>,
    unique: String,
    unplugged: AtomicBool,
    /// Calls between `enter` and the guard's drop (Linux's drm_unplug_srcu readers).
    inflight: AtomicUsize,
    /// Numbers already reported as unsupported or invalid (one Warn each, then Debug).
    warned: SpinLock<Warned>,
    /// DR-3: blocking waits inside the device (syncobj waits), woken by `unplug`.
    unplug_wakers: crate::fence::CbList,
}

impl Device {
    pub fn driver(&self) -> &dyn Driver {
        &*self.driver
    }
    pub fn index(&self) -> u32 {
        self.index
    }
    pub fn render_minor(&self) -> u32 {
        RENDER_BASE + self.index
    }
    /// The bus id GET_UNIQUE answers (`dev_name` of the PCI function).
    pub fn unique(&self) -> &str {
        &self.unique
    }
    /// `drm_dev_is_unplugged`: after `unregister`, open files answer ENODEV.
    pub fn unplugged(&self) -> bool {
        self.unplugged.load(Ordering::Acquire)
    }

    /// `drm_dev_enter`: None once the device is unplugged; while the guard lives, `unplug` waits for it.
    /// SeqCst on both sides: either `unplug` sees the count or the call sees the flag.
    pub fn enter(&self) -> Option<DevGuard<'_>> {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        if self.unplugged.load(Ordering::SeqCst) {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(DevGuard(self))
    }

    /// A blocking wait inside the device: `unplug` calls `cb` (the wait then ends with ENODEV).
    pub fn on_unplug(&self, cb: Arc<dyn crate::fence::FenceCb>) -> u64 {
        self.unplug_wakers.add(cb)
    }
    pub fn off_unplug(&self, id: u64) {
        self.unplug_wakers.remove(id)
    }

    /// Calls inside the device right now (tests).
    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    /// Table numbers: at most 512 keys (8-bit nr, plus the invalid flag), so the set stays small by itself.
    fn first_time(&self, nr: u32) -> bool {
        self.warned.lock().ioctls.insert(nr as u64)
    }

    /// An ioctl of the table that V2 does not do yet: EINVAL (Linux `drm_invalid_op`).
    pub fn unsupported(&self, d: &Desc) -> Result<i64, Errno> {
        let nr = crate::ioc::nr(d.cmd);
        if self.first_time(nr) {
            klog::event!(Warn, "drm", "ioctl.unsupported", minor = self.render_minor(), nr = klog::Hex(nr as u64), name = d.name;
                "not done in V2 yet: EINVAL");
        } else {
            klog::event!(Debug, "drm", "ioctl.unsupported", minor = self.render_minor(), nr = klog::Hex(nr as u64), name = d.name);
        }
        Err(EINVAL)
    }

    /// A query or op inside a driver ioctl that V2 does not know (EINVAL as Linux): Warn once per (what, key),
    /// then Debug. It names the next thing a program wants.
    /// The key is the program's value, so the remembered set is capped: past `UNKNOWN_REMEMBERED` keys a new one
    /// logs Debug only (the first ones already name what a program wants); kernel memory stays bounded.
    pub fn note_unknown(&self, what: &'static str, key: u32) {
        let tag = what.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
        let first = {
            let mut w = self.warned.lock();
            let k = ((tag as u64) << 32) | key as u64;
            if w.unknown.contains(&k) {
                false
            } else if w.unknown.len() < UNKNOWN_REMEMBERED {
                w.unknown.insert(k)
            } else {
                if !w.unknown_full {
                    w.unknown_full = true;
                    drop(w);
                    klog::event!(Info, "drm", "unknown.dedup_full", minor = self.render_minor(), kept = UNKNOWN_REMEMBERED;
                        "further unknown queries log Debug only");
                }
                false
            }
        };
        if first {
            klog::event!(Warn, "drm", what, minor = self.render_minor(), key = klog::Hex(key as u64); "not known in V2: EINVAL");
        } else {
            klog::event!(Debug, "drm", what, minor = self.render_minor(), key = klog::Hex(key as u64));
        }
    }

    /// Unknown keys remembered for the Warn-once rule (tests).
    pub fn unknown_remembered(&self) -> usize {
        self.warned.lock().unknown.len()
    }

    /// A number without a table entry (EINVAL as Linux).
    pub fn note_invalid(&self, nr: u32, cmd: u32) {
        if self.first_time(nr | 0x1_0000) {
            klog::event!(Info, "drm", "ioctl.invalid", minor = self.render_minor(), nr = klog::Hex(nr as u64), cmd = klog::Hex(cmd as u64);
                "no such DRM ioctl: EINVAL");
        }
    }
}

/// `drm_dev_exit` at drop.
pub struct DevGuard<'a>(&'a Device);

impl Drop for DevGuard<'_> {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

static DEVICES: SpinLock<Vec<Arc<Device>>> = SpinLock::named("drm.devices", Vec::new());

/// Adds a device under the lowest free render minor (ENOSPC after 64, Linux's range).
pub fn register(driver: Arc<dyn Driver>) -> Result<Arc<Device>, Errno> {
    let unique = driver.pci().slot_name();
    let mut l = DEVICES.lock();
    let index = (0..MINORS_PER_KIND).find(|i| !l.iter().any(|d| d.index == *i)).ok_or(ENOSPC)?;
    let dev = Arc::new(Device {
        index,
        driver,
        unique,
        unplugged: AtomicBool::new(false),
        inflight: AtomicUsize::new(0),
        warned: SpinLock::named("drm.device.warned", Warned::default()),
        unplug_wakers: crate::fence::CbList::new(),
    });
    let at = l.iter().position(|d| d.index > index).unwrap_or(l.len());
    l.insert(at, dev.clone());
    drop(l);
    let i = dev.driver.info();
    klog::event!(Info, "drm", "device.registered", minor = dev.render_minor(), driver = i.name, pci = dev.unique.as_str(),
        version = format!("{}.{}.{}", i.major, i.minor, i.patch).as_str());
    Ok(dev)
}

/// `drm_dev_unplug`: open files answer ENODEV from now on; waits (bounded) for the calls still inside the device.
/// The minor stays taken until `release_minor`, so the node can go first (`drm::remove_device`).
/// False when the device was unplugged already (nothing done).
pub fn unplug(dev: &Device) -> bool {
    if dev.unplugged.swap(true, Ordering::SeqCst) {
        return false;
    }
    // Waits inside the device look at the flag when woken (set first, then wake: FS-17).
    dev.unplug_wakers.call_all();
    let start = ksync::now_ns();
    let mut spins: u64 = 0;
    while dev.inflight.load(Ordering::SeqCst) > 0 {
        let late = match (start, ksync::now_ns()) {
            (Some(a), Some(b)) => b.saturating_sub(a) > UNPLUG_WAIT_NS,
            // No clock yet (early boot): a generous spin count stands in for the time.
            _ => spins > 100_000_000,
        };
        if late {
            klog::event!(Warn, "drm", "device.unplug_timeout", minor = dev.render_minor(), inflight = dev.inflight.load(Ordering::SeqCst);
                "calls still inside the device; teardown goes on");
            break;
        }
        spins += 1;
        ksync::yield_now();
    }
    klog::event!(Info, "drm", "device.unplugged", minor = dev.render_minor());
    true
}

/// The minor and the /sys entries go: a new device may take the minor from now on.
pub fn release_minor(dev: &Arc<Device>) {
    DEVICES.lock().retain(|d| !Arc::ptr_eq(d, dev));
}

/// `unplug` then `release_minor` (a device without a node: host tests).
pub fn unregister(dev: &Arc<Device>) {
    unplug(dev);
    release_minor(dev);
}

/// Every registered device.
pub fn devices() -> Vec<Arc<Device>> {
    DEVICES.lock().clone()
}

/// The device of a render minor.
pub fn by_render_minor(minor: u32) -> Option<Arc<Device>> {
    DEVICES.lock().iter().find(|d| d.render_minor() == minor).cloned()
}

// ---- CONFIG drm=auto|fake|off

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The hardware backend when one attaches (none yet: no DRM device, as Linux without a GPU driver).
    Auto = 0,
    /// The fake GPU (QEMU scenarios with Mesa, the ABI diff; never needed on hardware).
    Fake = 1,
    /// No DRM device at all.
    Off = 2,
}

static MODE: AtomicU8 = AtomicU8::new(Mode::Auto as u8);
static BOOT: ksync::Once<()> = ksync::Once::new();

/// CONFIG `drm=`: the kernel calls `drm::configure(&config::get)` at boot (desk change request); without the
/// call the mode is `auto`.
pub fn configure(get: &dyn Fn(&str) -> Option<String>) {
    let m = match get("drm").as_deref().map(str::trim) {
        None | Some("") | Some("auto") => Mode::Auto,
        Some("fake") => Mode::Fake,
        Some("off") => Mode::Off,
        Some(other) => {
            klog::event!(Warn, "drm", "config.unknown", value = other; "drm= takes auto, fake or off: auto");
            Mode::Auto
        }
    };
    MODE.store(m as u8, Ordering::Release);
}

pub fn mode() -> Mode {
    match MODE.load(Ordering::Acquire) {
        1 => Mode::Fake,
        2 => Mode::Off,
        _ => Mode::Auto,
    }
}

/// Whether the fake GPU stands in at boot.
pub fn fake_wanted(m: Mode) -> bool {
    m == Mode::Fake
}

/// The boot devices, once (`drm::boot`, after the VFS is up: the node goes into /dev). Under `drm=fake` the fake
/// gets renderD128 and its node.
pub(crate) fn boot_devices() {
    BOOT.get_or_init(|| {
        let m = mode();
        if !fake_wanted(m) {
            klog::event!(Info, "drm", "fake.off", mode = format!("{m:?}").as_str(); "no fake DRM device (drm=fake gives one)");
            return;
        }
        match crate::add_device(Arc::new(crate::fake::FakeGpu::radeon_780m())) {
            Ok(d) => klog::event!(Info, "drm", "fake.on", minor = d.render_minor();
                "the fake GPU answers on /dev/dri/renderD{}", d.render_minor()),
            Err(e) => klog::event!(Warn, "drm", "fake.failed", errno = e),
        }
    });
}

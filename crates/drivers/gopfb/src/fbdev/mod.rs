//! fbdev `/dev/fb0` on the GOP framebuffer (DP-2; spec/gpu.md §5.3 layer 2, V2-PLAN §2.8 "Picture").
//!
//! What a program sees is Linux's DRM fbdev emulation on a firmware framebuffer (simpledrm +
//! `drm_fbdev_generic`, linux-6.6): one 32-bit XRGB mode at the native resolution, `yres_virtual` twice the
//! height when memory allowed (double buffering by FBIOPAN_DISPLAY), FBIO_WAITFORVSYNC, read/write and `mmap`.
//! The rules live in `abi` (host-tested); this module is the device.
//!
//! **Memory and deferred io.** `mmap` hands out ordinary RAM pages (`mem`, MM-4 DMA buffers given to MM-5b's
//! device mapping), never the GOP BAR, so the kernel's RAM frame stays the truth and MCP `screen` sees the
//! program (Linux `fb_defio.c`): a read fault maps a page read-only, the first store faults again and marks it
//! dirty (`dirty`), the `fbdev` thread waits for the next vblank estimate, write-protects the dirty pages of the
//! visible window (MM's `DeviceMap::write_protect`, with the TLB shootdown), then copies their rows into the
//! screen frame and presents the damage. FBIOPAN_DISPLAY (and an applied FBIOPUT_VSCREENINFO) copies the whole
//! new window at once. Never a display register (R8c): only the GOP framebuffer memory, through DP-1's present.
//!
//! **Who has the screen.** The first process that draws (a store into its mapping from any of its threads,
//! write(2), a pan or an applied mode) takes the scanout from the console (`display::take_scanout_with`, with
//! fbdev's frame as the picture MCP `screen` shows), or the VT hands it over at once when the program's terminal
//! switches to KD_GRAPHICS (`display::request_scanout_for`, fbdev's `ProgramOwner`). It keeps it until it ends,
//! crashes or is killed (checked at least every 100 ms), or until the console asks for it back (`display::
//! request_scanout_back`, KD_TEXT); then the console's frame is shown whole again. Other processes may draw too
//! (fb0 is shared, as on Linux). After a hand-back every page is write-protected again, so the next store takes
//! the screen anew; so is every page after a claim that did not end in a take (another owner held the screen),
//! at most every 100 ms, so a program drawing unseen claims again once the screen is free. The VT is told
//! (`display::sync_vt`) and a returned guard dropped only after `fbdev.screen` is released.
//!
//! **Pacing (DP-3).** FBIO_WAITFORVSYNC returns at the next vblank of the live display's clock (gopfb's
//! estimate at the exact EDID or CONFIG rate); `FBIO_BITSOS_PACING` (`pacing`) tells a program that rate, its
//! source, the last and next vblank (CLOCK_MONOTONIC) and when fb0's last picture reached the scanout.
//!
//! **Flips (GX-3, `flip`).** With a display engine that flips (amdgpu on the Mini-PC), every present of fb0's
//! frame goes to the hidden buffer and is flipped in at the next vblank (no tearing); FBIOPAN_DISPLAY returns
//! once its flip landed (as Linux's `drm_fb_helper_pan_display`, an atomic commit that waits for flip_done),
//! FBIO_WAITFORVSYNC and pacing follow the engine's own vblank (source Hardware). A late flip falls back to the
//! copy path above.
//!
//! Locks: `fbdev.screen` (Mutex; the frame, the scanout guard, the holder) > `fbdev.meta` is never nested;
//! `fbdev.screen` > mm.as (write-protect); `fbdev.meta` > mm.as (FBIOPUTCMAP reads the channels under it);
//! the fault hook runs under mm.as and takes only the spin locks `fbdev.dirty` and (for a claim, the caller's
//! process) `proc.table`, so `write_protect` is never called with `fbdev.dirty` held; the mmap hook (`check`)
//! runs under mm.as and asks the policy (B3).

pub mod abi;
pub mod compositor;
pub mod cursor;
mod ktests_cursor;
mod ktests_compositor;
pub mod dirty;
pub mod flip;
pub mod handback;
mod ktests;
mod ktests_store;
pub mod mem;
pub mod pacing;
mod ktests_pacing;
mod scene;

use abi::{Cmap, CmapUser, Geometry, SetOutcome, UserIo, Var};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use dirty::Dirty;
use display::{RamFrame, Rect, ScanoutGuard, ScanoutSource};
use ksync::{Mutex, Once, SpinLock, WaitQueue};
use mem::FbMem;
use mm::device::{DeviceMap, DeviceOps, DevicePage};
use proc::Process;
use uapi::errno::{EACCES, EFAULT, EFBIG, EINVAL, ENODEV, ENOSPC, ENOTTY, ENXIO};
use vfs::pagecache::PageFault;
use vfs::{Errno, File};

pub const NODE: &str = "/dev/fb0";
/// The seat user owns the node (as IN-4's evdev nodes; logind's ACL on Linux), group root, 0660.
pub const NODE_UID: u32 = 1000;
pub const NODE_GID: u32 = 0;
const MS: u64 = 1_000_000;

klog::counter!(FLUSHES, "fbdev.flushes");
klog::counter!(FLUSHED_ROWS, "fbdev.flushed_rows");
// WY-1a CR5: the kernel's half of a frame (dirty rows into the frame, then the present into GOP's write-combined
// memory): time and pixels summed over every flush, and the longest flush since boot (a running maximum only grows).
klog::counter!(FLUSH_NS, "fbdev.flush_ns");
klog::counter!(FLUSH_PX, "fbdev.flush_px");
klog::counter!(FLUSH_MAX_NS, "fbdev.flush_max_ns");
static FLUSH_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
klog::counter!(WRITE_FAULTS, "fbdev.write_faults");
klog::counter!(REFRESHES, "fbdev.refreshes");
klog::counter!(TAKES, "fbdev.takes");
klog::counter!(RELEASES, "fbdev.releases");
klog::counter!(REARMS, "fbdev.rearms");
klog::counter!(PASSES, "fbdev.passes");
klog::counter!(WAKES_TAKE, "fbdev.wakes_take");
klog::counter!(WAKES_CLOSE, "fbdev.wakes_close");
klog::counter!(WAKES_BACK, "fbdev.wakes_back");

/// What the fault hook shares with the device (it runs under the address space's lock: a spin lock and atomics).
struct Shared {
    pages: u64,
    dirty: SpinLock<Dirty>,
    /// Work for the `fbdev` thread (dirty pages, a claim).
    pending: AtomicBool,
    /// The process whose store asked for the screen while nobody held it (0 = none).
    claim_pid: AtomicU32,
    /// The pid holding the screen (0 = the console has it).
    holder: AtomicU32,
    wq: WaitQueue,
}

impl Shared {
    fn kick(&self) {
        self.pending.store(true, Ordering::Release);
        self.wq.wake_one();
    }
}

struct Ops(Arc<Shared>);

impl DeviceOps for Ops {
    /// Linux's defio accepts any mapping (`fb_deferred_io_mmap`); pages past the buffer are SIGBUS at the fault.
    /// B3: the policy is asked here as on every ioctl, read and write (runs in the mmap caller's context).
    fn check(&self, _pgoff: u64, _pages: u64, _prot: u64, _shared: bool) -> Result<(), i64> {
        allowed(proc::current().as_ref())
    }
    fn fault(&self, pgoff: u64, write: bool) -> Result<DevicePage, PageFault> {
        let s = &self.0;
        if pgoff >= s.pages {
            return Err(PageFault::Bus);
        }
        let per = (mem::CHUNK / 4096) as u64;
        let writable = if write {
            s.dirty.lock().set(pgoff);
            WRITE_FAULTS.inc();
            // The hook runs in the storing thread: `proc::current` names its process for any of its threads.
            if s.holder.load(Ordering::Acquire) == 0
                && s.claim_pid.load(Ordering::Acquire) == 0
                && let Some(p) = proc::current()
            {
                let _ = s.claim_pid.compare_exchange(0, p.pid, Ordering::AcqRel, Ordering::Acquire);
            }
            s.kick();
            true
        } else {
            // A page already marked may stay writable: the next flush protects it again.
            s.dirty.lock().is_set(pgoff)
        };
        Ok(DevicePage { aperture: (pgoff / per) as usize, page: pgoff % per, writable })
    }
}

struct Meta {
    var: Var,
    cmap: Cmap,
}

/// A hand-back in progress: `release` took it under `fbdev.screen`, `settle` finishes it after.
struct Returned {
    guard: Option<ScanoutGuard>,
    pid: u32,
    reason: &'static str,
    release_ns: u64,
}

struct Holder {
    pid: u32,
    name: String,
    proc: Weak<Process>,
}

struct Screen {
    /// What the screen shows while fbdev has it: the visible window of the buffer.
    frame: RamFrame,
    guard: Option<ScanoutGuard>,
    holder: Option<Holder>,
    /// `display::release_generation()` at the take.
    release_gen: u64,
    /// A refused take was logged (once until the next success).
    busy_logged: bool,
    /// When the pages may be write-protected again after a claim that did not take the screen (ns).
    rearm_at: u64,
}

pub struct Fb {
    me: Weak<Fb>,
    /// GX-4 (CR 1): the mode follows a display mode switch (`resize`); changed under `fbdev.screen` with the
    /// screen frame, read anywhere (a leaf spin lock).
    g: SpinLock<Geometry>,
    mem: FbMem,
    map: Arc<DeviceMap>,
    shared: Arc<Shared>,
    meta: Mutex<Meta>,
    /// The visible window's first buffer row (the var's yoffset; changed only under `fbdev.meta`, with the var).
    yoffset: AtomicU32,
    /// Dirty pages wait for a re-arm (`Fb::rearm`): the thread looks again within 100 ms.
    rearm_pending: AtomicBool,
    /// GX-3: the last flip fb0's frame ordered (0 = none); FBIOPAN_DISPLAY waits for it to land.
    flip_seq: AtomicU64,
    /// GX-3: fb0's frames went to the hidden buffer; the first copy after flipping stopped is whole.
    flipping: AtomicBool,
    screen: Mutex<Screen>,
    /// Presents of fb0's picture and the time the last one finished (pacing), one record: a reader never pairs
    /// a count with another present's time. Written under `fbdev.screen`; nothing is taken under it.
    present: SpinLock<Presented>,
}

#[derive(Clone, Copy, Default)]
struct Presented {
    seq: u64,
    ns: u64,
}

static FB: Once<Option<Arc<Fb>>> = Once::new();

/// The device (None before the `gopfb.fbdev` initcall, headless, or without memory for the buffer).
pub fn device() -> Option<&'static Arc<Fb>> {
    FB.get().and_then(|o| o.as_ref())
}

/// Counters since boot (for tests and the Mini-PC checks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub flushes: u64,
    pub flushed_rows: u64,
    pub write_faults: u64,
    pub refreshes: u64,
    pub takes: u64,
    pub releases: u64,
    pub rearms: u64,
    /// Passes of the `fbdev` thread (a starved thread shows as no new passes).
    pub passes: u64,
    /// DP-2f: wakes of the thread by a take, by the close of an open fb0, by `request_scanout_back` (the tests
    /// check these paths, since a missing wake only shows as a slow hand-back, which QEMU does not judge).
    pub wakes_take: u64,
    pub wakes_close: u64,
    pub wakes_back: u64,
}

pub fn stats() -> Stats {
    Stats {
        flushes: FLUSHES.get(),
        flushed_rows: FLUSHED_ROWS.get(),
        write_faults: WRITE_FAULTS.get(),
        refreshes: REFRESHES.get(),
        takes: TAKES.get(),
        releases: RELEASES.get(),
        rearms: REARMS.get(),
        passes: PASSES.get(),
        wakes_take: WAKES_TAKE.get(),
        wakes_close: WAKES_CLOSE.get(),
        wakes_back: WAKES_BACK.get(),
    }
}

/// Rate, period, source and the vblanks around now of the clock fbdev paces by, from one reading: the live
/// GopFb's clock (DP-1's estimate with the exact EDID or CONFIG period) while it is the installed display;
/// another display through the trait (its own vblank and mode rate: a display engine's, so source Hardware; GX
/// later). Times on the kernel clock, which is CLOCK_MONOTONIC. None without a display.
pub fn clock_now() -> Option<display::vblank::Snapshot> {
    // GX-3: a flipping display engine's own vblank (its frame counter and scan position).
    if let Some(s) = flip::active().and_then(|f| f.vblank()).and_then(flip::snapshot) {
        return Some(s);
    }
    let now = time::now_ns();
    if let Some(c) = crate::live_vblank() {
        return Some(c.snapshot(now));
    }
    let dev = display::device()?;
    let mhz = dev.current_mode().refresh_mhz;
    let (mhz, source) = if mhz == 0 {
        (display::vblank::DEFAULT_REFRESH_MHZ, display::RefreshSource::Default)
    } else {
        (mhz, display::RefreshSource::Hardware)
    };
    let period_ns = display::vblank::period_ns(mhz);
    let last = dev.last_vblank();
    let next = devmodel::display::VblankEvent { sequence: last.sequence + 1, time_ns: last.time_ns + period_ns };
    Some(display::vblank::Snapshot { refresh_mhz: mhz, period_ns, source, last, next })
}

/// The last vblank at or before now and the next one's time (see `clock_now`).
pub fn vblanks() -> Option<(devmodel::display::VblankEvent, u64)> {
    clock_now().map(|s| (s.last, s.next.time_ns))
}

/// The next vblank (FBIO_WAITFORVSYNC's end).
pub fn next_vblank_ns() -> Option<u64> {
    clock_now().map(|s| s.next.time_ns)
}

/// Rate (mHz), period (ns) and source of the clock fbdev paces by (the 60 Hz default without a display).
pub fn vblank_rate() -> (u32, u64, display::RefreshSource) {
    vblank_rate_of(clock_now())
}

fn vblank_rate_of(c: Option<display::vblank::Snapshot>) -> (u32, u64, display::RefreshSource) {
    match c {
        Some(s) => (s.refresh_mhz, s.period_ns, s.source),
        None => (display::vblank::DEFAULT_REFRESH_MHZ, display::vblank::period_ns(0), display::RefreshSource::Default),
    }
}

/// FBIO_WAITFORVSYNC: to the next vblank. A flipping display engine's frame counter may move a few lines after
/// the blank starts (its time comes from the scan line), so the wait goes on until the counter moved, at most two
/// periods (B4).
fn wait_vsync() {
    if let Some(f) = flip::active()
        && flip::wait_vsync(f, &time::now_ns, &|ns| sched::sleep_ns(ns))
    {
        return;
    }
    if let Some(t) = next_vblank_ns() {
        sleep_until(t);
    }
}

fn sleep_until(t: u64) {
    let now = time::now_ns();
    if t > now {
        sched::sleep_ns(t - now);
    }
}

fn alive(h: &Holder) -> bool {
    h.proc.upgrade().is_some_and(|p| !p.has_exited())
}

fn init() -> Result<(), &'static str> {
    let Some(dev) = display::device() else {
        klog::event!(Info, "fbdev", "absent", reason = "no display"; "no /dev/fb0 picture: the kernel runs headless");
        FB.get_or_init(|| None);
        return Ok(());
    };
    let m = dev.current_mode();
    let line = m.width as usize * 4;
    let (rows, mem) = match FbMem::alloc(line * m.height as usize * 2) {
        Ok(mem) => (m.height * 2, mem),
        Err(e) => {
            klog::event!(Warn, "fbdev", "memory.single", width = m.width, height = m.height, error = alloc::format!("{e:?}");
                "no memory for two buffers: yres_virtual = yres, no panning");
            match FbMem::alloc(line * m.height as usize) {
                Ok(mem) => (m.height, mem),
                Err(e) => {
                    klog::event!(Error, "fbdev", "memory.failed", width = m.width, height = m.height, error = alloc::format!("{e:?}");
                        "no memory for the fbdev buffer: /dev/fb0 answers ENODEV");
                    FB.get_or_init(|| None);
                    return Err("fbdev: no memory");
                }
            }
        }
    };
    let pages = mem.pages();
    let Some(d) = Dirty::new(pages) else {
        FB.get_or_init(|| None);
        return Err("fbdev: no memory for the dirty map");
    };
    let shared = Arc::new(Shared {
        pages,
        dirty: SpinLock::named("fbdev.dirty", d),
        pending: AtomicBool::new(false),
        claim_pid: AtomicU32::new(0),
        holder: AtomicU32::new(0),
        wq: WaitQueue::new(),
    });
    let map = match DeviceMap::new(Arc::new(Ops(shared.clone())), mem.apertures()) {
        Ok(map) => map,
        Err(e) => {
            klog::event!(Error, "fbdev", "map.refused", error = alloc::format!("{e:?}"));
            FB.get_or_init(|| None);
            return Err("fbdev: device map refused");
        }
    };
    if !mm::device::register(vfs::abi::encode_dev(abi::FB_MAJOR, 0), map.clone()) {
        klog::event!(Error, "fbdev", "map.unregistered"; "29:0 is registered already: programs cannot map /dev/fb0");
    }
    let g = Geometry { width: m.width, height: m.height, rows, smem_len: mem.len() as u32 };
    let frame = RamFrame::new(m.width, m.height);
    let fb = Arc::new_cyclic(|me| Fb {
        me: me.clone(),
        g: SpinLock::named("fbdev.geometry", g),
        mem,
        map,
        shared,
        meta: Mutex::named("fbdev.meta", Meta { var: g.initial_var(), cmap: Cmap::default() }),
        yoffset: AtomicU32::new(0),
        rearm_pending: AtomicBool::new(false),
        flip_seq: AtomicU64::new(0),
        flipping: AtomicBool::new(false),
        screen: Mutex::named("fbdev.screen", Screen { frame, guard: None, holder: None, release_gen: 0, busy_logged: false, rearm_at: 0 }),
        present: SpinLock::named("fbdev.present", Presented::default()),
    });
    FB.get_or_init(|| Some(fb.clone()));
    if !display::set_program_owner(&PROGRAM_OWNER) {
        klog::event!(Warn, "fbdev", "owner.refused"; "another program owner is registered: KD_GRAPHICS does not hand fb0 the screen");
    }
    if let Err(e) = sched::spawn("fbdev", sched::Class::Interactive, move || fb.run()) {
        klog::event!(Error, "fbdev", "thread.failed", error = alloc::format!("{e:?}"); "no deferred io: stores into /dev/fb0 will not reach the screen");
    }
    let (refresh_mhz, period_ns, source) = vblank_rate();
    klog::event!(Info, "fbdev", "ready", width = g.width, height = g.height, yres_virtual = rows, smem_len = g.smem_len, double = rows > g.height,
        refresh_mhz = refresh_mhz, period_ns = period_ns, refresh_source = source.name());
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "gopfb.fbdev", init);

/// GX-4 (CR 1): whether fb0 can take the mode `w` x `h` now: no program holds the screen through fb0, and the
/// buffer holds at least one screen of it (the memory stays the boot allocation; programs' mappings stay valid).
pub fn can_resize(w: u32, h: u32) -> Result<(), String> {
    let Some(fb) = device() else { return Ok(()) };
    let pid = fb.holder_pid();
    if pid != 0 {
        return Err(alloc::format!("a program (pid {pid}) holds the screen through /dev/fb0"));
    }
    rows_for(fb.g().smem_len, w, h).map(|_| ())
}

/// The buffer rows for `w` x `h` in `smem_len` bytes: twice the height when it fits (double buffering), else once.
fn rows_for(smem_len: u32, w: u32, h: u32) -> Result<u32, String> {
    let screen = (u64::from(w) * 4).saturating_mul(u64::from(h));
    if w == 0 || h == 0 || screen > u64::from(smem_len) {
        return Err(alloc::format!("{w}x{h} does not fit fb0's {smem_len} bytes"));
    }
    Ok(if 2 * screen <= u64::from(smem_len) { 2 * h } else { h })
}

/// GX-4 (CR 1): fb0 follows a display mode switch: XRGB8888 at `w` x `h` (line_length = 4 w), `yres_virtual`
/// twice that when the boot buffer holds it, offsets 0, the var as after boot (`drm_fb_helper_fill_var`).
/// FBIOGET_VSCREENINFO/FSCREENINFO answer the new mode at once; FBIOPUT_VSCREENINFO accepts only it. Refused
/// while a program holds the screen (its picture would change under it).
pub fn resize(w: u32, h: u32) -> Result<(), String> {
    let Some(fb) = device() else { return Ok(()) };
    let frame = RamFrame::try_new(w, h).ok_or_else(|| String::from("no memory for fb0's screen frame"))?;
    {
        let mut scr = fb.screen.lock();
        if scr.holder.is_some() || fb.holder_pid() != 0 {
            return Err(String::from("a program holds the screen through /dev/fb0"));
        }
        let old = fb.g();
        let rows = rows_for(old.smem_len, w, h)?;
        let g = Geometry { width: w, height: h, rows, smem_len: old.smem_len };
        scr.frame = frame;
        *fb.g.lock() = g;
        fb.yoffset.store(0, Ordering::Release);
    }
    // `fbdev.meta` after `fbdev.screen` is released (never nested): the var as after boot for the new mode.
    let g = fb.g();
    fb.meta.lock().var = g.initial_var();
    klog::event!(Info, "fbdev", "resized", width = g.width, height = g.height, yres_virtual = g.rows, smem_len = g.smem_len);
    Ok(())
}

/// Gives `/dev/fb0` its owner (devfs makes registered nodes as root). True once done; false before devfs exists.
pub fn fix_node_owner() -> bool {
    match vfs::root_ns().lookup(NODE) {
        Ok(d) => {
            d.inode.meta.set_owner(NODE_UID, NODE_GID);
            true
        }
        Err(_) => false,
    }
}

impl Fb {
    pub fn geometry(&self) -> Geometry {
        *self.g.lock()
    }
    fn g(&self) -> Geometry {
        self.geometry()
    }
    pub fn holder_pid(&self) -> u32 {
        self.shared.holder.load(Ordering::Acquire)
    }
    fn line(&self) -> u64 {
        self.g().line_length() as u64
    }

    /// The `fbdev` thread: deferred io and the holder's life.
    fn run(self: Arc<Self>) {
        let mut owner_fixed = fix_node_owner();
        loop {
            let held = self.holder_pid() != 0;
            // B4: never an unbounded wait; a holder is checked for its end at least every 100 ms.
            let t = if held || !owner_fixed || self.rearm_pending.load(Ordering::Acquire) { 100 * MS } else { 1000 * MS };
            self.shared.wq.wait_until_timeout(|| self.shared.pending.load(Ordering::Acquire), t);
            self.shared.pending.store(false, Ordering::Release);
            if !owner_fixed {
                owner_fixed = fix_node_owner();
            }
            self.pass();
        }
    }

    /// One pass: the holder's end or a hand-back request, a pending claim (or the re-arm when none took the
    /// screen), then the dirty pages at the next vblank.
    fn pass(&self) {
        PASSES.inc();
        let mut returned = None;
        {
            let mut scr = self.screen.lock();
            if let Some(h) = &scr.holder {
                if !alive(h) {
                    returned = self.release(&mut scr, "ended");
                } else if display::release_generation() != scr.release_gen {
                    returned = self.release(&mut scr, "requested");
                }
            }
            let pid = self.shared.claim_pid.swap(0, Ordering::AcqRel);
            if scr.holder.is_none() && pid != 0 {
                match proc::find(pid) {
                    Some(p) if !p.has_exited() => {
                        self.take(&mut scr, &p, "store");
                    }
                    _ => klog::event!(Info, "fbdev", "claim.unknown", pid = pid; "a store into /dev/fb0 by a process that has ended"),
                }
            }
            if scr.guard.is_none() {
                self.rearm(&mut scr);
            }
        }
        self.settle(returned);
        if self.holder_pid() != 0 && self.visible_dirty() {
            // Deferred io: copy at the vblank estimate, so a frame's stores land together.
            if let Some(t) = next_vblank_ns() {
                sleep_until(t);
            }
            let mut scr = self.screen.lock();
            if scr.guard.is_some() {
                self.flush(&mut scr);
            }
        }
    }

    fn visible_pages(&self) -> (u64, u64) {
        dirty::pages_of_rows(self.yoffset.load(Ordering::Acquire) as u64, self.g().height as u64, self.line())
    }
    fn visible_dirty(&self) -> bool {
        let (a, b) = self.visible_pages();
        self.shared.dirty.lock().any_in(a, b)
    }

    /// Nobody took the screen, yet pages are dirty (a store whose claim was refused or came from no process):
    /// write-protect every page again so the next store faults and claims anew. The window is copied whole at
    /// the take, so nothing drawn is lost. At most every 100 ms (`rearm_pending` brings the thread back).
    fn rearm(&self, scr: &mut Screen) {
        if !self.shared.dirty.lock().any_in(0, self.shared.pages.saturating_sub(1)) {
            self.rearm_pending.store(false, Ordering::Release);
            return;
        }
        let now = time::now_ns();
        if now < scr.rearm_at {
            self.rearm_pending.store(true, Ordering::Release);
            return;
        }
        scr.rearm_at = now + 100 * MS;
        self.rearm_pending.store(false, Ordering::Release);
        self.shared.dirty.lock().clear();
        self.map.write_protect(0, self.shared.pages.saturating_sub(1));
        REARMS.inc();
    }

    /// After `fbdev.screen` is released: a returned guard is dropped (the console frame shown) and the VT is told
    /// who has the screen now (`display::sync_vt`, which may take console locks). A hand-back is timed
    /// (`handback::last`, event `screen.handback`).
    fn settle(&self, returned: Option<Returned>) {
        let Some(r) = returned else {
            display::sync_vt();
            return;
        };
        let px = display::stats().pixels;
        let t = time::now_ns();
        // The guard's drop shows the console frame (`display::returned_pixels` is that copy alone).
        drop(r.guard);
        display::sync_vt();
        let at_ns = time::now_ns();
        let h = handback::HandBack {
            pid: r.pid,
            reason: r.reason,
            at_ns,
            release_ns: r.release_ns,
            show_ns: at_ns - t,
            pixels: display::returned_pixels(),
            presented: display::stats().pixels.wrapping_sub(px),
        };
        handback::record(&h);
        compositor::handed_back(h.pid);
        cursor::handed_back(h.pid);
        klog::event!(Info, "fbdev", "screen.handback", pid = h.pid, reason = h.reason, release_us = h.release_ns / 1000,
            show_us = h.show_ns / 1000, pixels = h.pixels, presented = h.presented);
    }

    /// Takes the screen for `p` (no-op if fbdev holds it already). False if another owner holds it. The caller
    /// runs `settle` once `fbdev.screen` is released.
    fn take(&self, scr: &mut Screen, p: &Arc<Process>, via: &'static str) -> bool {
        if scr.guard.is_some() {
            return true;
        }
        // Every store of a program drawing unseen asks again: look first, so display's own `scanout.busy` event
        // is not repeated for each (fbdev says it once, below).
        let r = match display::scanout_holder() {
            Some((owner, _)) => Err(display::ScanoutError::Busy { owner }),
            None => match self.me.upgrade() {
                Some(me) => display::take_scanout_with("fbdev", Some(me as Arc<dyn ScanoutSource>), p.pid),
                None => return false,
            },
        };
        match r {
            Ok(g) => {
                scr.guard = Some(g);
                scr.holder = Some(Holder { pid: p.pid, name: p.name.clone(), proc: Arc::downgrade(p) });
                scr.release_gen = display::release_generation();
                scr.busy_logged = false;
                self.shared.holder.store(p.pid, Ordering::Release);
                // A claim raised between the pass's swap and the line above is covered by the refresh below.
                self.shared.claim_pid.store(0, Ordering::Release);
                TAKES.inc();
                // DP-2f: a take outside the thread (KD_GRAPHICS, write(2), a pan) wakes it, so it watches the holder
                // at its 100 ms poll from now on instead of finishing a 1 s idle wait first.
                self.shared.kick();
                WAKES_TAKE.inc();
                klog::event!(Info, "fbdev", "screen.taken", pid = p.pid, program = p.name.as_str(), via = via);
                // GX-3: a surface that fell back to one image after a late flip may flip again.
                if let Some(f) = flip::get() {
                    f.rearm();
                }
                self.refresh(scr);
                true
            }
            Err(e) => {
                if !scr.busy_logged {
                    scr.busy_logged = true;
                    klog::event!(Info, "fbdev", "screen.busy", pid = p.pid, program = p.name.as_str(), error = alloc::format!("{e:?}");
                        "another owner has the screen: the program draws into fb0 unseen");
                }
                false
            }
        }
    }

    /// Gives the screen back to the console and write-protects every page, so the next store claims it again.
    /// Returns the guard: the caller drops it in `settle`, after `fbdev.screen` (dropping it shows the console
    /// frame whole and tells the VT).
    #[must_use]
    fn release(&self, scr: &mut Screen, reason: &'static str) -> Option<Returned> {
        let t = time::now_ns();
        let g = scr.guard.take();
        let h = scr.holder.take();
        self.shared.holder.store(0, Ordering::Release);
        // Older claims are stale (every page is protected again below): only a new store claims the screen.
        self.shared.claim_pid.store(0, Ordering::Release);
        self.shared.dirty.lock().clear();
        self.map.write_protect(0, self.shared.pages.saturating_sub(1));
        RELEASES.inc();
        let (pid, name) = h.map(|h| (h.pid, h.name)).unwrap_or((0, String::new()));
        klog::event!(Info, "fbdev", "screen.returned", pid = pid, program = name.as_str(), reason = reason);
        Some(Returned { guard: g, pid, reason, release_ns: time::now_ns() - t })
    }

    /// Copies the whole visible window into the frame and presents it (a take, a pan, a mode set).
    fn refresh(&self, scr: &mut Screen) {
        let top = self.yoffset.load(Ordering::Acquire) as u64;
        let (a, b) = dirty::pages_of_rows(top, self.g().height as u64, self.line());
        self.shared.dirty.lock().take_runs(a, b, |_, _| {});
        self.map.write_protect(a, b);
        let Screen { frame, guard, .. } = scr;
        if !self.mem.read_pixels((top * self.line()) as usize, frame.pixels_mut()) {
            klog::event!(Error, "fbdev", "copy.refused", top = top; "the visible window lies outside the buffer (bug)");
            return;
        }
        frame.mark_all();
        REFRESHES.inc();
        if let Some(g) = guard.as_mut() {
            self.show(g, frame);
        }
    }

    /// Presents the frame's damage: into the hidden buffer and flipped in when the display engine flips (GX-3),
    /// else copied to the screen. A flip that fell back makes the next present whole (A0 lacks the frames A1 got).
    fn show(&self, g: &mut ScanoutGuard, frame: &mut RamFrame) {
        let shown = flip::frame(flip::ready(), &self.flipping, frame, |fr| (fr.pixels(), fr.width), |back, fr| {
            g.present_via(back, fr);
        });
        match shown {
            flip::Shown::Flipped(seq) => {
                self.flip_seq.store(seq, Ordering::Release);
                self.presented();
                return;
            }
            flip::Shown::Copy { whole: true } => frame.mark_all(),
            flip::Shown::Copy { whole: false } => {}
        }
        g.present(frame);
        self.presented();
    }

    /// FBIOPAN_DISPLAY's end: fb0's last flip landed (at once without one).
    fn wait_flip(&self) {
        flip::landed(flip::get(), self.flip_seq.load(Ordering::Acquire));
    }

    /// A picture of fb0 reached the scanout (pacing: FBIO_BITSOS_PACING's present_seq/present_ns).
    fn presented(&self) {
        let ns = time::now_ns();
        let mut p = self.present.lock();
        p.seq += 1;
        p.ns = ns;
    }

    /// `FBIO_BITSOS_PACING`'s answer now.
    pub fn pacing(&self) -> pacing::Pacing {
        let c = clock_now();
        let (refresh_mhz, period_ns, source) = vblank_rate_of(c);
        let (last, next) = c.map_or_else(Default::default, |c| (c.last, c.next.time_ns));
        let present = *self.present.lock();
        pacing::Pacing {
            flags: pacing::flags(source),
            refresh_mhz,
            source: source as u32,
            period_ns,
            vblank_seq: last.sequence,
            vblank_ns: last.time_ns,
            next_vblank_ns: next,
            present_seq: present.seq,
            present_ns: present.ns,
        }
    }

    /// Copies the rows of the dirty pages of the visible window and presents them.
    fn flush(&self, scr: &mut Screen) {
        let top = self.yoffset.load(Ordering::Acquire) as u64;
        let g = self.g();
        let (h, line, w) = (g.height as u64, self.line(), g.width);
        let (a, b) = dirty::pages_of_rows(top, h, line);
        let mut runs: Vec<(u64, u64)> = Vec::new();
        if runs.try_reserve_exact(((b - a + 2) / 2) as usize).is_err() {
            // Without room for the list, copy the whole window instead.
            self.refresh(scr);
            return;
        }
        self.shared.dirty.lock().take_runs(a, b, |x, y| runs.push((x, y)));
        let (Some(first), Some(last)) = (runs.first(), runs.last()) else { return };
        // Clean pages in between are read-only already (only a store makes a page writable, and a store marks it).
        self.map.write_protect(first.0, last.1);
        let t0 = time::now_ns();
        let Screen { frame, guard, .. } = scr;
        let mut rows = 0;
        for &(x, y) in &runs {
            let Some((r0, n)) = dirty::rows_of_pages(x, y, line, top, h) else { continue };
            let px = &mut frame.pixels_mut()[(r0 * w as u64) as usize..((r0 + n) * w as u64) as usize];
            if self.mem.read_pixels(((top + r0) * line) as usize, px) {
                frame.mark(Rect::new(0, r0 as u32, w, n as u32));
                rows += n;
            }
        }
        FLUSHES.inc();
        FLUSHED_ROWS.add(rows);
        if let Some(g) = guard.as_mut() {
            self.show(g, frame);
        }
        let d = time::now_ns().saturating_sub(t0);
        FLUSH_NS.add(d);
        FLUSH_PX.add(rows * w as u64);
        let old = FLUSH_MAX.fetch_max(d, Ordering::Relaxed);
        if d > old {
            FLUSH_MAX_NS.add(d - old);
        }
    }

    /// The picture moved (a pan or an applied mode; `yoffset` was set with the var): the new window is shown at once.
    fn show_offset(&self, who: Option<&Arc<Process>>, via: &'static str) {
        {
            let mut scr = self.screen.lock();
            if scr.guard.is_some() {
                self.refresh(&mut scr);
            } else if let Some(p) = who {
                self.take(&mut scr, p, via);
            }
        }
        self.settle(None);
    }

    /// KD_GRAPHICS on program `pid`'s terminal (`display::request_scanout_for`): the screen shows fb0's visible
    /// window for it now. True when `pid` holds the screen afterwards.
    pub fn take_for(&self, pid: u32) -> bool {
        let Some(p) = proc::find(pid).filter(|p| !p.has_exited()) else {
            klog::event!(Info, "fbdev", "graphics.unknown", pid = pid; "KD_GRAPHICS for a process that has ended");
            return false;
        };
        let ok = {
            let mut scr = self.screen.lock();
            match &scr.holder {
                Some(h) => h.pid == pid,
                None => self.take(&mut scr, &p, "kd_graphics"),
            }
        };
        self.settle(None);
        ok
    }

    /// A program wrote pages `a..=b` with write(2): mark them and make sure the screen shows fbdev.
    fn written(&self, a: u64, b: u64, who: Option<&Arc<Process>>) {
        self.shared.dirty.lock().set_range(a, b);
        if self.holder_pid() == 0
            && let Some(p) = who
        {
            {
                let mut scr = self.screen.lock();
                self.take(&mut scr, p, "write");
            }
            self.settle(None);
        }
        self.shared.kick();
    }

    /// fsync: the deferred io now (Linux `fb_deferred_io_fsync` runs the work at once).
    pub fn sync(&self) {
        let mut scr = self.screen.lock();
        if scr.guard.is_some() {
            self.flush(&mut scr);
        }
    }

    /// read(2) at `pos` (`fb_sys_read`).
    pub fn read(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let total = self.g().smem_len as u64;
        if pos >= total {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(total - pos) as usize;
        if !self.mem.read(pos as usize, &mut buf[..n]) {
            return Err(EFAULT);
        }
        Ok(n)
    }

    /// write(2) at `pos` (`fb_sys_write`: EFBIG past the end, a short write or ENOSPC at it).
    pub fn write(&self, pos: u64, buf: &[u8], who: Option<&Arc<Process>>) -> Result<usize, Errno> {
        let total = self.g().smem_len as u64;
        if pos > total {
            return Err(EFBIG);
        }
        let mut err = 0;
        let mut count = buf.len() as u64;
        if count > total {
            err = EFBIG;
            count = total;
        }
        if count + pos > total {
            if err == 0 {
                err = ENOSPC;
            }
            count = total - pos;
        }
        if count == 0 {
            return if err != 0 { Err(err) } else { Ok(0) };
        }
        if !self.mem.write(pos as usize, &buf[..count as usize]) {
            return Err(EFAULT);
        }
        self.written(pos / 4096, (pos + count - 1) / 4096, who);
        Ok(count as usize)
    }

    /// The fbdev ioctls (`do_fb_ioctl` with DRM's hooks, fbcon absent). Returns the bytes the caller copies to
    /// `arg` (Linux's final `copy_to_user(argp, ...)`; empty for none); `io` reads the arguments and writes the
    /// colour map's channels; `who` is the calling process (takes the screen on a pan or an applied mode).
    pub fn ioctl(&self, cmd: u64, arg: u64, io: &dyn UserIo, who: Option<&Arc<Process>>) -> Result<Vec<u8>, Errno> {
        match cmd {
            abi::FBIOGET_VSCREENINFO => Ok(self.meta.lock().var.to_bytes().to_vec()),
            abi::FBIOPUT_VSCREENINFO => {
                let mut b = [0u8; abi::VAR_SIZE];
                if !io.read(arg, &mut b) {
                    return Err(EFAULT);
                }
                let mut var = Var::from_bytes(&b);
                var.0[abi::ACTIVATE] &= !abi::FB_ACTIVATE_KD_TEXT;
                let applied = {
                    let mut m = self.meta.lock();
                    let cur = m.var;
                    match abi::set_var(&mut var, &cur, &self.g())? {
                        SetOutcome::Applied => {
                            m.var = var;
                            // fb_set_var ends with fb_pan_display(info, &info->var); its answer is ignored there.
                            let _ = abi::pan(&var, &mut m.var);
                            // With the var, so GET_VSCREENINFO and the shown window never disagree.
                            self.yoffset.store(m.var.0[abi::YOFFSET], Ordering::Release);
                            true
                        }
                        SetOutcome::Unchanged | SetOutcome::Tested => false,
                    }
                };
                if applied {
                    self.show_offset(who, "mode");
                }
                Ok(var.to_bytes().to_vec())
            }
            abi::FBIOGET_FSCREENINFO => Ok(self.g().fix_bytes().to_vec()),
            abi::FBIOGETCMAP => {
                let mut b = [0u8; abi::CMAP_USER_SIZE];
                if !io.read(arg, &mut b) {
                    return Err(EFAULT);
                }
                let map = self.meta.lock().cmap.clone();
                map.to_user(&CmapUser::from_bytes(&b), io)?;
                Ok(Vec::new())
            }
            abi::FBIOPUTCMAP => {
                let mut b = [0u8; abi::CMAP_USER_SIZE];
                if !io.read(arg, &mut b) {
                    return Err(EFAULT);
                }
                // Read into a copy under the lock (two callers never lose an update; a refusal changes nothing,
                // as Linux's temporary `umap`); user memory may fault in under it (fbdev.meta > mm.as).
                let mut m = self.meta.lock();
                let mut map = m.cmap.clone();
                map.set_user(&CmapUser::from_bytes(&b), io)?;
                m.cmap = map;
                Ok(Vec::new())
            }
            abi::FBIOPAN_DISPLAY => {
                let mut b = [0u8; abi::VAR_SIZE];
                if !io.read(arg, &mut b) {
                    return Err(EFAULT);
                }
                let var = Var::from_bytes(&b);
                {
                    let mut m = self.meta.lock();
                    abi::pan(&var, &mut m.var)?;
                    self.yoffset.store(m.var.0[abi::YOFFSET], Ordering::Release);
                }
                self.show_offset(who, "pan");
                self.wait_flip();
                Ok(var.to_bytes().to_vec())
            }
            abi::FBIO_CURSOR => Err(EINVAL),
            // fbcon is not built (include/linux/fbcon.h stubs answer 0 and copy nothing).
            abi::FBIOGET_CON2FBMAP | abi::FBIOPUT_CON2FBMAP => Ok(Vec::new()),
            abi::FBIOBLANK => {
                if arg > abi::FB_BLANK_POWERDOWN {
                    return Err(EINVAL);
                }
                // GOP cannot blank and display registers are off limits (R8c): accepted, the picture stays.
                klog::event!(Debug, "fbdev", "blank.ignored", level = arg);
                Ok(Vec::new())
            }
            abi::FBIO_WAITFORVSYNC => {
                // DRM waits for one vblank of the first CRTC and ignores the argument.
                wait_vsync();
                Ok(Vec::new())
            }
            // DP-3: BitsOS's pacing info (ENOTTY on Linux, where programs measure instead).
            pacing::FBIO_BITSOS_PACING => Ok(self.pacing().to_bytes().to_vec()),
            // SC-7a: the compositor grant, tied to the screen's holder (`compositor`).
            compositor::FBIO_BITSOS_COMPOSITOR => compositor::ioctl(self, arg, io, who),
            // GX-5a: the hardware cursor for the screen's holder (`cursor`).
            cursor::FBIO_BITSOS_CURSOR => cursor::ioctl(self, arg, io, who),
            _ => Err(ENOTTY),
        }
    }
}

impl ScanoutSource for Fb {
    /// MCP `screen` while a program has the screen: fbdev's frame. The lock is waited for at most 250 ms (a
    /// flush holds it for a few ms; MCP's thread is KernelRt, B4).
    fn with_picture(&self, f: &mut dyn FnMut(&[u32], u32, u32)) -> bool {
        let Some(scr) = self.screen.lock_timeout(250 * MS) else {
            klog::event!(Warn, "fbdev", "picture.busy"; "the screen frame stayed locked for 250 ms");
            return false;
        };
        f(scr.frame.pixels(), scr.frame.width, scr.frame.height);
        true
    }
}

/// The calling process's memory (the syscall's caller; kernel threads have none: every access faults).
struct CallerIo(Option<Arc<Process>>);

impl UserIo for CallerIo {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        self.0.as_ref().is_some_and(|p| p.mm.copy_from_user(buf, addr).is_ok())
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.0.as_ref().is_some_and(|p| p.mm.copy_to_user(addr, data).is_ok())
    }
}

/// KD_GRAPHICS's way to fbdev (`display::request_scanout_for`).
static PROGRAM_OWNER: display::ProgramOwner = display::ProgramOwner { take_for: take_for_pid, give_back };

/// KD_TEXT (`display::request_scanout_back`): the thread compares the release generation now (DP-2f).
fn give_back() {
    if let Some(fb) = device() {
        fb.shared.kick();
        WAKES_BACK.inc();
    }
}

fn take_for_pid(pid: u32) -> bool {
    device().is_some_and(|fb| fb.take_for(pid))
}

/// B3: a program reaching the display device.
fn allowed(p: Option<&Arc<Process>>) -> Result<(), Errno> {
    match p {
        Some(p) => policy::check(&p.subject(), &policy::Access::Device { class: policy::DeviceClass::Gpu, name: NODE }).map_err(|_| EACCES),
        None => Ok(()),
    }
}

/// The node's object, and one per open (`open_node`, Linux's `struct file`); the state lives in `Fb`.
pub struct Fb0 {
    /// An open file (not the node's own object).
    open: bool,
}

/// DP-2f: an open fb0 is closed (a holder's last close is usually its exit, `exit_files`): the thread looks at
/// the holder now instead of at its next 100 ms poll, so the console gets the screen back at once.
impl Drop for Fb0 {
    fn drop(&mut self) {
        if self.open
            && let Some(fb) = device()
        {
            fb.shared.kick();
            WAKES_CLOSE.inc();
        }
    }
}

impl File for Fb0 {
    /// Every open gets its own object, so its close is seen (Linux `fb_release`).
    fn open_node(&self, _flags: u64, _who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        (!self.open).then(|| Ok(Arc::new(Fb0 { open: true }) as Arc<dyn File>))
    }
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let fb = device().ok_or(ENODEV)?;
        allowed(proc::current().as_ref())?;
        fb.read(pos, buf)
    }
    fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        let fb = device().ok_or(ENODEV)?;
        let who = proc::current();
        allowed(who.as_ref())?;
        fb.write(pos, buf, who.as_ref())
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o660
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(abi::FB_MAJOR, 0)
    }
    /// FS-4 CR6: fb_fops has no `.poll`, so epoll_ctl answers EPERM and poll sees DEFAULT_POLLMASK.
    fn can_poll(&self) -> bool {
        false
    }
    /// `default_llseek` on a char device: positional reads and writes.
    fn seekable(&self) -> bool {
        true
    }
    /// `default_llseek` (linux-6.6/fs/read_write.c:230) with the node's i_size 0: SEEK_END is `0 + off`,
    /// SEEK_DATA is ENXIO, SEEK_HOLE is ENXIO or 0; a negative result is EINVAL. SET and CUR stay generic.
    fn lseek(&self, off: i64, whence: u32) -> Option<Result<u64, Errno>> {
        let neg = Err(EINVAL);
        match whence {
            vfs::abi::SEEK_END => Some(if off >= 0 { Ok(off as u64) } else { neg }),
            vfs::abi::SEEK_DATA => Some(if off >= 0 { Err(ENXIO) } else { neg }),
            vfs::abi::SEEK_HOLE => Some(if off >= 0 { Err(ENXIO) } else { Ok(0) }),
            _ => None,
        }
    }
    fn ioctl(&self, cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        let fb = device().ok_or(ENODEV)?;
        let who = proc::current();
        allowed(who.as_ref())?;
        let io = CallerIo(who.clone());
        fb.ioctl(cmd, arg, &io, who.as_ref())
    }
    fn fsync(&self) -> Result<(), Errno> {
        if let Some(fb) = device() {
            fb.sync();
        }
        Ok(())
    }
}

fn make_fb0() -> Arc<dyn File> {
    Arc::new(Fb0 { open: false })
}
vfs::devnode!("/dev/fb0", make_fb0);

#[cfg(test)]
mod resize_tests {
    use super::abi::{Geometry, XRES, XRES_VIRTUAL, YRES, YRES_VIRTUAL};
    use super::rows_for;

    /// GX-4 CR 1: the boot buffer of 2560x1440 double buffered; a smaller mode keeps double buffering, the var
    /// and fix follow (line_length 4 w); a size beyond the buffer is refused.
    #[test]
    fn a_smaller_mode_fits_the_boot_buffer() {
        let smem = 2560 * 4 * 1440 * 2;
        assert_eq!(rows_for(smem, 1920, 1080), Ok(2160));
        assert_eq!(rows_for(smem, 2560, 1440), Ok(2880));
        assert_eq!(rows_for(2560 * 4 * 1440, 2560, 1440), Ok(1440), "one screen when two do not fit");
        assert!(rows_for(smem, 3840, 2160).is_err());
        assert!(rows_for(smem, 0, 1080).is_err());
        assert!(rows_for(u32::MAX, u32::MAX, u32::MAX).is_err(), "no overflow");
        let g = Geometry { width: 1920, height: 1080, rows: 2160, smem_len: smem };
        let v = g.initial_var();
        assert_eq!((v.0[XRES], v.0[YRES], v.0[XRES_VIRTUAL], v.0[YRES_VIRTUAL]), (1920, 1080, 1920, 2160));
        assert_eq!(g.line_length(), 7680);
        let f = g.fix_bytes();
        // struct fb_fix_screeninfo: line_length at byte 48 (linux-6.6/include/uapi/linux/fb.h:158-174).
        assert_eq!(u32::from_le_bytes([f[48], f[49], f[50], f[51]]), 7680);
    }
}

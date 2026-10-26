//! THE LOCK ORDER (desk-owned; CONTRACTS.md §4 R5; V2-LANES §2: edited only through the desk).
//!
//! Outer locks have small levels. While a lock of level `a` is held, only locks of a **higher** level may be
//! taken. Classes the table does not name sit at `SUBSYSTEM` (the per-subsystem locks): those are unordered
//! among themselves by the table, and the debug checker instead watches the order they are actually taken in
//! (an A-then-B somewhere and a B-then-A elsewhere is reported as an inversion).
//!
//! The order (outermost first): request locks that drive a driver's bring-up (snd.output) > driver bring-up locks
//! (hda.hdmi, rtw89.core) > PCI's sleeping pci.enable > pci.cfg > pci.ecam (and hal's 0xCF8 pair, not a
//! ksync lock) > per-subsystem locks > ksync's wait-queue lock > leaves (pci.state, pci.devices) > **sched** >
//! klog (sinks, the console/display locks the sinks take, ring) > heap > frames.
//!
//! **sched is inside every subsystem lock, not outside** (SC-2, open point for the desk): a thread that holds a
//! `Mutex` and sleeps or yields takes the scheduler's lock with the mutex held, and so does every wake-up
//! (`Completion::complete`, a mutex release) done under a lock; the scheduler itself takes nothing but memory
//! under its lock. CONTRACTS' "sched > per-subsystem" cannot hold for that lock (the first gate run with the
//! checker found it on every contended mutex); V1 had SCHED as a leaf for the same reason.
//!
//! `alloc` marks the locks the allocator itself takes: nothing but another allocator lock may be taken under
//! them (and ksync never allocates while one is held). A new allocator lock (MM-1's buddy, MM-3's slab) must be
//! added here, by exact name, at `ALLOC` or above.
//!
//! Observed subsystem-level orders the table does not encode (the checker watches them): SC-7b's priority passing
//! takes `futex.bucket` > `sched.pi` > {`sched` (thread table), `futex.thread`} and `futex.bucket` > `proc.tasks`; a
//! `Mutex` release with recorded waiters takes `sched.pi`, then `sched.attr`/`sched.grants`, under whatever lock the
//! releaser holds (CONTRACTS §4 R5).
//!
//! Rules by prefix (`"klog."`) apply to every name that starts with it unless an exact name says otherwise.

use crate::class::LockClass;

/// SN-12: sound's output request lock (`snd.output`): one sysfs/MCP/console/boot request at a time, held across
/// HDMI sound's on/off (`hda.hdmi`, the GPU controller's restart) and the bounded make-before-break sleeps, so the
/// target and the display side change together. Nothing is held when it is taken.
pub const SND_OUTPUT: u16 = 180;
/// Driver locks that bring a PCI function up under them (they take pci.enable and config space inside).
pub const DRIVER_OUTER: u16 = 190;
/// PCI's sleeping enable lock and the config-space chain under it.
pub const PCI_ENABLE: u16 = 200;
pub const PCI_CFG: u16 = 210;
pub const PCI_ECAM: u16 = 220;
/// Every lock the table does not name.
pub const SUBSYSTEM: u16 = 500;
/// ksync's own wait-queue lock: taken under any subsystem lock, holds nothing but memory.
pub const WAITQUEUE: u16 = 700;
/// Leaves: only the scheduler (wake-ups), klog and memory below them.
pub const LEAF: u16 = 750;
/// The scheduler's lock: taken under any lock a sleeping or waking thread holds; holds only memory.
pub const SCHED: u16 = 770;
/// klog's sink list; the sinks run under it until RB-1's deferred sinks.
pub const KLOG_SINKS: u16 = 780;
/// What the sinks take under `klog.sinks` (console text grid, the display frame).
pub const SINK_TARGETS: u16 = 790;
/// The klog ring.
pub const KLOG: u16 = 800;
/// The allocator: heap, then frames under it (the heap grows from the frame allocator).
pub const ALLOC: u16 = 900;
pub const FRAMES: u16 = 910;
// Inside the allocator (CONTRACTS.md R5): heap.mags > heap.cache > heap.seed > frames per-CPU list > frames zone.
const HEAP_CACHE: u16 = ALLOC + 1;
const HEAP_SEED: u16 = ALLOC + 2;
const FRAMES_ZONE: u16 = FRAMES + 1;
const FRAMES_MODULES: u16 = FRAMES + 2;

/// Exact names (static classes; allocator locks must be here).
pub(crate) static TABLE: [LockClass; 22] = [
    LockClass::fixed("sched", SCHED, false),
    LockClass::fixed("pci.enable", PCI_ENABLE, false),
    LockClass::fixed("pci.cfg", PCI_CFG, false),
    LockClass::fixed("pci.ecam", PCI_ECAM, false),
    LockClass::fixed("waitqueue", WAITQUEUE, false),
    LockClass::fixed("pci.state", LEAF, false),
    LockClass::fixed("pci.devices", LEAF, false),
    LockClass::fixed("klog.sinks", KLOG_SINKS, false),
    LockClass::fixed("klog.ring", KLOG, false),
    LockClass::fixed("heap", ALLOC, true),
    LockClass::fixed("frames", FRAMES, true),
    // MM-1/MM-3 allocator locks by exact name (desk, 15.10.).
    LockClass::fixed("heap.mags", ALLOC, true),
    LockClass::fixed("heap.cache", HEAP_CACHE, true),
    LockClass::fixed("heap.seed", HEAP_SEED, true),
    LockClass::fixed("frames.pcp", FRAMES, true),
    LockClass::fixed("frames.dma32", FRAMES_ZONE, true),
    LockClass::fixed("frames.normal", FRAMES_ZONE, true),
    LockClass::fixed("frames.modules", FRAMES_MODULES, true),
    // DP-2's VT sync mutex (INT-A2a): an outer sleeping lock under which the VT's hooks take the console's own
    // locks and present; not one of the sink targets its `display.` prefix would make it.
    LockClass::fixed("display.vt", SUBSYSTEM, false),
    // SN-5's HDMI sequence lock (desk 22.10.): the GPU's HD Audio function is reset and brought up under it, which
    // takes pci.cfg/pci.ecam (the Mini-PC's checker saw hda.hdmi -> pci.* on every sn.dpaudio run).
    LockClass::fixed("hda.hdmi", DRIVER_OUTER, false),
    // SN-12 (desk-minipc-w12 follow-up 1): every path of SN-10/SN-11's output request (sysfs, MCP, console, the
    // boot switch) takes hda.hdmi under snd.output; at SUBSYSTEM that was the boot's lock.violation.
    LockClass::fixed("snd.output", SND_OUTPUT, false),
    // WL's chip mutex (W12): the driver thread runs rtw89-core's bring-up under it, whose bus reads and writes the
    // function's config space (MAC-off guard, PCIe link setup), like Linux's rtwdev->mutex around pci_*_config_*
    // (the Mini-PC's checker saw rtw89.core -> pci.cfg/pci.ecam at every boot).
    LockClass::fixed("rtw89.core", DRIVER_OUTER, false),
];

/// Prefix rules for names not in `TABLE` (longest prefix wins).
pub(crate) static PREFIXES: &[(&str, u16)] = &[("klog.", KLOG), ("console.", SINK_TARGETS), ("display.", SINK_TARGETS)];

/// Level of a name outside `TABLE`, and whether a prefix rule decided it.
pub(crate) fn level_of(name: &str) -> (u16, bool) {
    let mut best: Option<(usize, u16)> = None;
    for &(p, l) in PREFIXES {
        if name.starts_with(p) && best.is_none_or(|(n, _)| p.len() > n) {
            best = Some((p.len(), l));
        }
    }
    match best {
        Some((_, l)) => (l, true),
        None => (SUBSYSTEM, false),
    }
}

/// Short name of a level for reports.
pub fn level_name(l: u16) -> &'static str {
    match l {
        SND_OUTPUT => "request",
        DRIVER_OUTER => "driver",
        PCI_ENABLE..=PCI_ECAM => "pci",
        SUBSYSTEM => "subsystem",
        WAITQUEUE => "waitqueue",
        LEAF => "leaf",
        SCHED => "sched",
        KLOG_SINKS..=KLOG => "klog",
        ALLOC..=FRAMES_MODULES => "alloc",
        _ => "other",
    }
}

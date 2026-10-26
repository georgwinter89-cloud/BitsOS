//! Lock classes: every lock belongs to the class of its name (`SpinLock::named("pci.cfg", ..)`); all locks of
//! one name share a class, its place in the lock order (`order.rs`) and its statistics. Classes of the order
//! table are static; any other name gets a node on first use, allocated only when that is safe (no allocator
//! lock held on this CPU, not inside ksync's own bookkeeping). Until then the lock counts under `(pending)`
//! and tries again next time. Nodes are never freed (one per distinct name; no maximum, R1).

use crate::cpu::CpuSync;
use crate::order;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

/// Counters of one class. Times are TSC ticks (converted with `set_tsc_hz` when reported).
pub(crate) struct Stats {
    pub acquired: AtomicU64,
    pub contended: AtomicU64,
    pub wait_ticks: AtomicU64,
    pub wait_max: AtomicU64,
    pub hold_ticks: AtomicU64,
    pub hold_max: AtomicU64,
    pub try_failed: AtomicU64,
    pub timeouts: AtomicU64,
}

impl Stats {
    const fn new() -> Self {
        Stats {
            acquired: AtomicU64::new(0),
            contended: AtomicU64::new(0),
            wait_ticks: AtomicU64::new(0),
            wait_max: AtomicU64::new(0),
            hold_ticks: AtomicU64::new(0),
            hold_max: AtomicU64::new(0),
            try_failed: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
        }
    }
}

pub struct LockClass {
    pub(crate) name: &'static str,
    pub(crate) level: u16,
    /// A lock the allocator takes (`order.rs`): while one is held on a CPU ksync does not allocate there.
    pub(crate) alloc: bool,
    /// Not checked by the order checker (unnamed locks, `(pending)`).
    pub(crate) unchecked: bool,
    next: AtomicPtr<LockClass>,
    pub(crate) stats: Stats,
    /// Edges "taken while holding this class" between classes the table leaves unordered (debug builds).
    #[cfg(debug_assertions)]
    pub(crate) edges: AtomicPtr<crate::lockdep::imp::Edge>,
}

impl LockClass {
    pub(crate) const fn fixed(name: &'static str, level: u16, alloc: bool) -> Self {
        LockClass {
            name,
            level,
            alloc,
            unchecked: false,
            next: AtomicPtr::new(core::ptr::null_mut()),
            stats: Stats::new(),
            #[cfg(debug_assertions)]
            edges: AtomicPtr::new(core::ptr::null_mut()),
        }
    }
    const fn unchecked(name: &'static str) -> Self {
        let mut c = Self::fixed(name, order::SUBSYSTEM, false);
        c.unchecked = true;
        c
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
    pub fn level(&self) -> u16 {
        self.level
    }
}

/// Locks made with `new` (no name): counted together, never order-checked.
pub(crate) static UNNAMED: LockClass = LockClass::unchecked("(unnamed)");
/// A named lock whose class could not be created yet (no allocation possible at that moment).
pub(crate) static PENDING: LockClass = LockClass::unchecked("(pending)");

/// Classes created at run time (names outside the order table).
static DYNAMIC: AtomicPtr<LockClass> = AtomicPtr::new(core::ptr::null_mut());

fn find_dynamic(name: &str, mut p: *mut LockClass, stop: *mut LockClass) -> Option<&'static LockClass> {
    while !p.is_null() && p != stop {
        // SAFETY: nodes are fully built before they are published (Release) and never freed or changed after.
        let c = unsafe { &*p };
        if c.name == name {
            return Some(c);
        }
        p = c.next.load(Ordering::Acquire);
    }
    None
}

fn lookup(name: &str) -> Option<&'static LockClass> {
    if name.is_empty() {
        return Some(&UNNAMED);
    }
    if let Some(c) = order::TABLE.iter().find(|c| c.name == name) {
        return Some(c);
    }
    find_dynamic(name, DYNAMIC.load(Ordering::Acquire), core::ptr::null_mut())
}

/// The class of `name`, creating it if needed and allowed. Called inside the bookkeeping section of `cpu`.
pub(crate) fn resolve(name: &'static str, cpu: &CpuSync) -> Option<&'static LockClass> {
    if let Some(c) = lookup(name) {
        return Some(c);
    }
    if !cpu.may_alloc() {
        return None;
    }
    let layout = core::alloc::Layout::new::<LockClass>();
    // SAFETY: a non-zero-sized layout. A null result (no heap yet, out of memory) is handled below; we never
    // call `handle_alloc_error`, which would panic inside a lock operation.
    let p = unsafe { alloc::alloc::alloc(layout) } as *mut LockClass;
    if p.is_null() {
        return None;
    }
    let (level, _) = order::level_of(name);
    // SAFETY: `p` is a fresh allocation of the right layout, not yet visible to anyone.
    unsafe { p.write(LockClass::fixed(name, level, false)) };
    let mut head = DYNAMIC.load(Ordering::Acquire);
    let mut seen = core::ptr::null_mut();
    loop {
        // Another CPU may have added the same name since we looked: use theirs.
        if let Some(c) = find_dynamic(name, head, seen) {
            // SAFETY: ours was never published; give the memory back.
            unsafe { alloc::alloc::dealloc(p as *mut u8, layout) };
            return Some(c);
        }
        // SAFETY: still unpublished, we are the only writer.
        unsafe { (*p).next.store(head, Ordering::Relaxed) };
        match DYNAMIC.compare_exchange(head, p, Ordering::AcqRel, Ordering::Acquire) {
            // SAFETY: published now and never freed: a 'static shared reference from here on.
            Ok(_) => return Some(unsafe { &*p }),
            Err(h) => {
                seen = head;
                head = h;
            }
        }
    }
}

/// Every class: the order table first, then the ones made at run time, then `(unnamed)` and `(pending)`.
pub(crate) fn for_each(mut f: impl FnMut(&'static LockClass)) {
    for c in order::TABLE.iter() {
        f(c);
    }
    let mut p = DYNAMIC.load(Ordering::Acquire);
    while !p.is_null() {
        // SAFETY: as in `find_dynamic`.
        let c = unsafe { &*p };
        f(c);
        p = c.next.load(Ordering::Acquire);
    }
    f(&UNNAMED);
    f(&PENDING);
}

/// The class a lock instance caches after its first resolution.
pub(crate) struct ClassCache(AtomicPtr<LockClass>);

impl ClassCache {
    pub const fn new() -> Self {
        ClassCache(AtomicPtr::new(core::ptr::null_mut()))
    }
    pub fn get(&self) -> Option<&'static LockClass> {
        let p = self.0.load(Ordering::Acquire);
        // SAFETY: only `set` stores here, always a `&'static LockClass`.
        (!p.is_null()).then(|| unsafe { &*p })
    }
    pub fn set(&self, c: &'static LockClass) {
        self.0.store(c as *const LockClass as *mut LockClass, Ordering::Release);
    }
}

//! Lock-free table of event keys `(subsystem, name)` (RB-7a): the rate limiter's state per key, and the names
//! the trace rings refer to by number (lock names, `trace::mark`).
//!
//! A key is identified by the addresses and lengths of its two `&'static str` (one key per call site, like
//! Linux's `printk_ratelimited`), so a lookup never compares text. The emit path only reads and claims slots
//! (atomics, never allocates, never waits): a static first table, then a chain of tables that klogd adds
//! (`grow`) when the chain is half full (R1: no fixed limit). A key that finds no slot uses the shared overflow
//! slot (`OVERFLOW`, counted in `klog.keys.overflow`).

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use ksync::Once;

/// Slots probed per table before trying the next one.
const PROBE: usize = 16;
const FIRST_LEN: usize = 512;

pub(crate) struct Key {
    /// Hash of the key; 0 = free.
    id: AtomicU64,
    names: Once<(&'static str, &'static str)>,
    /// Names fixed at compile time (the overflow slot): never initialised at run time, so nobody waits on them.
    fixed: Option<(&'static str, &'static str)>,
    /// Rate limiter: theoretical arrival time (GCRA, `ratelimit`).
    pub(crate) tat: AtomicU64,
    /// Records suppressed since klogd last reported them.
    pub(crate) suppressed: AtomicU64,
}

impl Key {
    const fn new() -> Self {
        Key { id: AtomicU64::new(0), names: Once::new(), fixed: None, tat: AtomicU64::new(0), suppressed: AtomicU64::new(0) }
    }
    const fn fixed(sub: &'static str, name: &'static str) -> Self {
        Key { fixed: Some((sub, name)), ..Key::new() }
    }
    /// `(subsystem, name)`; `None` for a moment while the claimer stores them.
    pub(crate) fn names(&self) -> Option<(&'static str, &'static str)> {
        self.fixed.or_else(|| self.names.get().copied())
    }
}

struct More {
    keys: Box<[Key]>,
    used: AtomicUsize,
    next: Once<Box<More>>,
}

/// A chain of tables: a first table, then the tables `grow` adds (each twice the last). The kernel has one
/// (`KEYS`); host tests build small ones to fill them.
pub(crate) struct Chain {
    first: &'static [Key],
    used: AtomicUsize,
    more: Once<Box<More>>,
}

static FIRST: [Key; FIRST_LEN] = [const { Key::new() }; FIRST_LEN];
static KEYS: Chain = Chain::new(&FIRST);
pub(crate) static OVERFLOW: Key = Key::fixed("klog", "keys.overflow");
counter!(OVERFLOWS, "klog.keys.overflow");

/// Index of the overflow slot (`get` resolves it).
pub(crate) const OVERFLOW_ID: u32 = u32::MAX;

fn hash(sub: &'static str, name: &'static str) -> u64 {
    // splitmix64 finaliser over the four words; 0 is reserved for free slots.
    let mut x = (sub.as_ptr() as u64) ^ (name.as_ptr() as u64).rotate_left(29) ^ ((sub.len() as u64) << 48) ^ ((name.len() as u64) << 32);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    if x == 0 { 1 } else { x }
}

/// Probes `keys` for `h`; claims a free slot on the way. `Some(index)` found or claimed.
fn probe(keys: &[Key], used: &AtomicUsize, h: u64, sub: &'static str, name: &'static str) -> Option<usize> {
    let n = keys.len();
    let start = (h as usize) % n;
    for i in 0..PROBE.min(n) {
        let at = (start + i) % n;
        let k = &keys[at];
        let id = k.id.load(Ordering::Acquire);
        if id == h {
            return Some(at);
        }
        if id == 0 {
            match k.id.compare_exchange(0, h, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    // Only the claimer initialises, so nobody ever spins on `names` (an interrupt on this CPU
                    // that looks up the same key finds `id` set and skips the names).
                    k.names.get_or_init(|| (sub, name));
                    used.fetch_add(1, Ordering::Relaxed);
                    return Some(at);
                }
                Err(other) if other == h => return Some(at),
                Err(_) => {}
            }
        }
    }
    None
}

impl Chain {
    pub(crate) const fn new(first: &'static [Key]) -> Chain {
        Chain { first, used: AtomicUsize::new(0), more: Once::new() }
    }

    /// A host test's chain with a first table of `len` slots.
    #[cfg(test)]
    pub(crate) fn leaked(len: usize) -> &'static Chain {
        let first: &'static [Key] = Box::leak((0..len).map(|_| Key::new()).collect::<Box<[Key]>>());
        Box::leak(Box::new(Chain::new(first)))
    }

    /// The key's index and slot; `None` when every table is full. Never allocates, never waits.
    pub(crate) fn find(&'static self, sub: &'static str, name: &'static str) -> Option<(u32, &'static Key)> {
        let h = hash(sub, name);
        if let Some(i) = probe(self.first, &self.used, h, sub, name) {
            return Some((i as u32, &self.first[i]));
        }
        let mut base = self.first.len();
        let mut next = self.more.get();
        while let Some(m) = next {
            if let Some(i) = probe(&m.keys, &m.used, h, sub, name) {
                return Some(((base + i) as u32, &m.keys[i]));
            }
            base += m.keys.len();
            next = m.next.get();
        }
        None
    }

    /// The key's index and slot, or the overflow slot. Never allocates, never waits.
    pub(crate) fn lookup(&'static self, sub: &'static str, name: &'static str) -> (u32, &'static Key) {
        self.find(sub, name).unwrap_or_else(|| {
            OVERFLOWS.inc();
            (OVERFLOW_ID, &OVERFLOW)
        })
    }

    pub(crate) fn get(&'static self, id: u32) -> Option<&'static Key> {
        if id == OVERFLOW_ID {
            return Some(&OVERFLOW);
        }
        let mut i = id as usize;
        if i < self.first.len() {
            return Some(&self.first[i]);
        }
        i -= self.first.len();
        let mut next = self.more.get();
        while let Some(m) = next {
            if i < m.keys.len() {
                return Some(&m.keys[i]);
            }
            i -= m.keys.len();
            next = m.next.get();
        }
        None
    }

    /// Every claimed slot, the overflow slot last.
    pub(crate) fn for_each(&'static self, mut f: impl FnMut(&'static Key)) {
        self.first.iter().filter(|k| k.id.load(Ordering::Acquire) != 0).for_each(&mut f);
        let mut next = self.more.get();
        while let Some(m) = next {
            m.keys.iter().filter(|k| k.id.load(Ordering::Acquire) != 0).for_each(&mut f);
            next = m.next.get();
        }
        f(&OVERFLOW);
    }

    pub(crate) fn usage(&self) -> (usize, usize) {
        let (mut used, mut cap) = (self.used.load(Ordering::Relaxed), self.first.len());
        let mut next = self.more.get();
        while let Some(m) = next {
            used += m.used.load(Ordering::Relaxed);
            cap += m.keys.len();
            next = m.next.get();
        }
        (used, cap)
    }

    /// Thread context, may allocate: adds a table of twice the last size when the chain is half full.
    /// `Some((used, slots))` if it added one.
    pub(crate) fn grow(&'static self) -> Option<(usize, usize)> {
        let (used, cap) = self.usage();
        if used * 2 < cap {
            return None;
        }
        let mut last_len = self.first.len();
        let mut slot = &self.more;
        while let Some(m) = slot.get() {
            last_len = m.keys.len();
            slot = &m.next;
        }
        let len = last_len * 2;
        slot.get_or_init(|| Box::new(More { keys: (0..len).map(|_| Key::new()).collect(), used: AtomicUsize::new(0), next: Once::new() }));
        Some((used, cap + len))
    }
}

/// The key's index and slot (the overflow slot if every table is full). Never allocates, never waits.
pub(crate) fn lookup(sub: &'static str, name: &'static str) -> (u32, &'static Key) {
    KEYS.lookup(sub, name)
}

/// The slot with index `id` (from `lookup`).
pub(crate) fn get(id: u32) -> Option<&'static Key> {
    KEYS.get(id)
}

/// Every claimed slot (klogd's sweep for suppressed records), the overflow slot last.
pub(crate) fn for_each(f: impl FnMut(&'static Key)) {
    KEYS.for_each(f)
}

/// (slots used, slots in all tables).
pub fn usage() -> (usize, usize) {
    KEYS.usage()
}

/// klogd (thread context, may allocate): adds a table of twice the last size when the chain is half full, so
/// probes stay short and new keys keep finding a slot. True if it added one.
pub fn grow() -> bool {
    match KEYS.grow() {
        Some((used, slots)) => {
            event!(Info, "klog", "keys.grown", slots = slots, used = used);
            true
        }
        None => false,
    }
}

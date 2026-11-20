//! Who did it: the return addresses above a function, and bounded tables of call chains worth naming afterwards
//! (who took the HighAtomic reserve, `buddy::highatomic`; who asked the heap for several pages where it may not
//! reclaim, `heap::atomic`).
//!
//! 12.11.2026: counters said the reserve was taken 285 times in a minute and nothing said by whom; the size of a
//! refused heap request alone had not named its caller either (11.11., `report::heap_alloc_fail`).
//!
//! A table is written from any context (the allocator's cold paths: under the zone lock, the heap's locks, in an
//! interrupt handler): a walk of the stack's frame records, one hash, a few atomics, no lock, no allocation. Each
//! distinct chain gets one slot; when the table is full, further chains are only counted (`dropped`). Events are
//! made of it by `report::flush_reports` (the `mm.report` thread), a chain's first time and each time its count
//! doubled, so a site that takes a million times costs twenty lines.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Return addresses a table keeps per chain: enough to get past the allocator's own frames to its caller's
/// callers.
pub const DEPTH: usize = 16;

/// The boot thread's stack, [bottom, top): hal does not know it (the kernel's own static, `set_boot_stack`).
static BOOT_STACK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Tells where the boot thread's stack lies (the kernel, once, at its entry). Without it a chain on the boot
/// thread reads as empty: the in-kernel tests run there, and so does most of the boot.
pub fn set_boot_stack(bottom: u64, top: u64) {
    BOOT_STACK[0].store(bottom, Relaxed);
    BOOT_STACK[1].store(top, Relaxed);
}

/// The return addresses above this function, innermost first, as far as the frame records on this thread's stack
/// lead (the kernel is built with frame pointers).
#[cfg(target_os = "none")]
#[inline(never)]
pub(crate) fn callers<const N: usize>() -> [u64; N] {
    let mut out = [0u64; N];
    let mut st = hal::switch::current_stack();
    if st.top == 0 {
        // The boot thread (`hal::switch::current_stack`: zeros for it).
        (st.bottom, st.top) = (BOOT_STACK[0].load(Relaxed), BOOT_STACK[1].load(Relaxed));
    }
    let mut fp: u64;
    // SAFETY: reads a register, nothing else.
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack, preserves_flags)) };
    for slot in &mut out {
        if fp < st.bottom || fp % 8 != 0 || fp.checked_add(16).is_none_or(|end| end > st.top) {
            break;
        }
        // SAFETY: `fp` is 8-aligned and lies inside this thread's mapped kernel stack with 16 bytes below its top:
        // two readable stack words. In a function with a frame record they are the caller's frame pointer and the
        // return address; otherwise plain words, which the bounds check of the next round judges.
        let (next, ret) = unsafe { ((fp as *const u64).read(), ((fp + 8) as *const u64).read()) };
        *slot = ret;
        if next <= fp {
            break;
        }
        fp = next;
    }
    out
}

#[cfg(not(target_os = "none"))]
pub(crate) fn callers<const N: usize>() -> [u64; N] {
    [0; N]
}

struct Slot {
    /// 0: free; else the chain's hash with bit 0 set.
    key: AtomicU64,
    count: AtomicU64,
    amount: AtomicU64,
    info: AtomicU64,
    /// `count` when an event last told of this chain.
    told: AtomicU64,
    chain: [AtomicU64; DEPTH],
}

impl Slot {
    const fn new() -> Slot {
        Slot { key: AtomicU64::new(0), count: AtomicU64::new(0), amount: AtomicU64::new(0), info: AtomicU64::new(0), told: AtomicU64::new(0), chain: [const { AtomicU64::new(0) }; DEPTH] }
    }
}

/// One chain of a table, copied out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    /// Times the chain was noted.
    pub count: u64,
    /// The sum of what it was noted with (frames taken, bytes asked for).
    pub amount: u64,
    /// The last note's detail (the table's owner says what it is).
    pub info: u64,
    /// Return addresses, innermost first; 0 past the end.
    pub chain: [u64; DEPTH],
}

/// `N` distinct call chains with their counts.
pub struct CallerTable<const N: usize> {
    slots: [Slot; N],
    dropped: AtomicU64,
    dropped_told: AtomicU64,
}

impl<const N: usize> CallerTable<N> {
    pub const fn new() -> Self {
        CallerTable { slots: [const { Slot::new() }; N], dropped: AtomicU64::new(0), dropped_told: AtomicU64::new(0) }
    }

    /// Counts the calling chain once, with `amount` and the detail `info`. True: the chain is new to the table.
    #[inline(never)]
    pub fn note(&self, info: u64, amount: u64) -> bool {
        self.note_chain(&callers::<DEPTH>(), info, amount)
    }

    /// `note` for a given chain.
    pub fn note_chain(&self, chain: &[u64; DEPTH], info: u64, amount: u64) -> bool {
        // FNV-1a over the addresses; bit 0 marks a taken slot.
        let hash = chain.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, a| (h ^ a).wrapping_mul(0x0000_0100_0000_01b3));
        let key = hash | 1;
        for i in 0..N {
            let s = &self.slots[((hash >> 8) as usize).wrapping_add(i) % N];
            let fresh = match s.key.compare_exchange(0, key, Relaxed, Relaxed) {
                Ok(_) => {
                    for (to, from) in s.chain.iter().zip(chain) {
                        to.store(*from, Relaxed);
                    }
                    true
                }
                Err(k) if k == key => false,
                Err(_) => continue,
            };
            s.info.store(info, Relaxed);
            s.amount.fetch_add(amount, Relaxed);
            s.count.fetch_add(1, Relaxed);
            return fresh;
        }
        self.dropped.fetch_add(1, Relaxed);
        false
    }

    /// Notes that found the table full of other chains.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Relaxed)
    }

    /// The notes dropped since this was last asked (for a counter that says the list is not whole).
    pub(crate) fn dropped_news(&self) -> u64 {
        let now = self.dropped.load(Relaxed);
        now - self.dropped_told.swap(now, Relaxed)
    }

    fn site(s: &Slot) -> Option<Site> {
        let count = s.count.load(Relaxed);
        (s.key.load(Relaxed) != 0 && count > 0).then(|| Site { count, amount: s.amount.load(Relaxed), info: s.info.load(Relaxed), chain: core::array::from_fn(|i| s.chain[i].load(Relaxed)) })
    }

    /// Every chain noted so far (any order).
    pub fn each(&self, f: &mut dyn FnMut(Site)) {
        self.slots.iter().filter_map(Self::site).for_each(f);
    }

    /// The chains that are new, or whose count doubled, since this last told of them.
    pub fn each_news(&self, f: &mut dyn FnMut(Site)) {
        for s in &self.slots {
            let Some(site) = Self::site(s) else { continue };
            let told = s.told.load(Relaxed);
            if site.count >= told.saturating_mul(2).max(1) && s.told.compare_exchange(told, site.count, Relaxed, Relaxed).is_ok() {
                f(site);
            }
        }
    }
}

impl<const N: usize> Default for CallerTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Who took from the boot allocator's HighAtomic reserve (`buddy::highatomic`). A site's amount: frames taken; its
/// detail: order | kind << 8 | last try << 16 of the last take.
pub static RESERVE_TAKERS: CallerTable<64> = CallerTable::new();

/// Who asked the kernel heap for two pages or more where it may not reclaim (`heap::atomic`). A site's amount:
/// bytes asked for in all; its detail: the last size, and above bit 56 why (1: a spin lock or preemption off,
/// 2: an interrupt handler, 4: a grace section, 8: interrupts off).
pub static HEAP_ATOMIC_LARGE: CallerTable<256> = CallerTable::new();

/// One event for `$site` with its chain as c0..c15 (the caller holds no allocator lock: `flush_reports`).
macro_rules! site_event {
    ($lvl:ident, $name:expr, $site:expr $(, $k:ident = $v:expr)*; $($fmt:tt)+) => {{
        let s: $crate::callers::Site = $site;
        let c = |i: usize| klog::Hex(s.chain[i]);
        klog::event!($lvl, "mm", $name, count = s.count $(, $k = $v)*,
            c0 = c(0), c1 = c(1), c2 = c(2), c3 = c(3), c4 = c(4), c5 = c(5), c6 = c(6), c7 = c(7),
            c8 = c(8), c9 = c(9), c10 = c(10), c11 = c(11), c12 = c(12), c13 = c(13), c14 = c(14), c15 = c(15); $($fmt)+);
    }};
}
pub(crate) use site_event;

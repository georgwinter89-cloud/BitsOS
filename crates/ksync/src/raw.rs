//! What every lock kind shares: its name, its class (resolved once), and the bookkeeping around taking and
//! releasing it (statistics always, the order checker in debug builds, the per-CPU allocator-lock count).
//!
//! Order of events for one lock: `before` (resolve, check the order, count an allocator lock) → the lock word
//! is taken → `acquired` (statistics, held list) → ... → `release` (statistics, held list, then the word). The allocator
//! count goes up before the lock is taken and down after it is released, so while an allocator lock is held on
//! this CPU ksync never allocates there (no self-deadlock, also not from an interrupt handler).
//!
//! Every visit to the per-CPU record runs with interrupts off (`IrqOff`), so it cannot move to another CPU
//! halfway. The allocator count additionally needs the lock holder to stay on its CPU while it holds the
//! lock: allocator-class locks are spin locks (`order.rs`), and SC-3's preemption must not preempt a thread
//! that holds a spin lock (as Linux's `spin_lock` disables preemption).

use crate::class::{ClassCache, LockClass, PENDING};
use crate::cpu::{IrqOff, with_cpu};
use crate::lockdep::LockKind;
use core::sync::atomic::Ordering;

pub(crate) struct Meta {
    name: &'static str,
    class: ClassCache,
}

/// What a guard keeps about its acquisition.
#[derive(Clone, Copy)]
pub(crate) struct Acq {
    class: &'static LockClass,
    t0: u64,
    #[cfg(debug_assertions)]
    tracked: bool,
}

fn ticks() -> u64 {
    hal::cpu::rdtsc()
}

impl Meta {
    pub const fn new(name: &'static str) -> Self {
        Meta { name, class: ClassCache::new() }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The class (resolving it if possible), without any check.
    pub fn class(&self) -> &'static LockClass {
        if let Some(c) = self.class.get() {
            return c;
        }
        let _irq = IrqOff::new();
        with_cpu(|cpu| {
            let Some(cpu) = cpu else { return &PENDING };
            if !cpu.enter() {
                return &PENDING;
            }
            let c = self.resolve_in(cpu);
            cpu.leave();
            c
        })
    }

    fn resolve_in(&self, cpu: &crate::cpu::CpuSync) -> &'static LockClass {
        match crate::class::resolve(self.name, cpu) {
            Some(c) => {
                self.class.set(c);
                c
            }
            None => &PENDING,
        }
    }

    /// Before a lock that may wait: class, order check (debug), allocator count.
    pub fn before(&self, addr: usize, kind: LockKind) -> &'static LockClass {
        let cached = self.class.get();
        // Release builds: a known class that is not an allocator lock needs no per-CPU visit at all.
        #[cfg(not(debug_assertions))]
        if let Some(c) = cached
            && !c.alloc
        {
            let _ = (addr, kind);
            return c;
        }
        let _irq = IrqOff::new();
        let c = with_cpu(|cpu| {
            let Some(cpu) = cpu else { return cached.unwrap_or(&PENDING) };
            #[cfg(not(debug_assertions))]
            if let Some(c) = cached {
                Self::count_alloc(cpu, c);
                return c;
            }
            if !cpu.enter() {
                // Nested in ksync's own bookkeeping (the allocator it called, or an NMI): untracked.
                let c = cached.unwrap_or(&PENDING);
                Self::count_alloc(cpu, c);
                return c;
            }
            let c = match cached {
                Some(c) => c,
                None => self.resolve_in(cpu),
            };
            #[cfg(debug_assertions)]
            crate::lockdep::imp::check(cpu, c, addr, kind);
            Self::count_alloc(cpu, c);
            cpu.leave();
            c
        });
        let _ = (addr, kind);
        c
    }

    /// Before a `try_lock`: class and allocator count, no order check (a try cannot deadlock).
    pub fn before_try(&self) -> &'static LockClass {
        let c = self.class();
        if c.alloc {
            let _irq = IrqOff::new();
            with_cpu(|cpu| {
                if let Some(cpu) = cpu {
                    Self::count_alloc(cpu, c);
                }
            });
        }
        c
    }

    fn count_alloc(cpu: &crate::cpu::CpuSync, c: &LockClass) {
        if c.alloc {
            cpu.alloc_held.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn uncount_alloc(c: &LockClass) {
        if c.alloc {
            let _irq = IrqOff::new();
            with_cpu(|cpu| {
                if let Some(cpu) = cpu {
                    // Never below 0 (a release on a CPU without the matching count must not make that CPU
                    // look like it holds an allocator lock forever).
                    crate::cpu::update(&cpu.alloc_held, |n| n.saturating_sub(1));
                }
            });
        }
    }

    /// The lock was not taken after `before`/`before_try` (try failed, timeout).
    pub fn abandoned(&self, c: &'static LockClass, timeout: bool) {
        if timeout {
            c.stats.timeouts.fetch_add(1, Ordering::Relaxed);
        } else {
            c.stats.try_failed.fetch_add(1, Ordering::Relaxed);
        }
        Self::uncount_alloc(c);
    }

    /// The lock is taken; `wait_ticks` is Some when it had to wait.
    pub fn acquired(&self, c: &'static LockClass, addr: usize, kind: LockKind, wait_ticks: Option<u64>) -> Acq {
        let s = &c.stats;
        s.acquired.fetch_add(1, Ordering::Relaxed);
        if let Some(w) = wait_ticks {
            s.contended.fetch_add(1, Ordering::Relaxed);
            s.wait_ticks.fetch_add(w, Ordering::Relaxed);
            s.wait_max.fetch_max(w, Ordering::Relaxed);
            // RB-7a: the lock-wait trace (klog::trace::lock_wait takes no lock and never spins).
            if let Some(h) = SLOW_WAIT_HOOK.get() {
                h(c.name(), addr, w);
            }
        }
        #[cfg(debug_assertions)]
        let tracked = crate::lockdep::imp::tracked_push(c, addr, kind);
        let _ = (addr, kind);
        Acq {
            class: c,
            t0: ticks(),
            #[cfg(debug_assertions)]
            tracked,
        }
    }

    /// Releases: statistics and the held list first (so the list never shows a lock that is free), then
    /// `unlock` stores the lock word, then the allocator count goes down.
    pub fn release(&self, a: Acq, addr: usize, unlock: impl FnOnce()) {
        let held = ticks().wrapping_sub(a.t0);
        let s = &a.class.stats;
        s.hold_ticks.fetch_add(held, Ordering::Relaxed);
        s.hold_max.fetch_max(held, Ordering::Relaxed);
        #[cfg(debug_assertions)]
        if a.tracked {
            crate::lockdep::imp::tracked_pop(addr);
        }
        let _ = addr;
        unlock();
        Self::uncount_alloc(a.class);
    }
}

/// Starts a wait clock for a contended acquisition.
pub(crate) fn wait_start() -> u64 {
    ticks()
}

pub(crate) fn wait_since(t0: u64) -> u64 {
    ticks().wrapping_sub(t0)
}

/// RB-7a: called with (lock name, lock address, wait in TSC ticks) after every contended acquire.
static SLOW_WAIT_HOOK: crate::Once<fn(&'static str, usize, u64)> = crate::Once::new();

/// Installs the contended-acquire hook once (klogd: `klog::trace::lock_wait`); later calls keep the first.
pub fn set_slow_wait_hook(f: fn(&'static str, usize, u64)) {
    SLOW_WAIT_HOOK.get_or_init(|| f);
}

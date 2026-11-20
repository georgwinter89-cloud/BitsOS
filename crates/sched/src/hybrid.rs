//! Where work runs on a hybrid CPU (GP-CPU2; pure, host-tested): how busy a thread is, which core kind it wants,
//! and when a running thread should move up to a faster core.
//!
//! What Linux 7.3.1 does on the 265K: intel_pstate gives every CPU its capacity from its HWP highest performance and,
//! because the CPU is hybrid without SMT, turns ITMT off and lets the scheduler use asymmetric CPU capacity instead
//! (drivers/cpufreq/intel_pstate.c:1095-1124, :989-1062; "Hybrid CPU capacity scaling enabled" in this host's journal).
//! EAS stays off there: it needs the schedutil governor (drivers/cpufreq/cpufreq.c:3071-3084, kernel/sched/topology.c:
//! 217-262) and intel_pstate runs in active mode ("powersave" here). Thread Director (HFI, intel_hfi.c) only feeds the
//! thermal netlink and capacity updates, not the wake-up choice, so it is left out. What decides then is the task's
//! utilisation against a core's capacity: `fits_capacity` (kernel/sched/fair.c:110, 80 % margin), wake-up on a fitting
//! idle core (`select_idle_sibling` :9181-9240 with `asym_fits_cpu` :9153, `select_idle_capacity` :9037), and a
//! running task that no longer fits is a "misfit" that an idle bigger core pulls (`update_misfit_status` :6340-6365,
//! `capacity_greater` :118).
//!
//! BitsOS's shape: a thread's utilisation is kept in one step per wake-up (`util_next`, PELT's 32 ms half-life as a
//! time constant), its class decides as before where it is clear (Interactive and KernelRt on P cores, Background on
//! E cores), and a Normal thread is placed by its utilisation: busy on P, light on E once its previous core is taken.
//! The favoured cores (highest HWP number, the ITMT ranking) come first among P cores: `place` orders by capacity.

use crate::Class;
use crate::place::Pref;

/// Linux's `SCHED_CAPACITY_SCALE`: a thread that runs all the time.
pub const UTIL_MAX: u32 = 1024;
/// PELT's period (Linux: 1024 us; here 1 ms) and its half-life in periods (`LOAD_AVG_PERIOD`).
pub const PERIOD_NS: u64 = 1_000_000;
const HALF_LIFE: u64 = 32;
/// A running thread must have run this long on its core before it counts as misfit (Linux checks at the tick).
pub const MISFIT_MIN_RUN_NS: u64 = 4_000_000;

/// y^n for n < 32 with y^32 = 1/2, scaled by 2^32 (linux-7.3.1/kernel/sched/sched-pelt.h:5-12).
const Y_INV: [u32; 32] = [
    0xffffffff, 0xfa83b2da, 0xf5257d14, 0xefe4b99a, 0xeac0c6e6, 0xe5b906e6, 0xe0ccdeeb, 0xdbfbb796, 0xd744fcc9, 0xd2a81d91, 0xce248c14,
    0xc9b9bd85, 0xc5672a10, 0xc12c4cc9, 0xbd08a39e, 0xb8fbaf46, 0xb504f333, 0xb123f581, 0xad583ee9, 0xa9a15ab4, 0xa5fed6a9, 0xa2704302,
    0x9ef5325f, 0x9b8d39b9, 0x9837f050, 0x94f4efa8, 0x91c3d373, 0x8ea4398a, 0x8b95c1e3, 0x88980e80, 0x85aac367, 0x82cd8698,
];

/// `val * y^n` (Linux `decay_load`, kernel/sched/pelt.c:32-56).
pub fn decay(val: u32, n: u64) -> u32 {
    if n > HALF_LIFE * 63 {
        return 0;
    }
    let v = u64::from(val) >> (n / HALF_LIFE);
    ((v * u64::from(Y_INV[(n % HALF_LIFE) as usize])) >> 32) as u32
}

/// The utilisation after a window of `window_ns` in which the thread ran `busy_ns` (pure): the old value decays by
/// PELT's y per period, the window adds its busy share with the weight the decay took away (busy spread evenly over
/// the window, which PELT's per-period sums do not assume; close enough for a placement hint).
pub fn util_next(old: u32, busy_ns: u64, window_ns: u64) -> u32 {
    let n = window_ns / PERIOD_NS;
    let share = (u128::from(busy_ns.min(window_ns)) * u128::from(UTIL_MAX) / u128::from(window_ns.max(1))) as u32;
    (decay(old, n) + share - decay(share, n)).min(UTIL_MAX)
}

/// Linux `fits_capacity(cap, max)`: `util` leaves 20 % of `capacity` free.
pub fn fits(util: u32, capacity: u16) -> bool {
    u64::from(util) * 1280 < u64::from(capacity) * 1024
}

/// Linux `capacity_greater(cap1, cap2)`: more than about 5 % above.
pub fn capacity_greater(a: u16, b: u16) -> bool {
    u64::from(a) * 1024 > u64::from(b) * 1078
}

/// Which core kind a thread wants on a hybrid CPU (`smallest`: the capacity of the slowest online core).
pub fn pref_for(class: Class, util: u32, smallest: u16) -> Pref {
    match class {
        Class::KernelRt | Class::Interactive => Pref::Performance,
        Class::Background => Pref::Efficiency,
        Class::Normal if fits(util, smallest) => Pref::Light,
        Class::Normal => Pref::Performance,
    }
}

/// Whether the thread running on a core of `capacity` for `run_ns` (utilisation `util` when it started) should move to
/// a faster core: Interactive always, Normal once it no longer fits; never KernelRt (short by rule) or Background.
pub fn misfit(class: Class, util: u32, run_ns: u64, capacity: u16) -> bool {
    if run_ns < MISFIT_MIN_RUN_NS {
        return false;
    }
    match class {
        Class::Interactive => true,
        Class::Normal => !fits(util_next(util, run_ns, run_ns), capacity),
        Class::KernelRt | Class::Background => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn util_follows_pelt_roughly() {
        // Running all the time from nothing: about half after one half-life, near full after a few.
        assert!((480..=560).contains(&util_next(0, 32 * MS, 32 * MS)), "{}", util_next(0, 32 * MS, 32 * MS));
        assert!(util_next(0, 200 * MS, 200 * MS) > 820);
        // Idle for a half-life from full: about half.
        assert!((450..=560).contains(&util_next(1024, 0, 32 * MS)));
        // A 25 % duty cycle settles near 256.
        let mut u = 0;
        for _ in 0..100 {
            u = util_next(u, 2 * MS, 8 * MS);
        }
        assert!((240..=272).contains(&u), "{u}");
        assert_eq!(util_next(1024, 10 * MS, MS), 1024, "busy is clamped to the window, the result to the scale");
    }

    #[test]
    fn fits_and_greater_are_linuxs() {
        // The 265K's E core (802; 756 on this host since 15.11.): 641 still fits, 642 not.
        assert!(fits(641, 802) && !fits(642, 802));
        assert!(capacity_greater(1012, 802) && capacity_greater(1024, 756));
        assert!(!capacity_greater(1024, 1012), "the favoured P core is not a reason to move between P cores");
    }

    #[test]
    fn preferences_by_class_and_load() {
        assert_eq!(pref_for(Class::Interactive, 0, 802), Pref::Performance);
        assert_eq!(pref_for(Class::KernelRt, 0, 802), Pref::Performance);
        assert_eq!(pref_for(Class::Background, 1024, 802), Pref::Efficiency);
        assert_eq!(pref_for(Class::Normal, 100, 802), Pref::Light);
        assert_eq!(pref_for(Class::Normal, 700, 802), Pref::Performance);
    }

    #[test]
    fn misfit_after_a_while_for_busy_or_foreground() {
        assert!(!misfit(Class::Normal, 1024, 3 * MS, 802), "too short");
        assert!(misfit(Class::Interactive, 0, 5 * MS, 802));
        assert!(!misfit(Class::Normal, 0, 5 * MS, 802), "5 ms from nothing is not busy yet");
        assert!(misfit(Class::Normal, 0, 60 * MS, 802), "60 ms without a break is");
        assert!(misfit(Class::Normal, 700, 5 * MS, 802));
        assert!(!misfit(Class::Background, 1024, 500 * MS, 802));
        assert!(!misfit(Class::KernelRt, 1024, 500 * MS, 802));
    }
}

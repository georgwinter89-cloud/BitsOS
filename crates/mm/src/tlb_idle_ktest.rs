//! MM-tlb ktests: shootdowns into idle (halted) CPUs, and how slow rounds are counted. Run them with
//! `cargo xtask ktest mm.tlb -- -smp 16` (with fewer CPUs they run too, on what is there).
//!
//! - `mm.tlb.idle_storm`: the test CPU shoots kernel pages and user pages down for a second, paced so the other
//!   CPUs go back to `hlt` between rounds; every round must be answered, the idle CPUs take the IPIs, and under a
//!   hypervisor at most a few rounds may need a resend (timing on the Mini-PC is judged from the event only).
//! - `mm.tlb.resend_several`: a CPU busy 20 ms with interrupts off makes one round resend several times (Warn
//!   class), without a give-up; a CPU busy 5 ms is `mm.tlb.resend` (one resend, Info class).

use crate::linux::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use hal::apic::{IpiDest, IpiKind};
use hal::paging::{self, tlb, PageFlags};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const RW: u64 = 3;
const SEC: u64 = 1_000_000_000;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn others() -> Vec<usize> {
    let me = hal::percpu::this_cpu();
    (0..hal::percpu::cpu_count()).filter(|&c| c != me && hal::smp::is_online(c)).collect()
}

fn counter(name: &str) -> u64 {
    klog::metrics::snapshot().find(|(n, _)| *n == name).map_or(0, |(_, v)| v)
}

fn ap_activate(root: u64) -> u64 {
    paging::activate(root);
    paging::current_root()
}

fn ap_root(_: u64) -> u64 {
    paging::current_root()
}

/// The rounds' numbers between two snapshots.
struct Delta {
    rounds: u64,
    slow: u64,
    resent_once: u64,
    several: u64,
    resends: u64,
    give_ups: u64,
    irqs: u64,
    hist: [u64; tlb::WAIT_BUCKETS],
}

fn delta(a: &tlb::TlbStats, b: &tlb::TlbStats) -> Delta {
    Delta {
        rounds: b.rounds - a.rounds,
        slow: b.slow - a.slow,
        resent_once: b.resent_once - a.resent_once,
        several: b.several - a.several,
        resends: b.resends - a.resends,
        give_ups: b.give_ups - a.give_ups,
        irqs: b.irqs - a.irqs,
        hist: core::array::from_fn(|i| b.wait_hist[i] - a.wait_hist[i]),
    }
}

fn idle_storm() -> TestResult {
    let others = others();
    if others.is_empty() {
        return ktest::skip("one CPU");
    }
    const RUN_NS: u64 = SEC;
    let root = paging::kernel_root().ok_or("kernel still on the loader's tables")?;
    let fa = mm_core::frames().ok_or("no frame allocator")?;
    // A user space loaded (not lazily) on a quarter of the other CPUs: its mprotects go to them as IPIs while
    // they sit in their idle loop; the kernel pages go to every online CPU.
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let ubase = a.sys_mmap(0, P, RW, MAP_ANONYMOUS | MAP_PRIVATE, 0).map_err(e)?;
    a.write_u64(ubase, 0x7157).map_err(e)?;
    let loaded: Vec<usize> = others.iter().copied().take(others.len().div_ceil(4)).collect();
    for &c in &loaded {
        let r = hal::smp::call_on(c, ap_activate, a.root(), time::now_ns, SEC).map_err(e)?;
        kassert!(r == a.root(), "CPU {c} did not load the space");
    }
    let kva = paging::reserve_kernel_va(P).ok_or("no reservation")?;
    let irqs0: Vec<u64> = others.iter().map(|&c| tlb::cpu_irqs(c)).collect();
    let halts0 = counter("sched.halts");
    let s0 = tlb::stats();
    let t0 = time::now_ns();
    let (mut kernel_rounds, mut user_rounds, mut lost) = (0u64, 0u64, 0u64);
    let mut ro = false;
    let mut failed = None;
    while time::now_ns() < t0 + RUN_NS {
        // Kernel: map, touch here, unmap (one shootdown to every online CPU), free only when confirmed.
        let Some(f) = mm_core::alloc_zeroed(mm_core::Kind::Other) else {
            failed = Some(String::from("no frame"));
            break;
        };
        if let Err(x) = paging::map_4k(root, kva, f, PageFlags::kernel_data(), &mut mm_core::alloc_table_frame) {
            fa.free(f, 0, mm_core::Kind::Other);
            failed = Some(format!("map_4k: {x:?}"));
            break;
        }
        match paging::unmap_4k(root, kva) {
            Some(back) if back == f => fa.free(f, 0, mm_core::Kind::Other),
            Some(back) => {
                failed = Some(format!("unmap_4k gave {back:#x}, mapped {f:#x}"));
                break;
            }
            None => lost += 1, // unconfirmed: the frame stays out of use (counted by hal)
        }
        kernel_rounds += 1;
        // User: RW <-> RO on the loaded CPUs.
        let prot = if ro { RW } else { PROT_READ };
        if let Err(x) = a.sys_mprotect(ubase, P, prot) {
            failed = Some(format!("mprotect: {x:?}"));
            break;
        }
        ro = !ro;
        user_rounds += 1;
        // Paced: the other CPUs halt again before the next pair (a burst every 16th pair).
        if !kernel_rounds.is_multiple_of(16) {
            sched::sleep_ns(300_000);
        }
    }
    let s1 = tlb::stats();
    let took: u64 = others.iter().zip(&irqs0).map(|(&c, &i)| tlb::cpu_irqs(c) - i).sum();
    let halts = counter("sched.halts") - halts0;
    let released = paging::release_kernel_va(kva, P);
    let back: Vec<u64> = loaded.iter().filter_map(|&c| hal::smp::call_on(c, ap_root, 0, time::now_ns, SEC).ok()).collect();
    drop(a);
    let d = delta(&s0, &s1);
    let hv = hal::cpu::detect().hypervisor;
    klog::event!(Info, "mm", "ktest.tlb_idle_storm", cpus = others.len() + 1, loaded = loaded.len(), kernel_rounds = kernel_rounds,
        user_rounds = user_rounds, rounds = d.rounds, slow = d.slow, resent_once = d.resent_once, several = d.several, resends = d.resends,
        give_ups = d.give_ups, irqs = d.irqs, idle_irqs = took, halts = halts, lt10us = d.hist[0], lt100us = d.hist[1],
        lt1ms = d.hist[2], lt10ms = d.hist[3], ge10ms = d.hist[4], hypervisor = hv;
        "shootdowns into idle CPUs; on the Mini-PC: resent_once/several should be 0, the histogram shows the wake-up times");
    if let Some(x) = failed {
        return Err(x);
    }
    kassert!(released, "the kernel reservation could not be released");
    kassert!(back.len() == loaded.len(), "a loaded CPU did not answer afterwards");
    kassert!(d.give_ups == 0 && lost == 0, "{} give-ups, {lost} unconfirmed kernel rounds", d.give_ups);
    kassert!(kernel_rounds >= 100 && user_rounds >= 100, "too few rounds: {kernel_rounds} kernel, {user_rounds} user");
    kassert!(d.rounds >= kernel_rounds, "{} counted rounds for {kernel_rounds} kernel shootdowns", d.rounds);
    kassert!(took >= kernel_rounds, "idle CPUs took {took} shootdown IPIs for {kernel_rounds} kernel rounds");
    kassert!(d.hist.iter().sum::<u64>() == d.rounds, "histogram {:?} does not add up to {} rounds", d.hist, d.rounds);
    if hv {
        // Generous (QEMU timing is not judged): a resend is for a lost IPI or a CPU with interrupts off, and here
        // every other CPU idles with interrupts on.
        let resent = d.resent_once + d.several;
        kassert!(resent * 10 <= d.rounds, "{resent} of {} rounds needed a resend", d.rounds);
        kassert!(d.several * 50 <= d.rounds + 100, "{} of {} rounds needed several resends", d.several, d.rounds);
    }
    Ok(())
}
ktest!("mm.tlb.idle_storm", Safe, idle_storm);

// ---------------------------------------------------------------------------------------------------------

static BUSY_NS: AtomicU64 = AtomicU64::new(0);
/// The root `busy_irq` holds (not lazily) while it spins.
static BUSY_ROOT: AtomicU64 = AtomicU64::new(0);
static BUSY_STARTED: AtomicBool = AtomicBool::new(false);
static BUSY_DONE: AtomicBool = AtomicBool::new(false);

/// Spins with interrupts off without serving requests.
fn busy_irq(_v: u8) {
    // MM-tlb3: the CPU may have switched to a kernel thread since it loaded the root and hold it only lazily,
    // which a flush no longer waits for: load it again (no-op while loaded), so the round must wait for it.
    let root = BUSY_ROOT.load(SeqCst);
    if root != 0 {
        paging::activate(root);
    }
    BUSY_STARTED.store(true, SeqCst);
    let until = time::now_ns().saturating_add(BUSY_NS.load(SeqCst));
    while time::now_ns() < until {
        core::hint::spin_loop();
    }
    BUSY_DONE.store(true, SeqCst);
}

fn wait_flag(f: &AtomicBool, what: &str) -> TestResult {
    let until = time::now_ns() + 5 * SEC;
    while !f.load(SeqCst) {
        if time::now_ns() > until {
            return Err(format!("{what}: timed out"));
        }
        sched::yield_now();
    }
    Ok(())
}

fn resend_several() -> TestResult {
    let Some(&c) = others().first() else { return ktest::skip("one CPU") };
    const BUSY: u64 = 20_000_000;
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(e)?;
    let base = a.sys_mmap(0, P, RW, MAP_ANONYMOUS | MAP_PRIVATE, 0).map_err(e)?;
    a.write_u64(base, 1).map_err(e)?;
    kassert!(hal::smp::call_on(c, ap_activate, a.root(), time::now_ns, SEC).map_err(e)? == a.root());
    let v = hal::vectors::alloc(busy_irq, Some(c as u32)).map_err(e)?;
    BUSY_NS.store(BUSY, SeqCst);
    BUSY_ROOT.store(a.root(), SeqCst);
    BUSY_STARTED.store(false, SeqCst);
    BUSY_DONE.store(false, SeqCst);
    let sent = hal::smp::apic_id(c)
        .ok_or_else(|| String::from("AP not online"))
        .and_then(|apic| hal::apic::send_ipi(IpiDest::Physical(apic), IpiKind::Fixed(v.vector)).map_err(e));
    let nmi = hal::nmi::prepared();
    let samples0 = tlb::late_samples();
    let r = sent.and_then(|_| wait_flag(&BUSY_STARTED, "busy handler start")).and_then(|_| {
        // The round below waits about 19 ms: it samples the busy CPU by NMI after 8 ms.
        let _ = tlb::take_late_sample();
        tlb::allow_sample_now();
        let s0 = tlb::stats();
        let here = hal::percpu::this_cpu();
        let t0 = time::now_ns();
        let m = a.sys_mprotect(base, P, PROT_READ);
        let waited = time::now_ns() - t0;
        let s1 = tlb::stats();
        m.map_err(e)?;
        Ok((delta(&s0, &s1), waited, here == c || hal::percpu::this_cpu() == c))
    });
    let done = wait_flag(&BUSY_DONE, "busy handler end");
    hal::vectors::free(v);
    let (d, waited, on_busy) = r?;
    done?;
    drop(a);
    if on_busy {
        // This thread ran on the busy CPU itself (it may migrate since SC-3): the change came after the busy end.
        return ktest::skip("the test thread ran on the busy CPU");
    }
    klog::event!(Info, "mm", "ktest.tlb_resend_several", waited_us = waited / 1000, resends = d.resends, several = d.several,
        resent_once = d.resent_once, rounds = d.rounds);
    kassert!(d.give_ups == 0, "gave up on a CPU busy for 20 ms");
    // Resends at 2, 6 and 14 ms of the wait: at least two before the CPU is back (more if the wait began late).
    kassert!(d.resends >= 2 && d.several >= 1, "busy 20 ms: {} resends, {} rounds with several", d.resends, d.several);
    kassert!(waited >= tlb::RESEND_NS * 3, "the mprotect returned after {waited} ns while CPU {c} could not answer");
    kassert!(d.hist[3] + d.hist[4] >= 1, "no round in the 1 ms+ buckets: {:?}", d.hist);
    if !nmi {
        return ktest::skip("resends checked; no NMI sample (RB-2's NMI hook not prepared)");
    }
    kassert!(tlb::late_samples() > samples0, "a round waited {waited} ns and took no NMI sample");
    // `mm.report` may have taken the sample already (it reads once a second); then the count above is all.
    if let Some(s) = tlb::late_sample().filter(|s| s.cpu == c) {
        klog::event!(Info, "mm", "ktest.tlb_late_sample", cpu = s.cpu, rip = klog::Hex(s.rip), interrupts_on = s.interrupts_on, ntrace = s.ntrace);
        kassert!(!s.interrupts_on && !s.user, "the busy CPU spun with interrupts off in the kernel: {s:x?}");
        kassert!(s.rip >= paging::KERNEL_HALF && s.waited_ns >= tlb::SAMPLE_AFTER_NS, "sample {s:x?}");
    }
    Ok(())
}
ktest!("mm.tlb.resend_several", Safe, resend_several);

//! ktests of MM-6's rule "a root is freed only after no CPU has it loaded" against a root that was saved and
//! loaded again later (INT-A2a2). `current_root()` read while a CPU held a program's root lazily is that
//! program's root; loaded again after the program ended, it used to go into CR3 as it was, outside every CPU
//! mask, so no `leave` waited for that CPU and it ran on freed tables (the stack reaper then missed a live
//! stack's pages and freed it twice: `stack.slot_in_use`). Now only a live user root or a kernel root is loaded.

use crate::linux::{MAP_ANONYMOUS, MAP_PRIVATE};
use crate::{AddressSpace, Fill};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
use hal::paging::{self, tlb};
use ktest::{kassert, ktest, TestResult};

const SEC: u64 = 1_000_000_000;
const P: u64 = 4096;

ktest!("mm.tlb.ended_root_refused", Safe, ended_root_refused);
ktest!("mm.tlb.no_cpu_on_freed_root", Safe, no_cpu_on_freed_root);

fn space() -> Result<AddressSpace, String> {
    AddressSpace::with_fill(Fill::OnFault).map_err(|x| format!("address space: {x:?}"))
}

/// A root CR3 may hold: the kernel root or a live user root.
fn sound(root: u64) -> bool {
    root == paging::home_root() || tlb::has_context(root)
}

fn ap_root(_: u64) -> u64 {
    paging::current_root()
}

/// Loading the root of an address space that has ended loads the kernel root instead.
fn ended_root_refused() -> TestResult {
    let home = paging::home_root();
    let a = space()?;
    let root = a.root();
    a.activate();
    paging::activate(home);
    drop(a);
    let s0 = tlb::stats().foreign_refused;
    let (now, live) = {
        let _irq = hal::cpu::IrqGuard::new();
        // Its block may already be another space's root (then loading it is right): judged under the same guard.
        let live = tlb::has_context(root);
        paging::activate(root);
        let now = paging::current_root();
        paging::activate(home);
        (now, live)
    };
    if live {
        return ktest::skip("the freed root was reused at once");
    }
    kassert!(now == home, "the ended root {root:#x} was loaded (CR3 {now:#x}, kernel root {home:#x})");
    kassert!(tlb::stats().foreign_refused == s0 + 1, "refusal not counted");
    Ok(())
}

static BAD: AtomicU64 = AtomicU64::new(0);
static BAD_ROOT: AtomicU64 = AtomicU64::new(0);

/// Two threads per CPU make short-lived address spaces in turn, each saving `current_root()` first (often the
/// other thread's root, held lazily, which ends meanwhile) and loading it again after its own space ran: the
/// pattern of the ktests that broke. Every root a CPU holds must stay sound, and so must every CPU at the end.
fn no_cpu_on_freed_root() -> TestResult {
    const ROUNDS: u64 = 40;
    let cpus: Vec<usize> = (0..hal::percpu::cpu_count()).filter(|&c| hal::smp::is_online(c)).collect();
    BAD.store(0, SeqCst);
    BAD_ROOT.store(0, SeqCst);
    let s0 = tlb::stats().foreign_refused;
    let errs = Arc::new(ksync::SpinLock::new(Vec::<String>::new()));
    let mut hs = Vec::new();
    for &c in &cpus {
        for k in 0..2u64 {
            let errs = errs.clone();
            let o = sched::SpawnOpts { cpu: Some(c), pin: true, ..sched::SpawnOpts::default() };
            let h = sched::spawn_with("mm-root-churn", sched::Class::Normal, o, move || {
                for r in 0..ROUNDS {
                    if let Err(e) = churn_round(k, r) {
                        errs.lock().push(e);
                        return;
                    }
                }
            })
            .map_err(|x| format!("spawn on CPU {c}: {x:?}"))?;
            hs.push(h);
        }
    }
    let joined = hs.iter().all(|h| h.join_timeout(60 * SEC));
    kassert!(joined, "a churn thread did not finish within 60 s");
    if let Some(e) = errs.lock().first() {
        return Err(e.clone());
    }
    let bad = BAD.load(SeqCst);
    kassert!(bad == 0, "{bad} times a CPU held a root that is neither the kernel's nor live (last {:#x})", BAD_ROOT.load(SeqCst));
    for &c in cpus.iter().filter(|&&c| c != hal::percpu::this_cpu()) {
        let r = hal::smp::call_on(c, ap_root, 0, time::now_ns, SEC).map_err(|x| format!("call_on CPU {c}: {x:?}"))?;
        kassert!(sound(r), "CPU {c} holds {r:#x}, neither the kernel root nor a live user root");
    }
    klog::event!(Info, "mm", "ktest.no_cpu_on_freed_root", cpus = cpus.len(), rounds = ROUNDS, refused = tlb::stats().foreign_refused - s0);
    Ok(())
}

fn churn_round(k: u64, r: u64) -> Result<(), String> {
    let saved = paging::current_root();
    let a = space()?;
    let base = a.sys_mmap(0, P, 3, MAP_ANONYMOUS | MAP_PRIVATE, 0).map_err(|x| format!("mmap: {x:?}"))?;
    a.write_u64(base, k << 32 | r).map_err(|x| format!("write: {x:?}"))?;
    a.activate();
    sched::yield_now();
    {
        // Interrupts off: a live root loaded here cannot end before the check (its `leave` waits for this CPU).
        let _irq = hal::cpu::IrqGuard::new();
        paging::activate(saved);
        let now = paging::current_root();
        if !sound(now) {
            BAD.fetch_add(1, SeqCst);
            BAD_ROOT.store(now, SeqCst);
        }
        paging::activate(paging::home_root());
    }
    drop(a);
    sched::yield_now();
    Ok(())
}

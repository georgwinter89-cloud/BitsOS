//! ktests of spurious faults (desk fix/a2b): threads that fault on one fresh page at once each find it valid
//! after the first filled it; that must never count as a fault that repeats (ab.ab8.threads on the Mini-PC:
//! 8 glibc threads entering library text together, the fifth one was killed).

use crate::fault::{SpuriousRun, PF_FETCH, PF_PRESENT, PF_USER, SPURIOUS_REPEATS};
use crate::{AddressSpace, FaultReason, Fill, Prot, VmaKind};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
use ktest::{kassert, ktest, TestResult};

const P: u64 = 4096;
const TEXT: u64 = 0x7e00_0000_0000;
const PAGES: u64 = 64;
const RX: Prot = Prot { read: true, write: false, exec: true };
const SEC: u64 = 1_000_000_000;

fn text_space() -> Result<AddressSpace, String> {
    let a = AddressSpace::with_fill(Fill::OnFault).map_err(|e| format!("address space: {e:?}"))?;
    a.map_fixed(TEXT, PAGES * P, RX, VmaKind::Image).map_err(|e| format!("map: {e:?}"))?;
    Ok(a)
}

/// The order the race produces, without the race: one thread fills, fifteen more find the page valid. Each
/// has its own run; one run on the same page ends after `SPURIOUS_REPEATS`.
fn spurious_per_thread() -> TestResult {
    let a = text_space()?;
    let mut runs = [SpuriousRun::default(); 16];
    for (i, r) in runs.iter_mut().enumerate() {
        let got = a.handle_thread_fault(TEXT + 5, PF_USER | PF_FETCH, r);
        kassert!(got.is_ok(), "thread {i} on the freshly filled page: {got:?}");
    }
    let mut one = SpuriousRun::default();
    for i in 0..SPURIOUS_REPEATS {
        kassert!(a.handle_thread_fault(TEXT, PF_USER | PF_FETCH | PF_PRESENT, &mut one).is_ok(), "fault {i} of one thread's run");
    }
    let last = a.handle_thread_fault(TEXT, PF_USER | PF_FETCH | PF_PRESENT, &mut one);
    kassert!(last == Err(FaultReason::Repeated), "the run past {SPURIOUS_REPEATS} ends the program: {last:?}");
    Ok(())
}
ktest!("mm.as.spurious_per_thread", Safe, spurious_per_thread);

/// One pinned thread per CPU fetches from the same fresh text pages at the same moment.
fn spurious_race() -> TestResult {
    let cpus: Vec<usize> = (0..hal::percpu::cpu_count()).filter(|&c| hal::smp::is_online(c)).collect();
    let a = Arc::new(text_space()?);
    let ready = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicU64::new(0));
    let errs = Arc::new(ksync::SpinLock::new(Vec::<String>::new()));
    let mut hs = Vec::new();
    for &c in &cpus {
        let (a, ready, refused, errs, n) = (a.clone(), ready.clone(), refused.clone(), errs.clone(), cpus.len());
        let o = sched::SpawnOpts { cpu: Some(c), pin: true, ..sched::SpawnOpts::default() };
        let h = sched::spawn_with("mm-spurious-race", sched::Class::Normal, o, move || {
            ready.fetch_add(1, SeqCst);
            let until = time::now_ns() + 5 * SEC;
            while ready.load(SeqCst) < n && time::now_ns() < until {
                core::hint::spin_loop();
            }
            let mut run = SpuriousRun::default();
            for pg in 0..PAGES {
                if let Err(why) = a.handle_thread_fault(TEXT + pg * P + 0x40, PF_USER | PF_FETCH, &mut run) {
                    refused.fetch_add(1, SeqCst);
                    errs.lock().push(format!("CPU {c} page {pg}: {}", why.text()));
                    return;
                }
            }
        })
        .map_err(|x| format!("spawn on CPU {c}: {x:?}"))?;
        hs.push(h);
    }
    kassert!(hs.iter().all(|h| h.join_timeout(60 * SEC)), "a racing thread did not finish within 60 s");
    let n = refused.load(SeqCst);
    kassert!(n == 0, "{n} racing threads were refused, first {:?}", errs.lock().first());
    kassert!(a.resident_pages() == PAGES, "{} of {PAGES} pages filled", a.resident_pages());
    klog::event!(Info, "mm", "ktest.spurious_race", cpus = cpus.len(), pages = PAGES);
    Ok(())
}
ktest!("mm.as.spurious_race", Safe, spurious_race);

//! In-kernel test of GP-R3's storm naming against the live tables: the real reschedule vector, the shootdown IPIs
//! counted by cause, and every live MSI vector named by its own set. Safe, rerunnable: reads only (one shootdown
//! of a kernel page of its own makes sure a cause count moves when more than one CPU runs).

use crate::irqsource::{describe, tlb_ipis, top_cause};
use alloc::format;
use hal::paging::tlb;
use ktest::{TestResult, kassert, ktest};

fn names() -> TestResult {
    let r = sched::resched_vector();
    kassert!(r != 0, "no reschedule vector");
    kassert!(describe(0, r, None).0 == "resched", "{r:#x} is not named resched");
    let before = tlb_ipis();
    let _ = tlb::flush_kernel_as(0, 0, true, tlb::Cause::Kernel);
    let after = tlb_ipis();
    let (src, cause) = describe(0, tlb::VECTOR, Some((&before, &after)));
    kassert!(src == "tlb_shootdown", "{:#x} named {src}", tlb::VECTOR);
    // Programs may shoot down meanwhile, so the kernel's own round need not be the top cause: it must count.
    if hal::percpu::cpu_count() > 1 {
        let k = tlb::Cause::Kernel as usize;
        kassert!(after[k] > before[k], "a full kernel flush sent no IPI counted as kernel");
        kassert!(top_cause(&before, &after).is_some() && cause != "-", "cause {cause}");
    }
    let mut named = 0;
    for (at, name, _, stats) in pci::irq::sets() {
        for (i, s) in stats.iter().enumerate() {
            let Some(v) = s.vector else { continue };
            let want = format!("msi:{at}/{name}#{i}");
            let got = describe(v.cpu, v.vector, None).0;
            kassert!(got == want, "cpu {} vector {:#x}: {got}, not {want}", v.cpu, v.vector);
            named += 1;
        }
    }
    klog::event!(Info, "klogd", "ktest.irq_source", resched = klog::Hex(r as u64), msi_vectors = named, shootdown_cause = cause.as_str());
    Ok(())
}
ktest!("rb.irq.source", Safe, names);

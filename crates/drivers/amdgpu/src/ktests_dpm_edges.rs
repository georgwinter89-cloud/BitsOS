//! GX-9b on the fakes, the edges: a request coming in while an Allow goes out, steps on a block that stays asleep, a late
//! answer, a transfer never answered, the tool run that starts the backend, the one Quiesce hook.

use crate::dpm::{self, Awake, Next};
use crate::dpm_live::Hook;
use crate::dpm_table;
use crate::gpucore::smu::SmuTimeouts;
use crate::gpucore::smufake::{SmuHostile, msg, resp};
use crate::hw::chain::Chain;
use crate::hw::live::ToolGate;
use crate::ktests_dpm::{allow, mark, pass, taken_since, up};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use ksync::SpinLock;
use ktest::kassert;

/// `awake` skips the chain's lock when it reads `asleep` clear: here it comes in between `tick`'s look at the count and
/// the Allow (the fake's clock is read there). The holder's block must stay up.
pub(crate) fn allow_race() -> ktest::TestResult {
    let (r, d, _) = up()?;
    pass(&r, dpm::GFX_OFF_DELAY_US);
    let held: Arc<SpinLock<Option<Awake>>> = Arc::new(SpinLock::named("gx.ktest.held", None));
    let (d2, h2) = (d.clone(), held.clone());
    r.fake(|f| {
        f.on_now = Some(Box::new(move || {
            let a = d2.awake();
            *h2.lock() = Some(a);
        }))
    });
    let at = mark(&r);
    let n = d.tick();
    let a = held.lock().take().ok_or("the hook never ran")?;
    kassert!(a.up, "awake says no");
    kassert!(taken_since(&r, at).is_empty(), "the Allow went out past a request: {:x?}", taken_since(&r, at));
    kassert!(!d.flags().asleep.load(Ordering::Acquire), "asleep under a holder");
    kassert!(matches!(n, Next::AfterUs(_)), "{n:?}");
    // The hold over: the next looks after the delay send it after all.
    drop(a);
    pass(&r, dpm::GFX_OFF_DELAY_US);
    let _ = d.tick();
    pass(&r, dpm::GFX_OFF_DELAY_US);
    kassert!(d.tick() == Next::Idle && taken_since(&r, at) == [(msg::ALLOW_GFXOFF, 0)], "{:x?}", taken_since(&r, at));
    r.finish_jobs()
}
ktest::ktest!("gx.fake.dpm_allow_race", Safe, allow_race);

/// A step on a block that stays asleep: no register either way, memory and doorbells work, the next step asks again.
pub(crate) fn asleep() -> ktest::TestResult {
    let (r, d, w) = up()?;
    allow(&r, &d)?;
    let (reads, off) = r.fake(|f| (f.gc.asleep_reads, f.gc.off_access.len()));
    let carve = r.fake(|f| f.fb_mc);
    r.fake(|f| f.smu.hostile.push(SmuHostile::Answer { id: msg::DISALLOW_GFXOFF, code: resp::FAILED }));
    let mut seen = None;
    let mut cleared = Some(0);
    let mut rebuild = (true, true);
    let ran = d.with(&mut |o| {
        let hw = o.io();
        let mut b = [0u8; 4];
        seen = Some((hw.reg_read(w.gc.rlc_cntl), hw.reg_write(w.gc.rlc_pg_cntl, 0), hw.mem_read(carve, &mut b), hw.doorbell_write64(0x200, 0)));
        cleared = o.clear_fault(&r.vmi.addrs, 0);
        let rebuilt = o.rebuild_compute(&r.vmi);
        rebuild = (rebuilt.queue.is_ok(), rebuilt.stranded);
    });
    kassert!(ran, "the step did not run");
    kassert!(seen == Some((None, false, true, true)), "{seen:?}");
    kassert!(cleared.is_none() && rebuild == (false, false), "{cleared:?} {rebuild:?}");
    kassert!(r.fake(|f| f.gc.asleep_reads == reads && f.gc.off_access.len() == off), "GC touched asleep: {:x?}", r.fake(|f| f.gc.off_access.clone()));
    kassert!(d.flags().asleep.load(Ordering::Acquire) && d.report().requests == 0, "requests {}", d.report().requests);
    // PMFW answers again: the step has its registers.
    r.fake(|f| f.smu.hostile.clear());
    let mut read = None;
    kassert!(d.with(&mut |o| read = o.io().reg_read(w.gc.rlc_cntl)) && read.is_some_and(|v| v != u32::MAX), "{read:x?}");
    kassert!(!d.flags().asleep.load(Ordering::Acquire));
    r.finish_jobs()?;

    // PMFW silent on the Disallow: bounded once, later steps send nothing, and the stop does not open the registers.
    let (r, d, w) = up()?;
    allow(&r, &d)?;
    r.fake(|f| f.smu.hostile.push(SmuHostile::Never { id: msg::DISALLOW_GFXOFF }));
    let mut read = Some(0);
    kassert!(d.with(&mut |o| read = o.io().reg_read(w.gc.rlc_cntl)) && read.is_none());
    kassert!(d.report().dead, "not silent");
    let at = mark(&r);
    kassert!(d.with(&mut |o| read = o.io().reg_read(w.gc.rlc_cntl)) && read.is_none());
    kassert!(taken_since(&r, at).is_empty(), "a message past a silent PMFW");
    d.stop();
    kassert!(d.with(&mut |o| read = o.io().reg_read(w.gc.rlc_cntl)) && read.is_none(), "registers after a stop PMFW did not answer");
    r.fake(|f| f.smu.hostile.clear());
    d.flags().asleep.store(false, Ordering::Release);
    d.flags().pg_on.store(false, Ordering::Release);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_asleep", Safe, asleep);

/// One slow answer does not end DPM for the boot: the next look sees it, and everything works again.
pub(crate) fn late() -> ktest::TestResult {
    let (r, d, _) = up()?;
    let room = r.gpu.heaps_table_room();
    let over = SmuTimeouts::RUNNING.answer_us;
    r.fake(|f| f.smu.hostile.push(SmuHostile::Late { id: msg::TRANSFER_TABLE_SMU2DRAM, us: over + 100_000 }));
    kassert!(d.metrics().is_err() && d.report().dead && !d.enabled(), "the slow answer was not noticed");
    r.fake(|f| f.smu.hostile.clear());
    let at = mark(&r);
    kassert!(d.metrics().is_err() && taken_since(&r, at).is_empty(), "a message over the one in flight");
    pass(&r, over);
    kassert!(d.tick() != Next::Exit && !d.report().dead && d.enabled(), "the late answer was not seen");
    kassert!(d.metrics().is_ok(), "no metrics after it");
    // The silence held the Allow back; it goes now.
    pass(&r, dpm::GFX_OFF_DELAY_US);
    let _ = d.tick();
    pass(&r, dpm::GFX_OFF_DELAY_US);
    let _ = d.tick();
    kassert!(d.flags().asleep.load(Ordering::Acquire), "no Allow after the silence");
    d.stop();
    kassert!(r.gpu.heaps_table_room() > room, "nothing in flight: the page goes back");
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_late", Safe, late);

/// A metrics transfer never answered: the page PMFW may still write stays out of the allocator.
pub(crate) fn silent_metrics() -> ktest::TestResult {
    let (r, d, _) = up()?;
    let room = r.gpu.heaps_table_room();
    r.fake(|f| f.smu.hostile.push(SmuHostile::Never { id: msg::TRANSFER_TABLE_SMU2DRAM }));
    kassert!(d.metrics().is_err() && d.report().dead);
    d.stop();
    kassert!(r.gpu.heaps_table_room() == room, "the page went back with a transfer in flight");
    r.fake(|f| f.smu.hostile.clear());
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_silent_metrics", Safe, silent_metrics);

/// The transfer's order: the wiped page is flushed before PMFW is asked to write it.
pub(crate) fn flush_first() -> ktest::TestResult {
    let (r, d, _) = up()?;
    r.fake(|f| f.hdp_flushed_at = None);
    let at = mark(&r);
    kassert!(d.metrics().is_ok());
    kassert!(taken_since(&r, at) == [(msg::TRANSFER_TABLE_SMU2DRAM, dpm_table::TABLE_SMU_METRICS)]);
    kassert!(r.fake(|f| f.hdp_flushed_at) == Some(at), "flushed at {:?}, the transfer is message {at}", r.fake(|f| f.hdp_flushed_at));
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_flush_first", Safe, flush_first);

/// The run that starts the backend had nothing to hold when it entered: its hold keeps Allow back until it ends.
pub(crate) fn tool_hold() -> ktest::TestResult {
    let (r, d, _) = up()?;
    let gate = ToolGate::new();
    gate.hold(d.awake());
    kassert!(d.report().requests == 0, "a hold with no run");
    let run = gate.enter().ok_or("the gate is shut")?;
    gate.hold(d.awake());
    kassert!(d.report().requests == 1, "requests {}", d.report().requests);
    pass(&r, dpm::GFX_OFF_DELAY_US * 2);
    let at = mark(&r);
    kassert!(matches!(d.tick(), Next::AfterUs(_)) && taken_since(&r, at).is_empty(), "Allow under the run");
    drop(run);
    kassert!(d.report().requests == 0, "the hold outlived the run");
    kassert!(gate.enter().is_some(), "the gate stayed shut");
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_tool_hold", Safe, tool_hold);

/// One hook for the boot, working on the last start's flags (devmodel keeps every hook it is given).
pub(crate) fn one_hook() -> ktest::TestResult {
    let (r, d, w) = up()?;
    allow(&r, &d)?;
    let hook = Hook::new();
    let stale = Arc::new(dpm::Flags::default());
    stale.asleep.store(true, Ordering::Release);
    kassert!(hook.point(w, stale.clone()), "the first start registers it");
    kassert!(!hook.point(w, d.flags()), "a second hook");
    let at = mark(&r);
    r.fake(|f| hook.run(f));
    kassert!(taken_since(&r, at) == [(msg::DISALLOW_GFXOFF, 0)], "{:x?}", taken_since(&r, at));
    kassert!(stale.asleep.load(Ordering::Acquire), "the hook worked on an old start's flags");
    kassert!(!d.flags().asleep.load(Ordering::Acquire) && !d.flags().pg_on.load(Ordering::Acquire));
    r.finish_jobs()
}
ktest::ktest!("gx.hw.dpm_one_hook", Safe, one_hook);

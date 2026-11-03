//! The hang of 29.10. (plan/notes/gx-GX-7j3-hang.md): `gx_bringup`'s first step after the `LIVE` lock builds `Pre`
//! from the boot's statics; INT-W28 took `MES_UP` twice in that one struct literal, so the first guard was still
//! held when the second `.lock()` spun, forever, on the MCP worker's CPU. `pre_now` is that step for `run` and
//! `reset`; here it must come back (on its own thread, with a deadline, so the old spin fails the test instead of
//! hanging the suite) and carry `MES_UP` into both fields.

use crate::gpucore::fakechip;
use crate::gpucore::mes::MesAddrs;
use crate::gpucore::psp::Until;
use crate::psplive::{self, Pre};
use std::sync::mpsc;
use std::time::Duration;

/// `pre_now` on a thread of its own: `None` when it did not answer within 10 s (a spin lock taken twice).
fn pre_within_deadline(until: Until, kept: Option<bool>, reset: bool) -> Option<Pre> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(psplive::pre_now(until, false, kept, Ok(()), true, reset));
    });
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

#[test]
fn pre_state_takes_each_lock_once() {
    let ma = MesAddrs::from_discovery(&fakechip::discovery_780m()).unwrap();
    // The Mini-PC's case of 29.10.: a fresh cold boot (nothing kept, MES never started), the smallest calls.
    psplive::set_mes_up_for_test(None);
    for until in [Until::Ring, Until::Fw, Until::Gfx, Until::GfxRing] {
        let p = pre_within_deadline(until, None, false).unwrap_or_else(|| panic!("{until:?}: pre_now did not come back (a spin lock taken twice)"));
        assert!(!p.kept && !p.kept_gc_booted && !p.mes_up && !p.mes_queues && !p.reset, "{until:?}: {p:?}");
    }
    let p = pre_within_deadline(Until::Gfx, None, true).expect("until=reset: pre_now did not come back");
    assert!(p.reset && !p.gart_up && !p.kept, "{p:?}");
    // After a kept, passed gfx_mes: both MES fields from the one copy, the kept run's GC.
    psplive::set_mes_up_for_test(Some((ma, Some(0x1000), true)));
    let p = pre_within_deadline(Until::Vm, Some(true), false).expect("until=vm: pre_now did not come back");
    assert!(p.kept && p.kept_gc_booted && p.mes_up && p.mes_queues, "{p:?}");
    psplive::set_mes_up_for_test(Some((ma, None, false)));
    let p = pre_within_deadline(Until::Vm, Some(false), false).expect("until=vm (MES without queues): pre_now did not come back");
    assert!(p.kept && !p.kept_gc_booted && p.mes_up && !p.mes_queues, "{p:?}");
    psplive::set_mes_up_for_test(None);
}

//! ATOMIC-2 ktest: a run queue's room is made when threads are made, so queueing never allocates.
//! (The futex buckets' room: `futex::ktests`.)

use crate::{Class, SpawnOpts, cpus, run, table};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ksync::WaitQueue;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

const SEC: u64 = 1_000_000_000;

/// Every CPU's queue holds as many threads as the table can list.
fn queues_cover_the_table(when: &str) -> R {
    let listed = table::capacity();
    for cs in cpus::online() {
        let cap = cs.rq.lock().capacity();
        kassert!(cap >= listed, "{when}: CPU {} has queue room for {cap} threads, the table lists up to {listed}", cs.cpu);
    }
    Ok(())
}

ktest!("sc.sched.queue_room", Safe, queue_room);
/// 700 threads pinned to one CPU sleep on one gate and are woken in one go with preemption off, as a waker
/// under a spin lock does it: all of them are in that CPU's queue at once (seventeen kilobytes of entries) and
/// the queue's memory is the one it had before the wake.
fn queue_room() -> R {
    const N: usize = 700;
    queues_cover_the_table("at the start")?;
    let home = crate::current_cpu();
    // The waker shares the CPU with the threads, so whenever it runs each of them has either blocked or waits in
    // that queue, and none runs beside the wake.
    let _pin = crate::pin_for_now(home).ok_or("could not pin the test to its CPU")?;
    let cs = cpus::of(home).ok_or("no CPU state")?;
    let no_room = run::QUEUE_NO_ROOM.get();
    let (gate, open) = (Arc::new(WaitQueue::new()), Arc::new(AtomicBool::new(false)));
    let (started, finished) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut hs = Vec::with_capacity(N);
    for i in 0..N {
        let (g, o, s, f) = (gate.clone(), open.clone(), started.clone(), finished.clone());
        let opts = SpawnOpts { cpu: Some(home), pin: true, rt_prio: crate::RT_PRIO_DEFAULT };
        hs.push(
            crate::spawn_with("sc-room", Class::Normal, opts, move || {
                s.fetch_add(1, Ordering::AcqRel);
                g.wait_until(|| o.load(Ordering::Acquire));
                f.fetch_add(1, Ordering::AcqRel);
            })
            .map_err(|e| format!("spawn {i}: {e:?}"))?,
        );
    }
    let t0 = time::now_ns();
    // Every thread waits on the gate: `started` is not enough. A thread may still be blocked on its way into the
    // wait (the gate's room grows with an allocation, which may wait for memory, and fault or lock waits are
    // possible there too): the 12.11. gate found 155 of 700 elsewhere when the gate opened, and the wake missed
    // them. A waiter on the gate that has not blocked yet is queued here (Blocking counts as running).
    while (gate.waiter_count() < N || cs.nr_queued.load(Ordering::Acquire) > 0) && time::now_ns() - t0 < 30 * SEC {
        crate::sleep_ms(1);
    }
    kassert!(started.load(Ordering::Acquire) == N, "{} of {N} threads started", started.load(Ordering::Acquire));
    kassert!(gate.waiter_count() == N, "{} of {N} threads wait on the gate after 30 s", gate.waiter_count());
    kassert!(table::capacity() >= N, "the table lists {} threads at most with {N} alive", table::capacity());
    queues_cover_the_table("with the threads alive")?;
    let (cap, queued) = {
        let before = cs.rq.lock().capacity();
        let _atomic = ksync::PreemptGuard::new();
        kassert!(crate::current_cpu() == home, "the waker runs on CPU {} instead of {home}", crate::current_cpu());
        open.store(true, Ordering::Release);
        gate.wake_all();
        let queued = cs.nr_queued.load(Ordering::Acquire);
        kassert!(cs.rq.lock().capacity() == before, "the queue of CPU {home} changed its memory during the wake");
        (before, queued)
    };
    kassert!(queued >= N, "{queued} of {N} woken threads were in the queue of CPU {home} at once (room {cap})");
    for h in &hs {
        kassert!(h.join_timeout(60 * SEC), "thread {} did not finish within 60 s", h.id());
    }
    kassert!(finished.load(Ordering::Acquire) == N);
    kassert!(run::QUEUE_NO_ROOM.get() == no_room, "{} threads found no room in a run queue", run::QUEUE_NO_ROOM.get() - no_room);
    drop(hs);
    crate::reap();
    queues_cover_the_table("at the end")?;
    klog::event!(Info, "sched", "ktest.queue_room", threads = N, queued_at_once = queued, room = cap, listed = table::capacity());
    Ok(())
}

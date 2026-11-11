//! BL-1 ktests `nv.mq.*` on host/tests/nv/10-nvme.sh's controllers (QEMU `-device nvme`, image files; the scenario
//! boots them with -smp 1/4/16 and `max_ioqpairs` 1/8 too): the queues per CPU and vector, 64 commands in flight
//! with every byte checked (and which bytes went in place, which through bounce pages), an injected timeout on one
//! queue that leaves the others working, threads on every CPU reading at once, `read_speed` with many in flight, and
//! a Quiesce under load with a restart that builds the queues again. Only reads.

use crate::{Nvme, controllers, settle, test_pattern};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ktest::{TestResult, fail, kassert, ktest};

const SEC: u64 = 1_000_000_000;
/// NVQ0 ns2: 8 MiB, 4 KiB blocks, pattern seed 2 (10-nvme.sh).
const NS: u32 = 2;
const SEED: u8 = 2;
const BLOCKS: u64 = 2048;

fn nvq0() -> Result<Arc<Nvme>, String> {
    if !settle(30 * SEC) {
        return Err(String::from("a controller's bring-up did not finish in 30 s"));
    }
    controllers()
        .into_iter()
        .find(|c| c.info().ident.is_some_and(|i| i.serial == "NVQ0"))
        .ok_or_else(|| String::from("no controller with serial NVQ0 (host/tests/nv images)"))
}

macro_rules! need {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return fail(&e),
        }
    };
}

/// A buffer of `len` bytes starting `skew` bytes past a page boundary (0: page aligned, reached in place; 2: not
/// dword aligned, through bounce pages), and its backing.
fn buf(len: usize, skew: usize) -> (Vec<u8>, usize) {
    let v = vec![0u8; len + 4096 + skew];
    let off = v.as_ptr().align_offset(4096) + skew;
    (v, off)
}

fn check(k: usize, lba: u64, b: &[u8]) -> TestResult {
    let base = lba * 4096;
    if let Some(x) = (0..b.len()).find(|&x| b[x] != test_pattern(SEED, base + x as u64)) {
        return Err(format!("request {k} lba {lba} len {}: byte {x} is {:#x}, want {:#x}", b.len(), b[x], test_pattern(SEED, base + x as u64)));
    }
    Ok(())
}

/// What `many` read: the most in flight, and the bytes the device should have put in place and through bounce pages
/// (by the driver's own rule: in the direct map and dword aligned).
struct Many {
    most: usize,
    direct: u64,
    bounce: u64,
}

/// `n` reads on queue `k % queues`, sizes 4..32 KiB, every fourth through bounce pages; all in flight at once.
fn many(c: &Nvme, n: usize, first_lba: u64) -> Result<Many, String> {
    let nq = c.info().queues.len().max(1);
    let mut store: Vec<(Vec<u8>, usize, usize, u64)> = (0..n)
        .map(|k| {
            let len = 4096 * (1 + k % 8);
            let (v, off) = buf(len, if k % 4 == 3 { 2 } else { 0 });
            (v, off, len, (first_lba + k as u64 * 9) % (BLOCKS - 8))
        })
        .collect();
    let (mut direct, mut bounce) = (0u64, 0u64);
    for (k, (v, off, len, _)) in store.iter().enumerate() {
        let b = &v[*off..*off + *len];
        let inplace = crate::hw::Hw::dma_target(&c.hw, b).filter(|a| a.is_multiple_of(4)).is_some();
        if k % 4 == 3 && inplace {
            return Err(format!("request {k}: a buffer 2 bytes past a page start counts as reachable in place"));
        }
        if inplace { direct += *len as u64 } else { bounce += *len as u64 }
    }
    let mut reqs: Vec<(u64, &mut [u8])> = store.iter_mut().map(|(v, off, len, lba)| (*lba, &mut v[*off..*off + *len])).collect();
    let most = c.read_vectored(NS, &mut reqs, &|k| Some(k % nq), Some(n)).map_err(|e| format!("{}: {}", c.name, e.text()))?;
    drop(reqs);
    for (k, (v, off, len, lba)) in store.iter().enumerate() {
        check(k, *lba, &v[*off..*off + *len])?;
    }
    Ok(Many { most, direct, bounce })
}

/// The queues the bring-up made: one per CPU unless the controller grants or the vectors allow fewer, each on its
/// own vector with MSI-X, every queue's vector interrupting when its commands complete.
fn queues() -> TestResult {
    let c = need!(nvq0());
    let i = c.info();
    let cpus = hal::percpu::cpu_count().max(1);
    let nq = i.queues.len();
    kassert!(nq >= 1 && nq <= cpus, "{} queues for {cpus} CPUs", nq);
    // The scenario's matrix boots say how many (min of the CPUs and QEMU's max_ioqpairs).
    if let Some(want) = pci::registry::config_value("nv_mq_queues").and_then(|v| v.trim().parse::<usize>().ok()) {
        kassert!(nq == want, "{nq} queues, the scenario wants {want} ({cpus} CPUs)");
    }
    for (k, q) in i.queues.iter().enumerate() {
        kassert!(q.qid as usize == k + 1, "queue {k} has qid {}", q.qid);
    }
    // Every mode has its own shape; the scenario's boots name the one they expect (nvme_irq=off: polled).
    let mode = i.irq.unwrap_or(crate::hw::IrqMode::Polled);
    if let Some(want) = pci::registry::config_value("nv_mq_irq") {
        kassert!(mode.name() == want.trim(), "interrupt mode {}, the scenario wants {}", mode.name(), want.trim());
    }
    match mode {
        crate::hw::IrqMode::MsiX(n) if n >= 2 => {
            kassert!(nq < n as usize, "{nq} queues on {n} vectors (admin has its own)");
            for q in &i.queues {
                kassert!(q.vector == Some(q.qid), "queue {} on vector {:?}", q.qid, q.vector);
            }
        }
        // One vector: one queue on it, admin polls.
        crate::hw::IrqMode::MsiX(_) | crate::hw::IrqMode::Msi => {
            kassert!(nq == 1 && i.queues[0].vector == Some(0), "{} on one vector: {nq} queues, vector {:?}", mode.name(), i.queues[0].vector);
        }
        crate::hw::IrqMode::Polled => {
            kassert!(i.queues.iter().all(|q| q.vector.is_none()), "polled, yet a queue has a vector");
        }
    }
    // Each queue reads right; one with a vector interrupts when its command completes.
    for q in &i.queues {
        let before = q.vector.map(|v| crate::hw::Hw::irq_count(&c.hw, v));
        let mut b = vec![0u8; 4096];
        let lba = q.qid as u64;
        let mut reqs = [(lba, b.as_mut_slice())];
        let qi = q.qid as usize - 1;
        c.read_vectored(NS, &mut reqs, &|_| Some(qi), None).map_err(|e| e.text())?;
        check(0, lba, &b)?;
        if let (Some(v), Some(before)) = (q.vector, before) {
            // The waiter may see the completion in memory (its spin) before the message reaches the vector's CPU.
            let end = time::now_ns() + SEC;
            while crate::hw::Hw::irq_count(&c.hw, v) == before && time::now_ns() < end {
                sched::sleep_ns(1_000_000);
            }
            kassert!(crate::hw::Hw::irq_count(&c.hw, v) > before, "no interrupt on queue {}'s vector {v} in 1 s", q.qid);
        }
    }
    klog::event!(Info, "nvme", "ktest.queues", ctrl = c.name.as_str(), queues = nq, cpus = cpus, irq = i.irq.map_or("-", |m| m.name()));
    Ok(())
}
ktest!("nv.mq.queues", Run("nv/10-nvme"), queues);

/// 64 commands in flight at once over every queue, every byte checked, nothing left in flight after; the bytes went
/// in place or through bounce pages exactly as the driver's rule says (the counters are shared by every NVMe
/// controller, so they may only have grown more; this controller's byte count is exact).
fn inflight64() -> TestResult {
    let c = need!(nvq0());
    let (b0, d0, r0) = (crate::ctrl::COPY_NVME_BOUNCE.sum(), crate::ctrl::NVME_DIRECT.sum(), c.stats.bytes_read.load(Ordering::Relaxed));
    c.stats.inflight_max.store(0, Ordering::Relaxed);
    let m = many(&c, 64, 3)?;
    kassert!(m.most == 64, "only {} in flight", m.most);
    kassert!(c.stats.inflight_max.load(Ordering::Relaxed) >= 64, "inflight_max {} in this run", c.stats.inflight_max.load(Ordering::Relaxed));
    for q in c.queue_stats() {
        kassert!(q.inflight == 0 && q.free == q.tags, "queue {} left {} in flight, {} of {} tags free", q.qid, q.inflight, q.free, q.tags);
    }
    let (db, dd, dr) = (crate::ctrl::COPY_NVME_BOUNCE.sum() - b0, crate::ctrl::NVME_DIRECT.sum() - d0, c.stats.bytes_read.load(Ordering::Relaxed) - r0);
    kassert!(dr == m.direct + m.bounce, "{dr} bytes read, {} asked for", m.direct + m.bounce);
    // Kernel heap buffers lie in the direct map: the page-aligned ones go in place.
    kassert!(m.direct > 0, "no buffer of this run is reachable in place (heap outside the direct map?)");
    kassert!(dd >= m.direct, "{dd} bytes in place, {} expected", m.direct);
    kassert!(db >= m.bounce, "{db} bytes through bounce pages, {} expected", m.bounce);
    Ok(())
}
ktest!("nv.mq.inflight64", Run("nv/10-nvme"), inflight64);

/// Queue 1's commands time out at once (injected): each is aborted (QEMU leaves it running and it completes) while
/// the others on every queue go on; no reset, every byte right.
fn timeout_one_queue() -> TestResult {
    let c = need!(nvq0());
    let ld = |a: &core::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    let (t0, a0, r0) = (ld(&c.stats.timeouts), ld(&c.stats.aborts), ld(&c.stats.resets));
    kassert!(c.inject_timeouts(0, true), "no queue 1");
    let r = many(&c, 64, 100);
    c.inject_timeouts(0, false);
    r?;
    kassert!(ld(&c.stats.timeouts) > t0, "no timeout seen");
    kassert!(ld(&c.stats.aborts) > a0, "no Abort sent");
    kassert!(ld(&c.stats.resets) == r0, "the controller was reset ({} -> {})", r0, ld(&c.stats.resets));
    kassert!(c.state() == crate::State::Ready, "state {}", c.state().name());
    Ok(())
}
ktest!("nv.mq.timeout_one_queue", Run("nv/10-nvme"), timeout_one_queue);

/// A thread pinned to each CPU reads at once (each on its CPU's queue), 200 reads each, bytes checked.
fn every_cpu() -> TestResult {
    let c = need!(nvq0());
    let cpus = sched::wait_cpus_online(5 * SEC).max(1);
    let mut hs = Vec::new();
    let errs: Arc<ksync::SpinLock<Vec<String>>> = Arc::new(ksync::SpinLock::new(Vec::new()));
    for cpu in 0..cpus {
        let (c, errs) = (c.clone(), errs.clone());
        let opts = sched::SpawnOpts { cpu: Some(cpu), pin: true, ..sched::SpawnOpts::default() };
        let h = sched::spawn_with("ktest-nvme-cpu", sched::Class::Normal, opts, move || {
            let mut b = vec![0u8; 8192];
            for i in 0..200u64 {
                let lba = (cpu as u64 * 61 + i * 7) % (BLOCKS - 2);
                let r = c.read(NS, lba, &mut b, true).map_err(|e| e.text()).and_then(|()| check(i as usize, lba, &b));
                if let Err(e) = r {
                    errs.lock().push(format!("cpu {cpu}: {e}"));
                    return;
                }
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        hs.push(h);
    }
    for h in &hs {
        kassert!(h.join_timeout(60 * SEC), "a reader did not finish in 60 s");
    }
    if let Some(e) = errs.lock().first() {
        return Err(e.clone());
    }
    let used = c.queue_stats().iter().filter(|q| q.placed > 0).count();
    kassert!(used == c.info().queues.len(), "only {used} of {} queues were used by {cpus} CPUs", c.info().queues.len());
    Ok(())
}
ktest!("nv.mq.every_cpu", Run("nv/10-nvme"), every_cpu);

/// `read_speed` (the tool's speed_mb) keeps many commands in flight.
fn speed() -> TestResult {
    let c = need!(nvq0());
    let (bytes, _, most) = c.read_speed(NS, 0, 4 << 20).map_err(|e| e.text())?;
    kassert!(bytes == 4 << 20, "read {bytes}");
    let per = c.info().max_transfer as u64;
    let cmds = (4u64 << 20).div_ceil(per) as usize;
    kassert!(most > 1 || cmds == 1, "{most} in flight for {cmds} commands");
    Ok(())
}
ktest!("nv.mq.speed", Run("nv/10-nvme"), speed);

/// A Quiesce while a reader keeps commands out on every queue, then a restart: the reader only ever sees Gone or
/// NotReady in between (no wrong byte, no hang), the queues come back as many as before with every tag free, and
/// 64 in flight read right again.
fn restart() -> TestResult {
    let c = need!(nvq0());
    let before = c.info().queues.len();
    let stop = Arc::new(AtomicBool::new(false));
    let errs: Arc<ksync::SpinLock<Vec<String>>> = Arc::new(ksync::SpinLock::new(Vec::new()));
    let (c2, stop2, errs2) = (c.clone(), stop.clone(), errs.clone());
    let h = sched::spawn_with("ktest-nvme-quiesce", sched::Class::Normal, sched::SpawnOpts::default(), move || {
        let mut i = 0u64;
        while !stop2.load(Ordering::Acquire) {
            i += 1;
            let r = many(&c2, 16, i * 5).map(|_| ());
            match r {
                Ok(()) => {}
                // many()'s error is "<controller>: <what>".
                Err(e) if e.ends_with(": gone") || e.ends_with(": not ready") => sched::sleep_ns(1_000_000),
                Err(e) => {
                    errs2.lock().push(e);
                    return;
                }
            }
        }
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    sched::sleep_ns(20_000_000);
    c.quiesce();
    sched::sleep_ns(10_000_000);
    let r = c.restart();
    sched::sleep_ns(20_000_000);
    stop.store(true, Ordering::Release);
    kassert!(h.join_timeout(60 * SEC), "the reader did not finish in 60 s");
    r?;
    if let Some(e) = errs.lock().first() {
        return Err(format!("reader: {e}"));
    }
    kassert!(c.state() == crate::State::Ready, "state {}", c.state().name());
    let nq = c.info().queues.len();
    kassert!(nq == before, "{nq} queues after the restart, {before} before");
    if let Some(want) = pci::registry::config_value("nv_mq_queues").and_then(|v| v.trim().parse::<usize>().ok()) {
        kassert!(nq == want, "{nq} queues, the scenario wants {want}");
    }
    let qs = c.queue_stats();
    kassert!(qs.len() == nq && qs.iter().all(|q| q.inflight == 0 && q.free == q.tags), "after the restart: {qs:?}");
    let m = many(&c, 64, 11)?;
    kassert!(m.most == 64, "only {} in flight after the restart", m.most);
    Ok(())
}
ktest!("nv.mq.restart", Run("nv/10-nvme"), restart);

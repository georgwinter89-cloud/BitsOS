//! BL-1 host tests: queues per CPU and vector, many commands in flight, a lost command on one queue, Quiesce with
//! many out, tags and CIDs.

use super::*;
use crate::hw::Bell;
use crate::mq::IoQueue;
use core::sync::atomic::{AtomicU64, Ordering};

fn mq(cpus: usize, irq: IrqMode, grant: u16) -> (Arc<Controller<FakeHw>>, FakeHw) {
    let (c, hw) = ctrl(vec![fake::ns(1, 9, 8192)]);
    {
        let mut st = hw.st.lock();
        st.cpus = cpus;
        st.irq = irq;
        st.grant_queues = grant;
    }
    c.start().unwrap();
    assert_eq!(c.state(), State::Ready);
    (c, hw)
}

fn ld(a: &AtomicU64) -> u64 {
    a.load(Ordering::Relaxed)
}

#[test]
fn queues_follow_cpus_grants_and_vectors() {
    // One queue per CPU, queue q on vector q (admin on 0), each CPU on the queue whose vector it got.
    let (c, hw) = mq(8, IrqMode::MsiX(9), 64);
    let i = c.info();
    assert_eq!(i.queues.len(), 8);
    for (k, q) in i.queues.iter().enumerate() {
        assert_eq!((q.qid, q.vector), (k as u16 + 1, Some(k as u16 + 1)));
    }
    let mut b = vec![0u8; 4096];
    for cpu in 0..8 {
        hw.st.lock().cpu = cpu;
        c.read(1, cpu as u64 * 8, &mut b, true).unwrap();
        assert_eq!(b[5], fake::pattern(1, cpu as u64 * 8 * 512 + 5));
    }
    let qs = c.queue_stats();
    assert!(qs.iter().all(|q| q.placed == 1 && q.completed == 1 && q.inflight == 0 && q.free == q.tags), "{qs:?}");
    // Fewer granted, fewer vectors, one vector, MSI, polled.
    for (irq, grant, want, vec1) in [
        (IrqMode::MsiX(9), 3, 3, Some(1)),
        (IrqMode::MsiX(4), 64, 3, Some(1)),
        (IrqMode::MsiX(1), 64, 1, Some(0)),
        (IrqMode::Msi, 64, 1, Some(0)),
        (IrqMode::Polled, 64, 8, None),
    ] {
        let (c, hw) = mq(8, irq, grant);
        let i = c.info();
        assert_eq!(i.queues.len(), want, "{irq:?} grant {grant}");
        assert_eq!(i.queues[0].vector, vec1, "{irq:?}");
        for cpu in 0..8 {
            hw.st.lock().cpu = cpu;
            c.read(1, 100 + cpu as u64, &mut b[..512], true).unwrap();
            assert_eq!(b[0], fake::pattern(1, (100 + cpu as u64) * 512), "{irq:?} cpu {cpu}");
        }
        no_writes(&c, &hw);
    }
}

fn reqs_check(c: &Controller<FakeHw>, n: usize, pick: &dyn Fn(usize) -> Option<usize>) -> Result<usize, CmdError> {
    let mut bufs: Vec<Vec<u8>> = (0..n).map(|k| vec![0u8; 512 * (1 + k % 9)]).collect();
    let lbas: Vec<u64> = (0..n as u64).map(|k| k * 13 % 8000).collect();
    let mut reqs: Vec<(u64, &mut [u8])> = bufs.iter_mut().zip(&lbas).map(|(b, &l)| (l, b.as_mut_slice())).collect();
    let most = c.read_vectored(1, &mut reqs, pick, Some(n))?;
    for (k, (b, &l)) in bufs.iter().zip(&lbas).enumerate() {
        if let Some(x) = (0..b.len()).find(|&x| b[x] != fake::pattern(1, l * 512 + x as u64)) {
            panic!("request {k} lba {l}: byte {x} wrong");
        }
    }
    Ok(most)
}

#[test]
fn sixty_four_in_flight_every_byte_checked() {
    let (c, hw) = mq(4, IrqMode::MsiX(5), 64);
    let most = reqs_check(&c, 64, &|k| Some(k % 4)).unwrap();
    assert_eq!(most, 64);
    assert_eq!(ld(&c.stats.inflight_max), 64);
    let qs = c.queue_stats();
    assert_eq!(qs.iter().map(|q| q.placed).sum::<u64>(), 64);
    assert!(qs.iter().all(|q| q.inflight == 0 && q.free == q.tags && q.pool > 0), "{qs:?}");
    assert_eq!(ld(&c.stats.stale), 0);
    // read_speed: as many in flight as the queue has tags (here the commands of 1 MiB at MDTS 8 KiB).
    hw.st.lock().mdts = 1;
    c.restart().unwrap();
    let (bytes, _, most) = c.read_speed(1, 0, 1 << 20).unwrap();
    assert_eq!((bytes, most), (1 << 20, 128));
    no_writes(&c, &hw);
}

#[test]
fn a_timeout_on_one_queue_leaves_the_others_working() {
    let (c, hw) = mq(4, IrqMode::MsiX(5), 64);
    {
        let mut st = hw.st.lock();
        st.inject.drop_io = 1;
        st.inject.drop_on = Some(2);
    }
    reqs_check(&c, 64, &|k| Some(k % 4)).unwrap();
    assert_eq!((ld(&c.stats.timeouts), ld(&c.stats.aborts), ld(&c.stats.resets)), (1, 1, 0));
    assert_eq!(c.state(), State::Ready);
    let qs = c.queue_stats();
    for q in qs.iter().filter(|q| q.qid != 2) {
        assert_eq!((q.placed, q.completed), (16, 16), "{q:?}");
    }
    assert_eq!(qs[1].placed, 17, "queue 2 placed its lost command once more");
    // An Abort that leaves it out: the controller is reset; the commands of the other queues that had not
    // completed yet go again on the new queues.
    {
        let mut st = hw.st.lock();
        st.inject.drop_io = 1;
        st.inject.drop_on = Some(3);
        st.inject.abort_ignored = true;
    }
    reqs_check(&c, 64, &|k| Some(k % 4)).unwrap();
    assert_eq!(ld(&c.stats.resets), 1);
    assert_eq!(c.state(), State::Ready);
    assert_eq!(hw.st.lock().enables, 2);
    no_writes(&c, &hw);
}

/// A Quiesce with many commands out (none completes): every one ends with Gone, nothing hangs, the controller is
/// off; a restart brings it back.
#[test]
fn quiesce_with_many_in_flight() {
    let (c, hw) = mq(2, IrqMode::MsiX(3), 64);
    hw.st.lock().inject.drop_io = 1000;
    let q = c.clone();
    hw.hook(fake::HookAt::NthIoDoorbell(10), move || q.quiesce());
    assert_eq!(reqs_check(&c, 20, &|k| Some(k % 2)).err(), Some(CmdError::Gone));
    assert_eq!(c.state(), State::Quiesced);
    assert_eq!(ld(&c.stats.resets), 0);
    assert_eq!(ld(&c.stats.inflight), 0);
    {
        let st = hw.st.lock();
        assert_eq!(st.cc & regs::CC_EN, 0);
        assert!(!st.bus_master);
    }
    hw.st.lock().inject.drop_io = 0;
    c.restart().unwrap();
    reqs_check(&c, 20, &|k| Some(k % 2)).unwrap();
    no_writes(&c, &hw);
}

struct NoBell;
impl Bell for NoBell {
    fn ring(&self, _: usize, _: u32) {}
}

/// The CID carries the tag and its generation: never FFFFh, a completion of an older generation is stale.
#[test]
fn cids_carry_tag_and_generation() {
    let q: IoQueue<VecMem> = IoQueue::new(1, 6, mem(0x1000, 6 * 64), mem(0x2000, 6 * 16), Some(1), None, (0x1008, 0x100C), 4, 1000).unwrap();
    assert_eq!(q.tag_count(), 5);
    let t = q.tags.get().unwrap();
    let mut seen = Vec::new();
    for _ in 0..40000 {
        let (s, cid) = q.arm(t);
        assert_ne!(cid, 0xFFFF);
        assert_eq!(cid & 7, t);
        seen.push(s);
        q.disarm(t, s);
    }
    let (s, cid) = q.arm(t);
    let stale = AtomicU64::new(0);
    let post = |slot: usize, cid: u16, phase: u32| {
        assert!(q.cq.lock().mem.copy_in(slot * 16 + 8, &(1u32 << 16).to_le_bytes()));
        assert!(q.cq.lock().mem.copy_in(slot * 16 + 12, &(cid as u32 | phase << 16).to_le_bytes()));
    };
    // An older generation's late completion: stale, the slot stays in flight.
    post(0, cid.wrapping_sub(8), 1);
    assert_eq!(q.reap(&NoBell, &stale, false), 1);
    assert_eq!(stale.load(Ordering::Relaxed), 1);
    assert!(q.done(t, s).is_none());
    post(1, cid, 1);
    assert_eq!(q.reap(&NoBell, &stale, false), 1);
    assert_eq!(q.done(t, s).map(|c| c.cid), Some(cid));
    // The same completion twice: the second is stale.
    post(2, cid, 1);
    q.reap(&NoBell, &stale, false);
    assert_eq!(stale.load(Ordering::Relaxed), 2);
    q.disarm(t, s);
    q.tags.put(t);
    assert_eq!(q.tags.free(), 5);
    let all: Vec<u16> = (0..5).map(|_| q.tags.get().unwrap()).collect();
    assert!(q.tags.get().is_none());
    assert_eq!(all.iter().copied().collect::<std::collections::BTreeSet<_>>().len(), 5);
}

/// The bounce counter and the controller's byte count follow the reads (the fake reaches no host memory in place:
/// every byte bounces). The counter is shared by every test of the process, so it can only have grown more.
#[test]
fn bounce_bytes_are_counted() {
    let (c, _hw) = mq(4, IrqMode::MsiX(5), 64);
    let (b0, r0) = (crate::ctrl::COPY_NVME_BOUNCE.sum(), ld(&c.stats.bytes_read));
    reqs_check(&c, 64, &|k| Some(k % 4)).unwrap();
    let want: u64 = (0..64u64).map(|k| 512 * (1 + k % 9)).sum();
    assert_eq!(ld(&c.stats.bytes_read) - r0, want);
    assert!(crate::ctrl::COPY_NVME_BOUNCE.sum() - b0 >= want);
}

/// A device that reads all ones is given up like every other lost command: stopped (bus master off, stop_gen up)
/// before the command's pages go back, Failed, and the next command fails at once.
#[test]
fn a_vanished_device_is_given_up() {
    let (c, hw) = mq(2, IrqMode::MsiX(3), 64);
    hw.st.lock().inject.drop_io = 1;
    let h = hw.clone();
    hw.hook(fake::HookAt::IoDoorbell, move || h.st.lock().inject.gone = true);
    let g0 = c.stop_gen.load(Ordering::Acquire);
    let mut b = vec![0u8; 4096];
    assert_eq!(c.read(1, 0, &mut b, true), Err(CmdError::Gone));
    assert_eq!(c.state(), State::Failed);
    assert!(c.stop_gen.load(Ordering::Acquire) > g0);
    assert!(!hw.st.lock().bus_master);
    assert_eq!(ld(&c.stats.inflight), 0);
    hw.st.lock().inject.gone = false;
    let seen = hw.st.lock().seen_io.len();
    assert_eq!(c.read(1, 0, &mut b, true), Err(CmdError::NotReady));
    assert_eq!(hw.st.lock().seen_io.len(), seen, "nothing placed");
    c.restart().unwrap();
    reqs_check(&c, 8, &|k| Some(k % 2)).unwrap();
}

/// Two commands time out on different queues and the first one's recovery resets the controller: the second one
/// sends no Abort to the new controller (its qid/cid there name another command), it is placed once more.
#[test]
fn no_abort_for_a_command_of_reset_queues() {
    let (c, hw) = mq(4, IrqMode::MsiX(5), 64);
    {
        let mut st = hw.st.lock();
        st.inject.drop_io = 1;
        st.inject.drop_on = Some(1);
        st.inject.abort_ignored = true;
    }
    // Queue index 1's commands time out at once, after queue 1's lost one reset the controller.
    assert!(c.inject_timeouts(1, true));
    reqs_check(&c, 16, &|k| Some(k % 4)).unwrap();
    assert_eq!(ld(&c.stats.resets), 1);
    assert_eq!(ld(&c.stats.aborts), 1, "only the lost command's Abort");
    assert_eq!(hw.st.lock().aborts_seen, 1);
    assert_eq!(c.state(), State::Ready);
    assert!(c.queue_stats().iter().all(|q| q.inflight == 0 && q.free == q.tags));
    no_writes(&c, &hw);
}

/// read_speed holds every tag of the queue; a lost command among them is reset and placed again, and the call
/// ends with every byte read (a retry never waits for a tag while the call holds tags).
#[test]
fn a_retry_with_every_tag_held_goes_on() {
    let (c, hw) = mq(1, IrqMode::MsiX(2), 64);
    hw.st.lock().mdts = 1;
    c.restart().unwrap();
    {
        let mut st = hw.st.lock();
        st.inject.drop_io = 1;
        st.inject.abort_ignored = true;
    }
    let (bytes, _, most) = c.read_speed(1, 0, 1 << 20).unwrap();
    assert_eq!(bytes, 1 << 20);
    assert!(most > 1);
    assert_eq!(ld(&c.stats.resets), 1);
    assert!(c.queue_stats().iter().all(|q| q.inflight == 0 && q.free == q.tags));
}

/// Quiesce, change what the controller grants and the vectors there are, restart: the queues, vectors and tag sets
/// are built again from the new answer, and reads still verify.
#[test]
fn a_restart_builds_the_queues_from_the_new_grant() {
    let (c, hw) = mq(8, IrqMode::MsiX(9), 64);
    assert_eq!(c.queue_stats().len(), 8);
    for (irq, grant, want, v) in [
        (IrqMode::MsiX(9), 3, 3, Some(1)),
        (IrqMode::MsiX(2), 64, 1, Some(1)),
        (IrqMode::Msi, 64, 1, Some(0)),
        (IrqMode::Polled, 64, 8, None),
        (IrqMode::MsiX(9), 64, 8, Some(1)),
    ] {
        c.quiesce();
        {
            let mut st = hw.st.lock();
            st.irq = irq;
            st.grant_queues = grant;
        }
        c.restart().unwrap();
        let qs = c.queue_stats();
        assert_eq!(qs.len(), want, "{irq:?} grant {grant}");
        assert_eq!(c.info().queues.len(), want);
        assert_eq!(qs[0].vector, v, "{irq:?}");
        assert!(qs.iter().all(|q| q.free == q.tags && q.tags > 0 && q.inflight == 0), "{qs:?}");
        reqs_check(&c, 32, &|k| Some(k % want)).unwrap();
    }
    no_writes(&c, &hw);
}

/// No two commands out at once share a page (data, PRP list, DSM range), and a lost command's pages are not handed
/// to any other command before the device stopped (the reset).
#[test]
fn pool_pages_are_never_shared() {
    use crate::io::{Bufs, Fin};
    let (c, hw) = mq(4, IrqMode::MsiX(5), 64);
    c.arm_window(1, 4000, 1000);
    let mut bufs: Vec<Vec<u8>> = (0..48).map(|k| vec![0u8; 512 * (1 + k % 40)]).collect();
    let mut reqs: Vec<(u64, &mut [u8])> = bufs.iter_mut().enumerate().map(|(k, b)| (k as u64 * 50, b.as_mut_slice())).collect();
    assert_eq!(c.read_vectored(1, &mut reqs, &|k| Some(k % 3), None).unwrap(), 2, "bounce reads: two at once per call");
    drop(reqs);
    // 48 reads of 0.5..20 KiB at once on queues 1-3 (lists from 3 pages on); at the 20th doorbell a DSM on queue 4
    // (the hook runs inside the 20th placement, which holds its own queue's submission lock).
    let cpu4 = c.info().queues[3].cpu.unwrap() as usize;
    let cc = c.clone();
    hw.hook(fake::HookAt::NthIoDoorbell(20), move || {
        fake::on_cpu(cpu4);
        cc.deallocate(1, 4000, 8).unwrap();
    });
    let mut reqs: Vec<(u64, &mut [u8])> = bufs.iter_mut().enumerate().map(|(k, b)| (k as u64 * 50, b.as_mut_slice())).collect();
    c.read_vectored(1, &mut reqs, &|k| Some(k % 3), Some(48)).unwrap();
    drop(reqs);
    for (k, b) in bufs.iter().enumerate() {
        assert!((0..b.len()).all(|x| b[x] == fake::pattern(1, k as u64 * 50 * 512 + x as u64)), "request {k}");
    }
    {
        let st = hw.st.lock();
        let n = st.seen_cmds.len();
        // The last 48 reads were all out at once, the DSM came while the first 19 of them were.
        let reads: Vec<&fake::SeenCmd> = st.seen_cmds[n - 49..].iter().filter(|s| s.op == 0x02).collect();
        assert_eq!(reads.len(), 48);
        let dsm = st.seen_cmds.iter().rposition(|s| s.op == 0x09).unwrap();
        assert_eq!(dsm, n - 49 + 20, "the DSM came after the 20th read");
        assert_eq!(st.seen_cmds[dsm].qid, 4);
        let mut all = std::collections::BTreeSet::new();
        for s in st.seen_cmds[n - 49..].iter() {
            if s.op == 0x02 {
                for p in &s.pages {
                    assert!(all.insert(*p), "page {p:#x} in two commands at once");
                }
            }
        }
        assert!(reads.iter().filter(|s| s.pages.len() > 2).count() > 10, "lists were used");
        let early = &st.seen_cmds[n - 49..dsm];
        for p in &st.seen_cmds[dsm].pages {
            assert!(early.iter().all(|s| !s.pages.contains(p)), "the range page {p:#x} is a live read's");
        }
    }
    // A lost three-page read with a list stays out while 40 reads run on its queue: none gets its pages.
    {
        let mut st = hw.st.lock();
        st.inject.drop_io = 1;
        st.inject.abort_ignored = true;
    }
    let make = |_: usize, b: &mut Bufs<VecMem>| -> Result<IoCmd, CmdError> {
        for _ in 0..4 {
            b.page()?;
        }
        assert!(b.pages[3].wr64(0, b.pages[1].dev_addr()) && b.pages[3].wr64(8, b.pages[2].dev_addr()));
        Ok(IoCmd::read(1, 0, 24, b.pages[0].dev_addr(), b.pages[3].dev_addr()))
    };
    let t = c.place(Some(1), true, 0, &make).unwrap().unwrap();
    let (lost, epoch) = {
        let st = hw.st.lock();
        let l = st.seen_cmds.last().unwrap();
        assert!(l.lost && l.pages.len() == 4);
        (l.pages.clone(), l.epoch)
    };
    let from = hw.st.lock().seen_cmds.len();
    reqs_check(&c, 40, &|_| Some(1)).unwrap();
    let g0 = c.stop_gen.load(Ordering::Acquire);
    match c.finish(t, true) {
        Fin::Retry(e) => assert_eq!(e, CmdError::Timeout),
        _ => panic!("the lost read is reset and placed again"),
    }
    assert!(c.stop_gen.load(Ordering::Acquire) > g0, "the device stopped before its pages went back");
    let st = hw.st.lock();
    let during: Vec<&fake::SeenCmd> = st.seen_cmds[from..].iter().filter(|s| s.epoch == epoch).collect();
    assert_eq!(during.len(), 40);
    for s in during {
        assert!(lost.iter().all(|p| !s.pages.contains(p)), "a page of the lost read went to cid {:#x}", s.cid);
    }
}

/// Writes and Dataset Management from four CPUs at once (one queue each), reads beside them: only what lies inside
/// nvme_data's window or the nvme_boot window reaches the device; everything else is refused before a command
/// exists. A lost write whose recovery comes after the window was disarmed is refused, not sent again.
#[test]
fn writes_on_every_queue_stay_in_both_windows() {
    const DATA: (u64, u64) = (2000, 1000);
    const BOOT: (u64, u64) = (5000, 500);
    let (c, hw) = mq(4, IrqMode::MsiX(5), 64);
    c.arm_window(1, DATA.0, DATA.1);
    assert!(c.open_boot_window(1, BOOT.0, BOOT.1));
    let before = hw.st.lock().ns[0].data.clone();
    let inside = |l: u64, n: u64| (l >= DATA.0 && l + n <= DATA.0 + DATA.1) || (l >= BOOT.0 && l + n <= BOOT.0 + BOOT.1);
    std::thread::scope(|s| {
        for cpu in 0..4u64 {
            let c = &c;
            s.spawn(move || {
                fake::on_cpu(cpu as usize);
                for i in 0..30u64 {
                    let n = 1 + i % 9;
                    let lba = match i % 6 {
                        0 => DATA.0 + cpu * 200 + i * 3,
                        1 => BOOT.0 + cpu * 100 + i,
                        2 => DATA.0 + 1 - n,
                        3 => BOOT.0 + BOOT.1 - 1,
                        4 => 100 + cpu * 40 + i,
                        _ => DATA.0 + DATA.1 - 1 + i % 2,
                    };
                    let data = vec![(cpu * 31 + i) as u8 | 1; n as usize * 512];
                    let r = if i % 4 == 3 { c.deallocate(1, lba, n) } else { c.write(1, lba, &data, i % 2 == 0) };
                    if inside(lba, n) {
                        assert_eq!(r, Ok(()), "cpu {cpu} lba {lba}+{n}");
                    } else {
                        assert_eq!(r, Err(CmdError::OutsideWindow), "cpu {cpu} lba {lba}+{n}");
                    }
                }
            });
        }
        // Many reads in flight beside them, outside both windows.
        let c = &c;
        s.spawn(move || {
            fake::on_cpu(1);
            for _ in 0..4 {
                reqs_check(c, 32, &|k| Some(k % 4)).unwrap();
            }
        });
    });
    {
        let st = hw.st.lock();
        assert!(st.writes.iter().all(|w| inside(w.lba, w.blocks)), "{:?}", st.writes);
        assert!(st.deallocs.iter().all(|d| inside(d.1, d.2)), "{:?}", st.deallocs);
        let qids: std::collections::BTreeSet<u16> = st.seen_cmds.iter().filter(|s| s.op == 0x01).map(|s| s.qid).collect();
        assert_eq!(qids.len(), 4, "writes on every queue: {qids:?}");
        let now = &st.ns[0].data;
        for blk in 0..(now.len() / 512) as u64 {
            if !inside(blk, 1) {
                let r = blk as usize * 512..(blk as usize + 1) * 512;
                assert!(now[r.clone()] == before[r], "block {blk} outside the windows changed");
            }
        }
    }
    // The lost write: its recovery finds the window disarmed (a hook disarms it at the doorbell).
    let qid = c.info().queues.iter().find(|q| q.cpu == Some(2)).map(|q| q.qid).unwrap();
    {
        let mut st = hw.st.lock();
        st.inject.drop_io = 1;
        st.inject.drop_on = Some(qid);
        st.inject.abort_ignored = true;
    }
    let cc = c.clone();
    hw.hook(fake::HookAt::IoDoorbell, move || cc.disarm_window());
    fake::on_cpu(2);
    let writes = |hw: &FakeHw| hw.st.lock().seen_cmds.iter().filter(|s| s.op == 0x01).count();
    let w0 = writes(&hw);
    assert_eq!(c.write(1, DATA.0, &[0x42u8; 512], false), Err(CmdError::OutsideWindow));
    assert_eq!(writes(&hw), w0 + 1, "the write went once, not again after the reset");
    assert_eq!(ld(&c.stats.resets), 1);
    assert_eq!(c.state(), State::Ready);
    assert_eq!(c.deallocate(1, DATA.0, 1), Err(CmdError::OutsideWindow));
    let st = hw.st.lock();
    assert_eq!(st.seen_cmds.iter().filter(|s| s.op == 0x09 && s.epoch == st.enables).count(), 0);
}

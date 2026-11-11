//! GX-7n: the IH ring on the fakes: the reader's own rules on a plain ring (`MemRing`), the chip's side and the job half on
//! the fake chain (`IhFake`, GR-2's interpreters raising what they raise), the vector path with QEMU's edu device. Nothing
//! here reaches a chip: Safe and rerunnable, also while the real backend is up. Host tests run the same checks (`tests/ih.rs`).

use crate::gpucore::ih::{self as chip, GENERIC0_INT_ENABLE, IhAddrs, TIME_STAMP_INT_ENABLE, TRAP_ENABLE, VM_FAULT_INTERRUPTS};
use crate::gpucore::ihfake::{MemRing, TestLine, iv};
use crate::gpucore::psp::Hw;
use crate::gpucore::fakechip;
use crate::hw::chain::{Chain, ChainOwner};
use crate::hw::frame::RingType;
use crate::hw::heap::Window;
use crate::hw::ih::{FloodCfg, Ih, IhIo, IrqFeed, PASS_ENTRIES, PortIo};
use crate::hw::ihwatch::{BACK_AFTER, IrqWatch, LOST_LIMIT, SETTLE_NS, Told, Wait};
use crate::hw::jobs::JobCfg;
use crate::ipmod::ih6::{self, Iv, Source, f};
use crate::ktests_hw::{IhRig, Rig};
use crate::ktests_hw_jobs::{Cx, DATA, HOLE, Mem, job, kassert_end, stuck_ib, sub, write_ib};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use drm::fence;
use drm::gpu::{GpuDevice, GpuVm};
use linux::errno::ETIME;
use ktest::kassert;

const M: u64 = 1 << 20;
/// Past a suspect's settling, however often the rounds come.
const SETTLED_NS: u64 = 6_000_000;
const FOREVER: FloodCfg = FloodCfg { per_window: u32::MAX, window_ns: 1_000_000_000, hold_ns: 1 };

/// `ns` of the clock with the job thread going (host tests have no thread: the kicks are its rounds).
fn pause(g: &crate::hw::HardwareGpu, ns: u64) {
    let t0 = crate::ktests_hw_jobs::clock();
    while crate::ktests_hw_jobs::clock().saturating_sub(t0) < ns {
        crate::ktests_hw_jobs::give_way(g, ns / 4);
    }
    crate::ktests_hw_jobs::give_way(g, 1_000_000);
}

fn eop() -> [u32; 8] {
    iv(ih6::CLIENT_GRBM_CP, ih6::SRC_CP_EOP, 0, 0, 0).encode()
}

fn reader(mem: &Arc<MemRing>, cfg: FloodCfg) -> Result<Arc<Ih>, alloc::string::String> {
    let ih = Ih::start(IrqFeed::new(), mem.clone(), TestLine::new(), cfg)?;
    ih.ring_on();
    Ok(ih)
}

/// The entry layout (amdgpu_ih.c:252), the routing table, the registers against what Linux left on the Mini-PC.
pub(crate) fn check_format() -> ktest::TestResult {
    let d = [0x82b3_c4d5u32, 0x1122_3344, 0x8000_5566, 0x0077_8899, 1, 2, 3, 4];
    let v = Iv::decode(&d);
    kassert!((v.client, v.src, v.ring_id, v.vmid, v.vmid_src) == (0xd5, 0xc4, 0xb3, 2, 1), "{v:?}");
    kassert!(v.timestamp == 0x5566_1122_3344 && v.timestamp_src == 1, "{v:?}");
    kassert!(v.pasid == 0x8899 && v.node_id == 0x77 && v.data == [1, 2, 3, 4], "{v:?}");
    kassert!(v.encode() == [d[0], d[1], d[2], d[3], 1, 2, 3, 4], "encode");
    let fault = Iv { data: [0x123, 0xa, 0, 0], ..Iv::default() };
    kassert!(fault.fault_addr() == 0x123 << 12 | 0xa << 44, "{:#x}", fault.fault_addr());
    for (c, s, want) in [
        (0x14, 181, Source::Eop),
        (0x14, 184, Source::PrivReg),
        (0x14, 185, Source::PrivInstr),
        (0x0a, 49, Source::SdmaTrap),
        (0x0a, 0, Source::GfxFault),
        (0x12, 0, Source::MmhubFault),
        (0x00, 0, Source::IhSelf),
        (0x04, 9, Source::DcnHpd),
        (0x04, 0x1e, Source::DcnVline0),
        (0x04, 0x3c, Source::DcnVblank),
        (0x04, 0x4f, Source::DcnFlip),
        (0x04, 0x57, Source::DcnVupdate),
        (0x04, 0x68, Source::DcnOutbox),
        (0x04, 0x77, Source::DcnOther),
        (0x1f, 3, Source::Unknown),
    ] {
        kassert!(ih6::route(c, s) == want, "client {c:#x} src {s}: {:?}", ih6::route(c, s));
    }
    kassert!(Source::ALL.iter().enumerate().all(|(i, s)| s.index() == i), "the table's order");
    // The Mini-PC: 0x40610000 cold, 0x40330121 after Linux (bit 31 reads back 0).
    kassert!(ih6::toggled(ih6::rb_cntl(0x4061_0000, ih6::RING_BYTES, true), true) & !f::WPTR_OVERFLOW_CLEAR == 0x4033_0121, "{:#x}", ih6::rb_cntl(0x4061_0000, ih6::RING_BYTES, true));
    kassert!(ih6::doorbell_rptr() == 0x1000_02f0, "{:#x}", ih6::doorbell_rptr());
    kassert!(ih6::doorbell_range(0) == 0x2_0bc0, "{:#x}", ih6::doorbell_range(0));
    kassert!(ih6::bif_interrupt_cntl(0x41) == 0x48, "{:#x}", ih6::bif_interrupt_cntl(0x41));
    Ok(())
}
ktest::ktest!("gx.ih.format", Safe, check_format);

/// The watch: a late interrupt is none lost; three lost in a row and it polls; eight good in a row and it trusts again.
pub(crate) fn check_watch() -> ktest::TestResult {
    let mut w = IrqWatch::new(0, 0);
    let mut now = 1;
    kassert!(w.after_round(0, 0, now).0 == Wait::Interrupts, "nothing moved");
    kassert!(w.after_round(1, 1, now).0 == Wait::Interrupts, "fence with its interrupt");
    let (a, t) = w.after_round(2, 1, now);
    kassert!(a == Wait::Settle && t == Told::default(), "a suspect: {a:?} {t:?}");
    let (a, t) = w.after_round(2, 2, now);
    kassert!(a == Wait::Interrupts && !t.lost, "late, not lost: {a:?} {t:?}");
    // A kick brings the next round at once: too soon to say, with another fence on top; then late after all.
    let (a, _) = w.after_round(3, 2, now);
    kassert!(a == Wait::Settle, "a suspect: {a:?}");
    for (fences, later) in [(3, 1), (4, SETTLE_NS - 2)] {
        let (a, t) = w.after_round(fences, 2, now + later);
        kassert!(a == Wait::Settle && !t.lost, "before its time, {later} ns: {a:?} {t:?}");
    }
    let (a, t) = w.after_round(4, 3, now + SETTLE_NS - 1);
    kassert!(a == Wait::Interrupts && !t.lost, "late, not lost: {a:?} {t:?}");
    let (mut fences, events) = (4u64, 3u64);
    for k in 1..=LOST_LIMIT {
        fences += 1;
        now += SETTLE_NS;
        kassert!(w.after_round(fences, events, now).0 == Wait::Settle, "suspect {k}");
        now += SETTLE_NS;
        let (a, t) = w.after_round(fences, events, now);
        kassert!(t.lost && t.fell_back == (k == LOST_LIMIT), "{k}: {t:?}");
        kassert!(a == if k == LOST_LIMIT { Wait::Poll } else { Wait::Interrupts }, "{k}: {a:?}");
    }
    kassert!(w.polling(), "must poll");
    for k in 1..=BACK_AFTER {
        let (a, t) = w.after_round(fences + u64::from(k), events + u64::from(k), now);
        kassert!(t.came_back == (k == BACK_AFTER), "{k}: {t:?}");
        kassert!(a == if k == BACK_AFTER { Wait::Interrupts } else { Wait::Poll }, "{k}: {a:?}");
    }
    Ok(())
}
ktest::ktest!("gx.ih.watch", Safe, check_watch);

/// Entries over the ring's end, the read pointer every 32 entries, an entry that comes while the pointer is on its way, a
/// pass's bound, garbage from the chip.
pub(crate) fn check_reader() -> ktest::TestResult {
    let mem = MemRing::new();
    let ih = reader(&mem, FOREVER)?;
    kassert!(ih.pass_now() == Default::default(), "an empty ring");
    // Up to the end, then over it.
    let start = ih6::RING_BYTES - 64;
    mem.with(|s| {
        s.wreg = start;
        s.wb = start;
    });
    while ih.pass_now().more {}
    let at = mem.with(|s| (s.rptr, s.rptr_writes));
    kassert!(at.0 == start, "{at:?}");
    for _ in 0..100 {
        mem.push(eop());
    }
    let p = ih.pass_now();
    let (rptr, wreg, writes) = mem.with(|s| (s.rptr, s.wreg, s.rptr_writes));
    kassert!(rptr == wreg && rptr == 100 * 32 - 64, "rptr {rptr:#x} wreg {wreg:#x}");
    kassert!(writes >= at.1 + 3, "the read pointer stays behind: {writes} writes");
    kassert!(ih.feed.count(Source::Eop) >= 100 && p.entries >= 100 && !p.more, "{p:?}");
    // The classic: the chip writes while the pointer goes back; no new message comes for it.
    let before = ih.feed.count(Source::Eop);
    mem.push(eop());
    mem.with(|s| s.on_rptr.push(eop()));
    ih.pass_now();
    kassert!(ih.feed.count(Source::Eop) == before + 2, "the entry behind the read pointer's write is lost: {}", ih.feed.count(Source::Eop) - before);
    // Bounded: the rest stays for the next pass.
    for _ in 0..PASS_ENTRIES + 500 {
        mem.push(eop());
    }
    let p = ih.pass_now();
    kassert!(p.entries == PASS_ENTRIES && p.more, "{p:?}");
    let p = ih.pass_now();
    kassert!(p.entries == 500 && !p.more, "{p:?}");
    // A chip that says nonsense: cut to an entry's place, never followed past it, no hang.
    let at = mem.with(|s| s.rptr);
    mem.with(|s| s.wb = (at + 32 + 0x13) & (ih6::RING_BYTES - 1) & !1);
    kassert!(ih.pass_now().entries == 1, "a misaligned write pointer");
    mem.with(|s| {
        s.wreg = s.rptr;
        s.wb = s.rptr;
        s.bad_reads = true;
    });
    let bad = ih.feed.bad_reads.load(Ordering::Acquire);
    kassert!(ih.pass_now().entries == 0 && ih.feed.bad_reads.load(Ordering::Acquire) > bad, "unreadable pointer");
    ih.stop();
    kassert!(ih.pass_now() == Default::default(), "a stopped reader reads");
    Ok(())
}
ktest::ktest!("gx.ih.reader", Safe, check_reader);

/// Overflow as Linux: the write-back flags it, the register decides, reading goes on behind the write pointer, the flag is
/// cleared through WPTR_OVERFLOW_CLEAR, and it goes on working.
pub(crate) fn check_overflow() -> ktest::TestResult {
    let mem = MemRing::new();
    let ih = reader(&mem, FOREVER)?;
    let cap = ih6::RING_BYTES / ih6::ENTRY_BYTES;
    for _ in 0..cap + 10 {
        mem.push(eop());
    }
    let (wb, wreg) = mem.with(|s| (s.wb, s.wreg));
    kassert!(wb & f::RB_OVERFLOW != 0 && wreg & f::RB_OVERFLOW != 0, "the ring did not overflow");
    let mut total = 0;
    loop {
        let p = ih.pass_now();
        total += p.entries;
        if !p.more {
            break;
        }
    }
    kassert!(ih.feed.overflows.load(Ordering::Acquire) == 1, "{} overflows", ih.feed.overflows.load(Ordering::Acquire));
    kassert!(mem.with(|s| s.clears) == 1, "WPTR_OVERFLOW_CLEAR written {} times", mem.with(|s| s.clears));
    kassert!(total == cap - 1, "the not overwritten entries: {total} of {}", cap - 1);
    kassert!(mem.with(|s| s.rptr == s.wreg), "the ring is not empty after it");
    mem.push(eop());
    kassert!(ih.pass_now().entries == 1 && ih.feed.overflows.load(Ordering::Acquire) == 1, "it does not go on");
    // A stale flag in the write-back alone: the register decides, nothing jumps.
    mem.push(eop());
    mem.with(|s| s.wb |= f::RB_OVERFLOW);
    kassert!(ih.pass_now().entries == 1 && ih.feed.overflows.load(Ordering::Acquire) == 1, "a stale flag counted");
    ih.stop();
    Ok(())
}
ktest::ktest!("gx.ih.overflow", Safe, check_overflow);

/// A flood of one source: counted, held back for the hold, its hook not called; another source goes on; it ends by itself.
pub(crate) fn check_flood() -> ktest::TestResult {
    let mem = MemRing::new();
    let cfg = FloodCfg { per_window: 10, window_ns: 1 << 40, hold_ns: 1 << 30 };
    let ih = reader(&mem, cfg)?;
    let feed = &ih.feed;
    let hpd = Arc::new(AtomicU64::new(0));
    let flip = Arc::new(AtomicU64::new(0));
    let (a, b) = (hpd.clone(), flip.clone());
    feed.set_hook(Source::DcnHpd, Some(Arc::new(move |_: &Iv| {
        a.fetch_add(1, Ordering::Relaxed);
    })));
    feed.set_hook(Source::DcnFlip, Some(Arc::new(move |_: &Iv| {
        b.fetch_add(1, Ordering::Relaxed);
    })));
    let t0 = 1 << 41;
    for _ in 0..30 {
        mem.push(iv(ih6::CLIENT_DCN, 9, 0, 0, 0).encode());
    }
    mem.push(iv(ih6::CLIENT_DCN, 0x4f, 0, 0, 0).encode());
    ih.pass_at(t0);
    kassert!(feed.count(Source::DcnHpd) == 30, "counted: {}", feed.count(Source::DcnHpd));
    kassert!(hpd.load(Ordering::Relaxed) == 10, "the hook past the limit: {}", hpd.load(Ordering::Relaxed));
    kassert!(flip.load(Ordering::Relaxed) == 1, "the flood took another source with it");
    kassert!(feed.flood_holds.load(Ordering::Acquire) == 1, "{} holds", feed.flood_holds.load(Ordering::Acquire));
    mem.push(iv(ih6::CLIENT_DCN, 9, 0, 0, 0).encode());
    ih.pass_at(t0 + 1);
    kassert!(hpd.load(Ordering::Relaxed) == 10, "still held");
    // The hold ends.
    mem.push(iv(ih6::CLIENT_DCN, 9, 0, 0, 0).encode());
    ih.pass_at(t0 + (1 << 31));
    kassert!(hpd.load(Ordering::Relaxed) == 11, "after the hold: {}", hpd.load(Ordering::Relaxed));
    kassert!(feed.flood_held.load(Ordering::Acquire) == 21, "held: {}", feed.flood_held.load(Ordering::Acquire));
    // Past the limit on a fence source: every event counts (the job thread's look must not read them as lost).
    for _ in 0..40 {
        mem.push(eop());
    }
    ih.pass_at(t0 + (1 << 32));
    kassert!(feed.fence_events.load(Ordering::Acquire) == 40, "{}", feed.fence_events.load(Ordering::Acquire));
    kassert!(feed.flood_holds.load(Ordering::Acquire) == 2, "EOP's hold");
    ih.stop();
    Ok(())
}
ktest::ktest!("gx.ih.flood", Safe, check_flood);

fn rig() -> Result<(Rig, IhRig), alloc::string::String> {
    let d = fakechip::discovery_780m();
    let r = Rig::chain(&d, 512 * M)?;
    let x = r.start_ih(&d)?;
    Ok((r, x))
}

fn jobs_on(r: &Rig, x: &IhRig) -> Result<(), alloc::string::String> {
    r.gpu.start_jobs_irq(r.fake_chain(), r.vmi, JobCfg::default(), Some(x.ih.feed.clone()))
}

/// The bring-up in Linux's order, what it leaves, and the stop's way back; the display's registers are never named.
pub(crate) fn check_chip_side() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    let r = Rig::chain(&d, 512 * M)?;
    let a = IhAddrs::from_discovery(&d).ok_or("no addresses")?;
    let mark = r.fake(|f| f.writes.len());
    let x = r.start_ih(&d)?;
    let writes: Vec<(u32, u32)> = r.fake(|f| f.writes[mark..].to_vec());
    // Off, the write-back, nbio, enable_ring, the range, flood control, on.
    let mut order: Vec<u32> = vec![a.cntl, a.rptr, a.wptr];
    order.extend([a.rsmu_index, a.rsmu_index, a.rsmu_data, a.rsmu_index, a.rsmu_index, a.rsmu_data]);
    order.extend([a.base, a.base_hi, a.cntl, a.wptr_lo, a.wptr_hi, a.wptr, a.rptr, a.doorbell_rptr]);
    order.extend([a.rsmu_index, a.rsmu_index, a.rsmu_data]);
    order.extend([a.storm_list, a.flood, a.msi_storm, a.cntl]);
    let got: Vec<u32> = writes.iter().map(|w| w.0).take(order.len()).collect();
    kassert!(got == order, "the order differs:\n{got:x?}\n{order:x?}");
    r.fake(|f| -> ktest::TestResult {
        kassert!(f.reg(a.cntl) == 0x4033_0121, "IH_RB_CNTL {:#x} (Linux left 0x40330121)", f.reg(a.cntl));
        kassert!(f.reg(a.doorbell_rptr) == 0x1000_02f0, "{:#x}", f.reg(a.doorbell_rptr));
        kassert!(u64::from(f.reg(a.base)) << 8 | u64::from(f.reg(a.base_hi)) << 40 == x.place.ring_mc, "the ring's address");
        kassert!(u64::from(f.reg(a.wptr_lo)) | u64::from(f.reg(a.wptr_hi)) << 32 == x.place.wptr_mc, "the write-back's address");
        kassert!(f.sdma.port.get(&a.range) == Some(&0x2_0bc0), "{:x?}", f.sdma.port.get(&a.range));
        kassert!(f.sdma.port.get(&a.int_cntl2) == Some(&((x.place.dummy_phys >> 8) as u32)), "the dummy page");
        kassert!(f.sdma.port.get(&a.int_cntl).is_some_and(|v| v & f::IH_REQ_NONSNOOP_EN != 0 && v & f::IH_DUMMY_RD_OVERRIDE == 0), "BIF_INTERRUPT_CNTL");
        kassert!(f.reg(a.flood) & f::FLOOD_CNTL_ENABLE != 0 && f.reg(a.storm_list) & f::CLIENT18_IS_STORM_CLIENT != 0 && f.reg(a.msi_storm) & f::MSI_STORM_DELAY_MASK == 3, "flood control");
        Ok(())
    })?;
    r.with_owner(|o| -> ktest::TestResult {
        kassert!(o.reg_read(a.sdma0_cntl).unwrap_or(0) & TRAP_ENABLE != 0, "SDMA trap");
        kassert!(o.reg_read(a.cp_int_ring0).unwrap_or(0) & (TIME_STAMP_INT_ENABLE | GENERIC0_INT_ENABLE) == TIME_STAMP_INT_ENABLE | GENERIC0_INT_ENABLE, "CP ring 0");
        kassert!(a.cp_me1_pipe.iter().all(|&p| o.reg_read(p).unwrap_or(0) & TIME_STAMP_INT_ENABLE != 0), "the MEC's pipes");
        kassert!(a.gc_ctx.iter().all(|&c| o.reg_read(c).unwrap_or(0) & VM_FAULT_INTERRUPTS == VM_FAULT_INTERRUPTS), "GFXHUB's contexts");
        Ok(())
    })?;
    // MMHUB's contexts and the display's registers: not one write (the fake's guard and the list above).
    let hub = crate::gpucore::gmc::HubAddrs::from_discovery(&d, crate::ipmod::gmc11::Hub::Mm).ok_or("MMHUB")?;
    let mm: Vec<u32> = (0..16).filter_map(|n| hub.byte(crate::ipmod::gmc11::R::Ctx(n))).collect();
    let writes: Vec<(u32, u32)> = r.fake(|f| f.writes[mark..].to_vec());
    kassert!(writes.iter().all(|w| !mm.contains(&w.0)), "an MMHUB context written");
    r.stop_ih(&x)?;
    r.fake(|f| -> ktest::TestResult {
        kassert!(f.reg(a.cntl) & (f::RB_ENABLE | f::ENABLE_INTR) == 0, "the ring stays on: {:#x}", f.reg(a.cntl));
        kassert!(f.reg(a.doorbell_rptr) == x.found.doorbell_rptr, "the doorbell as found");
        kassert!(f.sdma.port.get(&a.range) == Some(&x.found.range), "the range as found");
        kassert!(f.ih.read_ptr(a.rptr) == 0 && f.ih.read_ptr(a.wptr) == 0, "the pointers");
        kassert!(!f.ih_raise(&iv(ih6::CLIENT_GFX, ih6::SRC_SDMA_TRAP, 0, 0, 0)), "an entry on a ring that is off");
        Ok(())
    })?;
    r.with_owner(|o| -> ktest::TestResult {
        kassert!(o.reg_read(a.sdma0_cntl).unwrap_or(1) & TRAP_ENABLE == 0, "SDMA trap stays on");
        kassert!(o.reg_read(a.cp_int_ring0).unwrap_or(1) & TIME_STAMP_INT_ENABLE == 0, "CP stays on");
        kassert!(a.gc_ctx.iter().all(|&c| o.reg_read(c).unwrap_or(1) & VM_FAULT_INTERRUPTS == 0), "GFXHUB's contexts stay on");
        Ok(())
    })?;
    r.finish()
}
ktest::ktest!("gx.ih.chip_side", Safe, check_chip_side);

/// RPTR_REARM: one message until the read pointer moves; the doorbell moves it only with its side set up; entries left
/// behind it send the next at once. The display's entries are counted, dropped, and cost no write.
pub(crate) fn check_rearm_masked() -> ktest::TestResult {
    let (r, x) = rig()?;
    let trap = iv(ih6::CLIENT_GFX, ih6::SRC_SDMA_TRAP, 0, 0, 0);
    kassert!(r.fake(|f| f.ih_raise(&trap) && f.ih_raise(&trap)), "the ring refuses");
    kassert!(r.fake(|f| f.ih.messages) == 1, "two messages with no read pointer update");
    kassert!(x.ih.pass_now().entries == 2, "the reader");
    kassert!(r.fake(|f| f.ih.messages) == 1 && x.ih.feed.count(Source::SdmaTrap) == 2, "a message for nothing");
    kassert!(r.fake(|f| f.ih_raise(&trap)), "the ring");
    kassert!(r.fake(|f| f.ih.messages) == 2, "no message after the read pointer moved: the classic");
    x.ih.pass_now();
    // The display's sources: an entry that arrives anyway is counted and dropped.
    let mark = r.fake(|f| f.writes.len());
    for src in [9, 0x1e, 0x3c, 0x4f, 0x57, 0x68, 0x77] {
        kassert!(r.fake(|f| f.ih_raise(&iv(ih6::CLIENT_DCN, src, 0, 0, 0))), "the ring");
    }
    x.ih.pass_now();
    let n: u64 = Source::ALL.iter().filter(|s| s.is_display()).map(|s| x.ih.feed.count(*s)).sum();
    kassert!(n == 7, "{n} display entries");
    kassert!(r.fake(|f| f.writes[mark..].iter().all(|w| w.0 == x.a.cntl)), "the reader wrote a register");
    r.stop_ih(&x)?;
    r.finish()
}
ktest::ktest!("gx.ih.rearm_masked", Safe, check_rearm_masked);

/// The proof's jobs with interrupts on: a fence that raises makes one entry (GR-2's check 3), the thread trusts them.
pub(crate) fn check_jobs() -> ktest::TestResult {
    let (r, x) = rig()?;
    jobs_on(&r, &x)?;
    let lines = crate::hw::check::jobs(&r.gpu)?;
    kassert!(lines.len() == 6, "{lines:?}");
    let j = r.gpu.jobs().ok_or("no jobs")?;
    let done = crate::ktests_hw_jobs::clock();
    while x.ih.feed.fence_events.load(Ordering::Acquire) < 6 && crate::ktests_hw_jobs::clock().saturating_sub(done) < 5_000_000_000 {
        crate::ktests_hw_jobs::give_way(&r.gpu, 1_000_000);
    }
    let f = &x.ih.feed;
    kassert!(f.count(Source::Eop) == 4 && f.count(Source::SdmaTrap) == 2, "EOP {} trap {}: {:?}", f.count(Source::Eop), f.count(Source::SdmaTrap), j.describe());
    kassert!(f.lost.load(Ordering::Acquire) == 0 && f.fell_back.load(Ordering::Acquire) == 0, "{:?}", j.describe());
    kassert!(j.waits_on_interrupts(), "{:?}", j.describe());
    kassert!(j.describe().iter().any(|l| l.contains("EOP 4") && l.contains("SDMA trap 2")), "{:?}", j.describe());
    drop(r.gpu.stop_jobs());
    r.stop_ih(&x)?;
    r.finish()
}
ktest::ktest!("gx.ih.jobs", Safe, check_jobs);

/// Messages stop: no job hangs, a counter and an event say it, the thread polls; they come back and it trusts them again.
pub(crate) fn check_lost() -> ktest::TestResult {
    let (r, x) = rig()?;
    jobs_on(&r, &x)?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let ty = RingType::Sdma;
    let j = g.jobs().ok_or("no jobs")?;
    let run = |k: u64| -> ktest::TestResult {
        let (a, e) = job(cx.entity(ty), vec![m.ib(&write_ib(ty, DATA + (k % 16) * 4, k as u32), 0)]);
        g.submit(sub(g, &m, vec![a], Vec::new(), None)).map_err(|x| format!("{x}"))?;
        g.kick();
        kassert_end(g, &e, ty)
    };
    run(1)?;
    let f = &x.ih.feed;
    kassert!(f.lost.load(Ordering::Acquire) == 0, "{:?}", j.describe());
    r.fake(|f| f.ih.lose_messages = true);
    #[cfg(not(test))]
    let seq = klog::next_seq();
    let mut k = 2;
    while f.fell_back.load(Ordering::Acquire) == 0 && k < 40 {
        run(k)?;
        // The suspect's second look.
        pause(g, SETTLED_NS);
        k += 1;
    }
    kassert!(f.fell_back.load(Ordering::Acquire) == 1 && f.lost.load(Ordering::Acquire) >= u64::from(LOST_LIMIT), "{:?}", j.describe());
    kassert!(!j.waits_on_interrupts(), "{:?}", j.describe());
    // In the kernel only: the host's log is one small ring under every test running beside this one (the gate lost the
    // record that way). `fell_back`, counted beside the event, is checked above in both.
    #[cfg(not(test))]
    kassert!(klog::read_since(seq, usize::MAX).iter().any(|r| r.name == "ih.fallback_poll"), "no event");
    kassert!(j.describe().iter().any(|l| l.contains("polls (interrupts did not come)")), "{:?}", j.describe());
    // Back. A look re-arms what the lost messages left.
    r.fake(|f| f.ih.lose_messages = false);
    x.ih.pass_now();
    let at = k;
    while !j.waits_on_interrupts() && k < at + 40 {
        run(k)?;
        crate::ktests_hw_jobs::give_way(g, 1_000_000);
        k += 1;
    }
    kassert!(j.waits_on_interrupts(), "they came back and the thread still polls: {:?}", j.describe());
    drop(m);
    drop(r.gpu.stop_jobs());
    r.stop_ih(&x)?;
    r.finish()
}
ktest::ktest!("gx.ih.lost", Safe, check_lost);

/// The chain, with a reader faster than any round: every entry is counted the moment the fake raised it, also between
/// the round's look at the fences and its end.
struct FastReader(Arc<dyn Chain>, Arc<Ih>);

impl Chain for FastReader {
    fn with(&self, f: &mut dyn FnMut(&mut dyn ChainOwner)) -> bool {
        let done = self.0.with(f);
        self.1.pass_now();
        done
    }
}

/// An entry counted after the round looked at the fences is no lost interrupt when the fence shows in the next round.
pub(crate) fn check_fast_reader() -> ktest::TestResult {
    let (r, x) = rig()?;
    r.gpu.start_jobs_irq(Arc::new(FastReader(r.fake_chain(), x.ih.clone())), r.vmi, JobCfg::default(), Some(x.ih.feed.clone()))?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let ty = RingType::Sdma;
    let f = &x.ih.feed;
    for k in 0..u64::from(LOST_LIMIT) * 3 {
        let (a, e) = job(cx.entity(ty), vec![m.ib(&write_ib(ty, DATA + (k % 16) * 4, k as u32), 0)]);
        g.submit(sub(g, &m, vec![a], Vec::new(), None)).map_err(|x| format!("{x}"))?;
        g.kick();
        kassert_end(g, &e, ty)?;
        pause(g, SETTLED_NS);
    }
    let j = g.jobs().ok_or("no jobs")?;
    kassert!(f.count(Source::SdmaTrap) >= u64::from(LOST_LIMIT) * 3, "the jobs raised nothing: {:?}", j.describe());
    kassert!(f.lost.load(Ordering::Acquire) == 0 && f.fell_back.load(Ordering::Acquire) == 0, "{:?}", j.describe());
    kassert!(j.waits_on_interrupts(), "{:?}", j.describe());
    drop(m);
    drop(r.gpu.stop_jobs());
    r.stop_ih(&x)?;
    r.finish()
}
ktest::ktest!("gx.ih.fast_reader", Safe, check_fast_reader);

/// A fault raises its entry and wakes the look; the job goes on as without interrupts.
pub(crate) fn check_fault() -> ktest::TestResult {
    let (r, x) = rig()?;
    jobs_on(&r, &x)?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let ty = RingType::Sdma;
    let (a, e) = job(cx.entity(ty), vec![m.ib(&write_ib(ty, HOLE, 0xdead), 0)]);
    g.submit(sub(g, &m, vec![a], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    g.kick();
    kassert_end(g, &e, ty)?;
    let t0 = crate::ktests_hw_jobs::clock();
    while x.ih.feed.fault_events.load(Ordering::Acquire) == 0 && crate::ktests_hw_jobs::clock().saturating_sub(t0) < 5_000_000_000 {
        crate::ktests_hw_jobs::give_way(g, 1_000_000);
    }
    kassert!(x.ih.feed.count(Source::GfxFault) >= 1, "no fault entry: {:?}", g.jobs().map(|j| j.describe()));
    kassert!(m.vm.last_fault().is_some_and(|f| f.addr == HOLE & !0xfff), "{:?}", m.vm.last_fault());
    // CPC stops at a fault and no interrupt is coming for that: the poll's grace still finds it.
    let c = RingType::Compute;
    let (a, e) = job(cx.entity(c), vec![m.ib(&write_ib(c, HOLE + 8, 0xdead), 0)]);
    g.submit(sub(g, &m, vec![a], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    g.kick();
    kassert_end(g, &e, c)?;
    kassert!(fence::status(&e.finished) == -(ETIME as i32) && cx.is_guilty(), "the stopped job: {}", fence::status(&e.finished));
    drop(m);
    drop(r.gpu.stop_jobs());
    r.stop_ih(&x)?;
    r.finish()
}
ktest::ktest!("gx.ih.fault", Safe, check_fault);

/// A hang on interrupts: nothing raises, the look finds it at the ring's timeout.
pub(crate) fn check_hang() -> ktest::TestResult {
    let (r, x) = rig()?;
    let cfg = JobCfg { timeouts_us: Some([50_000, 50_000, 50_000]), ..JobCfg::default() };
    r.gpu.start_jobs_irq(r.fake_chain(), r.vmi, cfg, Some(x.ih.feed.clone()))?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let ty = RingType::Gfx;
    let (a, e) = job(cx.entity(ty), vec![m.ib(&stuck_ib(DATA), 0)]);
    g.submit(sub(g, &m, vec![a], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    g.kick();
    kassert_end(g, &e, ty)?;
    kassert!(fence::status(&e.finished) == -(ETIME as i32), "the hung job: {}", fence::status(&e.finished));
    kassert!(g.jobs().is_some_and(|j| j.shared.hangs.load(Ordering::Acquire) == 1), "no hang counted");
    drop(m);
    drop(r.gpu.stop_jobs());
    r.stop_ih(&x)?;
    r.finish()
}
ktest::ktest!("gx.ih.hang", Safe, check_hang);

/// `until=stop` and back, then a hand-over's raw form and the next kernel's start: every time a ring with nothing on it
/// (an older run's write-back reads as nothing), jobs run, sources as they must be.
pub(crate) fn check_cycles() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    let r = Rig::chain(&d, 512 * M)?;
    let a = IhAddrs::from_discovery(&d).ok_or("no addresses")?;
    for round in 0..2 {
        let x = r.start_ih(&d)?;
        kassert!(x.ih.pass_now().entries == 0, "round {round}: entries on a new ring");
        jobs_on(&r, &x)?;
        crate::hw::check::jobs(&r.gpu)?;
        drop(r.gpu.stop_jobs());
        x.ih.pass_now();
        kassert!(x.ih.feed.count(Source::Eop) == 4 && x.ih.feed.count(Source::SdmaTrap) == 2, "round {round}: {:?}", x.ih.feed.describe());
        if round == 0 {
            r.stop_ih(&x)?;
        } else {
            // The hook: the registers alone, the owner not asked; the next kernel's reader is a new one on the same memory.
            kassert!(r.fake(|f| chip::quiesce(f, &a)), "the raw form");
            kassert!(r.fake(|f| f.reg(a.cntl) & (f::RB_ENABLE | f::ENABLE_INTR) == 0), "the ring stays on");
            x.ih.stop();
            let next = r.start_ih_at(&d, x.place)?;
            kassert!(next.ih.pass_now().entries == 0, "the next kernel's ring reads the last one's entries");
            kassert!(r.fake(|f| f.ih_wptr_wb()) == Some(0), "the write-back");
            r.stop_ih(&next)?;
        }
    }
    r.finish()
}
ktest::ktest!("gx.ih.cycles", Safe, check_cycles);

/// What a waiting job costs in rounds: bounded on interrupts, whatever the poll does. The numbers themselves are the
/// Mini-PC's (`until=irq_load`).
#[cfg(not(test))]
fn load() -> ktest::TestResult {
    let (r, x) = rig()?;
    jobs_on(&r, &x)?;
    let lines = crate::hw::check::hold_load(&r.gpu, 100_000_000, &|| r.fake(|f| f.rerun_legacy()))?;
    kassert!(lines.len() == 2, "{lines:?}");
    let rounds = |l: &str| l.split(':').nth(2).and_then(|t| t.split_whitespace().next()).and_then(|n| n.parse::<u64>().ok());
    let on = rounds(&lines[0]).ok_or(format!("{lines:?}"))?;
    kassert!(on <= 12, "{on} rounds in 100 ms on interrupts: {lines:?}");
    drop(r.gpu.stop_jobs());
    r.stop_ih(&x)?;
    r.finish()
}
#[cfg(not(test))]
ktest::ktest!("gx.ih.load", Safe, load);

/// The ring's buffer against the display's: the GOP's surface ends inside a piece (2560x1440 at 4 bytes leaves its tail
/// free), and the write-back's first write maps the whole piece uncached. The piece must hold nothing else mapped, by
/// the same list a run's `PspHw` refuses by (`psplive`: everything in use but the pool).
pub(crate) fn check_bo_piece() -> ktest::TestResult {
    use crate::bo::{BoManager, Reserved};
    use crate::gpucore::psp::BoAlloc;
    use crate::gpucore::pspbringup::{Pool, Pooled};
    use crate::psplive::{CHUNK, meets};
    let gop = 0..2560 * 1440 * 4;
    let mut bos = BoManager::new(512 * M, vec![Reserved { name: "GOP".into(), range: gop.clone() }]);
    let mut pool = Pool::new();
    let (_, r) = chip::alloc_bo(&mut Pooled(&mut pool, &mut bos)).map_err(|e| format!("{e:?}"))?;
    kassert!(r.end - r.start == chip::BO_BYTES, "{r:?}");
    kassert!(r.start % CHUNK == 0, "not at a piece's start: {r:?}");
    let forbid: Vec<_> = bos.used().filter(|x| !pool.holds(x)).collect();
    for c in r.start / CHUNK..=(r.end - 1) / CHUNK {
        kassert!(meets(c * CHUNK..(c + 1) * CHUNK, &forbid).is_none(), "piece {c} of {r:?} meets {forbid:x?}");
    }
    // What 4 KiB alignment gets on the same manager: the tail of the GOP's piece. That is the case this guards.
    let (_, tail) = Pooled(&mut pool, &mut bos).alloc(chip::BO_BYTES, 4096).map_err(|e| format!("{e:?}"))?;
    kassert!(tail.start == gop.end, "the first fit moved: {tail:?}");
    kassert!(meets(tail.start / CHUNK * CHUNK..(tail.start / CHUNK + 1) * CHUNK, &forbid) == Some(gop), "{tail:?}");
    Ok(())
}
ktest::ktest!("gx.ih.bo_piece", Safe, check_bo_piece);

/// What `PortIo` asks of the chip, recorded.
struct Ports {
    log: ksync::SpinLock<Vec<(&'static str, u64, u32)>>,
}

impl crate::hw::ih::IhPorts for Arc<Ports> {
    fn read(&self, byte: u32) -> Option<u32> {
        self.log.lock().push(("read", u64::from(byte), 0));
        Some(0x77)
    }
    fn write(&self, byte: u32, v: u32) -> bool {
        self.log.lock().push(("write", u64::from(byte), v));
        true
    }
    fn doorbell(&self, byte: usize, v: u32) -> bool {
        self.log.lock().push(("doorbell", byte as u64, v));
        true
    }
}

/// A window that answers its offset.
struct OffsetWindow;

impl Window for OffsetWindow {
    fn read64(&self, off: u64) -> Option<u64> {
        Some(off | off << 32)
    }
    fn write64(&self, _off: u64, _value: u64) -> bool {
        false
    }
    fn read(&self, _off: u64, _out: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _off: u64, _data: &[u8]) -> bool {
        false
    }
}

/// `RawRegs` of `psplive`, on a memory: its read filter is what the hook's toggle goes through.
struct FilteredRegs {
    cntl: u32,
    cntl_at: u32,
    writes: Vec<(u32, u32)>,
}

impl Hw for FilteredRegs {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        (crate::state::wide_readable(byte) && byte == self.cntl_at).then_some(self.cntl)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.writes.push((byte, value));
        true
    }
    fn mem_read(&mut self, _mc: u64, _buf: &mut [u8]) -> bool {
        false
    }
    fn mem_write(&mut self, _mc: u64, _data: &[u8]) -> bool {
        false
    }
    fn now_us(&mut self) -> u64 {
        0
    }
    fn delay_us(&mut self, _us: u64) {}
}

/// The glue the chip alone runs: the registers as osssys_6_0_0_offset.h has them (the literals here on purpose, not
/// `ih6`'s), what `PortIo` touches for each call, the doorbell inside the one page mapped and where IH_DOORBELL_RPTR
/// says, the hook's raw form through `RawRegs`' read filter.
pub(crate) fn check_live_glue() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    let a = IhAddrs::from_discovery(&d).ok_or("no addresses")?;
    let oss = |dw: u32| d.reg(crate::gpucore::hwid::OSSSYS, 0, 0, dw).map(|x| x * 4);
    for (byte, dw, name) in [(a.cntl, 0x80, "IH_RB_CNTL"), (a.base, 0x81, "IH_RB_BASE"), (a.base_hi, 0x82, "IH_RB_BASE_HI"), (a.rptr, 0x83, "IH_RB_RPTR"), (a.wptr, 0x84, "IH_RB_WPTR"), (a.wptr_hi, 0x85, "IH_RB_WPTR_ADDR_HI"), (a.wptr_lo, 0x86, "IH_RB_WPTR_ADDR_LO"), (a.doorbell_rptr, 0x87, "IH_DOORBELL_RPTR")] {
        kassert!(oss(dw) == Some(byte), "{name}: {byte:#x}, not {:x?}", oss(dw));
    }
    let ports = Arc::new(Ports { log: ksync::SpinLock::named("gx.ih.test_ports", Vec::new()) });
    let io = PortIo { ports: ports.clone(), a, win: Arc::new(OffsetWindow) };
    kassert!(io.cntl() == Some(0x77) && io.wptr_reg() == Some(0x77) && io.set_cntl(5) && io.set_rptr(0x40), "a call failed");
    let want = [("read", u64::from(a.cntl), 0), ("read", u64::from(a.wptr), 0), ("write", u64::from(a.cntl), 5), ("doorbell", 0xbc0, 0x40)];
    let log = ports.log.lock().clone();
    kassert!(log == want, "{log:x?}");
    kassert!(0xbc0 + 4 <= 4096 && (ih6::doorbell_rptr() & !f::DOORBELL_ENABLE) * 4 == 0xbc0, "the doorbell's place");
    let w = chip::WB_OFFSET;
    kassert!(w == u64::from(ih6::RING_BYTES) && io.wptr_wb() == Some(w as u32), "the write-back's place");
    let mut e = [0u32; 8];
    kassert!(io.entry(32, &mut e) && e == [32, 32, 40, 40, 48, 48, 56, 56], "{e:?}");
    let mut raw = FilteredRegs { cntl: 0x4033_0121, cntl_at: a.cntl, writes: Vec::new() };
    kassert!(chip::quiesce(&mut raw, &a), "the hook's toggle: IH_RB_CNTL does not pass RawRegs' read filter");
    kassert!(raw.writes == [(a.cntl, 0x4033_0121 & !(f::RB_ENABLE | f::ENABLE_INTR)), (a.rptr, 0), (a.wptr, 0)], "{:x?}", raw.writes);
    Ok(())
}
ktest::ktest!("gx.ih.live_glue", Safe, check_live_glue);

/// `ring_off` with passes on their way, one reading and one about to: when it returns neither writes to the chip again.
#[cfg(not(test))]
fn ring_off_waits() -> ktest::TestResult {
    let mem = MemRing::new();
    let ih = reader(&mem, FOREVER)?;
    let (entered, left) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (a, b) = (entered.clone(), left.clone());
    ih.feed.set_hook(Source::DcnHpd, Some(Arc::new(move |_: &Iv| {
        a.fetch_add(1, Ordering::AcqRel);
        sched::sleep_ns(30_000_000);
        b.fetch_add(1, Ordering::AcqRel);
    })));
    mem.push(iv(ih6::CLIENT_DCN, 9, 0, 0, 0).encode());
    let spawn = |ih: &Arc<Ih>| {
        let ih = ih.clone();
        sched::spawn("gx-ih-test", sched::Class::Normal, move || {
            ih.pass_now();
        })
        .map_err(|e| format!("{e:?}"))
    };
    let first = spawn(&ih)?;
    let t0 = time::now_ns();
    while entered.load(Ordering::Acquire) == 0 {
        kassert!(time::now_ns() - t0 < 5_000_000_000, "the pass did not come");
        sched::sleep_ns(100_000);
    }
    // Behind the reader's lock, past its look at `running`.
    let second = spawn(&ih)?;
    sched::sleep_ns(5_000_000);
    ih.ring_off();
    kassert!(left.load(Ordering::Acquire) == 1, "ring_off did not wait for the pass");
    let at = (mem.with(|s| s.rptr_writes), ih.feed.entries.load(Ordering::Acquire));
    kassert!(first.join_timeout(5_000_000_000) && second.join_timeout(5_000_000_000), "a pass stays");
    sched::sleep_ns(10_000_000);
    kassert!(mem.with(|s| s.rptr_writes) == at.0, "a write to the chip after ring_off");
    kassert!(ih.feed.entries.load(Ordering::Acquire) == at.1, "a pass read after ring_off");
    ih.stop();
    Ok(())
}
#[cfg(not(test))]
ktest::ktest!("gx.ih.ring_off_waits", Safe, ring_off_waits);

/// The job thread's look: nothing while the reader is taken, and short when it runs: the rest comes with the next message.
pub(crate) fn check_look() -> ktest::TestResult {
    let mem = MemRing::new();
    let ih = reader(&mem, FOREVER)?;
    for _ in 0..100 {
        mem.push(eop());
    }
    ih.feed.look_now();
    kassert!(ih.feed.count(Source::Eop) == 32, "a look read {}", ih.feed.count(Source::Eop));
    kassert!(mem.with(|s| s.rptr == 32 * 32 && s.rptr_writes > 0), "the read pointer is what re-arms");
    kassert!(ih.feed.late_looks.load(Ordering::Acquire) == 1, "not counted");
    let held = ih.hold_reader();
    ih.feed.look_now();
    drop(held);
    kassert!(ih.feed.count(Source::Eop) == 32 && ih.feed.late_looks.load(Ordering::Acquire) == 1, "a look past a taken reader");
    while ih.pass_now().more {}
    kassert!(ih.feed.count(Source::Eop) == 100, "{}", ih.feed.count(Source::Eop));
    ih.stop();
    Ok(())
}
ktest::ktest!("gx.ih.look", Safe, check_look);

/// The vector path: edu's real MSI wakes the real thread through the line the chip uses; the ring is plain memory, the
/// entries the test's. Up, messages, a storm of raises (the core's guard: the line turns to looks, nothing is lost),
/// the line let go; twice.
#[cfg(not(test))]
fn edu_msi() -> ktest::TestResult {
    const EDU_RAISE: usize = 0x60;
    const EDU_ACK: usize = 0x64;
    const EDU_STATUS: usize = 0x24;
    let n = pci::tree().into_iter().find(|n| (n.vendor, n.device) == (0x1234, 0x11e8)).ok_or("no edu (host/tests/gx/20-ih.sh adds one)")?;
    let f = pci::function(n.loc).ok_or("edu: no function")?;
    f.enable_device(true).map_err(|e| format!("{e}"))?;
    let Some(pci::Bar::Mem { phys, size, .. }) = f.bar(0) else { return ktest::fail("edu: BAR0") };
    let io = hal::mmio::map_device(phys, size.min(0x2_0000) as usize, hal::paging::CacheType::Uncached).map_err(|e| format!("{e:?}"))?;
    let until = |ns: u64, c: &dyn Fn() -> bool| -> bool {
        let end = time::now_ns().saturating_add(ns);
        loop {
            if c() {
                return true;
            }
            if time::now_ns() > end {
                return false;
            }
            sched::sleep_ns(1_000_000);
        }
    };
    for round in 0..2 {
        let line = crate::hw::live::LiveLine::new(&f).map_err(|e| format!("{e}"))?;
        kassert!(crate::hw::ih::IrqLine::describe(&*line).contains("vector"), "{}", crate::hw::ih::IrqLine::describe(&*line));
        let mem = MemRing::new();
        let w = drm::fence::Waiter::new();
        let feed = IrqFeed::new();
        feed.attach(w.clone());
        let ih = Ih::start(feed.clone(), mem.clone(), line.clone(), FloodCfg::default())?;
        ih.ring_on();
        let vs = pci::irq::first_vector(n.loc).ok_or("no vector")?;
        for k in 1..=5u64 {
            mem.push(eop());
            let _ = io.write32(EDU_RAISE, 1);
            kassert!(until(5_000_000_000, &|| feed.fence_events.load(Ordering::Acquire) == k), "round {round}, raise {k}: {} events, {} messages", feed.fence_events.load(Ordering::Acquire), crate::hw::ih::IrqLine::messages(&*line));
            let st = io.read32(EDU_STATUS).unwrap_or(0);
            let _ = io.write32(EDU_ACK, st);
        }
        kassert!(crate::hw::ih::IrqLine::messages(&*line) >= 5, "{} messages", crate::hw::ih::IrqLine::messages(&*line));
        kassert!(feed.late_looks.load(Ordering::Acquire) == 0, "a look had to find them");
        kassert!(hal::vectors::count(vs) >= 5, "the vector counted {}", hal::vectors::count(vs));
        // Far past the core's limit: its guard takes the line, the thread goes on by looks, everything arrives.
        for _ in 0..2000 {
            mem.push(eop());
            let _ = io.write32(EDU_RAISE, 1);
        }
        let st = io.read32(EDU_STATUS).unwrap_or(0);
        let _ = io.write32(EDU_ACK, st);
        kassert!(until(15_000_000_000, &|| feed.fence_events.load(Ordering::Acquire) == 2005), "round {round}: {} of 2005 after the raises", feed.fence_events.load(Ordering::Acquire));
        ih.stop();
        kassert!(pci::irq::first_vector(n.loc).is_none() && !hal::vectors::is_registered(vs), "the vector stays");
    }
    Ok(())
}
#[cfg(not(test))]
ktest::ktest!("gx.ih.edu_msi", Run("gx/20-ih"), edu_msi);

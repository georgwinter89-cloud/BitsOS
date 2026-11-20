//! Wave 44, item RG: the stage `ringtest`, the kernel rings' rungs after `gcring`. Each rung is one bounded step
//! with a plain answer:
//!
//! | rung | what | Linux |
//! |---|---|---|
//! | eop-on | the end-of-pipe interrupts on for the gfx pipe and MEC 1's two pipes | `amdgpu_fence_driver_hw_init` amdgpu_fence.c:608-635 with `gfx_v12_0_set_eop_interrupt_state` gfx_v12_0.c:4752-4873 |
//! | ib-gfx, ib-comp0..3 | per ring an IB that writes 0xDEADBEEF into a write-back slot, framed as Linux frames an IB, its fence awaited with its interrupt | `gfx_v12_0_ring_test_ib` :494-557 over `amdgpu_ib_schedule` amdgpu_ib.c:124-335 |
//! | fence-irq | a fence alone on the gfx ring and on comp_1.0.0: its end-of-pipe entry must arrive through the IH reader | `gfx_v12_0_ring_emit_fence` :4567-4596, `gfx_v12_0_eop_irq` :4875-4933 |
//! | remap | comp_1.0.0 unmapped through MES and mapped again, then Linux's ring test on it | `amdgpu_gfx_disable_kcq` amdgpu_gfx.c:581-596, `gfx_v12_0_kcq_init_queue`'s restore :3488-3495, `amdgpu_gfx_mes_enable_kcq` :698-737 |
//!
//! Linux enables the end-of-pipe interrupts after the blocks (the trace's section late, init.txt 9817-9822) and
//! runs its IB tests from the delayed work two seconds after init (amdgpu_device.c:3005-3011): the stage stands
//! outside the chain, and its register accesses are held against Linux's forms (`ringseq`). The interrupts stay
//! on after the stage, as Linux keeps them from its fence driver's start to its fini. What the rungs take they give
//! back when all passed: the write-back slots, the IB page's GART entries (unbound with the flush through MES);
//! the IB page itself is a system page kept for the boot, as every piece of the card's. A rung that fails keeps
//! the page bound and an IB test's slot taken: a queue may still read the one and write the other.

use super::gart12::{Gart, flush_hdp};
use super::ihring::IhUp;
use super::meskq::{self, KernelQueue};
use super::messtage::{Up, q_stop};
use super::nb::Io;
use super::ringih;
use super::ringops;
use super::ringstage::{State, ring_name, ring_test};
use super::sdmaih;
use super::sdmaq::{sys_read32, sys_write};
use super::stage::{Ctx, Session, Stop, psp_stop, stop};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::gc12ring as rg;
use rdna4_core::hubseq::Op;
use rdna4_core::mes12api::Unmap;
use rdna4_core::pte12::PAGE;

pub const RUNGS: [&str; 8] = ["eop-on", "ib-gfx", "ib-comp0", "ib-comp1", "ib-comp2", "ib-comp3", "fence-irq", "remap"];
/// Linux waits a second for an IB test's fence (`AMDGPU_IB_TEST_TIMEOUT`, amdgpu_ib.c:38): polls of the fence
/// slot `POLL_US` apart, the IH read at each.
pub const FENCE_POLLS: u32 = 10_000;
pub const POLL_US: u64 = 100;
/// How much longer an end-of-pipe entry may take once its fence stands.
pub const EOP_POLLS: u32 = 1_000;
/// The IB test's IB: five dwords (`ib.length_dw = 5`).
const IB_DW: u32 = 5;

struct Rig<'a, 'b> {
    io: &'a mut Io<'b>,
    ih: &'a mut IhUp,
    hdp: (u32, u32),
    out: &'a mut Vec<String>,
    done: &'a mut Vec<(&'static str, bool)>,
}

struct Fenced {
    seq: u32,
    polls: u32,
    eop: bool,
}

impl Rig<'_, '_> {
    fn rung(&mut self, name: &'static str, pass: bool, plain: &'static str, detail: String) -> Result<(), Stop> {
        self.done.push((name, pass));
        klog::event!(Info, "gx", "rdna4.ring.rung", rung = name, pass = pass, what = detail.as_str(); "a ring rung ended");
        self.out.push(format!("rung {name}: {}: {detail}", if pass { "PASS" } else { "FAIL" }));
        if pass { Ok(()) } else { Err(stop(plain, format!("rung {name}: {detail}"))) }
    }

    /// One frame on queue `i` (around an IB, or a fence alone), the doorbell, then the fence awaited with the IH
    /// rings read at each poll.
    fn submit(&mut self, up: &Up, st: &mut State, i: usize, ib: Option<(u64, u32)>) -> Result<Fenced, Stop> {
        let q = KernelQueue::of(up, i).ok_or_else(|| stop("Reihenfolge stimmt nicht", "no such kernel queue"))?;
        let before = ringih::eops(self.ih, &q.kq);
        st.seq[i] = st.seq[i].wrapping_add(1);
        let seq = st.seq[i];
        let words = ringops::ib_frame(&up.place, &q, ib, seq, self.hdp).map_err(q_stop)?;
        st.wptr[i] = ringops::commit(self.io.card, &up.place, &q, st.wptr[i], &words).map_err(q_stop)?;
        let fence_at = up.place.slot_sys(q.slots.fence);
        let (mut polls, mut fenced, mut extra) = (0, false, 0);
        loop {
            polls += 1;
            fenced = fenced || sys_read32(self.io.card, fence_at).map_err(q_stop)? == seq;
            sdmaih::read(self.ih, self.io.card, self.io.n);
            let eop = ringih::eops(self.ih, &q.kq) > before;
            if fenced && (eop || extra >= EOP_POLLS) {
                return Ok(Fenced { seq, polls, eop });
            }
            if fenced {
                extra += 1;
            } else if polls >= FENCE_POLLS {
                let got = sys_read32(self.io.card, fence_at).map_err(q_stop)?;
                let faults = ringih::faults(self.ih, &q.kq);
                return Err(stop("ein Befehlsring hat einen Auftrag nicht beendet", format!("ring {}: fence {seq} did not come within {FENCE_POLLS} polls {POLL_US} us apart (the slot reads {got}; fault entries of the ring {faults}); the stage `state` shows the engines' registers", ring_name(&q.kq))));
            }
            self.io.card.hw().delay_us(POLL_US);
        }
    }

    /// `gfx_v12_0_ring_test_ib` on queue `i`.
    fn ib_test(&mut self, up: &mut Up, st: &mut State, i: usize, name: &'static str, ib_sys: u64, ib_mc: u64) -> Result<(), Stop> {
        let slot = up.pool.get().map_err(|e| q_stop(super::sdmaq::QStop::Slots(e)))?;
        let at = up.place.slot_sys(slot);
        let mut ib = Vec::new();
        let made = sys_write(self.io.card, at, &amdgpu_fmt::gfx12::RING_TEST_POISON.to_le_bytes())
            .and_then(|()| amdgpu_fmt::gfx12::ib_test(&mut ib, up.place.slot_mc(slot)).map_err(ringops::enc))
            .and_then(|()| sys_write(self.io.card, ib_sys, &ib.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>()));
        let f = made.map_err(q_stop).and_then(|()| self.submit(up, st, i, Some((ib_mc, IB_DW))));
        let got = sys_read32(self.io.card, at).map_err(q_stop);
        // An IB whose fence did not come may still run and write the slot: it stays taken for the boot, as an
        // unanswered MES command's status slot does (`mesring::Port::submit`).
        let f = f.map_err(|e| {
            self.out.push(format!("kept after the failed IB test (its IB may still write it): write-back slot {}", slot.index()));
            e
        })?;
        up.pool.free(slot).map_err(|e| q_stop(super::sdmaq::QStop::Slots(e)))?;
        let got = got?;
        let k = up.kq_slots.get(i).map(|_| &rdna4_core::mes12::KERNEL_QUEUES[i]).ok_or_else(|| stop("Reihenfolge stimmt nicht", "no such kernel queue"))?;
        self.rung(name, got == amdgpu_fmt::gfx12::RING_TEST_VALUE, "ein Befehlsring hat einen Auftrag falsch ausgeführt", format!("ring {}: the IB's slot reads {got:#010x} (want 0xdeadbeef), fence {} after {} polls, its end-of-pipe entry came through the IH reader: {}", ring_name(k), f.seq, f.polls, f.eop))
    }
}

pub fn run(s: &mut Session, c: &Ctx, io: &mut Io, out: &mut Vec<String>) -> Result<(), Stop> {
    if !s.rings.passed {
        return Err(stop("zuerst die Befehlsringe prüfen", "ringtest needs the gcring stage of this boot"));
    }
    let hdp = super::sdmaregs::SdmaRegs::from(&c.att.discovery).and_then(|r| r.hdp_flush_dwords()).ok_or_else(|| stop("Bauteilliste nennt die Register nicht", "NBIF's HDP flush registers have no place in BAR5"))?;
    let page = match s.rings.ib_page {
        Some(p) => p,
        None => *s.rings.ib_page.insert(io.card.pages(PAGE).ok_or_else(|| stop("kein Arbeitsspeicher für die Karte frei", "no page for the rings' IBs"))?),
    };
    s.rings.rungs.clear();
    let mut done = Vec::new();
    let r = rungs(s, io, hdp, page, out, &mut done);
    s.rings.rungs = done;
    match r {
        Ok(()) => {
            out.push(String::from("given back: the write-back slots, the IB page's GART entries (unbound and flushed through MES); the IB page stays the card's for the boot"));
            Ok(())
        }
        Err(e) => {
            out.push(format!("kept after the failed rung (a queue may still read it): the IB page at {page:#x}{}", s.rings.ib_mc.map_or(String::new(), |mc| format!(", bound at mc {mc:#x}"))));
            Err(e)
        }
    }
}

fn bind(gart: &mut Gart, io: &mut Io, mes: &mut super::messtage::State, page: u64, out: &mut Vec<String>) -> Result<u64, Stop> {
    let mut tlb = super::mesflush::tlb(io.flush()?, mes);
    gart.map_with(io.card, &mut tlb, "ring IB page", &[page], out).map_err(|e| stop("der Speicher für die Befehlsringe lässt sich nicht einbinden", format!("{e:?}")))
}

fn rungs(s: &mut Session, io: &mut Io, hdp: (u32, u32), page: u64, out: &mut Vec<String>, done: &mut Vec<(&'static str, bool)>) -> Result<(), Stop> {
    let Session { mes, rings, nb, .. } = s;
    let (Some(ih), Some(gart)) = (nb.ih.as_mut(), nb.gart.as_mut()) else { return Err(stop("Reihenfolge stimmt nicht", "no IH or GART")) };
    ringih::register(ih);
    let mut r = Rig { io, ih, hdp, out, done };
    if rings.eop_on {
        r.rung(RUNGS[0], true, "", String::from("the end-of-pipe interrupts are on since an earlier call in this boot; nothing written"))?;
    } else {
        let mut said = Vec::new();
        for op in rg::EOP_ENABLE_OPS {
            let Op::Modify(reg, f) = op else { continue };
            let was = r.io.read(reg)?;
            r.io.write(reg, f(was))?;
            said.push(format!("{} {was:#x} to {:#x}", reg.name, f(was)));
        }
        rings.eop_on = true;
        r.rung(RUNGS[0], true, "", format!("TIME_STAMP and GENERIC0 on: {}", said.join(", ")))?;
    }
    let ib_mc = match rings.ib_mc {
        Some(mc) => mc,
        None => *rings.ib_mc.insert(bind(gart, r.io, mes, page, r.out)?),
    };
    let up = mes.up.as_mut().ok_or_else(|| stop("zuerst den Planer der Karte starten", "MES does not run in this boot"))?;
    for (n, i) in super::ringstage::TEST_ORDER.into_iter().enumerate() {
        r.ib_test(up, rings, i, RUNGS[1 + n], page, ib_mc)?;
    }
    // A fence alone on the gfx ring and on comp_1.0.0: the entry itself is the rung.
    let mut said = Vec::new();
    let mut all = true;
    for i in [4, 0] {
        let f = r.submit(up, rings, i, None)?;
        all &= f.eop;
        said.push(format!("{}: fence {} after {} polls, its entry (client {:#x}, source {}, ring id {:#x}) arrived: {}", ring_name(&rdna4_core::mes12::KERNEL_QUEUES[i]), f.seq, f.polls, rg::CLIENT_GRBM_CP, rg::SRC_EOP, ringih::ring_id(&rdna4_core::mes12::KERNEL_QUEUES[i]), f.eop));
    }
    r.rung(RUNGS[6], all, "die Meldung eines Befehlsrings ist nicht angekommen", said.join("; "))?;
    remap(&mut r, up, rings)?;
    // Everything passed: the IB page leaves the GART again.
    let mut tlb = super::mesflush::tlb(r.io.flush()?, mes);
    gart.unmap_with(r.io.card, &mut tlb, ib_mc, r.out).map_err(|e| stop("der Speicher der Befehlsringe lässt sich nicht wieder ausbinden", format!("{e:?}")))?;
    rings.ib_mc = None;
    Ok(())
}

/// comp_1.0.0 off its pipe as Linux's suspend takes it (`RESET_QUEUES` through MES), restored as its resume restores
/// it (the MQD as built, the write pointer 0, the ring cleared), the HDP flush and its map again, then Linux's ring
/// test on it.
fn remap(r: &mut Rig, up: &mut Up, st: &mut State) -> Result<(), Stop> {
    let q = KernelQueue::of(up, 0).ok_or_else(|| stop("Reihenfolge stimmt nicht", "no such kernel queue"))?;
    let legacy = q.legacy(&up.place);
    let (sent, line) = meskq::unmap_legacy(up, r.io.card, &legacy, Unmap::ResetQueues, (0, 0)).map_err(q_stop)?;
    r.out.push(line.clone());
    if !sent.ok() {
        return r.rung(RUNGS[7], false, "der Planer gibt eine Rechen-Warteschlange nicht frei", format!("REMOVE_QUEUE of {}: {line}", ring_name(&q.kq)));
    }
    let mqd = rdna4_core::mqd12::compute(&q.prop(&up.place, 0));
    let bytes: Vec<u8> = mqd.iter().flat_map(|d| d.to_le_bytes()).collect();
    sys_write(r.io.card, q.mqd_sys, &bytes).map_err(q_stop)?;
    st.wptr[0] = 0;
    sys_write(r.io.card, up.place.slot_sys(q.slots.wptr), &0u64.to_le_bytes()).map_err(q_stop)?;
    ringops::clear(r.io.card, &q).map_err(q_stop)?;
    let f = r.io.flush()?;
    flush_hdp(r.io.card.hw(), &f).map_err(psp_stop)?;
    let (sent, line) = meskq::map_legacy(up, r.io.card, &legacy).map_err(q_stop)?;
    r.out.push(line.clone());
    if !sent.ok() {
        return r.rung(RUNGS[7], false, "der Planer nimmt eine Rechen-Warteschlange nicht wieder an", format!("ADD_QUEUE of {}: {line}", ring_name(&q.kq)));
    }
    let mut lines = Vec::new();
    let t = ring_test(r.io, up, 0, &mut st.wptr[0], &mut lines);
    r.out.extend(lines);
    let passed = t.is_ok();
    let detail = match t {
        Ok(()) => format!("{} unmapped (RESET_QUEUES), restored and mapped again through MES; its ring test passed (write pointer {})", ring_name(&q.kq), st.wptr[0]),
        Err(Stop(_, d)) => format!("{} mapped again, but: {d}", ring_name(&q.kq)),
    };
    r.rung(RUNGS[7], passed, "eine Rechen-Warteschlange arbeitet nach dem neuen Anmelden nicht", detail)
}

//! Wave 43 (piece 12): the stage `sdmatest`, the engines' rungs after their start. Each rung is one bounded step
//! with a plain answer, VRAM targets first and system pages last:
//!
//! | rung | what | Linux |
//! |---|---|---|
//! | ib-test-0, ib-test-1 | an IB that writes 0xDEADBEEF into a write-back slot, framed as Linux frames an IB, its fence awaited | `sdma_v7_0_ring_test_ib` sdma_v7_0.c:979-1045 over `amdgpu_ib_schedule` amdgpu_ib.c:124-335 |
//! | fill-vram | CONST_FILL of two pages of VRAM between two guard pages | `sdma_v7_0_emit_fill_buffer` :1797-1808 |
//! | copy-vram | COPY_LINEAR of 8 KiB VRAM to VRAM, compared byte for byte | `sdma_v7_0_emit_copy_buffer` :1753-1785 |
//! | trap-0, trap-1 | a fence with its trap alone: the trap's entry must arrive through the IH reader | `sdma_v7_0_ring_emit_fence` :358-388, `process_trap_irq` :1544-1571 |
//! | fill-sys | the fill into a system page behind the GART, its neighbours untouched | |
//! | copy-vram-to-sys, copy-sys-to-vram | the copy in both directions across the GART | |
//!
//! Linux runs its IB tests from the delayed work two seconds after init (amdgpu_device.c:3005-3011, queued at
//! :4163-4164), so they are in no section of the init trace, and the rungs behind them are BitsOS's own: the
//! stage stands outside Linux's chain. Its only register accesses are the TLB flushes after the test pages' bind
//! and unbind, and those are held against Linux's flush line for line: the call's answer ends in that
//! `linuxseq` line (`sdmaseq::rungs_verdict`). It needs the `sdma` stage and the IH of this boot and asks for
//! nothing else. The ring (the first rung of the ladder, Linux's ring test) was tested by
//! `sdma` itself, as Linux tests it inside its hw_init.
//!
//! The ring, the slots and the IB stay where Linux has them, in system pages behind the GART: "VRAM first" is the
//! order of the rungs' targets. What the rungs take they give back when all passed: the VRAM range
//! (`vram::Bump::take`) and the GART's entries for the test pages (`Gart::map`, with Linux's flush after the bind
//! and after the unbind). A rung that fails leaves both taken: the engine may still write there.

use super::gart12::Gart;
use super::ihring::IhUp;
use super::nb::{Io, pages_of};
use super::sdmaih;
use super::sdmaq::{self, QStop, lay};
use super::sdmaregs::SdmaRegs;
use super::sdmastage::{Up, q_stop};
use super::stage::{Ctx, Session, Stop, stop};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::ring::pad_ib;
use amdgpu_fmt::{sdma as pkt, sdma7 as pkt7};
use core::ops::Range;
use rdna4_core::pte12::PAGE;
use rdna4_core::sdma7::{self, INSTANCES};

pub const RUNGS: [&str; 9] = ["ib-test-0", "ib-test-1", "fill-vram", "copy-vram", "trap-0", "trap-1", "fill-sys", "copy-vram-to-sys", "copy-sys-to-vram"];

/// The VRAM the rungs take: a guard page, two pages to fill, a guard page, the copy's source and its target.
pub const VRAM_BYTES: u64 = 8 * PAGE;
pub const FILL_AT: u64 = PAGE;
pub const FILL_BYTES: u64 = 2 * PAGE;
pub const SRC_AT: u64 = 4 * PAGE;
pub const DST_AT: u64 = 6 * PAGE;
pub const COPY_BYTES: u64 = 2 * PAGE;
/// The fill's value: its low byte is what a fill of bytes writes, the whole of it what a fill of dwords writes.
pub const FILL_VALUE: u32 = 0xA5;
/// Linux waits a second for an IB test's fence (`AMDGPU_IB_TEST_TIMEOUT`, amdgpu_ib.c:38): here as polls of
/// the fence slot `POLL_US` apart, the IH read at each.
pub const FENCE_POLLS: u32 = 10_000;
pub const POLL_US: u64 = 100;
/// How much longer a trap's entry may take once its fence stands.
pub const TRAP_POLLS: u32 = 1_000;

/// The rungs' VRAM range as MC addresses, taken from the session's manager if the stage is going to run (the
/// stage's guard lists it). A range a failed run left taken is used again.
pub fn take_vram(s: &mut Session) -> Option<Range<u64>> {
    s.sdma.up.as_ref()?;
    let l = s.layout.as_ref()?;
    if s.sdma.held.0.is_none() {
        s.sdma.held.0 = s.bump.as_mut()?.take("SDMA test ranges", VRAM_BYTES, PAGE).ok();
    }
    s.sdma.held.0.as_ref().map(|r| l.mc(r.start)..l.mc(r.end))
}

/// Dwords that differ at every place, none of them zero.
pub fn pattern(seed: u32, bytes: u64) -> Vec<u8> {
    (0..bytes as u32 / 4).flat_map(|i| (seed ^ i.wrapping_mul(0x9E37_79B9) | 1).to_le_bytes()).collect()
}

/// What a fill left in `got`: bytes of the value's low byte, dwords of the value, or neither.
pub fn fill_form(got: &[u8]) -> Option<&'static str> {
    if got.iter().all(|b| *b == FILL_VALUE as u8) {
        return Some("bytes");
    }
    got.chunks_exact(4).all(|d| d == FILL_VALUE.to_le_bytes()).then_some("dwords")
}

struct Rig<'a, 'b> {
    io: &'a mut Io<'b>,
    up: &'a mut Up,
    ih: &'a mut IhUp,
    hdp: (u32, u32),
    out: &'a mut Vec<String>,
    done: &'a mut Vec<(&'static str, bool)>,
}

/// What became of one submission.
struct Fenced {
    seq: u32,
    polls: u32,
    trap: bool,
}

fn vram_stop(mc: u64) -> Stop {
    stop("der Speicher der Karte lässt sich nicht lesen oder beschreiben", format!("VRAM at mc {mc:#x}"))
}

impl Rig<'_, '_> {
    fn vram_write(&mut self, mc: u64, data: &[u8]) -> Result<(), Stop> {
        if self.io.card.hw().mem_write(mc, data) { Ok(()) } else { Err(vram_stop(mc)) }
    }

    fn vram_read(&mut self, mc: u64, bytes: u64) -> Result<Vec<u8>, Stop> {
        let mut b = alloc::vec![0u8; bytes as usize];
        if self.io.card.hw().mem_read(mc, &mut b) { Ok(b) } else { Err(vram_stop(mc)) }
    }

    fn sys_write(&mut self, at: u64, data: &[u8]) -> Result<(), Stop> {
        sdmaq::sys_write(self.io.card, at, data).map_err(q_stop)
    }

    fn sys_read(&mut self, at: u64, bytes: u64) -> Result<Vec<u8>, Stop> {
        let mut b = alloc::vec![0u8; bytes as usize];
        if self.io.card.sys_read(at, &mut b) { Ok(b) } else { Err(q_stop(QStop::Mem { at })) }
    }

    /// The rung's answer: its line, its place in the session's record, and the stop when it failed.
    fn rung(&mut self, name: &'static str, pass: bool, plain: &'static str, detail: String) -> Result<(), Stop> {
        self.done.push((name, pass));
        klog::event!(Info, "gx", "rdna4.sdma.rung", rung = name, pass = pass, what = detail.as_str(); "an SDMA rung ended");
        self.out.push(format!("rung {name}: {}: {detail}", if pass { "PASS" } else { "FAIL" }));
        if pass { Ok(()) } else { Err(stop(plain, format!("rung {name}: {detail}"))) }
    }

    /// One frame on engine `i`'s ring (around `ib`, already in the IB page, or with no IB), the doorbell, then
    /// the fence awaited with the IH rings read at each poll.
    fn submit(&mut self, i: u32, ib_dw: Option<u32>) -> Result<Fenced, Stop> {
        let p = self.up.place;
        let before = sdmaih::traps(self.ih, i);
        let q = &mut self.up.q[i as usize];
        q.read_rptr(self.io.card, &p).map_err(q_stop)?;
        let (words, seq) = q.frame(&p, self.hdp, ib_dw.map(|dw| (p.ib_mc(), dw))).map_err(q_stop)?;
        q.submit(self.io.card, &p, &words).map_err(q_stop)?;
        let (mut polls, mut fenced, mut extra) = (0, false, 0);
        loop {
            polls += 1;
            fenced = fenced || self.up.q[i as usize].read_fence(self.io.card, &p).map_err(q_stop)? == seq;
            sdmaih::read(self.ih, self.io.card, self.io.n);
            let trap = sdmaih::traps(self.ih, i) > before;
            if fenced && (trap || extra >= TRAP_POLLS) {
                return Ok(Fenced { seq, polls, trap });
            }
            if fenced {
                extra += 1;
            } else if polls >= FENCE_POLLS {
                let got = self.up.q[i as usize].read_fence(self.io.card, &p).map_err(q_stop)?;
                return Err(stop("die Kopiereinheit hat einen Auftrag nicht beendet", format!("engine {i}: fence {seq} did not come within {FENCE_POLLS} polls {POLL_US} us apart (the slot reads {got}); the stage `state` shows the engine's registers")));
            }
            self.io.card.hw().delay_us(POLL_US);
        }
    }

    /// `ib` padded to Linux's IB alignment, into the IB page, and submitted on engine `i`.
    fn run_ib(&mut self, i: u32, mut ib: Vec<u32>) -> Result<Fenced, Stop> {
        pad_ib(self.up.q[i as usize].kind(), &mut ib, amdgpu_fmt::ring::SDMA_IB_ALIGN_DW).map_err(|e| q_stop(QStop::Ring(e.into())))?;
        let bytes: Vec<u8> = ib.iter().flat_map(|d| d.to_le_bytes()).collect();
        self.sys_write(self.up.place.sys + lay::IB, &bytes)?;
        self.submit(i, Some(ib.len() as u32))
    }

    /// `sdma_v7_0_ring_test_ib`.
    fn ib_test(&mut self, i: u32, name: &'static str) -> Result<(), Stop> {
        let p = self.up.place;
        let slot = self.up.pool.get().map_err(|e| q_stop(QStop::Slots(e)))?;
        self.sys_write(p.slot_sys(slot), &sdma7::POISON.to_le_bytes())?;
        let mut ib = Vec::new();
        pkt::write_linear(&mut ib, p.slot_mc(slot), &[sdma7::RING_TEST]).map_err(|e| q_stop(QStop::Ring(e.into())))?;
        // Linux fills the IB's eight dwords with three plain NOPs.
        ib.extend([pkt::NOP; 3]);
        let f = self.run_ib(i, ib);
        let got = sdmaq::sys_read32(self.io.card, p.slot_sys(slot)).map_err(q_stop);
        self.up.pool.free(slot).map_err(|e| q_stop(QStop::Slots(e)))?;
        let (f, got) = (f?, got?);
        self.rung(name, got == sdma7::RING_TEST, "die Kopiereinheit hat einen Auftrag falsch ausgeführt", format!("the IB's slot reads {got:#010x} (want {:#010x}), fence {} after {} polls, its trap came through the IH reader: {}", sdma7::RING_TEST, f.seq, f.polls, f.trap))
    }

    fn trap(&mut self, i: u32, name: &'static str) -> Result<(), Stop> {
        let f = self.submit(i, None)?;
        self.rung(name, f.trap, "die Meldung der Kopiereinheit ist nicht angekommen", format!("fence {} after {} polls; its trap (client {}, source {}, ring id {}) came through the IH reader: {}", f.seq, f.polls, sdma7::CLIENT_GFX, sdma7::SRC_TRAP, sdma7::trap_ring_id(i), f.trap))
    }
}

pub fn run(s: &mut Session, c: &Ctx, io: &mut Io, out: &mut Vec<String>) -> Result<(), Stop> {
    let Session { sdma, nb, bump, layout, mes, .. } = s;
    let Some(up) = sdma.up.as_mut() else {
        return Err(stop("zuerst die Kopiereinheit der Karte starten", "sdmatest needs the sdma stage of this boot"));
    };
    let (Some(ih), Some(gart)) = (nb.ih.as_mut(), nb.gart.as_mut()) else {
        return Err(stop("zuerst die Meldungen der Karte einschalten", "no IH rings in this boot: the engines' trap has nowhere to arrive"));
    };
    let (Some(l), Some(vram)) = (layout.as_ref(), sdma.held.0.clone()) else {
        return Err(stop("kein Platz im Speicher der Karte", "no VRAM range for the rungs"));
    };
    let hdp = SdmaRegs::from(&c.att.discovery).and_then(|r| r.hdp_flush_dwords()).ok_or_else(|| stop("Bauteilliste nennt die Register nicht", "NBIF's HDP flush registers have no place in BAR5"))?;
    sdmaih::register(ih);
    sdma.rungs.clear();
    let mut rig = Rig { io, up, ih, hdp, out, done: &mut sdma.rungs };
    let r = rungs(&mut rig, gart, mes, &mut sdma.held.1, l.mc(vram.start));
    match r {
        Ok(()) => {
            let back = bump.as_mut().is_some_and(|b| b.give_back(&vram));
            sdma.held.0 = None;
            rig.out.push(format!("given back: the test pages' GART entries (unbound and flushed), vram {:#x}..{:#x}: {}", vram.start, vram.end, if back { "yes" } else { "it was not the manager's" }));
            Ok(())
        }
        Err(e) => {
            rig.out.push(format!("kept after the failed rung (an engine may still write there): vram {:#x}..{:#x}{}", vram.start, vram.end, sdma.held.1.map_or(String::new(), |mc| format!(", the test pages bound at mc {mc:#x}"))));
            Err(e)
        }
    }
}

fn rungs(r: &mut Rig, gart: &mut Gart, mes: &mut super::messtage::State, test_mc: &mut Option<u64>, v: u64) -> Result<(), Stop> {
    for i in 0..INSTANCES {
        r.ib_test(i, RUNGS[i as usize])?;
    }
    // VRAM: the guard pages and the fill's target hold a pattern, the engine fills the two pages between.
    let wrong_fill = "die Kopiereinheit hat falsch gefüllt";
    let before = pattern(0x1111_0000, VRAM_BYTES / 2);
    r.vram_write(v, &before)?;
    let mut ib = Vec::new();
    pkt7::const_fill(&mut ib, v + FILL_AT, FILL_VALUE, FILL_BYTES).map_err(|e| q_stop(QStop::Ring(e.into())))?;
    let f = r.run_ib(0, ib)?;
    let got = r.vram_read(v, VRAM_BYTES / 2)?;
    let (a, b) = (FILL_AT as usize, (FILL_AT + FILL_BYTES) as usize);
    let guards = got[..a] == before[..a] && got[b..] == before[b..];
    let form = fill_form(&got[a..b]);
    r.rung(RUNGS[2], guards && form.is_some(), wrong_fill, format!("{FILL_BYTES} bytes of VRAM at mc {:#x} filled as {}, the pages before and behind untouched: {guards} (fence {} after {} polls)", v + FILL_AT, form.unwrap_or("neither bytes nor dwords of the value"), f.seq, f.polls))?;
    // VRAM to VRAM.
    let wrong_copy = "die Kopiereinheit hat falsch kopiert";
    let src = pattern(0x2222_0000, COPY_BYTES);
    r.vram_write(v + SRC_AT, &src)?;
    r.vram_write(v + DST_AT, &alloc::vec![0u8; COPY_BYTES as usize])?;
    let mut ib = Vec::new();
    pkt7::copy_linear(&mut ib, v + SRC_AT, v + DST_AT, COPY_BYTES).map_err(|e| q_stop(QStop::Ring(e.into())))?;
    let f = r.run_ib(0, ib)?;
    let got = r.vram_read(v + DST_AT, COPY_BYTES)?;
    let same = got.iter().zip(&src).filter(|(x, y)| x == y).count();
    r.rung(RUNGS[3], got == src, wrong_copy, format!("{same} of {COPY_BYTES} bytes arrived from vram mc {:#x} at {:#x} (fence {} after {} polls)", v + SRC_AT, v + DST_AT, f.seq, f.polls))?;
    for i in 0..INSTANCES {
        r.trap(i, RUNGS[4 + i as usize])?;
    }
    // System pages last: the four test pages into the GART, with Linux's flush after the bind.
    let sys = r.up.place.sys + lay::TEST;
    let t = match *test_mc {
        Some(t) => t,
        None => {
            // The flush in this boot's form: through MES once it runs.
            let mut tlb = super::mesflush::tlb(r.io.flush()?, mes);
            let mut lines = Vec::new();
            let t = gart.map_with(r.io.card, &mut tlb, "SDMA test pages", &pages_of(sys, lay::TEST_BYTES), &mut lines).map_err(|e| stop("der Speicher für die Kopiereinheit lässt sich nicht einbinden", format!("{e:?}")))?;
            r.out.extend(lines);
            *test_mc.insert(t)
        }
    };
    let before = pattern(0x3333_0000, lay::TEST_BYTES);
    r.sys_write(sys, &before)?;
    let mut ib = Vec::new();
    pkt7::const_fill(&mut ib, t + PAGE, FILL_VALUE, PAGE).map_err(|e| q_stop(QStop::Ring(e.into())))?;
    let f = r.run_ib(0, ib)?;
    let got = r.sys_read(sys, lay::TEST_BYTES)?;
    let (a, b) = (PAGE as usize, 2 * PAGE as usize);
    let guards = got[..a] == before[..a] && got[b..] == before[b..];
    let form = fill_form(&got[a..b]);
    r.rung(RUNGS[6], guards && form.is_some(), wrong_fill, format!("one system page behind the GART at mc {:#x} filled as {}, its neighbours untouched: {guards} (fence {} after {} polls)", t + PAGE, form.unwrap_or("neither bytes nor dwords of the value"), f.seq, f.polls))?;
    // VRAM to system pages.
    let mut ib = Vec::new();
    pkt7::copy_linear(&mut ib, v + SRC_AT, t + 2 * PAGE, COPY_BYTES).map_err(|e| q_stop(QStop::Ring(e.into())))?;
    let f = r.run_ib(0, ib)?;
    let got = r.sys_read(sys + 2 * PAGE, COPY_BYTES)?;
    let same = got.iter().zip(&src).filter(|(x, y)| x == y).count();
    r.rung(RUNGS[7], got == src, wrong_copy, format!("{same} of {COPY_BYTES} bytes arrived from vram mc {:#x} in system pages at mc {:#x} (fence {} after {} polls)", v + SRC_AT, t + 2 * PAGE, f.seq, f.polls))?;
    // System pages to VRAM.
    let back = pattern(0x4444_0000, COPY_BYTES);
    r.sys_write(sys + 2 * PAGE, &back)?;
    r.vram_write(v + DST_AT, &alloc::vec![0u8; COPY_BYTES as usize])?;
    let mut ib = Vec::new();
    pkt7::copy_linear(&mut ib, t + 2 * PAGE, v + DST_AT, COPY_BYTES).map_err(|e| q_stop(QStop::Ring(e.into())))?;
    let f = r.run_ib(0, ib)?;
    let got = r.vram_read(v + DST_AT, COPY_BYTES)?;
    let same = got.iter().zip(&back).filter(|(x, y)| x == y).count();
    r.rung(RUNGS[8], got == back, wrong_copy, format!("{same} of {COPY_BYTES} bytes arrived from system pages at mc {:#x} in vram at mc {:#x} (fence {} after {} polls)", t + 2 * PAGE, v + DST_AT, f.seq, f.polls))?;
    // Everything passed: the engines are idle, the test pages leave the GART again.
    let mut tlb = super::mesflush::tlb(r.io.flush()?, mes);
    let mut lines = Vec::new();
    gart.unmap_with(r.io.card, &mut tlb, t, &mut lines).map_err(|e| stop("der Speicher der Kopiereinheit lässt sich nicht wieder ausbinden", format!("{e:?}")))?;
    r.out.extend(lines);
    *test_mc = None;
    Ok(())
}

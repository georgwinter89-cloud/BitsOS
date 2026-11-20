//! GP-GPU2 in the kernel: the card's stages in Linux's order on the model that refuses what Linux would not do
//! (`rdna4::fakemodel`), with the fixture files, under the kernel's own locks, clock and klog; then the same stages
//! out of order, refused before any access; the bootloader's refusals, also after a Reload; and the whole chain of
//! wave 42 (`Stage::CHAIN`, from a cold card to loaded firmware) held against the register trace of Linux's own
//! cold start, every covered section in one go. QEMU has no RX 9070 XT: the model stands in for it.

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::Card;
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakemodel::{STAGE_STACK, stack_at};
use crate::rdna4::stage::{self, Ctx, Opts, Session, SessionSlot, Sos, Stage};
use alloc::format;
use alloc::vec::Vec;
use ktest::kassert;
use rdna4_core::card::CARD_A;

fn rig() -> Result<(crate::rdna4::attach::PciFacts, crate::rdna4::attach::Attached), &'static str> {
    let (card, _) = Card::attach(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), FakeCard::measured());
    let facts = card.facts;
    card.state.map(|a| (facts, a)).map_err(|_| "the measured card did not attach")
}

/// The cold model with the three blocks before the secure processor started by their stages, as Linux's
/// secure-processor block meets the card; its record of writes starts here.
fn started() -> Result<crate::rdna4::fakemodel::Model, &'static str> {
    let (facts, att) = rig()?;
    let c = Ctx { index: 0, display: true, facts: &facts, att: &att, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX };
    let mut s = Session::default();
    let mut m = model();
    for st in BEFORE_PSP {
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut Stick::full());
        m = back;
        if !d.pass {
            return Err("a block before the secure processor did not start on the model");
        }
    }
    m.writes.clear();
    Ok(m)
}

/// From a cold card to loaded firmware, in Linux's order: the three blocks before the secure processor, then it.
const CHAIN: [Stage; 19] = Stage::CHAIN;
/// What Linux starts before the secure processor.
const BEFORE_PSP: [Stage; 3] = [Stage::Nbif, Stage::Gmc, Stage::Ih];

fn stages() -> ktest::TestResult {
    let (facts, att) = rig()?;
    let c = Ctx { index: 0, display: true, facts: &facts, att: &att, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX };
    // The device's own slot type, so the session lives under the lock the live tool takes.
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let mut m = model();
    let mut stick = Stick::full();
    // The looks on the cold card (no stage of the chain needs them), the chain, the engines' rungs, the state,
    // the ring's destroy.
    let top = stack_at();
    for st in [Stage::Alive, Stage::Bootloader, Stage::Smu].into_iter().chain(CHAIN).chain([Stage::SdmaTest, Stage::State, Stage::Destroy]) {
        m.deepest = usize::MAX;
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut stick);
        m = back;
        kassert!(d.pass && d.refused.is_empty(), "{st:?}: {} {:?}", d.line, d.lines);
        let deep = top.saturating_sub(m.deepest);
        kassert!(deep <= STAGE_STACK, "{st:?} reached the card {deep} bytes deep in the stack, over {STAGE_STACK}: a stage runs on MCP's 64 KiB thread, so what is large lies on the heap");
    }
    kassert!(m.refused.is_empty() && m.next().is_none() && m.commands.len() == 31 && m.ring.is_none(), "{:?} after {} commands", m.refused, m.commands.len());
    kassert!(m.boot.taken == 9 && m.sol != 0 && s.boot_sent() == 9, "the nine bootloader loads: {} taken, {} sent", m.boot.taken, s.boot_sent());
    Ok(())
}
ktest::ktest!("gx.rdna4.stages", Safe, stages);

fn out_of_order() -> ktest::TestResult {
    let (facts, att) = rig()?;
    let c = Ctx { index: 0, display: true, facts: &facts, att: &att, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX };
    for skip in 0..CHAIN.len() - 1 {
        let mut s = Session::default();
        let mut m = model();
        let mut stopped = false;
        for (i, st) in CHAIN.iter().enumerate().filter(|(i, _)| *i != skip).map(|(_, st)| st).enumerate() {
            let (before, host) = (m.writes.len(), m.nb.host_changes.len());
            let (d, back) = stage::run(&mut s, Opts { stage: *st }, &c, m, &mut Stick::full());
            m = back;
            if !d.pass {
                // The stage after the one left out, across the blocks too: no access at all, nothing on the
                // machine's side.
                kassert!(d.totals.writes == 0 && d.trail.is_empty() && m.writes.len() == before && m.nb.host_changes.len() == host, "{}", format!("without {skip}: step {i} ({st:?}) touched the card before refusing"));
                kassert!(*st == CHAIN[skip + 1] && s.next() == Some(CHAIN[skip]), "without {skip}: {st:?} refused, the session's next is {:?}", s.next());
                stopped = true;
                break;
            }
        }
        kassert!(stopped && m.refused.is_empty(), "without {skip}: {:?}", m.refused);
    }
    // On a card where nothing ran, every stage of the chain but its first refuses without an access.
    for st in &CHAIN[1..] {
        let (d, m) = stage::run(&mut Session::default(), Opts { stage: *st }, &c, model(), &mut Stick::full());
        kassert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && m.nb.host_changes.is_empty(), "{st:?}: {}", d.line);
    }
    Ok(())
}
ktest::ktest!("gx.rdna4.out_of_order", Safe, out_of_order);

/// The bootloader's mailbox in the kernel: nothing reaches it through a stage's guard but Linux's nine loads at
/// their step, and the model refuses on its own what a driver without that guard might send.
fn bootloader_refusals() -> ktest::TestResult {
    use crate::rdna4::guard::{Allow, Guard};
    use rdna4_core::bootseq::{Gate, SEQUENCE};
    let m = started()?;
    let r = m.regs.clone();
    let window = 0x80_0120_0000u64..0x80_0520_0000;
    let (c35, c36, c81) = (r.psp.c2pmsg_35, r.c2pmsg_36, r.psp.c2pmsg_81);
    let listed = Allow { stage: "test", regs: alloc::vec![c35, c36], mem: Vec::new() };
    // No gate: every value is refused, the nine included.
    let mut g = Guard::new(m, listed.clone(), r.clone());
    for (_, command) in SEQUENCE {
        kassert!(!g.reg_write(c35, command), "{command:#x} passed without a gate");
    }
    kassert!(!g.reg_write(c36, 0x8_0013) && g.hw.writes.is_empty(), "the address passed without a gate");
    // A gate: only the next command, only after the sign of life (0), the ready bit and the address.
    let mut g = Guard::new(g.hw, listed, r.clone()).with_gate(Some(Gate::new(window)));
    kassert!(!g.reg_write(c35, 0x8_0000) && !g.reg_write(c36, 0x8_0013), "before any read");
    let _ = (g.reg_read(c81), g.reg_read(c35));
    for bad in [0x3u32, 0x8_0011, 0x8_0052] {
        kassert!(!g.reg_write(c36, bad), "address {bad:#x} outside the window passed");
    }
    kassert!(!g.reg_write(c35, 0x8_0000), "a command before its address");
    kassert!(g.reg_write(c36, 0x8_0013), "the address in the window: {:?}", g.hw.refused);
    for bad in [0x1000_0000u32, 0x2_0000, 0x10_0000, 0x20_0000, 0x2000_0000, 0x0480_0000, 0x0200_0000, 0, 0x8_0001] {
        kassert!(!g.reg_write(c35, bad), "command {bad:#x} passed");
    }
    kassert!(g.hw.writes == [(c36, 0x8_0013)], "only the address reached the card: {:x?}", g.hw.writes);
    kassert!(g.boot.as_ref().is_some_and(|b| b.sent() == 0), "nothing counted as sent");
    // The model on its own account (no guard): the first command without an address, out of order, unknown.
    let mut m = started()?;
    let _ = m.reg_read(c35);
    kassert!(!m.reg_write(c35, 0x8_0000) && !m.reg_write(c35, 0x1000_0000) && !m.reg_write(c35, 0x10_0000), "the model took a command");
    kassert!(!m.reg_write(c36, 0x3), "the model took Linux's GART address");
    kassert!(m.refused.len() == 4 && m.boot.taken == 0, "{:?}", m.refused);
    // And before GMC and IH are started it takes nothing at all, whatever the value (Linux's order of blocks).
    let mut cold = model();
    let _ = cold.reg_read(c35);
    kassert!(!cold.reg_write(c36, 0x8_0013) && !cold.reg_write(c35, 0x8_0000), "a card without GART and IH took a bootloader write");
    kassert!(cold.refused.iter().all(|r| r.contains("before GMC and IH were started")) && cold.refused.len() == 2, "{:?}", cold.refused);
    // A card whose secure OS runs: the model refuses the mailbox altogether.
    let mut w = crate::rdna4::fakefiles::warm();
    let _ = w.reg_read(c35);
    kassert!(!w.reg_write(c36, 0x8_0013) && !w.reg_write(c35, 0x8_0000), "a warm card took a bootloader write");
    Ok(())
}
ktest::ktest!("gx.rdna4.bootloader_refusals", Safe, bootloader_refusals);

/// The loads where no good run goes, in the kernel: a secure OS that shows between two of Linux's loads gets nothing
/// more and the ring follows, as Linux; and a bootloader that got parts from the kernel before a Reload (its note)
/// gets nothing from this one while the secure OS does not run.
fn bootloader_reload() -> ktest::TestResult {
    use crate::rdna4::bootnote;
    use rdna4_core::bootnote::BootNote;
    use rdna4_core::bootseq::Part;
    let (facts, att) = rig()?;
    let c = Ctx { index: 0, display: true, facts: &facts, att: &att, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX };
    let mut stick = Stick::full();
    let mut s = Session::default();
    let mut m = model();
    m.boot.alive_after = Some(4);
    for st in BEFORE_PSP.into_iter().chain([Stage::Sos, Stage::Ring]) {
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut stick);
        m = back;
        kassert!(d.pass && d.refused.is_empty(), "{st:?}: {} {:?}", d.line, d.lines);
    }
    kassert!(matches!(s.sos, Some(Sos::Warm { .. })) && s.boot_sent() == 4 && m.boot.taken == 4 && m.refused.is_empty() && s.ring_up(), "{:?} after {} sent, {} taken", s.sos, s.boot_sent(), m.boot.taken);
    kassert!(bootnote::hand_on(&s) == BootNote { sent: 4, failed: None }, "{:?}", bootnote::hand_on(&s));
    // After a Reload: a fresh session, the note of a kernel whose fourth part was not taken, the secure OS not up.
    let note = BootNote { sent: 4, failed: Some((Part::SocDrv, 0)) };
    for before in [note, BootNote::IN_FLIGHT] {
        let mut s = Session::default();
        s.boot_before = BootNote::handed(Some(&before.bytes()));
        let mut m = model();
        for st in BEFORE_PSP {
            let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut stick);
            m = back;
            kassert!(d.pass, "{st:?}: {}", d.line);
        }
        let seen = m.writes.len();
        let (d, m) = stage::run(&mut s, Opts { stage: Stage::Sos }, &c, m, &mut stick);
        kassert!(!d.pass && d.totals.writes == 0 && d.refused.is_empty() && d.trail.len() == 1, "{} {:?}", d.line, d.lines);
        kassert!(m.writes.len() == seen && m.refused.is_empty() && m.boot.taken == 0, "the card saw {:x?}", &m.writes[seen..]);
        kassert!(s.sos.is_none() && bootnote::hand_on(&s) == before, "{:?}", bootnote::hand_on(&s));
        let (d, m) = stage::run(&mut s, Opts { stage: Stage::Ring }, &c, m, &mut stick);
        kassert!(!d.pass && d.totals.writes == 0 && m.writes.len() == seen, "{}", d.line);
    }
    // The same note with the secure OS running: nothing to load, nothing refused, and the note is handed on.
    let mut s = Session::default();
    s.boot_before = Some(note);
    let mut m = crate::rdna4::fakefiles::warm();
    for st in BEFORE_PSP {
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut stick);
        m = back;
        kassert!(d.pass, "{st:?}: {}", d.line);
    }
    let seen = m.writes.len();
    let (d, m) = stage::run(&mut s, Opts { stage: Stage::Sos }, &c, m, &mut stick);
    kassert!(d.pass && d.totals.writes == 0, "sos: {}", d.line);
    kassert!(matches!(s.sos, Some(Sos::Warm { .. })) && m.writes.len() == seen && bootnote::hand_on(&s) == note, "{:?}", s.sos);
    Ok(())
}
ktest::ktest!("gx.rdna4.bootloader_reload", Safe, bootloader_reload);

/// GX-10: the comparison with Linux's trace, in the kernel, under the kernel's clock: the whole chain from a cold
/// card to loaded firmware, the started power controller, the graphics block's start and on to the copy engines
/// leaves whole trails, and they equal the nine sections it covers (nbif, mmhub, ih, psp-boot, psp-ring, smu,
/// gc-boot, gc-hub, sdma) line for line, in one go, with only the declared rules (`rdna4::chainseq`).
fn linuxseq() -> ktest::TestResult {
    use crate::rdna4::chainseq;
    use crate::rdna4::linuxseq::{SECTIONS, section};
    for (n, _) in SECTIONS {
        if let Err(e) = section(n) {
            return ktest::fail(&e);
        }
    }
    match crate::rdna4::linuxseq::bind_flush() {
        Ok(unit) => kassert!(unit.len() == 9, "Linux's TLB flush after a bind is nine lines: {}", unit.len()),
        Err(e) => return ktest::fail(&e),
    }
    match crate::rdna4::linuxseq::bind_flush_both() {
        Ok(unit) => kassert!(unit.len() == 12, "Linux's TLB flush of both hubs after a bind is twelve lines: {}", unit.len()),
        Err(e) => return ktest::fail(&e),
    }
    let (facts, att) = rig()?;
    let c = Ctx { index: 0, display: true, facts: &facts, att: &att, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX };
    let mut s = Session::default();
    let mut m = model();
    let mut stick = Stick::full();
    let mut done = Vec::new();
    for st in CHAIN {
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut stick);
        m = back;
        kassert!(d.pass && d.trail_lost == 0, "{st:?}: {} lost {}", d.line, d.trail_lost);
        // What the live tool prints for the hardware round: the stage's own word on how it stands against the
        // trace, the last line of its answer.
        let said = d.lines.last().map_or("", |l| l.as_str());
        let whole = chainseq::COVER.iter().any(|(_, stages)| stages.last() == Some(&st));
        kassert!(said.starts_with("linuxseq ") && if whole { said.ends_with(": equal") } else { said.contains(": equal so far (Linux goes on at init.txt line ") }, "{st:?}: {said}");
        done.push(d);
    }
    kassert!(matches!(s.sos, Some(Sos::Loaded { sol: 0x0E5B_5A34 })) && m.refused.is_empty() && s.next().is_none(), "{:?} {:?}", s.sos, m.refused);
    // The rungs behind the chain, in the same run: every rung passes, what they took is back, and the call's
    // own line holds its two flushes through MES against the trace.
    let (rungs, m) = stage::run(&mut s, Opts { stage: Stage::SdmaTest }, &c, m, &mut stick);
    let said = rungs.lines.last().map_or("", |l| l.as_str());
    kassert!(rungs.pass && m.refused.is_empty() && s.sdma.held == (None, None) && said.starts_with("linuxseq mes: equal: the call's only register accesses are 2 of the TLB flush after a GART change once MES runs"), "{} {said}", rungs.line);
    let regs = crate::rdna4::regs14::Regs14::from(&att.discovery).ok_or("the table does not name the registers")?;
    let l = s.layout.clone().ok_or("no window planned")?;
    match chainseq::equal(&done, &regs, &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound) {
        Ok(compared) => kassert!(compared.len() == chainseq::COVER.len() && compared.iter().all(|(_, lines, accesses)| *lines > 0 && *accesses > 0), "{compared:?}"),
        Err(e) => return ktest::fail(&format!("the chain is not Linux's sequence: {e}")),
    }
    Ok(())
}
ktest::ktest!("gx.rdna4.linuxseq", Safe, linuxseq);

/// `gx_bringup` with `stage` on whatever this machine is: its schema names every stage; a call without a card device (the 780M's default, or QEMU
/// with none) is refused by name before anything runs; the 780M never runs its default stage for it.
fn tool_args() -> ktest::TestResult {
    use mcp::{Json, ToolContext, ToolError};
    let t = mcp::find("gx_bringup").ok_or("gx_bringup not registered")?;
    // What the tool offers a caller is the stages that exist, in their order (the chain first).
    let offered = format!(r#""stage":{{"type":"string","enum":[{}]"#, Stage::ALL.map(|st| format!("\"{}\"", st.name())).join(","));
    kassert!(t.input_schema.contains(&offered), "the schema's stages are not {offered}");
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
    let r = (t.handler)(&ctx, &Json::parse(r#"{"stage":"alive"}"#).map_err(|_| "json")?);
    match r {
        Err(ToolError::BadArgument(e)) => kassert!(e.starts_with("stage belongs to an RX 9070 XT"), "{e}"),
        other => return ktest::fail(&format!("not refused: ok={}", other.is_ok())),
    }
    Ok(())
}
ktest::ktest!("gx.rdna4.tool_args", Safe, tool_args);

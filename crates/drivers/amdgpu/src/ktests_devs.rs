//! GP-GPU1 (plan/spec/gpu-rdna4.md piece 3): two device objects side by side, as a machine with a 780M and an RX 9070 XT gives them
//! (the Mini-PC with the card on OCuLink; the Gaming-PC's two cards are the same shape with two of one family). The
//! 780M's fake chain brings its render node up on its own device and runs its jobs; the card's fake is attached
//! read-only beside it and sees no write; neither device's state is the other's.

use crate::attach::Gop;
use crate::devs::{Device, Family, Key, Registry, Seen};
use crate::hw::check;
use crate::hw::jobs::JobCfg;
use crate::ktests_hw_jobs::rig;
use crate::rdna4::Card;
use crate::rdna4::attach::{BarFact, Link, PciFacts};
use crate::rdna4::fakecard::FakeCard;
use alloc::format;
use alloc::vec;
use ktest::kassert;
use rdna4_core::card::CARD_A;

/// The Mini-PC's 780M (c7:00.0); its BAR0 holds the firmware's framebuffer.
const APU: Key = Key::new(0, 0xc7, 0, 0);
const APU_BAR0: (u64, u64) = (0xD0_0000_0000, 256 << 20);

/// The card as lspci showed it on the Gaming-PC (plan/spec/gpu-rdna4.md section 1), here beside the 780M.
pub(crate) fn card_facts(key: Key) -> PciFacts {
    PciFacts {
        key,
        vendor: 0x1002,
        device: 0x7550,
        revision: 0xC0,
        subsystem: (0x148c, 0x2435),
        class: 0x03_00_00,
        command: 0x0006,
        power_state: Some(0),
        bar0: Some(BarFact { phys: CARD_A.bar0_base, bytes: CARD_A.bar0_bytes, is64: true, prefetch: true }),
        bar2: Some(BarFact { phys: CARD_A.bar0_base + (16 << 30), bytes: 256 << 20, is64: true, prefetch: true }),
        bar5: Some(BarFact { phys: 0x9120_0000, bytes: 512 << 10, is64: false, prefetch: false }),
        link: Some(Link { speed: 5, width: 16, max_speed: 5, max_width: 16 }),
        upstream: None,
        // The Gaming-PC's path on 19.11.: the card, its own switch's two ports and the root port, each 32 GT/s x16.
        path: alloc::vec![crate::rdna4::attach::LinkNow { speed: 5, width: 16 }; 4],
        link_host: crate::rdna4::attach::LinkHost::Fixed,
        msi: true,
        msix: false,
    }
}

pub(crate) fn side_by_side() -> ktest::TestResult {
    let card_key = Key::new(0, CARD_A.at.bus, CARD_A.at.device, CARD_A.at.function);
    let found = vec![
        (Seen { key: card_key, family: Family::Rdna4, bar0: Some((CARD_A.bar0_base, CARD_A.bar0_bytes)) }, None),
        (Seen { key: APU, family: Family::Phoenix, bar0: Some(APU_BAR0) }, None),
    ];
    let reg = Registry::build(found, Some(Gop { phys: APU_BAR0.0, bytes: 2560 * 1440 * 4 }));
    // The stable order: the boot display first although its address is the higher one; node numbers by place.
    let (apu, card) = (reg.all()[0], reg.all()[1]);
    kassert!(apu.key == APU && apu.display && apu.family == Family::Phoenix && apu.index == 0, "card0: {} {:?}", apu.key, apu.family);
    kassert!(card.key == card_key && !card.display && card.family == Family::Rdna4 && card.index == 1, "card1: {}", card.key);
    kassert!((apu.card_name().as_str(), apu.render_minor(), card.card_name().as_str(), card.render_minor()) == ("card0", 128, "card1", 129), "node numbers");
    kassert!(reg.pick(None).is_ok_and(|d| d.key == APU) && reg.pick(Some("card1")).is_ok_and(|d| d.key == card_key), "the tools' device argument");
    kassert!(reg.first(Family::Phoenix).is_some_and(|d| d.key == APU) && reg.display().is_some_and(|d| d.key == APU), "roles");

    // The card: attached read-only (the fake fails on any write and keeps it).
    let (attached, fake) = Card::attach(card_facts(card_key), FakeCard::measured());
    kassert!(attached.clean(), "the card's attach: {:?} {:?}", attached.state.as_ref().err(), attached.totals);
    let seen = card.card.get_or_attach(|| attached);
    kassert!(seen.state.as_ref().is_ok_and(|a| a.discovery.ips.len() == 46 && a.differs.is_empty()), "the card's table");

    // The 780M: its fake chain, its jobs, its node on its own device's slot.
    let r = rig(JobCfg::default())?;
    let minor = apu.hw.node.register(r.gpu.clone())?;
    kassert!(apu.hw.node.minor() == Some(minor) && card.hw.node.minor().is_none(), "the node is the 780M's device's only");
    kassert!(drm::device::by_render_minor(minor).is_some(), "renderD{minor} is registered");
    // The place decides the number when it is free (another test's device may hold 128 meanwhile).
    kassert!(minor == apu.render_minor() || drm::device::by_render_minor(apu.render_minor()).is_some(), "renderD{minor} for card0");
    let lines = check::jobs(&r.gpu)?;
    kassert!(lines.len() == 6, "a job per ring on the 780M's fake: {lines:?}");

    // One device's run does not hold the other's: tool gates, PSP locks, held jobs.
    let run = apu.hw.tool.enter().ok_or("the 780M's gate was busy")?;
    kassert!(apu.hw.tool.enter().is_none(), "two runs on one device");
    kassert!(card.hw.tool.enter().is_some(), "the 780M's run shut the card's gate");
    drop(run);
    let other = crate::psplive::with_live_held(&apu.psp, |held| (held, crate::psplive::with_live_held(&card.psp, |free| free)));
    kassert!(other == (true, true), "one device's PSP lock is the other's: {other:?}");

    // The stop: the 780M's node goes, its held jobs stay its own.
    let note = apu.hw.node.stop_gate(&apu.hw.held).map_err(|e| format!("stop: {e}"))?;
    kassert!(note.is_some() && apu.hw.node.minor().is_none() && drm::device::by_render_minor(minor).is_none(), "the node stayed");
    kassert!(card.hw.held.count() == 0 && card.hw.halts.proven(), "the card's slots moved");

    // And through all of it the card saw no write, and no read beyond its attach.
    kassert!(fake.tried.is_empty(), "the card was written: {:?}", fake.tried);
    // Its attach: two fixed dwords, the table, the revision (STRAP0) and the runtime database's header, directory
    // and SCPM entry.
    kassert!(fake.reg_reads.len() == 3 && fake.mem_reads.len() == 4, "reads: {:?} {:?}", fake.reg_reads, fake.mem_reads);
    let totals = card.card.get().map(|c| c.totals);
    kassert!(totals == Some(Default::default()), "the card's owner counted {totals:?}");
    r.finish()
}
ktest::ktest!("gx.devs.side_by_side", Safe, side_by_side);

/// A card whose own firmware is not ready keeps its attach asleep for up to 2 s. A second caller in that time (the
/// report page, MCP `gpu`, the binder's probe) sleeps on the slot's lock and gets the first caller's card; a waiter
/// of `ksync::Once` would spin on a CPU for those 2 s.
fn slow_attach() -> ktest::TestResult {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let key = Key::new(0, CARD_A.at.bus, CARD_A.at.device, CARD_A.at.function);
    let dev = Device::detached(key, 1, Family::Rdna4, false);
    let asleep = Arc::new(AtomicUsize::new(0));
    let seen = asleep.clone();
    let first = sched::spawn("gx-devs-test", sched::Class::Normal, move || {
        dev.card.get_or_attach(|| {
            // The slow card: asleep in steps until a caller sleeps for it, at most the 2 s of the firmware wait.
            let t0 = time::now_ns();
            while dev.card.waiting() == 0 && time::now_ns() - t0 < 2_000_000_000 {
                sched::sleep_ns(1_000_000);
            }
            seen.store(dev.card.waiting(), Ordering::Release);
            Card::attach(card_facts(key), FakeCard { ready_after_ms: 80, ..FakeCard::measured() }).0
        });
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    let t0 = time::now_ns();
    while !dev.card.attaching() {
        kassert!(time::now_ns() - t0 < 5_000_000_000, "the attach did not start");
        sched::sleep_ns(100_000);
    }
    kassert!(dev.card.get().is_none(), "a card before its attach ended");
    let again = AtomicBool::new(false);
    let got = dev.card.get_or_attach(|| {
        again.store(true, Ordering::Release);
        Card::attach(card_facts(key), FakeCard::measured()).0
    });
    kassert!(first.join_timeout(5_000_000_000), "the attach stays");
    kassert!(asleep.load(Ordering::Acquire) == 1, "the second caller did not sleep on the lock: {} asleep", asleep.load(Ordering::Acquire));
    kassert!(!again.load(Ordering::Acquire) && got.state.as_ref().is_ok_and(|a| a.ready_after_ms == 80), "a second attach ran");
    kassert!(got.clean() && !dev.card.attaching() && dev.card.waiting() == 0, "the slot after the attach");
    Ok(())
}
ktest::ktest!("gx.devs.slow_attach", Safe, slow_attach);

/// MCP `gpu` with its device argument, on whatever machine this is: `list` names every device of the registry in its
/// order, a device that is not there is refused by name with the list of what is, and the Mini-PC's 780M is card0.
fn tool() -> ktest::TestResult {
    use mcp::{Json, ToolContext, ToolError, ToolOutput};
    let t = mcp::find("gpu").ok_or("gpu not registered")?;
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
    let call = |args: &str| (t.handler)(&ctx, &Json::parse(args).map_err(|e| ToolError::Failed(format!("{e:?}")))?);
    let reg = crate::devs::registry();
    let list = match call(r#"{"device":"list"}"#).ok().as_ref().and_then(|v| v.first()) {
        Some(ToolOutput::Text(s)) => s.clone(),
        _ => return ktest::fail("device=list: no text answer"),
    };
    kassert!(list.starts_with(&format!("gpu devices: {}\n", reg.all().len())) && list.lines().count() == reg.all().len() + 1, "{list}");
    for d in reg.all() {
        kassert!(list.contains(&format!("{} {} renderD{}: ", d.card_name(), d.key, d.render_minor())), "{}: {list}", d.key);
    }
    let gone = format!("card{}", reg.all().len());
    match call(&format!(r#"{{"device":"{gone}"}}"#)) {
        Err(ToolError::BadArgument(e)) => kassert!(e.starts_with(&format!("no GPU device '{gone}': ")), "{e}"),
        other => return ktest::fail(&format!("{gone}: {:?}", other.is_ok())),
    }
    kassert!(matches!(call(r#"{"device":7}"#), Err(ToolError::BadArgument(_))), "a device that is no text");
    // The stable order's first rule, on the machine itself.
    kassert!(reg.all().iter().filter(|d| d.display).count() <= 1 && reg.all().iter().skip(1).all(|d| !d.display), "the display is not first");
    Ok(())
}
ktest::ktest!("gx.devs.tool", Safe, tool);

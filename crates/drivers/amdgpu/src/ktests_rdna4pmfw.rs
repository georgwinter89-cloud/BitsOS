//! Wave 43 in the kernel: the stage `pmfw` (Linux's start of the power controller) on the model, under the
//! kernel's own locks, clock and klog: it passes and equals the trace's section smu, leaves the clocks' levels and
//! the graphics block's power in the session, and is not started a second time; every refusal of its guard, its
//! gate and the model's power controller, each by doing what Linux would not do; and its stops, each of which
//! names its message and leaves nothing half-opened. QEMU has no RX 9070 XT: the model stands in for it.
//!
//! A test's parts are functions of their own that are never inlined: a model and a session are a few KiB by value
//! each, and the gate's release run holds every test to 48 KiB of stack (MCP's thread has 64).

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::Card;
use crate::rdna4::attach::{Attached, LinkHost, LinkNow, PciFacts};
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakemodel::{Model, STAGE_STACK, stack_at};
use crate::rdna4::fakepmfw::FEATURES;
use crate::rdna4::guard::{Guard, Mc};
use crate::rdna4::stage::{self, Ctx, Done, Opts, Session, Stage};
use crate::rdna4::stage_pmfw;
use alloc::boxed::Box;
use alloc::string::String;
use ktest::kassert;
use rdna4_core::card::CARD_A;
use rdna4_core::refuse::{SMU_NEVER, SMU_START};
use rdna4_core::smugate::Gate;
use rdna4_core::smuseq::{Script, Tables};

/// The card attached, on the heap.
fn rig(card: FakeCard) -> Result<Box<(PciFacts, Attached)>, &'static str> {
    let (card, _) = Card::attach(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), card);
    let facts = card.facts;
    card.state.map(|a| Box::new((facts, a))).map_err(|_| "the measured card did not attach")
}

fn ctx(r: &(PciFacts, Attached)) -> Ctx<'_> {
    Ctx { index: 0, display: true, facts: &r.0, att: &r.1, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX }
}

/// Linux's chain up to `ta` on a cold model: the secure processor loaded the SMU's firmware.
#[inline(never)]
fn loaded(c: &Ctx, s: &mut Session) -> Result<Box<Model>, String> {
    let mut m = Box::new(model());
    for st in &Stage::CHAIN[..11] {
        let (d, back) = stage::run(s, Opts { stage: *st }, c, *m, &mut Stick::full());
        *m = back;
        if !d.pass {
            return Err(alloc::format!("{st:?} did not pass: {}", d.line));
        }
    }
    Ok(m)
}

#[inline(never)]
fn pmfw(c: &Ctx, s: &mut Session, m: Box<Model>) -> (Box<Done>, Box<Model>) {
    let (d, back) = stage::run(s, Opts { stage: Stage::Pmfw }, c, *m, &mut Stick::full());
    (Box::new(d), Box::new(back))
}

fn starts(d: &Done, with: &str) -> bool {
    d.lines.iter().any(|l| l.starts_with(with))
}

/// The stage on the model: Linux's 125 messages, the trace's section whole, the session's facts, once.
pub(crate) fn start() -> ktest::TestResult {
    let r = rig(FakeCard::measured())?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let mut m = loaded(&c, &mut s)?;
    kassert!(!m.gc_powered() && s.next() == Some(Stage::Pmfw), "before the start: {:?}", s.next());
    let top = stack_at();
    m.deepest = usize::MAX;
    let (d, m) = pmfw(&c, &mut s, m);
    kassert!(d.pass && d.refused.is_empty() && m.refused.is_empty() && d.trail_lost == 0, "{} {:?} {:?}", d.line, d.refused, m.refused);
    kassert!(d.lines.last().is_some_and(|l| l == "linuxseq smu: equal"), "{:?}", d.lines.last());
    let deep = top.saturating_sub(m.deepest);
    kassert!(deep <= STAGE_STACK, "pmfw reached the card {deep} bytes deep in the stack, over {STAGE_STACK}");
    kassert!(m.messages.len() == 125 && d.totals.writes == 1 + 3 * 125 + 4, "{} messages, {} writes", m.messages.len(), d.totals.writes);
    kassert!(m.gc_powered() && s.pmfw.gc_powered() && s.passed(Stage::Pmfw) && s.next() == Some(Stage::GcBoot), "the graphics block's power: card {} session {}", m.gc_powered(), s.pmfw.gc_powered());
    kassert!(s.pmfw.facts.supported == Some(FEATURES) && s.pmfw.facts.dpm.len() == 7, "{:x?}", s.pmfw.facts.supported);
    let mhz = |clock: &str| s.pmfw.facts.dpm.iter().find(|t| t.clock == clock).map(|t| t.mhz.clone());
    let uclk = mhz("UCLK");
    kassert!(uclk.as_deref() == Some(&[96, 456, 772, 875, 1124, 1258][..]) && mhz("GFXCLK").as_deref() == Some(&[500, 2400][..]), "{uclk:?}");
    kassert!(!m.pmfw.vcn_up && !m.pmfw.jpeg_up && m.pmfw.ih_allowed && m.pmfw.thm_ctrl == 0xB8FF_0076, "vcn {} jpeg {} thm {:#x}", m.pmfw.vcn_up, m.pmfw.jpeg_up, m.pmfw.thm_ctrl);
    // Once per load of the PMFW's firmware: refused before any access.
    let seen = m.writes.len();
    let (d, m) = pmfw(&c, &mut s, m);
    kassert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && m.writes.len() == seen, "a second start: {}", d.line);
    Ok(())
}
ktest::ktest!("gx.rdna4.pmfw", Safe, start);

/// One message on the model as Linux puts it on the bus: whether the model took the message write.
fn raw(m: &mut Model, msg: u32, arg: u32) -> bool {
    let (answer, argument, message) = (m.regs.smu_answer, m.regs.smu_argument, m.regs.smu_message);
    let _ = m.reg_read(answer);
    let took = m.reg_write(answer, 0) && m.reg_write(argument, arg) && m.reg_write(message, msg);
    while took && m.reg_read(answer) == Some(0) {}
    took
}

/// The stage itself: a card whose runtime database does not say SCPM, a host where Linux lets the link's speed
/// switch. Refused before any access.
#[inline(never)]
fn stage_refuses(scpm_off: bool) -> ktest::TestResult {
    let mut r = rig(if scpm_off { FakeCard { runtime_db: rdna4_core::scpm::synthetic(0).to_vec(), ..FakeCard::measured() } } else { FakeCard::measured() })?;
    if !scpm_off {
        r.0.link_host = LinkHost::Switching;
    }
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let m = loaded(&c, &mut s)?;
    let seen = m.writes.len();
    let (d, m) = pmfw(&c, &mut s, m);
    kassert!(!d.pass && d.trail.is_empty() && m.writes.len() == seen && m.messages.is_empty() && !s.pmfw.began, "scpm off {scpm_off}: {}", d.line);
    Ok(())
}

/// The guard with the SMU's gate: only Linux's next bus access reaches the card; without the gate the stage's
/// list alone opens no message of the start.
#[inline(never)]
fn guard_refuses() -> ktest::TestResult {
    let r = rig(FakeCard::measured())?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let m = loaded(&c, &mut s)?;
    let regs = m.regs.clone();
    let l = s.layout.clone().ok_or("no layout")?;
    let tables = Tables { driver_mc: l.mc(0x600_0000), tools_mc: l.mc(0x610_0000) };
    let gate = Some(Box::new(Gate::new(Script::new(tables, (4, 6)))));
    let mut g = Box::new(Guard::new(Mc { hw: *m, base: l.mc_base, bytes: l.vram_bytes }, stage_pmfw::allow(&regs), regs.clone()).with_smu(gate));
    let seen = g.hw.hw.writes.len();
    kassert!(g.reg_write(regs.smu_answer, 0) && !g.reg_write(regs.smu_argument, 1) && g.reg_write(regs.smu_argument, 0), "the first message's argument");
    for (msg, _) in SMU_NEVER.iter().chain(SMU_START.iter()) {
        kassert!(!g.reg_write(regs.smu_message, u32::from(*msg)), "{msg:#x} passed where GetDriverIfVersion is next");
    }
    kassert!(!g.reg_write(regs.smu_message, 0x02) && g.reg_write(regs.smu_message, 0x03), "the first message");
    kassert!(!g.reg_write(regs.smu_answer, 0), "a second message with no answer read");
    let _ = (g.reg_read(regs.smu_answer), g.reg_read(regs.smu_argument));
    kassert!(!g.reg_write(regs.smu_answer, 0), "a second message without Linux's look at the answer register");
    let _ = g.reg_read(regs.smu_answer);
    kassert!(g.reg_write(regs.smu_answer, 0) && g.reg_write(regs.smu_argument, 0) && !g.reg_write(regs.smu_message, 0x06) && !g.reg_write(regs.smu_message, 0x0E), "EnableAllSmuFeatures where GetSmuVersion is next");
    kassert!(!g.reg_write(regs.thm_int_ctrl, 0xB8FF_0076) && !g.reg_write(regs.mp1_ih_sw_int, 0xFE), "the thermal alert before its step");
    let m = &g.hw.hw;
    kassert!(m.writes.len() - seen == 5 && m.refused.is_empty() && m.messages == [(0x03, 0)], "the card saw {:x?}", &m.writes[seen..]);
    Ok(())
}

#[inline(never)]
fn guard_without_a_gate() -> ktest::TestResult {
    let r = rig(FakeCard::measured())?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let m = loaded(&c, &mut s)?;
    let regs = m.regs.clone();
    let mut g = Box::new(Guard::new(*m, stage_pmfw::allow(&regs), regs.clone()));
    kassert!(g.reg_write(regs.smu_answer, 0) && g.reg_write(regs.smu_argument, 0), "the look's protocol");
    for (msg, _) in SMU_START {
        kassert!(!g.reg_write(regs.smu_message, u32::from(msg)), "{msg:#x} passed without a gate");
    }
    Ok(())
}

/// The model's power controller on its own account (no guard), up to EnableAllSmuFeatures.
#[inline(never)]
fn model_refuses_before_the_features(m: &mut Model) -> ktest::TestResult {
    let thm = m.regs.thm_int_ctrl;
    kassert!(!raw(m, 0x12, 1), "a table transfer before the tables' addresses");
    kassert!(!raw(m, 0x06, 0), "EnableAllSmuFeatures first");
    kassert!(raw(m, 0x0E, 0x80) && !raw(m, 0x0F, 0x0600_0800) && raw(m, 0x0F, 0x0600_0000), "the driver table's address");
    kassert!(!raw(m, 0x12, 1), "a table transfer before the tools table's address");
    kassert!(raw(m, 0x10, 0x80) && !raw(m, 0x11, 0x0600_1000) && raw(m, 0x11, 0x0610_0000), "the tools table's address");
    kassert!(!raw(m, 0x36, 0) && !raw(m, 0x12, 2) && raw(m, 0x12, 1), "the transfer");
    kassert!(!raw(m, 0x20, 0x0406) && !raw(m, 0x36, 1) && raw(m, 0x36, 0) && !raw(m, 0x36, 0), "RunDcBtc");
    kassert!(!raw(m, 0x06, 0) && !raw(m, 0x20, 0x1_0406) && !raw(m, 0x20, 0x0404), "before the link levels");
    kassert!(raw(m, 0x20, 0x0406) && raw(m, 0x20, 0x1_0406) && !raw(m, 0x06, 0) && raw(m, 0x20, 0x2_0406), "the link levels");
    kassert!(!raw(m, 0x2A, 0) && !m.reg_write(thm, 0xB8FF_0076) && !m.gc_powered(), "before the features are on");
    kassert!(!raw(m, 0x06, 1) && raw(m, 0x06, 0) && m.gc_powered() && !raw(m, 0x06, 0), "EnableAllSmuFeatures, once");
    Ok(())
}

/// The same, once the features are on.
#[inline(never)]
fn model_refuses_after_the_features(m: &mut Model) -> ktest::TestResult {
    let (thm, ena, sw, sw_ctrl, answer) = (m.regs.thm_int_ctrl, m.regs.thm_int_ena, m.regs.mp1_ih_sw_int, m.regs.mp1_ih_sw_int_ctrl, m.regs.smu_answer);
    kassert!(!raw(m, 0x1F, 0x5_00FF) && !raw(m, 0x2C, 0) && raw(m, 0x2A, 0) && raw(m, 0x2C, 0), "the video block");
    kassert!(raw(m, 0x1F, 0x1_00FF) && !raw(m, 0x1F, 0x1_0002) && !raw(m, 0x1F, 0x6_00FF), "a level past the count");
    m.pmfw.features &= !(1 << 3);
    kassert!(!raw(m, 0x1F, 0x2_00FF), "a clock whose feature is off");
    kassert!(!raw(m, 0x2B, 0) && raw(m, 0x2D, 0) && raw(m, 0x2B, 0) && !raw(m, 0x4D, 0), "the power-down and the interrupt's order");
    for (msg, _) in SMU_NEVER {
        kassert!(!raw(m, u32::from(msg), 0), "{msg:#x} of the never list");
    }
    kassert!(!m.reg_write(ena, 0x38) && !m.reg_write(thm, 0xB8FF_0077) && m.reg_write(thm, 0xB8FF_0076), "the thermal alert's first register");
    kassert!(m.reg_write(ena, 0x38) && m.reg_write(sw, 0xFE) && !raw(m, 0x4D, 0) && m.reg_write(sw_ctrl, 0), "the thermal alert's order");
    kassert!(raw(m, 0x4D, 0) && !raw(m, 0x4D, 0), "AllowIHHostInterrupt, once");
    // A message whose sender did not read the answer register as answered since the one before.
    kassert!(!m.reg_write(answer, 0), "a message with no look at the answer register since the last one");
    Ok(())
}

#[inline(never)]
fn model_refuses() -> ktest::TestResult {
    let r = rig(FakeCard::measured())?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let mut m = loaded(&c, &mut s)?;
    model_refuses_before_the_features(&mut m)?;
    model_refuses_after_the_features(&mut m)?;
    // A start message on a card whose PMFW the secure processor did not load in this boot.
    let mut cold = Box::new(model());
    kassert!(!raw(&mut cold, 0x0E, 0x80) && raw(&mut cold, 0x02, 0), "a cold card took a message of the start, or refused a question");
    Ok(())
}

/// A load of the SMU's firmware that the secure processor does not answer leaves it unknown which power
/// controller runs: the question a look left unanswered is still owed, so the next look waits on the answer
/// register as Linux's pre-poll does and writes nothing over a message that may still run.
#[inline(never)]
fn a_failed_load_keeps_the_old_power_controller() -> ktest::TestResult {
    let r = rig(FakeCard::measured())?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let mut m = Box::new(model());
    for st in Stage::CHAIN[..7].iter().copied().chain([Stage::Smu, Stage::Fw, Stage::Smu]) {
        let looks = st == Stage::Smu;
        match (st, s.smu_asked) {
            (Stage::Smu, false) => m.smu_silent_on = Some(0x02),
            (Stage::Fw, _) => (m.smu_silent_on, m.hang_on) = (None, Some(crate::gpucore::psp::cmd::LOAD_IP_FW)),
            _ => {}
        }
        let sent = m.messages.len();
        let again = looks && s.smu_asked;
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, *m, &mut Stick::full());
        *m = back;
        match st {
            Stage::Smu if again => kassert!(!d.pass && m.messages.len() == sent && m.refused.is_empty(), "the look after the failed load: {} {:?}", d.line, m.refused),
            Stage::Smu => kassert!(!d.pass && s.pmfw.owes_answer && s.smu_asked, "the look whose question stays open: {}", d.line),
            Stage::Fw => kassert!(!d.pass && d.totals.writes > 0 && s.loaded.is_empty() && s.pmfw.owes_answer && s.smu_asked, "fw: {} owes {} asked {}", d.line, s.pmfw.owes_answer, s.smu_asked),
            _ => kassert!(d.pass, "{st:?} did not pass: {}", d.line),
        }
    }
    Ok(())
}

/// What the stage refuses before any access, what its guard keeps from the card, and what the model's power
/// controller refuses on its own account.
pub(crate) fn refusals() -> ktest::TestResult {
    stage_refuses(true)?;
    stage_refuses(false)?;
    guard_refuses()?;
    guard_without_a_gate()?;
    model_refuses()?;
    a_failed_load_keeps_the_old_power_controller()
}
ktest::ktest!("gx.rdna4.pmfw_refusals", Safe, refusals);

/// One stop: the message is named with its answer, the video block is down again (but what `left_up` names, as
/// (VCN, JPEG): a power-down the card answered with an error), what stays on the card is said (`says`), what the
/// stop did after it is outside the comparison with Linux's trace, and it is final for this PMFW.
#[inline(never)]
fn one_stop(what: &str, setup: fn(&mut Model), tail: &[(u16, u32)], stopped: &str, left_up: (bool, bool), says: &str) -> ktest::TestResult {
    let r = rig(FakeCard::measured())?;
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let mut m = loaded(&c, &mut s)?;
    setup(&mut m);
    let (d, m) = pmfw(&c, &mut s, m);
    kassert!(!d.pass && d.refused.is_empty() && m.refused.is_empty(), "{what}: {} {:?}", d.line, m.refused);
    kassert!(d.lines.last().is_some_and(|l| l.starts_with(stopped)), "{what}: {:?}", d.lines.last());
    kassert!(m.messages.ends_with(tail), "{what}: the card's last messages {:x?}", &m.messages[m.messages.len().saturating_sub(5)..]);
    kassert!((m.pmfw.vcn_up, m.pmfw.jpeg_up) == left_up && s.pmfw.began && !s.pmfw.passed, "{what}: vcn {} jpeg {}", m.pmfw.vcn_up, m.pmfw.jpeg_up);
    kassert!(starts(&d, "linuxseq smu: the same as Linux's as far as the stage came") && starts(&d, says), "{what}: the stop's lines are missing");
    kassert!(tail.len() == 1 || starts(&d, "linuxseq: "), "{what}: the stop's own accesses are not said to be outside the comparison");
    let sent = m.messages.len();
    let (d, m) = pmfw(&c, &mut s, m);
    kassert!(!d.pass && d.trail.is_empty() && m.messages.len() == sent, "{what}: a second start after the stop");
    Ok(())
}

/// A narrower link: Linux's other arguments; a card that expects the trace's refuses them.
#[inline(never)]
fn narrower_link(card_expects_it: bool) -> ktest::TestResult {
    let mut r = rig(FakeCard::measured())?;
    r.0.path[3] = LinkNow { speed: 5, width: 8 };
    let c = ctx(&r);
    let mut s = Box::new(Session::default());
    let mut m = loaded(&c, &mut s)?;
    if card_expects_it {
        m.pmfw.link = (4, 4);
    }
    let (d, m) = pmfw(&c, &mut s, m);
    if card_expects_it {
        kassert!(d.pass && m.pmfw.pcie == [0x0404, 0x1_0404, 0x2_0404], "x8 at the root port: {} {:x?}", d.line, m.pmfw.pcie);
    } else {
        kassert!(!d.pass && m.refused.len() == 1 && m.pmfw.pcie.is_empty(), "the model took another path's argument: {:?}", m.refused);
    }
    Ok(())
}

pub(crate) fn stops() -> ktest::TestResult {
    const DOWN: (bool, bool) = (false, false);
    const READ_BACK: &str = "after the stop: EnableAllSmuFeatures went out; running features read back ";
    one_stop("no answer to EnableAllSmuFeatures", |m| m.smu_silent_on = Some(0x06), &[(0x06, 0)], "stopped: EnableAllSmuFeatures (0x06, argument 0x0): no answer within Linux's 2000000 reads", DOWN, "after the stop: EnableAllSmuFeatures went out; the running features were not read back")?;
    one_stop("EnableAllSmuFeatures refused", |m| m.pmfw.answer_with = Some((0x06, 0xFF)), &[(0x06, 0), (0x0D, 0), (0x0C, 0)], "stopped: EnableAllSmuFeatures (0x06, argument 0x0) answered 0xff: Linux's -EIO", DOWN, READ_BACK)?;
    one_stop("a clock question fails", |m| m.pmfw.answer_with = Some((0x1F, 0xFE)), &[(0x1F, 0x1_00FF), (0x2D, 0), (0x2B, 0), (0x0D, 0), (0x0C, 0)], "stopped: GetDpmFreqByIndex (0x1f, argument 0x100ff) answered 0xfe: Linux's -EOPNOTSUPP", DOWN, "after the stop: PowerDownJpeg, PowerDownVcn answered OK")?;
    one_stop("PowerUpJpeg fails", |m| m.pmfw.answer_with = Some((0x2C, 0xFD)), &[(0x2C, 0), (0x2B, 0), (0x0D, 0), (0x0C, 0)], "stopped: PowerUpJpeg (0x2c, argument 0x0) answered 0xfd: Linux's -EINVAL", DOWN, "after the stop: PowerDownVcn answered OK")?;
    // PowerDownJpeg fails: VCN is powered down all the same, as Linux's err_out does (amdgpu_smu.c:912-921); the
    // model's JPEG stays up, and the answer says so.
    one_stop("PowerDownJpeg fails", |m| m.pmfw.answer_with = Some((0x2D, 0xFF)), &[(0x2D, 0), (0x2B, 0), (0x0D, 0), (0x0C, 0)], "stopped: PowerDownJpeg (0x2d, argument 0x0) answered 0xff: Linux's -EIO", (false, true), "after the stop: the video block stays powered up (VCN false, JPEG true)")?;
    // AllowIHHostInterrupt fails: the thermal alert's four registers stay as written, and the answer says so.
    one_stop("AllowIHHostInterrupt fails", |m| m.pmfw.answer_with = Some((0x4D, 0xFF)), &[(0x4D, 0), (0x0D, 0), (0x0C, 0)], "stopped: AllowIHHostInterrupt (0x4d, argument 0x0) answered 0xff: Linux's -EIO", DOWN, "after the stop: 4 of the thermal alert's four registers were written")?;
    // The driver table does not read out of VRAM: nothing after the transfer.
    one_stop("the driver table does not read", |m| m.pmfw.table_unreadable = true, &[(0x12, 1)], "stopped: the driver table at mc ", DOWN, "after the stop: EnableAllSmuFeatures did not go out")?;
    narrower_link(true)?;
    narrower_link(false)
}
ktest::ktest!("gx.rdna4.pmfw_stops", Safe, stops);

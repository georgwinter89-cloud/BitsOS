//! SN-5: the HDMI glue's undo bookkeeping (`hdmi::Ctx`) over the Mini-PC's register image (saved-5.json): armed
//! before the first write, Quiesce undoes only then, a busy endpoint pair is not waited for, an undo that did not
//! restore everything stays armed.

extern crate std;

use crate::hdmi::{Ctx, Latch, QuiesceNote, Raw};
use amdgpu::dcn::DcnError;
use hdacodec::dcnsmu;
use hdacodec::dpaudio::{self, Loc, OnError, RegError, Regs};
use hdacodec::dpaudio_fake::{Image, firmware};
use hdacodec::eld::{AzConfig, Sink};
use ksync::IrqSpinLock;
use alloc::vec::Vec;
use std::sync::Mutex;

/// The image as `DcnRegs` would give it: the owner's own pair handling is the glue's, so the image does not count
/// unpaired accesses (and logs only real register writes, not the index selects).
pub(crate) struct Fake(Mutex<Image>, Mutex<Pmfw>);

/// SN-13 (w25): the display's SMU mailbox apart from the image (its writes are not display writes): the response
/// reads OK when idle; a message answers `answer` at once (None: never).
pub(crate) struct Pmfw {
    pub(crate) resp: u32,
    pub(crate) answer: Option<u32>,
    pub(crate) log: Vec<(u32, u32)>,
    /// The display writes the image had logged when each message was sent.
    pub(crate) msg_at: Vec<usize>,
}

impl Fake {
    pub(crate) fn new(mut img: Image) -> Self {
        img.depth = 1;
        img.log.clear();
        Fake(Mutex::new(img), Mutex::new(Pmfw { resp: dcnsmu::OK, answer: Some(dcnsmu::OK), log: Vec::new(), msg_at: Vec::new() }))
    }
    fn img(&self) -> std::sync::MutexGuard<'_, Image> {
        self.0.lock().unwrap()
    }
    pub(crate) fn smu(&self) -> std::sync::MutexGuard<'_, Pmfw> {
        self.1.lock().unwrap()
    }
    /// The messages sent (each `UpdatePmeRestore` is RESP, ARG, MSG).
    pub(crate) fn messages(&self) -> usize {
        self.smu().log.iter().filter(|w| w.0 == dcnsmu::MSG).count()
    }
    fn written(&self) -> usize {
        self.img().written()
    }
}

fn back(e: RegError, offset: u32, wrote: u32) -> DcnError {
    match e {
        RegError::NotAttached | RegError::PairBusy => DcnError::NotAttached,
        RegError::OutOfRange => DcnError::OutOfRange,
        RegError::ReadBack { read } => DcnError::ReadBackMismatch { offset, wrote, read },
    }
}

impl Raw for Fake {
    fn read(&self, byte: u32) -> Result<u32, DcnError> {
        if byte == dcnsmu::RESP {
            return Ok(self.smu().resp);
        }
        self.img().read(byte).map_err(|e| back(e, byte, 0))
    }
    fn write(&self, byte: u32, v: u32) -> Result<(), DcnError> {
        assert!(![dcnsmu::MSG, dcnsmu::ARG, dcnsmu::RESP].contains(&byte), "a mailbox write as a display write");
        self.img().write(byte, v).map_err(|e| back(e, byte, v))
    }
    fn write_unchecked(&self, byte: u32, v: u32) -> Result<(), DcnError> {
        let mut p = self.smu();
        p.log.push((byte, v));
        if byte == dcnsmu::RESP {
            p.resp = v;
        } else if byte == dcnsmu::MSG {
            p.resp = p.answer.unwrap_or(dcnsmu::BUSY);
            let n = self.img().written();
            p.msg_at.push(n);
        }
        Ok(())
    }
}

pub(crate) fn az() -> AzConfig {
    AzConfig::from_sink(&Sink::generic())
}

fn fw() -> Image {
    let mut i = firmware();
    i.depth = 1;
    i
}

pub(crate) fn restored(f: &Fake) {
    let mut want = fw();
    want.log.clear();
    f.img().same(&want).unwrap();
}

#[test]
fn on_then_quiesce_restores_the_firmware() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    assert!(matches!(c.on(&az()), Ok(dpaudio::On::Written { writes: 48 })));
    assert!(latch.armed());
    let n = f.written();
    c.quiesce();
    assert_eq!(latch.note(), Some(QuiesceNote { refused: false, pair_busy: false, writes: 41, mismatched: 0, pme: 1 }));
    // The 41st is the endpoint index register itself, which the image (paired) does not log.
    assert_eq!(f.written() - n, 40);
    assert!(!latch.armed());
    restored(&f);
    // Disarmed: a second Quiesce (reboot after a failed Reload) writes nothing.
    let n = f.written();
    c.quiesce();
    assert_eq!(f.written(), n);
}

#[test]
fn quiesce_without_on_writes_nothing() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    Ctx { raw: &f, latch: &latch, pair: &pair }.quiesce();
    assert_eq!(f.written(), 0);
    assert_eq!(latch.note(), None);
}

#[test]
fn quiesce_with_the_pair_held_does_not_wait() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    c.on(&az()).unwrap();
    {
        let _held = pair.lock();
        c.quiesce();
    }
    let note = latch.note().expect("a note");
    assert!(note.refused && note.pair_busy, "{note:?}");
    assert!(latch.armed(), "endpoint registers not undone: still armed");
    // The pair free again: the next Quiesce (or hdmi_off) finishes the undo.
    c.quiesce();
    assert_eq!(latch.note(), Some(QuiesceNote { refused: false, pair_busy: false, writes: 41, mismatched: 0, pme: 1 }));
    restored(&f);
}

#[test]
fn off_twice() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    c.on(&az()).unwrap();
    assert_eq!(c.off().unwrap().mismatched, 0);
    assert!(!latch.armed());
    restored(&f);
    let o = c.off().unwrap();
    assert_eq!(o.mismatched, 0);
    assert!(!latch.armed());
    restored(&f);
    c.quiesce();
    assert_eq!(latch.note(), None, "nothing armed: Quiesce did nothing");
}

/// The engine goes away right after the first write: `on` fails and its undo fails too. The glue must stay armed,
/// and the Quiesce undo (engine back) restores the firmware's values.
#[test]
fn failed_on_with_failed_undo_stays_armed() {
    // The number of reads `on` makes before its first write's read-back.
    let mut n = 0;
    let reads = loop {
        let f = Fake::new(firmware());
        f.img().reads_left = Some(n);
        let latch = Latch::new();
        let _ = Ctx { raw: &f, latch: &latch, pair: &IrqSpinLock::new(()) }.on(&az());
        if f.written() > 0 {
            break n;
        }
        n += 1;
    };
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    f.img().reads_left = Some(reads);
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    match c.on(&az()) {
        Err(e @ OnError::Access { .. }) => assert!(matches!(e.undo(), Some(Err(_))), "{e:?}"),
        r => panic!("{r:?}"),
    }
    assert_eq!(f.written(), 1);
    assert!(latch.armed(), "a write stayed in the display registers: Quiesce must undo");
    f.img().reads_left = None;
    c.quiesce();
    assert_eq!(latch.note().map(|n| n.refused), Some(false));
    assert!(!latch.armed());
    restored(&f);
}

/// A gate refuses after an earlier `on` succeeded (the link changed): the earlier writes still need the undo.
#[test]
fn refused_on_keeps_an_earlier_arm() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    c.on(&az()).unwrap();
    // Gate 2 onwards reads the link; break the last BAR gate (0xE334) only.
    let v = f.img().g(Loc::Bar(0xE334));
    f.img().bar.insert(0xE334, !v);
    assert!(matches!(c.on(&az()), Err(OnError::Gate { .. })));
    assert!(latch.armed());
    f.img().bar.insert(0xE334, v);
    c.quiesce();
    assert_eq!(latch.note().map(|n| (n.refused, n.writes)), Some((false, 41)));
    restored(&f);
}

/// SN-13: every write that reaches the owner, the index selects included (the image does not log those).
struct Counting {
    f: Fake,
    raw: Mutex<Vec<(u32, u32)>>,
}

impl Counting {
    fn new() -> Self {
        Counting { f: Fake::new(firmware()), raw: Mutex::new(Vec::new()) }
    }
    fn raw(&self) -> Vec<(u32, u32)> {
        self.raw.lock().unwrap().clone()
    }
}

impl Raw for Counting {
    fn read(&self, byte: u32) -> Result<u32, DcnError> {
        self.f.read(byte)
    }
    fn write(&self, byte: u32, v: u32) -> Result<(), DcnError> {
        self.raw.lock().unwrap().push((byte, v));
        self.f.write(byte, v)
    }
    fn write_unchecked(&self, byte: u32, v: u32) -> Result<(), DcnError> {
        self.f.write_unchecked(byte, v)
    }
}

/// SN-13 (desk-minipc-w13 FAIL 1): reading an endpoint register selects it in `EP_INDEX` and puts the index back;
/// both writes reach the owner but are not display writes, so `write_calls` stays. `sense` and `state` the same.
#[test]
fn endpoint_reads_select_the_index_but_write_nothing() {
    let (c, latch, pair) = (Counting::new(), Latch::new(), IrqSpinLock::new(()));
    let ctx = Ctx { raw: &c, latch: &latch, pair: &pair };
    let old = c.f.img().index();
    let v = dpaudio::get(&mut ctx.bus(false), Loc::Ep(0x54)).unwrap();
    assert_eq!(v, c.f.img().g(Loc::Ep(0x54)));
    assert_eq!(c.raw(), [(dpaudio::EP_INDEX, 0x54), (dpaudio::EP_INDEX, old)], "the index dance reaches the owner");
    assert_eq!((latch.write_calls(), latch.selects()), (0, 2));
    let _ = dpaudio::state(&mut ctx.bus(false));
    let _ = dpaudio::sense(&mut ctx.bus(false));
    assert!(c.raw().iter().all(|&(b, _)| b == dpaudio::EP_INDEX), "{:X?}", c.raw());
    assert_eq!(latch.write_calls(), 0);
    assert_eq!(latch.selects(), c.raw().len() as u64);
    assert_eq!(c.f.written(), 0);
}

/// SN-13: a real write still counts: an endpoint register through `EP_DATA`, any BAR register, and an `EP_INDEX`
/// write without the pair held. `on` and the Quiesce undo count every write but the paired index selects.
#[test]
fn display_writes_still_count() {
    let (c, latch, pair) = (Counting::new(), Latch::new(), IrqSpinLock::new(()));
    let ctx = Ctx { raw: &c, latch: &latch, pair: &pair };
    let old = c.f.img().g(Loc::Ep(0x54));
    dpaudio::put(&mut ctx.bus(false), Loc::Ep(0x54), old).unwrap();
    assert_eq!((latch.write_calls(), latch.selects()), (1, 2), "endpoint write: {:X?}", c.raw());
    let bar = c.f.img().g(Loc::Bar(0x157AC));
    dpaudio::put(&mut ctx.bus(false), Loc::Bar(0x157AC), bar).unwrap();
    assert_eq!(latch.write_calls(), 2);
    let ix = c.f.img().index();
    Regs::write(&mut ctx.bus(false), dpaudio::EP_INDEX, ix).unwrap();
    assert_eq!((latch.write_calls(), latch.selects()), (3, 2), "an unpaired index write is a display write");

    let (c, latch, pair) = (Counting::new(), Latch::new(), IrqSpinLock::new(()));
    let ctx = Ctx { raw: &c, latch: &latch, pair: &pair };
    assert!(ctx.on(&az()).is_ok());
    ctx.quiesce();
    assert_eq!(latch.note().map(|n| n.mismatched), Some(0));
    let real = c.raw().iter().filter(|&&(b, _)| b != dpaudio::EP_INDEX).count() as u64;
    assert!(real > 0);
    // + 1: the undo's last write puts the endpoint index as saved back as a register of its own (no pair held).
    assert_eq!(latch.write_calls(), real + 1);
    assert_eq!(latch.write_calls() + latch.selects(), c.raw().len() as u64);
    restored(&c.f);
}

/// SN-13 (w25): Linux's DP audio on sends `UpdatePmeRestore` between the endpoint's enable and the packets, its
/// off after the packets; so does the glue, and the mailbox writes are not display writes.
#[test]
fn on_and_off_send_the_pme_message_in_linux_order() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    assert!(matches!(c.on(&az()), Ok(dpaudio::On::Written { writes: 48 })));
    let stage4 = dpaudio::on_writes(&az()).iter().filter(|w| w.stage <= 4).count();
    // The image does not log the paired index selects; it logs every endpoint and BAR write.
    assert_eq!(f.smu().msg_at, [stage4]);
    assert_eq!(f.smu().log, [(dcnsmu::RESP, dcnsmu::BUSY), (dcnsmu::ARG, 0), (dcnsmu::MSG, 0xD)]);
    assert_eq!(latch.smu_writes(), 3);
    assert_eq!(latch.write_calls(), 48, "display writes as before");
    assert_eq!(latch.pme_last(), ("ok", "none"));
    let n = f.written();
    c.off().unwrap();
    assert_eq!(f.messages(), 2);
    assert_eq!(*f.smu().msg_at.last().unwrap(), n + 40, "after the whole undo");
    assert_eq!(latch.pme_last(), ("ok", "ok"));
    restored(&f);
    // Off again (nothing armed): the undo is written again and so is the message, as `hdmi_off` does it.
    c.off().unwrap();
    assert_eq!(f.messages(), 3);
}

#[test]
fn quiesce_sends_the_pme_message_after_the_undo() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    c.on(&az()).unwrap();
    let n = f.written();
    c.quiesce();
    let note = latch.note().unwrap();
    assert_eq!((note.writes, note.pme_name()), (41, "ok"));
    let at = f.smu().msg_at.clone();
    assert_eq!(at, [at[0], n + 40]);
    // Disarmed: the next Quiesce sends nothing.
    c.quiesce();
    assert_eq!(f.messages(), 2);
    // Nothing ever armed: no message.
    let (g, latch2) = (Fake::new(firmware()), Latch::new());
    Ctx { raw: &g, latch: &latch2, pair: &pair }.quiesce();
    assert_eq!(g.messages(), 0);
}

#[test]
fn quiesce_with_a_silent_or_held_mailbox_is_bounded() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    c.on(&az()).unwrap();
    f.smu().answer = None;
    let polls = core::cell::Cell::new(0u32);
    let clock = || {
        polls.set(polls.get() + 1);
        u64::from(polls.get()) * 100_000
    };
    c.quiesce_with(&clock);
    let note = latch.note().unwrap();
    assert_eq!((note.refused, note.pme_name()), (false, "no_answer"));
    assert!(polls.get() < 200, "{} clock reads: the 5 ms bound on the given clock ended it", polls.get());
    restored(&f);
    // A sender holds the mailbox (a CPU stopped inside `on`): Quiesce does not wait, nothing sent.
    c.on(&az()).unwrap();
    let sent = f.messages();
    latch.smu_held.store(true, core::sync::atomic::Ordering::Release);
    c.quiesce();
    assert_eq!(latch.note().unwrap().pme_name(), "busy");
    assert_eq!(f.messages(), sent);
    restored(&f);
}

#[test]
fn a_busy_mailbox_does_not_stop_on() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    f.smu().resp = dcnsmu::BUSY;
    let r = c.on_timed(&az(), || {
        let mut left = 50;
        move || {
            left -= 1;
            left > 0
        }
    });
    assert!(matches!(r, Ok(dpaudio::On::Written { writes: 48 })), "{r:?}");
    assert_eq!(latch.pme_last().0, "not_ready");
    assert_eq!(f.messages(), 0);
    assert_eq!(latch.smu_writes(), 0);
}

/// SN-13 fixer: each message's bound is made when that message is sent, after the writes before it (its deadline
/// is its own), once per message.
#[test]
fn each_pme_message_gets_its_own_bound() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    let made = std::cell::RefCell::new(Vec::new());
    let r = c.on_timed(&az(), || {
        made.borrow_mut().push(f.written());
        || true
    });
    assert!(matches!(r, Ok(dpaudio::On::Written { writes: 48 })), "{r:?}");
    let stage4 = dpaudio::on_writes(&az()).iter().filter(|w| w.stage <= 4).count();
    assert_eq!(*made.borrow(), [stage4], "one bound, made after the endpoint's enable");
    let n = f.written();
    c.off_timed(|| {
        made.borrow_mut().push(f.written());
        || true
    })
    .unwrap();
    assert_eq!(made.borrow()[1], n + 40, "off's bound made after the whole undo");
}

/// SN-13 fixer: the thread-context bound spins only for its first `PME_SPIN_NS`, then sleeps between polls, and
/// ends at its deadline on the given clock.
#[test]
fn thread_bound_sleeps_after_the_first_two_ms() {
    let t = core::cell::Cell::new(0u64);
    let clock = || {
        t.set(t.get() + 10_000);
        t.get()
    };
    let slept = core::cell::Cell::new(0u64);
    let first_sleep_at = core::cell::Cell::new(None);
    let sleep = |ns: u64| {
        if first_sleep_at.get().is_none() {
            first_sleep_at.set(Some(t.get()));
        }
        slept.set(slept.get() + ns);
        t.set(t.get() + ns);
    };
    let mut more = crate::hdmi::pme_bound_with(&clock, 100_000_000, Some(&sleep));
    let mut polls = 0;
    while more() {
        polls += 1;
    }
    let at = first_sleep_at.get().expect("it sleeps once the spin time is over");
    assert!((2_000_000..2_100_000).contains(&at), "first sleep at {at} ns");
    // Each poll reads the clock twice (10 us each on this clock) besides its 50 us sleep.
    assert!(slept.get() > 60_000_000, "slept {} ns of the 100 ms", slept.get());
    assert!(t.get() < 101_000_000, "ended at {} ns", t.get());
    assert!(polls > 100);
    // Without a sleep (Quiesce, panic path) it never sleeps.
    let mut q = crate::hdmi::pme_bound(&clock, 5_000_000);
    while q() {}
}

/// SN-13 fixer: `on` and `off` in thread context (real clock, sleeping bound) end with a mailbox the PMFW left
/// BUSY, and with one that takes the message and never answers.
#[test]
fn thread_on_and_off_end_with_a_stuck_mailbox() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    let t0 = std::time::Instant::now();
    f.smu().resp = dcnsmu::BUSY;
    assert!(matches!(c.on(&az()), Ok(dpaudio::On::Written { writes: 48 })));
    assert_eq!(latch.pme_last().0, "not_ready");
    c.off().unwrap();
    assert_eq!(latch.pme_last().1, "not_ready");
    assert_eq!(f.messages(), 0);
    f.smu().resp = dcnsmu::OK;
    f.smu().answer = None;
    c.on(&az()).unwrap();
    assert_eq!(latch.pme_last().0, "no_answer");
    assert!(t0.elapsed() < std::time::Duration::from_secs(3), "{:?}", t0.elapsed());
    restored_after_off(&c, &f);
}

fn restored_after_off(c: &Ctx<'_, Fake>, f: &Fake) {
    // The message left the mailbox BUSY (never answered): off's message is not sent.
    c.off().unwrap();
    assert_eq!(c.latch.pme_last().1, "not_ready");
    restored(f);
}

/// SN-13 fixer: the panic path's call (Quiesce on the panic clock) with a PMFW that never answers: bounded on that
/// clock, and the panic line says `pme=no_answer`.
#[test]
fn panic_line_with_a_silent_pmfw() {
    let (f, latch, pair) = (Fake::new(firmware()), Latch::new(), IrqSpinLock::new(()));
    let c = Ctx { raw: &f, latch: &latch, pair: &pair };
    c.on(&az()).unwrap();
    f.smu().answer = None;
    let t = core::cell::Cell::new(0u64);
    let clock = || {
        t.set(t.get() + 1_000);
        t.get()
    };
    c.quiesce_with(&clock);
    assert!(t.get() <= 5_100_000, "the panic clock ran {} ns: more than the 5 ms bound", t.get());
    let ctrl = crate::QuiesceResult { was: crate::State::Up, stuck: false, codec: crate::CodecStop::Done };
    let mut text = std::string::String::new();
    crate::panic::report_line("hda1", crate::PanicStop { ctrl, display: latch.note() }, &mut |a| text = std::format!("{a}"));
    assert!(text.ends_with(" display_undo=done writes=41 mismatched=0 pme=no_answer"), "{text}");
    restored(&f);
}

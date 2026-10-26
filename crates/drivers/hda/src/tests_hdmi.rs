//! SN-5: the HDMI glue's undo bookkeeping (`hdmi::Ctx`) over the Mini-PC's register image (saved-5.json): armed
//! before the first write, Quiesce undoes only then, a busy endpoint pair is not waited for, an undo that did not
//! restore everything stays armed.

extern crate std;

use crate::hdmi::{Ctx, Latch, QuiesceNote, Raw};
use amdgpu::dcn::DcnError;
use hdacodec::dpaudio::{self, Loc, OnError, RegError, Regs};
use hdacodec::dpaudio_fake::{Image, firmware};
use hdacodec::eld::{AzConfig, Sink};
use ksync::IrqSpinLock;
use alloc::vec::Vec;
use std::sync::Mutex;

/// The image as `DcnRegs` would give it: the owner's own pair handling is the glue's, so the image does not count
/// unpaired accesses (and logs only real register writes, not the index selects).
struct Fake(Mutex<Image>);

impl Fake {
    fn new(mut img: Image) -> Self {
        img.depth = 1;
        img.log.clear();
        Fake(Mutex::new(img))
    }
    fn img(&self) -> std::sync::MutexGuard<'_, Image> {
        self.0.lock().unwrap()
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
        self.img().read(byte).map_err(|e| back(e, byte, 0))
    }
    fn write(&self, byte: u32, v: u32) -> Result<(), DcnError> {
        self.img().write(byte, v).map_err(|e| back(e, byte, v))
    }
}

fn az() -> AzConfig {
    AzConfig::from_sink(&Sink::generic())
}

fn fw() -> Image {
    let mut i = firmware();
    i.depth = 1;
    i
}

fn restored(f: &Fake) {
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
    assert_eq!(latch.note(), Some(QuiesceNote { refused: false, pair_busy: false, writes: 41, mismatched: 0 }));
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
    assert_eq!(latch.note(), Some(QuiesceNote { refused: false, pair_busy: false, writes: 41, mismatched: 0 }));
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

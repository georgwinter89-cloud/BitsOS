//! SN-7: Quiesce in Linux's shutdown order (`hda_intel.c` azx_shutdown: azx_stop_chip without the link reset)
//! and nothing on the registers after it, whatever the mixer still calls before the reset.
extern crate std;

use crate::ctrl::{Controller, HdaError, Kind, OutputId, State};
use crate::fake::{Ev, FakeHda, FakeHw};
use crate::hw::Hw;
use crate::regs::*;
use crate::tests::up;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::audio::PcmFormat;

const ALC269: &str = include_str!("../../../hdacodec/fixtures/alc269-synthetic.txt");
const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };
const HP: OutputId = OutputId { cad: 0, pin: 0x15 };
/// The first output descriptor (QEMU: index 4, after four inputs).
pub(crate) const OUT0: usize = 4;

pub(crate) fn playing() -> (Arc<FakeHda>, Arc<Controller<FakeHw>>, u32) {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let c = Arc::new(c);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    assert_eq!(c.write(sid, &[0; 4096]), Ok(4096), "silence, as the muted video's stream");
    c.start(sid).unwrap();
    f.advance(1024);
    assert!(f.sd(OUT0).running());
    (f, c, sid)
}

#[test]
fn quiesce_stops_the_chip_and_keeps_the_link() {
    let (f, c, _sid) = playing();
    let hw = FakeHw(f.clone());
    hw.wr32(DPLBASE, 0x1001);
    c.quiesce();
    assert_eq!(c.state(), State::Quiesced);
    assert!(!f.sd(OUT0).running(), "stream DMA still running");
    assert_eq!(hw.rd8(CORBCTL) & CORBCTL_RUN, 0, "CORB still running");
    assert_eq!(hw.rd8(RIRBCTL) & RIRBCTL_DMA, 0, "RIRB still running");
    assert_eq!(f.reg32(INTCTL), 0);
    assert_eq!(f.reg32(DPLBASE), 0, "position buffer still on");
    assert_eq!(f.reg32(GCTL) & GCTL_UNSOL, 0, "unsolicited responses still on");
    assert_ne!(f.reg32(GCTL) & GCTL_CRST, 0, "Linux's shutdown leaves the link out of reset");
    assert!(f.has(&Ev::PciOff) && !f.bus_master());
    assert!(f.bad_accesses().is_empty(), "{:?}", f.bad_accesses());
}

/// The reboot order: the mixer's flag, then hda's Quiesce, then the mixer winds down during the reset's sleeps
/// (stop, position, delay, close). None of it reaches the registers, nothing starts again, the buffers of an
/// engine seen stopped go back.
#[test]
fn nothing_reaches_the_registers_after_quiesce() {
    let (f, c, sid) = playing();
    let live = f.live_buffers();
    c.quiesce();
    let n = f.accesses.load(Ordering::Acquire);
    let events = f.trace.lock().len();
    assert_eq!(c.start(sid), Err(HdaError::NotUp));
    assert_eq!(c.write(sid, &[0; 64]), Err(HdaError::NotUp));
    assert!(c.position(sid).is_ok());
    assert!(c.delay(sid).is_ok());
    assert_eq!(c.stop(sid), Ok(()));
    assert_eq!(c.open(HP, F48, 1024, 256), Err(HdaError::NotUp));
    c.service();
    assert_eq!(c.close(sid), Ok(()));
    assert_eq!(f.accesses.load(Ordering::Acquire), n, "a register access after Quiesce");
    assert!(!f.sd(OUT0).running());
    assert_eq!(c.kept_buffers(), 0);
    assert_eq!(f.live_buffers(), live - 2, "BDL and ring of a stopped engine go back");
    assert!(!f.trace.lock()[events..].iter().any(|e| matches!(e, Ev::Enable(_) | Ev::DmaWithoutBme)), "bus master or DMA after Quiesce");
}

/// SN-13 (w25): Quiesce clears the codecs' state change bits as Linux's `azx_int_clear` does (write 1 to clear),
/// before bus master goes off; a second Quiesce finds them clear and leaves them so.
#[test]
fn quiesce_clears_statests_before_pci_off() {
    let (f, c, _sid) = playing();
    f.set_statests(0x0003);
    assert_eq!(FakeHw(f.clone()).rd16(STATESTS), 0x0003);
    let from = f.trace.lock().len();
    c.quiesce();
    assert_eq!(FakeHw(f.clone()).rd16(STATESTS), 0, "codec state change bits left set");
    let t = f.trace.lock()[from..].to_vec();
    let clear = t.iter().position(|e| *e == Ev::Statests(STATESTS_MASK)).expect("STATESTS written with every bit");
    let off = t.iter().position(|e| *e == Ev::PciOff).expect("pci_off");
    assert!(clear < off, "STATESTS cleared after bus master off: {t:?}");
    c.quiesce();
    assert_eq!(FakeHw(f.clone()).rd16(STATESTS), 0);
    assert!(f.bad_accesses().is_empty(), "{:?}", f.bad_accesses());
}

/// An engine that ignores its stop: Quiesce says so, bus master off holds it, a close while quiesced keeps its
/// buffers until the next bring-up's link reset.
#[test]
fn stuck_engine_at_quiesce_keeps_its_buffers() {
    let (f, c, sid) = playing();
    let live = f.live_buffers();
    f.stuck.store(true, Ordering::Release);
    c.quiesce();
    assert!(f.sd(OUT0).running(), "the fake's engine ignores the stop");
    assert!(!f.bus_master());
    let n = f.accesses.load(Ordering::Acquire);
    c.close(sid).unwrap();
    assert_eq!(f.accesses.load(Ordering::Acquire), n);
    assert_eq!(c.kept_buffers(), 2);
    assert_eq!(f.live_buffers(), live, "nothing freed under an engine not seen stopped");
    f.stuck.store(false, Ordering::Release);
    c.bring_up().unwrap();
    assert_eq!(c.kept_buffers(), 0);
    assert!(!f.sd(OUT0).running());
}

/// A Quiesce between `start`'s state check and its run bit (it takes no lock): the start is taken back.
#[test]
fn start_racing_a_quiesce_is_taken_back() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let c = Arc::new(c);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    let once = Arc::new(AtomicBool::new(false));
    let w = Arc::downgrade(&c);
    let o = once.clone();
    *f.on_write.lock() = Some(Arc::new(move |off, v| {
        if off == sd(OUT0) + SD_CTL && v as u8 & SD_CTL_RUN != 0 && !o.swap(true, Ordering::AcqRel)
            && let Some(c) = w.upgrade()
        {
            c.quiesce();
        }
    }));
    assert_eq!(c.start(sid), Err(HdaError::NotUp));
    assert!(once.load(Ordering::Acquire), "the hook did not run");
    assert!(!f.sd(OUT0).running(), "the stream runs after Quiesce");
    *f.on_write.lock() = None;
}

/// A close on another CPU between Quiesce's state swap and its stop: a start that took its run bit back without
/// waiting leaves the slot's stop unseen, so the quiesced close keeps the buffers until the next link reset.
#[test]
fn start_taken_back_then_closed_keeps_its_buffers() {
    let (f, c) = up(Kind::Intel, &[(0, ALC269)]);
    let c = Arc::new(c);
    let sid = c.open(HP, F48, 1024, 256).unwrap();
    let live = f.live_buffers();
    let w = Arc::downgrade(&c);
    let once = Arc::new(AtomicBool::new(false));
    let o = once.clone();
    *f.on_write.lock() = Some(Arc::new(move |off, v| {
        if off == sd(OUT0) + SD_CTL && v as u8 & SD_CTL_RUN != 0 && !o.swap(true, Ordering::AcqRel)
            && let Some(c) = w.upgrade()
        {
            c.quiesce();
        }
    }));
    assert_eq!(c.start(sid), Err(HdaError::NotUp));
    *f.on_write.lock() = None;
    assert!(once.load(Ordering::Acquire), "the hook did not run");
    c.close(sid).unwrap();
    assert_eq!(c.kept_buffers(), 2, "a take-back is not a stop seen");
    assert_eq!(f.live_buffers(), live, "nothing freed under an engine not seen stopped");
    c.bring_up().unwrap();
    assert_eq!(c.kept_buffers(), 0);
}

/// A close that sees Quiesced before Quiesce has answered for the engines (it sets the state first): the close
/// keeps the buffers. Modelled with the hook on Quiesce's first stream access, after the state swap.
#[test]
fn close_inside_quiesce_keeps_its_buffers() {
    let (f, c, sid) = playing();
    let live = f.live_buffers();
    let w = Arc::downgrade(&c);
    let once = Arc::new(AtomicBool::new(false));
    let o = once.clone();
    *f.on_write.lock() = Some(Arc::new(move |off, _| {
        if off == INTCTL && !o.swap(true, Ordering::AcqRel)
            && let Some(c) = w.upgrade()
        {
            assert_eq!(c.state(), State::Quiesced);
            c.close(sid).unwrap();
        }
    }));
    c.quiesce();
    *f.on_write.lock() = None;
    assert!(once.load(Ordering::Acquire), "the hook did not run");
    assert_eq!(c.kept_buffers(), 2);
    assert_eq!(f.live_buffers(), live);
    assert!(!f.sd(OUT0).running());
}

const AA01: &str = include_str!("../../../hdacodec/fixtures/aa01-synthetic.txt");

/// The GPU controller streaming on the DP audio pin, as Brave's muted video on the Mini-PC.
fn gpu_playing() -> (Arc<FakeHda>, Arc<Controller<FakeHw>>) {
    let (f, c) = up(Kind::AtiHdmi, &[(0, AA01)]);
    c.set_streaming(true);
    c.bring_up().unwrap();
    let c = Arc::new(c);
    let pin = OutputId { cad: 0, pin: crate::DP_AUDIO_PIN };
    let sid = c.open(pin, F48, 1024, 256).unwrap();
    assert_eq!(c.write(sid, &[0; 4096]), Ok(4096));
    c.start(sid).unwrap();
    f.advance(1024);
    assert!(f.sd(OUT0).running());
    FakeHw(f.clone()).wr32(DPLBASE, 0x1001);
    (f, c)
}

/// What the controller looked like when the display undo ran.
#[derive(Debug, PartialEq)]
struct AtUndo {
    running: bool,
    dplbase: u32,
    link_up: bool,
    bus_master: bool,
}

fn reboot_quiesce(f: &Arc<FakeHda>, c: &Controller<FakeHw>) -> (Option<AtUndo>, crate::tests_hdmi::Fake, crate::hdmi::Latch) {
    use crate::hdmi::{Ctx, Latch};
    let (disp, latch, pair) = (crate::tests_hdmi::Fake::new(hdacodec::dpaudio_fake::firmware()), Latch::new(), ksync::IrqSpinLock::new(()));
    let ctx = Ctx { raw: &disp, latch: &latch, pair: &pair };
    assert!(ctx.on(&crate::tests_hdmi::az()).is_ok());
    let mut seen = None;
    crate::quiesce_in_order(c, || {
        seen = Some(AtUndo {
            running: f.sd(OUT0).running(),
            dplbase: f.reg32(DPLBASE),
            link_up: f.reg32(GCTL) & GCTL_CRST != 0,
            bus_master: f.bus_master(),
        });
        ctx.quiesce();
    });
    drop(ctx);
    (seen, disp, latch)
}

/// SN-7: the GPU controller's Quiesce stops the stream DMA (and the position buffer) before the display's audio
/// undo, the link out of reset, and the undo restores every display register.
#[test]
fn gpu_quiesce_stops_the_stream_before_the_display_undo() {
    let (f, c) = gpu_playing();
    let (seen, disp, latch) = reboot_quiesce(&f, &c);
    assert_eq!(seen, Some(AtUndo { running: false, dplbase: 0, link_up: true, bus_master: false }));
    assert_eq!(latch.note().map(|n| (n.refused, n.writes, n.mismatched)), Some((false, 41, 0)));
    assert!(!latch.armed());
    crate::tests_hdmi::restored(&disp);
    assert!(!f.sd(OUT0).running());
}

/// A stream engine that ignores its stop: Quiesce notes it, and the display undo still runs (bus master is off).
#[test]
fn gpu_quiesce_with_a_stuck_engine_still_undoes_the_display() {
    let (f, c) = gpu_playing();
    f.stuck.store(true, Ordering::Release);
    let (seen, disp, latch) = reboot_quiesce(&f, &c);
    assert_eq!(seen, Some(AtUndo { running: true, dplbase: 0, link_up: true, bus_master: false }));
    assert!(!latch.armed());
    crate::tests_hdmi::restored(&disp);
    f.stuck.store(false, Ordering::Release);
}

/// An analog controller has no display side.
#[test]
fn analog_quiesce_skips_the_display() {
    let (_f, c, _sid) = playing();
    let mut called = false;
    crate::quiesce_in_order(&*c, || called = true);
    assert!(!called);
    assert_eq!(c.state(), State::Quiesced);
}

//! SN-12: the codecs' shutdown inside Quiesce (Linux `snd_hda_codec_shutdown` before `azx_stop_chip`): the
//! streams' DMA stopped first, then every converter off its stream tag and the codec in D3 over the running
//! command rings, then the rings and the chip; never waiting for a lock, every answer bounded.
extern crate std;

use crate::ctrl::{CodecStop, Kind, State};
use crate::fake::FakeHw;
use crate::hw::Hw;
use crate::regs::*;
use crate::tests::up;
use crate::tests_quiesce::{playing, OUT0};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hdacodec::setup::Action;
use hdacodec::verb::{set, PWRST_D3, SET_CHANNEL_STREAMID, SET_POWER_STATE};

const AA01: &str = include_str!("../../../hdacodec/fixtures/aa01-synthetic.txt");

fn list_sets(list: &[Action]) -> Vec<(u8, u32)> {
    list.iter().filter_map(|a| if let Action::Set { nid, verb } = *a { Some((nid, verb)) } else { None }).collect()
}

#[test]
fn quiesce_cleans_the_converters_then_d3_before_the_rings_stop() {
    let (f, c, _sid) = playing();
    let want = list_sets(&c.codecs()[0].shutdown);
    f.clear_sets(0);
    // At every command Quiesce sends: the stream's DMA already stopped, the rings still running.
    let bad = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicU64::new(0));
    let (b, n, f2) = (bad.clone(), seen.clone(), Arc::downgrade(&f));
    *f.on_write.lock() = Some(Arc::new(move |off, _| {
        if off == CORBWP
            && let Some(f) = f2.upgrade()
        {
            n.fetch_add(1, Ordering::AcqRel);
            if f.sd(OUT0).running() || f.reg32(INTCTL) != 0 {
                b.store(true, Ordering::Release);
            }
        }
    }));
    let r = c.quiesce();
    *f.on_write.lock() = None;
    assert_eq!(r.codec, CodecStop::Done);
    assert!(!r.stuck);
    assert_eq!(r.was, State::Up);
    assert!(!bad.load(Ordering::Acquire), "a verb went out while the stream still ran");
    assert!(seen.load(Ordering::Acquire) as usize > want.len(), "the D3 wait reads too");
    assert_eq!(f.sets(0), want, "the codec got its shutdown list in order");
    let hp_cvt = c.outputs().iter().find(|o| o.id.pin == 0x15).map(|o| o.converter).unwrap();
    assert!(f.sets(0).contains(&(hp_cvt, set(SET_CHANNEL_STREAMID, 0))), "the playing converter left its stream tag");
    assert!(f.sets(0).iter().any(|&(_, v)| v == set(SET_POWER_STATE, PWRST_D3)));
    let hw = FakeHw(f.clone());
    assert_eq!(hw.rd8(CORBCTL) & CORBCTL_RUN, 0, "rings off after the verbs");
    assert_eq!(hw.rd8(RIRBCTL) & RIRBCTL_DMA, 0);
    assert_ne!(f.reg32(GCTL) & GCTL_CRST, 0, "still no link reset");
    assert!(!f.bus_master());
    assert!(f.bad_accesses().is_empty(), "{:?}", f.bad_accesses());
    // Twice (Reload after a ktest's Quiesce): the second sends nothing (the rings are off).
    f.clear_sets(0);
    assert_eq!(c.quiesce().codec, CodecStop::None);
    assert!(f.sets(0).is_empty());
    // The next bring-up has the codec back (link reset, D0 on the path), and it plays.
    c.bring_up().unwrap();
    assert_eq!(c.state(), State::Up);
}

#[test]
fn quiesce_with_the_link_held_sends_nothing_and_still_stops() {
    let (f, c, _sid) = playing();
    f.clear_sets(0);
    let g = c.hold_link();
    let t0 = f.clock();
    let r = c.quiesce();
    drop(g);
    assert_eq!(r.codec, CodecStop::Busy);
    assert!(f.sets(0).is_empty());
    assert!(!f.sd(OUT0).running() && !f.bus_master());
    assert!(f.clock() - t0 <= 3_000_000, "no wait for the lock: {} ns", f.clock() - t0);
}

#[test]
fn quiesce_with_a_silent_codec_is_bounded() {
    let (f, c, _sid) = playing();
    f.corb_dead.store(true, Ordering::Release);
    let t0 = f.clock();
    let r = c.quiesce();
    assert_eq!(r.codec, CodecStop::NoAnswer);
    assert!(!r.stuck, "the rings stop although the codec never answered");
    // One unanswered verb (1 ms), the stream and ring waits: no 50 ms answers, no list-long wait.
    assert!(f.clock() - t0 <= 4_000_000, "{} ns", f.clock() - t0);
    assert!(!f.sd(OUT0).running() && !f.bus_master());
}

#[test]
fn read_only_gpu_controller_gets_no_shutdown_verbs() {
    let (f, c) = up(Kind::AtiHdmi, &[(0, AA01)]);
    assert!(c.read_only());
    let r = c.quiesce();
    assert_eq!(r.codec, CodecStop::None);
    assert!(f.sets(0).is_empty(), "{:?}", f.sets(0));
}

#[test]
fn streaming_gpu_controller_shuts_its_codec_down() {
    let (f, c) = up(Kind::AtiHdmi, &[(0, AA01)]);
    c.set_streaming(true);
    c.bring_up().unwrap();
    let want = list_sets(&c.codecs()[0].shutdown);
    f.clear_sets(0);
    assert_eq!(c.quiesce().codec, CodecStop::Done);
    assert_eq!(f.sets(0), want);
    let cvts = c.outputs().iter().map(|o| o.converter).collect::<Vec<_>>();
    assert!(cvts.iter().all(|&n| f.sets(0).contains(&(n, set(SET_CHANNEL_STREAMID, 0)))));
}

/// The panic path's clock: Quiesce's waits use it, not the controller's (the kernel clock may be stuck).
#[test]
fn quiesce_with_its_own_clock_never_reads_the_controller_clock() {
    let (f, c, _sid) = playing();
    f.corb_dead.store(true, Ordering::Release);
    let tsc = AtomicU64::new(0);
    let now = || tsc.fetch_add(1000, Ordering::AcqRel);
    let t0 = f.clock();
    let r = c.quiesce_with(Some(&now), false);
    assert_eq!(r.codec, CodecStop::NoAnswer);
    assert_eq!(f.clock(), t0, "the controller's clock was used");
    assert!(tsc.load(Ordering::Acquire) > 1_000_000, "the given clock bounded the waits");
    assert!(!f.sd(OUT0).running() && !f.bus_master());
}

/// QEMU's codecs keep no power state: the D3 set is answered but never shows, which is not waited for.
#[test]
fn codec_without_power_states_is_not_waited_for() {
    let (f, c, _sid) = playing();
    f.no_power.store(true, Ordering::Release);
    let t0 = f.clock();
    assert_eq!(c.quiesce().codec, CodecStop::NoPowerStates);
    assert!(f.clock() - t0 < 3_000_000, "{} ns", f.clock() - t0);
}

// ---- SN-12 fixer: hostile RIRB, a codec stuck in D0, the note's level, the terminal panic path, the helpers.

/// Unsolicited responses and stray answers (another codec) in the RIRB before and after every answer to the
/// shutdown verbs: the list still goes out in full and in order, each verb gets its own answer, the strays are
/// counted and dropped, nothing is queued (no allocation inside Quiesce).
#[test]
fn stray_and_unsolicited_rirb_entries_do_not_derail_the_shutdown() {
    let (f, c, _sid) = playing();
    let want = list_sets(&c.codecs()[0].shutdown);
    f.clear_sets(0);
    let (unsol0, cap0, strays0) = {
        let g = c.hold_link();
        let l = g.as_ref().unwrap();
        (l.unsol.len(), l.unsol.capacity(), l.strays)
    };
    let injected = Arc::new(AtomicU64::new(0));
    let (n, f2) = (injected.clone(), Arc::downgrade(&f));
    // RIRBSTS is written after each drain: before a verb goes out and after its answer came.
    *f.on_write.lock() = Some(Arc::new(move |off, _| {
        if off == RIRBSTS
            && let Some(f) = f2.upgrade()
            && FakeHw(f.clone()).rd8(RIRBCTL) & RIRBCTL_DMA != 0
        {
            f.rirb_raw(0x0400_0000, RIRB_EX_UNSOL); // unsolicited, codec 0 (a jack tag)
            f.rirb_raw(0xDEAD_BEEF, 5); // a solicited answer from codec 5, which was never asked
            n.fetch_add(2, Ordering::AcqRel);
        }
    }));
    let r = c.quiesce();
    *f.on_write.lock() = None;
    assert_eq!(r.codec, CodecStop::Done);
    assert!(!r.stuck);
    assert_eq!(f.sets(0), want, "every verb went out once, in order");
    let g = c.hold_link();
    let l = g.as_ref().unwrap();
    assert_eq!((l.unsol.len(), l.unsol.capacity()), (unsol0, cap0), "unsolicited responses queued inside Quiesce");
    let seen = l.strays - strays0;
    assert!(injected.load(Ordering::Acquire) as usize > 2 * want.len(), "the hook fired around every verb");
    assert!(seen as usize >= 2 * want.len(), "strays dropped and counted: {seen}");
}

/// A codec that reports the D3 set but stays in D0: `not_d3` after the capped wait, the chip stopped anyway.
#[test]
fn codec_staying_in_d0_gives_not_d3_within_the_cap() {
    let (f, c, _sid) = playing();
    f.stays_d0.store(true, Ordering::Release);
    let waits = c.codecs()[0].shutdown.iter().filter(|a| matches!(a, Action::WaitFor { .. })).count() as u64;
    assert!(waits >= 1);
    let t0 = f.clock();
    let r = c.quiesce();
    assert_eq!(r.codec, CodecStop::NotD3);
    assert!(!r.stuck);
    assert!(f.clock() - t0 <= waits * 10_000_000 + 4_000_000, "{} ns", f.clock() - t0);
    assert!(!f.sd(OUT0).running() && !f.bus_master());
}

/// The `hda.quiesced` event's level: Warn when an engine was not seen stopped or the codec's shutdown did not go
/// through in full (busy, no_answer, not_d3), Info otherwise. And the note round trip: what Quiesce stored is
/// what `last_quiesce` reads, also after the next bring-up logged the note.
#[test]
fn quiesce_note_level_and_round_trip() {
    use crate::ctrl::quiesce_note_kind as kind;
    use crate::ctrl::{NoteKind, QuiesceResult};
    let r = |stuck, codec| QuiesceResult { was: State::Up, stuck, codec };
    for (codec, k) in [
        (CodecStop::Busy, NoteKind::CodecPartial),
        (CodecStop::NoAnswer, NoteKind::CodecPartial),
        (CodecStop::NotD3, NoteKind::CodecPartial),
        (CodecStop::Done, NoteKind::Fine),
        (CodecStop::NoPowerStates, NoteKind::Fine),
        (CodecStop::None, NoteKind::Fine),
    ] {
        assert_eq!(kind(&r(false, codec)), k, "{codec:?}");
        assert_eq!(kind(&r(true, codec)), NoteKind::Stuck, "{codec:?} stuck");
    }
    // Each partial case for real, read back from the note.
    let (f, c, _sid) = playing();
    let g = c.hold_link();
    assert_eq!(c.quiesce().codec, CodecStop::Busy);
    drop(g);
    assert_eq!(c.last_quiesce(), Some(r(false, CodecStop::Busy)));
    assert_eq!(kind(&c.last_quiesce().unwrap()), NoteKind::CodecPartial);
    c.bring_up().unwrap();
    assert_eq!(c.last_quiesce(), Some(r(false, CodecStop::Busy)), "kept after the bring-up logged the note");
    for (knob, want) in [(&f.corb_dead, CodecStop::NoAnswer), (&f.stays_d0, CodecStop::NotD3)] {
        knob.store(true, Ordering::Release);
        assert_eq!(c.quiesce().codec, want);
        assert_eq!(c.last_quiesce(), Some(r(false, want)));
        knob.store(false, Ordering::Release);
        c.bring_up().unwrap();
    }
    f.stuck.store(true, Ordering::Release);
    let q = c.quiesce();
    assert!(q.stuck);
    assert_eq!(kind(&c.last_quiesce().unwrap()), NoteKind::Stuck);
}

/// The real panic path (terminal): the verbs' locks stay taken (no release, so no wake through the scheduler);
/// the ktest's path (not terminal) releases them.
#[test]
fn terminal_panic_quiesce_keeps_the_verb_locks() {
    let tsc = AtomicU64::new(0);
    let now = || tsc.fetch_add(1000, Ordering::AcqRel);
    let (_f, c, _sid) = playing();
    assert_eq!(c.quiesce_with(Some(&now), false).codec, CodecStop::Done);
    assert_eq!(c.locks_held(), (false, false));
    let (f, c, _sid) = playing();
    let r = c.quiesce_with(Some(&now), true);
    assert_eq!(r.codec, CodecStop::Done);
    assert_eq!(c.locks_held(), (true, true), "a guard was released on the terminal path");
    assert!(!f.sd(OUT0).running() && !f.bus_master());
    assert_eq!(FakeHw(f.clone()).rd8(CORBCTL) & CORBCTL_RUN, 0);
}

/// `stop_streams` leaves the command rings running, `stop_cmd_io` stops them; both bounded (1 ms) and false when
/// an engine ignores its stop.
#[test]
fn stop_streams_then_stop_cmd_io() {
    let (f, c, _sid) = playing();
    let hw = FakeHw(f.clone());
    let total = c.gcap().total();
    assert!(crate::link::stop_streams(&hw, total));
    assert!(!f.sd(OUT0).running());
    assert_eq!(f.reg32(INTCTL), 0);
    assert_ne!(hw.rd8(CORBCTL) & CORBCTL_RUN, 0, "rings stopped with the streams");
    assert_ne!(hw.rd8(RIRBCTL) & RIRBCTL_DMA, 0);
    assert!(crate::link::stop_cmd_io(&hw));
    assert_eq!(hw.rd8(CORBCTL) & CORBCTL_RUN, 0);
    assert_eq!(hw.rd8(RIRBCTL) & RIRBCTL_DMA, 0);

    let (f, c, _sid) = playing();
    let hw = FakeHw(f.clone());
    f.stuck.store(true, Ordering::Release);
    let t0 = f.clock();
    assert!(!crate::link::stop_streams(&hw, c.gcap().total()));
    assert!(!crate::link::stop_cmd_io(&hw));
    assert!(f.clock() - t0 <= 2_500_000, "{} ns", f.clock() - t0);
}

/// `cmd_quiet`: an answer when the codec answers; a dead CORB times out after `answer_us`, not the normal 50 ms.
#[test]
fn cmd_quiet_is_bounded_by_its_answer_time() {
    use hdacodec::verb::{get_param, PAR_VENDOR_ID};
    let (f, c, _sid) = playing();
    let hw = FakeHw(f.clone());
    let mut g = c.hold_link();
    let l = g.as_mut().unwrap();
    assert!(l.cmd_quiet(&hw, 0, 0, get_param(PAR_VENDOR_ID), 1000).is_ok());
    f.corb_dead.store(true, Ordering::Release);
    let t0 = f.clock();
    assert_eq!(l.cmd_quiet(&hw, 0, 0, get_param(PAR_VENDOR_ID), 1000), Err(crate::link::CmdError::Timeout));
    assert!(f.clock() - t0 <= 1_500_000, "{} ns", f.clock() - t0);
}

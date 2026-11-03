//! GX-7f: the stop note across a Reload, and what the classifier and the admission make of it.

use crate::gpucore::classify::{self, Start};
use crate::gpucore::fakechip::{Crash, StartAs, discovery_780m};
use crate::gpucore::handoff::{Arrival, GcFate, LEN, NoteIo, RingFate, StopNote};
use crate::gpucore::psp::Hw;
use crate::gpucore::pspbringup::admit;
use crate::gpucore::pspfake::{FakeGpu, Hostile};

fn note(ring: RingFate, gc: GcFate) -> StopNote {
    StopNote { ring, gc, tmr_destroyed: ring == RingFate::Destroyed, bus_master_left_on: false, wptr: 0x10, size: 0x1000, addr_lo: 0x339000, addr_hi: 0x80 }
}

#[test]
fn note_bytes_round_trip_and_refuse_damage() {
    for ring in [RingFate::None, RingFate::Destroyed, RingFate::NotDestroyed] {
        for gc in [GcFate::Untouched, GcFate::Booted, GcFate::Unsettled, GcFate::Unloaded] {
            for bm in [false, true] {
                let n = StopNote { bus_master_left_on: bm, ..note(ring, gc) };
                assert_eq!(StopNote::parse(&n.bytes()), Some(n));
            }
        }
    }
    let b = note(RingFate::Destroyed, GcFate::Booted).bytes();
    assert_eq!(b.len(), LEN);
    assert_eq!(&b[..8], b"GXSTOP01");
    // Every single flipped bit is refused (magic, fields, reserved byte or checksum).
    for i in 0..LEN * 8 {
        let mut x = b;
        x[i / 8] ^= 1 << (i % 8);
        assert_eq!(StopNote::parse(&x), None, "bit {i}");
    }
    assert_eq!(StopNote::parse(&b[..LEN - 1]), None, "short");
    assert_eq!(StopNote::parse(&[0u8; LEN]), None, "zeros");
    assert_eq!(StopNote::parse(&[0xffu8; 64]), None, "ones");
    let mut long = b.to_vec();
    long.extend_from_slice(&[1, 2, 3]);
    assert!(StopNote::parse(&long).is_some(), "the slot may be longer than the note");
}

/// The fake as a clean BitsOS stop left it (the PSP's ring registers kept, the Mini-PC 28.10.), and its note.
fn after_stop() -> (FakeGpu, StopNote) {
    let d = discovery_780m();
    let mut f = FakeGpu::new(Hostile::None);
    f.start_as(&d, StartAs::AfterBitsOs);
    let n = f.stop_note(StartAs::AfterBitsOs).unwrap();
    (f, n)
}

#[test]
fn the_note_tells_a_clean_stop_from_a_crash() {
    let d = discovery_780m();
    let (mut f, n) = after_stop();
    assert_eq!((n.wptr, n.size), (0x10, 0x1000));
    let cls = |f: &mut FakeGpu, n: Option<&StopNote>| classify::classify_with(&d, &mut |b| f.reg_read(b), n);
    // No note: the registers look like a crash (what the desk saw after the 28.10. Reload).
    let st = cls(&mut f, None);
    assert_eq!(st.start, Start::AfterCrash);
    assert!(st.why.iter().any(|w| w.contains("write pointer 16) and is not known destroyed")), "{:?}", st.why);
    // The note: clean, GC booted.
    let st = cls(&mut f, Some(&n));
    assert_eq!(st.start, Start::AfterBitsOs, "{:?}", st.why);
    assert!(st.why.iter().any(|w| w.contains("destroyed cleanly by the last stop")), "{:?}", st.why);
    // GX-7j3: the same with the unload toward PMFW in the note: clean too, and the GC stage then catches nothing up.
    let st = cls(&mut f, Some(&StopNote { gc: GcFate::Unloaded, ..n }));
    assert_eq!(st.start, Start::AfterBitsOs, "{:?}", st.why);
    assert!(!crate::gpucore::pspbringup::unload_first(st.start, true) && crate::gpucore::pspbringup::unload_first(st.start, false));
    assert!(!crate::gpucore::pspbringup::unload_first(Start::Cold, false) && !crate::gpucore::pspbringup::unload_first(Start::AfterLinux, false));
    // A clean stop with GC never booted (an M1/M2 keep and stop), GC power-gated as after a cold start.
    f.gc.power = crate::gpucore::gcfake::Power::Off;
    f.gc.booted = false;
    let st = cls(&mut f, Some(&StopNote { gc: GcFate::Untouched, ..n }));
    assert_eq!(st.start, Start::AfterBitsOsNoGc, "{:?}", st.why);
    assert_eq!(admit(st.start), Ok(true), "destroy first, then load");
    // The note's registers no longer match (something wrote the ring since): not trusted.
    for changed in [StopNote { wptr: 0x20, ..n }, StopNote { size: 0x2000, ..n }, StopNote { addr_lo: 0, ..n }, StopNote { addr_hi: 0, ..n }] {
        assert_eq!(cls(&mut f, Some(&changed)).start, Start::AfterCrash, "{changed:?}");
    }
    // A ring the kernel before left up, or a PMFW message without answer: still a crash.
    let st = cls(&mut f, Some(&StopNote { ring: RingFate::NotDestroyed, ..n }));
    assert_eq!(st.start, Start::AfterCrash);
    assert!(st.why.iter().any(|w| w.contains("left it up")), "{:?}", st.why);
    f.smu.stale_message(0x19, 0);
    assert_eq!(cls(&mut f, Some(&n)).start, Start::AfterCrash, "an unanswered message wins");
}

#[test]
fn an_unsettled_gc_is_refused_until_a_reboot() {
    let d = discovery_780m();
    let (mut f, n) = after_stop();
    let u = StopNote { ring: RingFate::NotDestroyed, gc: GcFate::Unsettled, tmr_destroyed: false, ..n };
    let st = classify::classify_with(&d, &mut |b| f.reg_read(b), Some(&u));
    assert_eq!(st.start, Start::GcUnsettled, "{:?}", st.why);
    assert!(admit(st.start).is_err_and(|w| w.contains("reboot")));
    // A crash fake with no note is still a crash; a note for another ring is ignored.
    let mut c = FakeGpu::new(Hostile::None);
    c.start_as(&d, StartAs::AfterCrash(Crash::RingLeft));
    assert_eq!(classify::classify_with(&d, &mut |b| c.reg_read(b), Some(&u)).start, Start::AfterCrash);
}

/// GX-7i2: how the note arrived, with the reason the probe and the warn line give.
#[test]
fn the_arrival_says_why() {
    fn put(_: u32, _: &[u8]) -> bool {
        true
    }
    fn nothing(_: u32) -> Option<std::vec::Vec<u8>> {
        None
    }
    fn broken(_: u32) -> Option<std::vec::Vec<u8>> {
        Some(std::vec![7; 12])
    }
    fn good(t: u32) -> Option<std::vec::Vec<u8>> {
        assert_eq!(t, crate::gpucore::handoff::TAG);
        Some(note(RingFate::Destroyed, GcFate::Booted).bytes().to_vec())
    }
    assert_eq!(Arrival::of(None), Arrival::Unbound);
    assert!(Arrival::of(None).text().starts_with("none: this kernel bound no way"));
    let a = Arrival::of(Some(&NoteIo { put, take: nothing, from_reload: || false }));
    assert_eq!(a, Arrival::Nothing);
    assert!(a.text().contains("a firmware start") && a.note().is_none());
    let r = Arrival::of(Some(&NoteIo { put, take: nothing, from_reload: || true }));
    assert_eq!(r, Arrival::ReloadWithoutNote);
    assert!(r.text().starts_with("none: nothing handed on (a Reload") && !r.text().contains("firmware") && r.note().is_none());
    let a = Arrival::of(Some(&NoteIo { put, take: broken, from_reload: || true }));
    assert_eq!(a, Arrival::Broken { len: 12 });
    assert!(a.text().contains("12 bytes") && a.note().is_none());
    let a = Arrival::of(Some(&NoteIo { put, take: good, from_reload: || true }));
    assert_eq!(a.note(), Some(note(RingFate::Destroyed, GcFate::Booted)));
    assert!(a.text().starts_with("inherited: ring Destroyed"), "{}", a.text());
    // gx_probe's two lines (psplive::note_lines builds them from the boot's arrival and LAST) and the warn line.
    let own = note(RingFate::NotDestroyed, GcFate::Booted);
    for (arr, why) in [(Arrival::Unbound, "none: this kernel bound no way"), (Arrival::Nothing, "none: nothing handed on"), (Arrival::Broken { len: 12 }, "none: 12 bytes"), (a, "inherited: ring Destroyed")] {
        let l = crate::psplive::note_lines_of(&arr, Some(None));
        assert!(l.len() == 2 && l[0].starts_with(&std::format!("note from the kernel before: {why}")) && l[1] == "note of this boot: none (nothing done to the PSP in this boot)", "{l:?}");
        let name = if arr.note().is_some() { "note.inherited" } else { "note.none" };
        // The host's log ring is shared with the tests running in parallel (gx.bringup.reinit floods it): an event
        // overwritten before it is read is logged again, a few times at most.
        let filter = klog::Filter { level: None, subsystem: Some("gx"), name: Some(name) };
        let seen = (0..8).any(|_| {
            let seq = klog::next_seq();
            crate::psplive::log_arrival(&arr, true);
            klog::read(seq, 64, &filter).records.iter().any(|e| e.name == name && e.subsystem == "gx" && e.level == klog::Level::Warn)
        });
        assert!(seen, "{name} not at warn");
    }
    let l = crate::psplive::note_lines_of(&Arrival::Nothing, Some(Some(own)));
    assert_eq!(l[1], std::format!("note of this boot: {}", own.text()));
    assert_eq!(crate::psplive::note_lines_of(&Arrival::Nothing, None)[1], "note of this boot: not read (a stop is setting it)");
}

/// GX-7i2: the note and BitsOS's own traces come before the Linux signs: the doorbell aperture BitsOS's SDMA
/// stage turned on is not a Linux sign (the desk's 28.10. probe after a stop said "after Linux").
#[test]
fn own_note_and_traces_before_the_linux_signs() {
    use crate::gpucore::classify::Own;
    use crate::gpucore::sdma::SdmaAddrs;
    let d = discovery_780m();
    let (mut f, n) = after_stop();
    let aper = SdmaAddrs::from_discovery(&d).and_then(|a| a.nbio).unwrap().aper_en;
    assert!(classify::addresses(&d).iter().any(|(x, b)| *x == "RCC_DOORBELL_APER_EN" && *b == Some(aper)), "the classifier reads the stage's register");
    assert!(f.reg_write(aper, 1));
    let cls = |f: &mut FakeGpu, n: Option<&StopNote>, own: Own| classify::classify_traced(&d, &mut |b| f.reg_read(b), n, own);
    // Neither note nor trace: Linux's sign.
    let st = cls(&mut f, None, Own::default());
    assert_eq!(st.start, Start::AfterLinux, "{:?}", st.why);
    // The note (it matches the PSP registers): a BitsOS stop, GC booted.
    let st = cls(&mut f, Some(&n), Own::default());
    assert_eq!(st.start, Start::AfterBitsOs, "{:?}", st.why);
    assert!(st.why.iter().any(|w| w.contains("left by a BitsOS kernel's SDMA or GFX ring stage")), "{:?}", st.why);
    // This boot's own trace without a note: not Linux (the ring pointer without a note is still unclean).
    let st = cls(&mut f, None, Own { doorbell_aperture: true });
    assert_eq!(st.start, Start::AfterCrash, "{:?}", st.why);
    assert!(st.why.iter().any(|w| w.contains("this boot's SDMA or GFX ring stage turned it on")), "{:?}", st.why);
    // A note that no longer matches (something wrote the ring since) is no word of BitsOS: Linux's sign again.
    assert_eq!(cls(&mut f, Some(&StopNote { wptr: 0x20, ..n }), Own::default()).start, Start::AfterLinux);
}

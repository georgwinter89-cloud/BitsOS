use super::*;
use std::vec;
use std::vec::Vec;

fn g(number: u32, markers: &str) -> GenDir {
    let mut d = GenDir { number, kernel: true, initrd: true, ..Default::default() };
    for m in markers.split('+').filter(|m| !m.is_empty()) {
        d.note(m, false);
    }
    d
}

fn started(c: &Choice) -> (u32, u32, u32, u32, bool) {
    match c {
        Choice::Start { record: r, mark_try, .. } => (r.number, r.reason, r.skipped, r.flags, *mark_try),
        Choice::Refuse(why) => panic!("refused: {why}"),
    }
}

#[test]
fn names_and_pin() {
    assert_eq!(parse_name("12"), Some(12));
    assert_eq!(parse_name("007"), Some(7));
    for bad in ["", "+3", "-1", "1a", "PIN", "99999999999"] {
        assert_eq!(parse_name(bad), None, "{bad}");
    }
    assert_eq!(parse_pin(b"3\r\n"), Some(3));
    assert_eq!(parse_pin(b"  12 "), Some(12));
    for bad in [&b""[..], b"\n", b"three", b"3 4", b"\xff", b"-2"] {
        assert_eq!(parse_pin(bad), None);
    }
}

#[test]
fn listing_notes_markers_case_insensitively() {
    let mut d = GenDir { number: 1, ..Default::default() };
    for (name, dir) in [("kernel", false), ("InitRD", false), ("new", false), ("OK", true), ("try", false), ("x", false)] {
        d.note(name, dir);
    }
    assert!(d.kernel && d.initrd && d.new && d.tried && !d.ok, "a directory named OK is no marker");
    assert_eq!(d.markers(), "NEW+TRY");
    assert_eq!(GenDir::default().markers(), "no marker");
}

#[test]
fn the_five_rules() {
    use gen_reason::*;
    // Rule 1: a pin to a confirmed generation.
    let gens = [g(1, "OK"), g(2, "OK"), g(3, "NEW")];
    assert_eq!(started(&choose(&gens, Pin::Number(1), &[], &[])), (1, PINNED, 3, 0, false));
    // Rule 2: the newest has TRY -> newest OK.
    let gens = [g(1, "OK"), g(2, "OK"), g(3, "TRY")];
    assert_eq!(started(&choose(&gens, Pin::None, &[], &[])), (2, FALLBACK_AFTER_TRY, 3, 0, false));
    // Rule 3: NEW -> write TRY and start it.
    let gens = [g(1, "OK"), g(2, "NEW")];
    assert_eq!(started(&choose(&gens, Pin::None, &[], &[])), (2, NEWEST_TRY, 0, 0, true));
    // Rule 4: newest OK stays.
    let gens = [g(1, "OK"), g(2, "OK")];
    assert_eq!(started(&choose(&gens, Pin::None, &[], &[])), (2, NEWEST_OK, 0, 0, false));
    // Rule 5: bad files -> newest OK at once, no try.
    let gens = [g(1, "OK"), g(2, "OK"), g(3, "NEW")];
    assert_eq!(started(&choose(&gens, Pin::None, &[3], &[])), (2, FALLBACK_BAD_FILES, 3, 0, false));
    let mut nokernel = g(3, "NEW");
    nokernel.kernel = false;
    assert_eq!(started(&choose(&[g(1, "OK"), nokernel], Pin::None, &[], &[])), (1, FALLBACK_BAD_FILES, 3, 0, false));
    let mut noinitrd = g(3, "OK");
    noinitrd.initrd = false;
    assert_eq!(started(&choose(&[g(1, "OK"), noinitrd], Pin::None, &[], &[])), (1, FALLBACK_BAD_FILES, 3, 0, false));
}

#[test]
fn fallbacks_skip_bad_and_unconfirmed() {
    use gen_reason::*;
    let gens = [g(1, "OK"), g(2, "TRY"), g(3, ""), g(4, "OK"), g(5, "TRY")];
    assert_eq!(started(&choose(&gens, Pin::None, &[], &[])), (4, FALLBACK_AFTER_TRY, 5, 0, false));
    assert_eq!(started(&choose(&gens, Pin::None, &[4], &[])), (1, FALLBACK_AFTER_TRY, 5, flag::OTHERS_SKIPPED, false));
    assert!(matches!(choose(&gens, Pin::None, &[4, 5, 1], &[]), Choice::Refuse(_)));
}

#[test]
fn no_confirmed_generation() {
    use gen_reason::*;
    // The first generation on a fresh stick: tried once, nothing to fall back to -> started again.
    let (n, r, _, f, m) = started(&choose(&[g(1, "TRY")], Pin::None, &[], &[]));
    assert_eq!((n, r, f, m), (1, NEWEST_TRY, flag::NO_CONFIRMED_FALLBACK, false));
    // NEW whose marker cannot be written: fallback if possible, else started without the marker.
    assert_eq!(started(&choose(&[g(1, "OK"), g(2, "NEW")], Pin::None, &[], &[2])), (1, FALLBACK_AFTER_TRY, 2, 0, false));
    let (n, _, _, f, m) = started(&choose(&[g(2, "NEW")], Pin::None, &[], &[2]));
    assert_eq!((n, f, m), (2, flag::NO_CONFIRMED_FALLBACK | flag::MARKER_WRITE_FAILED, false));
    // Bad files and no OK: refused, the reason names the generation.
    match choose(&[g(1, "TRY"), g(2, "NEW")], Pin::None, &[2], &[]) {
        Choice::Refuse(why) => assert!(why.contains("generation 2 failed the checks"), "{why}"),
        c => panic!("{c:?}"),
    }
    assert!(matches!(choose(&[], Pin::None, &[], &[]), Choice::Refuse(_)));
}

#[test]
fn unmarked_and_combined_markers() {
    use gen_reason::*;
    assert_eq!(started(&choose(&[g(1, "")], Pin::None, &[], &[])), (1, NEWEST_NO_RULE, 0, flag::UNMARKED, false));
    assert_eq!(started(&choose(&[g(1, "TRY"), g(2, "")], Pin::None, &[], &[])), (2, NEWEST_NO_RULE, 0, flag::UNMARKED, false));
    // An unmarked newest generation with a confirmed one below it gets one try like NEW (a hand copy or an
    // interrupted install must not take the fallback away), then the confirmed one.
    assert_eq!(started(&choose(&[g(1, "OK"), g(2, "")], Pin::None, &[], &[])), (2, NEWEST_TRY, 0, flag::UNMARKED, true));
    assert_eq!(started(&choose(&[g(1, "OK"), g(2, "TRY")], Pin::None, &[], &[])), (1, FALLBACK_AFTER_TRY, 2, 0, false));
    assert_eq!(started(&choose(&[g(1, "OK"), g(2, "")], Pin::None, &[], &[2])), (1, FALLBACK_AFTER_TRY, 2, 0, false));
    // ... and if that confirmed one then turns out bad: started without the marker, flagged.
    let f = flag::UNMARKED | flag::MARKER_WRITE_FAILED | flag::NO_CONFIRMED_FALLBACK | flag::OTHERS_SKIPPED;
    assert_eq!(started(&choose(&[g(1, "OK"), g(2, "")], Pin::None, &[1], &[2])), (2, NEWEST_TRY, 0, f, false));
    // An unmarked older generation is no fallback.
    assert!(matches!(choose(&[g(1, ""), g(2, "TRY")], Pin::None, &[], &[]), Choice::Start { record: Generation { number: 2, .. }, .. }));
    // OK beats TRY beats NEW.
    assert_eq!(started(&choose(&[g(1, "OK"), g(2, "TRY+OK")], Pin::None, &[], &[])).0, 2);
    assert_eq!(started(&choose(&[g(1, "OK"), g(2, "NEW+TRY")], Pin::None, &[], &[])).0, 1);
    assert_eq!(started(&choose(&[g(1, "OK"), g(2, "NEW+OK")], Pin::None, &[], &[])).1, NEWEST_OK);
}

#[test]
fn pins() {
    use gen_reason::*;
    let gens = [g(1, "OK"), g(2, "TRY"), g(3, "OK")];
    assert_eq!(started(&choose(&gens, Pin::Number(3), &[], &[])), (3, PINNED, 0, 0, false));
    for (pin, why) in [(Pin::Number(2), "not confirmed"), (Pin::Number(9), "no such generation"), (Pin::Invalid, "no generation number")] {
        match choose(&gens, pin, &[], &[]) {
            Choice::Start { record, why: w, .. } => {
                assert_eq!((record.number, record.reason, record.flags), (3, NEWEST_OK, flag::PIN_IGNORED));
                assert!(w.contains(why), "{w}");
            }
            c => panic!("{c:?}"),
        }
    }
    let (n, _, _, f, _) = started(&choose(&gens, Pin::Number(1), &[1], &[]));
    assert_eq!((n, f & flag::PIN_IGNORED), (3, flag::PIN_IGNORED), "a pin with bad files is ignored");
}

#[test]
fn marker_paths_only_below_sys() {
    assert!(is_marker_path("\\BITSOS2\\SYS\\7\\TRY"));
    assert!(is_marker_path(&path(123, "TRY")));
    for p in [
        "\\BITSOS2\\SYS\\7\\OK",
        "\\BITSOS2\\SYS\\7\\KERNEL",
        "\\BITSOS2\\SYS\\PIN",
        "\\BITSOS2\\SYS\\TRY",
        "\\BITSOS2\\SYS\\x\\TRY",
        "\\BITSOS2\\SYS\\..\\TRY",
        "\\BITSOS2\\SYS\\7\\TRY\\x",
        "\\BITSOS2\\CONFIG.TXT",
        "\\EFI\\BOOT\\BOOTX64.EFI",
        "\\BITSOS2\\SYSX\\7\\TRY",
        "BITSOS2\\SYS\\7\\TRY",
    ] {
        assert!(!is_marker_path(p), "{p}");
    }
}

#[test]
fn initrd_header() {
    let mut ok = b"070701".to_vec();
    ok.resize(110, b'0');
    assert!(check_initrd(&ok).is_ok());
    ok[5] = b'2';
    assert!(check_initrd(&ok).is_ok());
    ok[0] = b'1';
    assert!(check_initrd(&ok).is_err());
    assert!(check_initrd(b"").is_err());
    assert!(check_initrd(b"070701").is_err());
}

/// What a generation looks like to the simulated loader.
#[derive(Clone, Copy, Debug)]
struct Sim {
    dir: GenDir,
    /// Its KERNEL or INITRD fails the checks when loaded.
    corrupt: bool,
}

/// The loader's loop (load.rs) against the rule: returns the started generation's record, or None.
fn run(sims: &[Sim], pin: Pin, write_fails: bool) -> Option<(Generation, bool)> {
    let dirs: Vec<GenDir> = sims.iter().map(|s| s.dir).collect();
    let (mut bad, mut mark_failed) = (Vec::new(), Vec::new());
    for _ in 0..=2 * sims.len() + 1 {
        match choose(&dirs, pin, &bad, &mark_failed) {
            Choice::Refuse(_) => return None,
            Choice::Start { record, mark_try, .. } => {
                let s = sims.iter().find(|s| s.dir.number == record.number).expect("starts an existing generation");
                assert!(!bad.contains(&record.number), "never a generation already found bad");
                if s.corrupt {
                    bad.push(record.number);
                } else if mark_try && write_fails {
                    mark_failed.push(record.number);
                } else {
                    return Some((record, mark_try));
                }
            }
        }
    }
    panic!("the loader's loop did not end");
}

/// Every marker combination and file state of the newest generation and two older ones, with and without
/// pins and failing marker writes: the invariants of V2-PLAN §7 hold.
#[test]
fn every_combination() {
    use gen_reason::*;
    let files = |i: u32| -> (bool, bool, bool) { (i != 1, i != 2, i == 3) }; // kernel, initrd, corrupt
    let mk = |number: u32, m: u32, f: u32| {
        let (kernel, initrd, corrupt) = files(f);
        Sim { dir: GenDir { number, kernel, initrd, new: m & 1 != 0, tried: m & 2 != 0, ok: m & 4 != 0 }, corrupt }
    };
    let mut cases = 0;
    for nm in 0..8 {
        for nf in 0..4 {
            for older in 0..3usize {
                for om in 0..8u32.pow(older as u32) {
                    for of in 0..4u32.pow(older as u32) {
                        let mut sims = vec![mk(9, nm, nf)];
                        for k in 0..older as u32 {
                            sims.push(mk(4 + k, (om >> (3 * k)) & 7, (of >> (2 * k)) & 3));
                        }
                        for pin in [Pin::None, Pin::Invalid, Pin::Number(9), Pin::Number(4), Pin::Number(5), Pin::Number(77)] {
                            for wf in [false, true] {
                                check(&sims, pin, wf, run(&sims, pin, wf));
                                cases += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(cases > 100_000, "{cases}");

    fn check(sims: &[Sim], pin: Pin, wf: bool, got: Option<(Generation, bool)>) {
        let fine = |s: &Sim| s.dir.kernel && s.dir.initrd && !s.corrupt;
        let n = sims[0];
        let newest_ok = sims[1..].iter().filter(|s| s.dir.ok && fine(s)).map(|s| s.dir.number).max();
        // An older OK generation as the listing shows it (before any file is loaded): it makes an unmarked
        // newest generation take a try like NEW. A pinned one was loaded first, so its corruption is known.
        let listed_ok = sims[1..]
            .iter()
            .any(|s| s.dir.ok && s.dir.kernel && s.dir.initrd && !(s.corrupt && pin == Pin::Number(s.dir.number)));
        let pinned = match pin {
            Pin::Number(p) => sims.iter().find(|s| s.dir.number == p && s.dir.ok && fine(s)).map(|s| s.dir.number),
            _ => None,
        };
        let ctx = || std::format!("{sims:?} pin {pin:?} write_fails {wf}");
        let Some((r, mark)) = got else {
            assert!(pinned.is_none() && newest_ok.is_none() && !fine(&n), "refused although something could start: {}", ctx());
            return;
        };
        let s = sims.iter().find(|s| s.dir.number == r.number).unwrap();
        assert!(fine(s), "started a generation with bad files: {}", ctx());
        assert_eq!(r.flags & flag::PIN_IGNORED != 0, pin != Pin::None && pinned.is_none(), "{}", ctx());
        if let Some(p) = pinned {
            assert_eq!((r.number, r.reason), (p, PINNED), "{}", ctx());
            return;
        }
        if r.number != n.dir.number {
            assert!(s.dir.ok, "a fallback is always confirmed: {}", ctx());
            assert_eq!(Some(r.number), newest_ok, "the newest confirmed one: {}", ctx());
            assert_eq!(r.skipped, n.dir.number, "{}", ctx());
            // A tried newest one is never loaded (its corruption stays unseen); a new one is, before its marker.
            let listed = n.dir.kernel && n.dir.initrd;
            let skipped_untried = listed && !n.dir.ok && (n.dir.tried || ((n.dir.new || listed_ok) && wf && !n.corrupt));
            let want = if skipped_untried { FALLBACK_AFTER_TRY } else { FALLBACK_BAD_FILES };
            assert_eq!(r.reason, want, "{}", ctx());
            assert!(!fine(&n) || skipped_untried, "a good newest one is only passed over after a try: {}", ctx());
            return;
        }
        // The newest generation itself.
        let d = n.dir;
        let unmarked = !d.ok && !d.tried && !d.new;
        assert_eq!(mark, !d.ok && !d.tried && (d.new || listed_ok) && !wf, "{}", ctx());
        match r.reason {
            NEWEST_OK => assert!(d.ok),
            NEWEST_TRY => assert!(!d.ok && (d.new || d.tried || listed_ok)),
            NEWEST_NO_RULE => assert!(unmarked && !listed_ok && r.flags & flag::UNMARKED != 0),
            other => panic!("reason {other} for the newest: {}", ctx()),
        }
        assert_eq!(r.flags & flag::UNMARKED != 0, unmarked, "{}", ctx());
        if !d.ok && (d.tried || ((d.new || listed_ok) && wf)) {
            assert!(newest_ok.is_none(), "an unconfirmed retry only without a fallback: {}", ctx());
            assert!(r.flags & flag::NO_CONFIRMED_FALLBACK != 0, "{}", ctx());
        }
    }
}

#[test]
fn safe_mode_writes_nothing() {
    use gen_reason::*;
    let s = flag::SAFE_MODE;
    // NEW next to an older OK: the OK one starts, the new one keeps its try.
    let gens = [g(1, "OK"), g(2, "NEW")];
    assert_eq!(started(&choose(&gens, Pin::None, &[], &[])), (2, NEWEST_TRY, 0, 0, true));
    assert_eq!(started(&choose_read_only(&gens, Pin::None, &[])), (1, FALLBACK_AFTER_TRY, 2, s, false));
    // An unmarked newest with an older OK: the same.
    assert_eq!(started(&choose_read_only(&[g(1, "OK"), g(2, "")], Pin::None, &[])), (1, FALLBACK_AFTER_TRY, 2, s, false));
    // Only a NEW generation: it starts without TRY, never as NEWEST_TRY (the kernel would try to confirm it).
    let (n, reason, skipped, flags, mark) = started(&choose_read_only(&[g(1, "NEW")], Pin::None, &[]));
    assert_eq!((n, reason, skipped, mark), (1, NEWEST_NO_RULE, 0, false));
    assert_eq!(flags, s | flag::NO_CONFIRMED_FALLBACK);
    // Choices that write nothing anyway stay as they are, with the flag.
    assert_eq!(started(&choose_read_only(&[g(1, "OK"), g(2, "OK")], Pin::None, &[])), (2, NEWEST_OK, 0, s, false));
    assert_eq!(started(&choose_read_only(&[g(1, "OK"), g(2, "NEW+TRY")], Pin::None, &[])), (1, FALLBACK_AFTER_TRY, 2, s, false));
    assert_eq!(started(&choose_read_only(&[g(1, "OK"), g(2, "NEW")], Pin::Number(1), &[])), (1, PINNED, 2, s, false));
    // A bad PIN is still reported; bad files of the confirmed one leave only the new one.
    assert_eq!(started(&choose_read_only(&gens, Pin::Invalid, &[])).3, s | flag::PIN_IGNORED);
    let (n, _, _, flags, mark) = started(&choose_read_only(&gens, Pin::None, &[1]));
    assert_eq!((n, mark), (2, false));
    assert_eq!(flags & (s | flag::NO_CONFIRMED_FALLBACK), s | flag::NO_CONFIRMED_FALLBACK);
    // Only a generation that already had its try: started again, still never as NEWEST_TRY.
    let only_tried = [g(1, "NEW+TRY")];
    let normal = started(&choose(&only_tried, Pin::None, &[], &[]));
    let (n, reason, _, flags, mark) = started(&choose_read_only(&only_tried, Pin::None, &[]));
    assert_eq!((normal.1, normal.4), (NEWEST_TRY, false));
    assert_eq!((n, mark), (normal.0, false));
    assert_ne!(reason, NEWEST_TRY);
    assert_eq!(flags & s, s);
    // Nothing at all: the same refusal as normal.
    assert!(matches!(choose_read_only(&[], Pin::None, &[]), Choice::Refuse(_)));
    assert_eq!(flag_names(s | flag::UNMARKED), "UNMARKED,SAFE_MODE");
}

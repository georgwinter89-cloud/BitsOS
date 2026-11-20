//! UPDATE host tests: the version marks of a bundle, the slot a numbered version installs as, the refusals that
//! keep an install going forward only, the "back" target, the desktop confirmation rule, and the store migration:
//! a medium holding the old counting's generations 103-107 takes version 164 beside them, and every older kernel's
//! and bitsboot's view of it stays a stick they handle (the folder names are numbers, nothing else changed).

extern crate std;

use super::check;
use super::confirm::{DESKTOP_INPUTS, DesktopWatch, Proofs, desktop_proof_for};
use super::plan::{self, Gen, PlanError};
use super::tests::{Model, boot, initrd, kernel, newc, stick_with};
use super::{Confirmed, InstallError, Opts, confirm_on, install_on};
use crate::testkit::{Seg, elf};
use alloc::vec;
use alloc::vec::Vec;
use bootinfo::layout::KERNEL_BASE;
use std::format;

/// The mark as the kernel's build writes it (0xB7 as one byte, then a newline).
fn raw_mark(n: u32) -> Vec<u8> {
    let mut m = format!("BITSOS-VERSION={n:08};").into_bytes();
    m.extend_from_slice(b"\xb7\n");
    m
}

/// A kernel-like ELF of version `n` (tag makes kernels differ).
fn vkernel(n: u32, tag: u8) -> Vec<u8> {
    let text: Vec<u8> = (0..6000u32).map(|i| (i as u8).wrapping_mul(5) ^ tag).collect();
    let mut rodata = b"....BitsOS V2 kernel: entry\n....".to_vec();
    rodata.extend_from_slice(&raw_mark(n));
    rodata.push(tag);
    elf(
        &[Seg { vaddr: KERNEL_BASE, flags: 5, data: &text, memsz: text.len() as u64 }, Seg { vaddr: KERNEL_BASE + 0x2000, flags: 4, data: &rodata, memsz: rodata.len() as u64 }],
        KERNEL_BASE + 0x10,
    )
}

fn vinitrd(n: u32, tag: u8) -> Vec<u8> {
    let v = format!("{n}\n");
    newc(&[("bin/init", &[tag; 300]), ("etc/bitsos/version", v.as_bytes())])
}

fn g(n: u32, marks: &str) -> Gen {
    Gen { n, new: marks.contains('N'), tried: marks.contains('T'), ok: marks.contains('O'), kernel: true, initrd: true, bytes: 1000 }
}

#[test]
fn a_bundle_names_its_version_in_both_halves() {
    let k = check::kernel(&vkernel(165, 1)).unwrap();
    let i = check::initrd(&vinitrd(165, 1)).unwrap();
    assert_eq!((k.version, i.version), (Some(165), Some(165)));
    assert_eq!(check::bundle_version(&k, &i), Ok(Some(165)));
    // A development build: no mark (an older kernel) or mark 0, and no entry.
    let old = check::kernel(&kernel(1)).unwrap();
    let dev = check::kernel(&vkernel(0, 1)).unwrap();
    let plain = check::initrd(&initrd(1)).unwrap();
    assert_eq!((old.version, dev.version, plain.version), (None, None, None));
    assert_eq!(check::bundle_version(&dev, &plain), Ok(None));
    // Mixed pairs are refused: the slot's number would be true of one half only.
    for (k, i, want) in [(&k, &plain, "INITRD names no version"), (&old, &i, "KERNEL names no version"), (&k, &check::initrd(&vinitrd(166, 1)).unwrap(), "version 166")] {
        let why = check::bundle_version(k, i).unwrap_err();
        assert!(why.contains(want), "{why}");
    }
    // The mark appended outside the loaded segments is not the kernel's.
    let mut tail = kernel(2);
    tail.extend_from_slice(&raw_mark(170));
    assert_eq!(check::kernel(&tail).unwrap().version, None);
    // Two different marks in the segments: refused.
    let mut two = vkernel(165, 3);
    let at = two.windows(raw_mark(165).len()).position(|w| w == raw_mark(165).as_slice()).unwrap();
    let mut both = b"....BitsOS V2 kernel: entry\n....".to_vec();
    both.extend_from_slice(&raw_mark(165));
    both.extend_from_slice(&raw_mark(166));
    let k2 = elf(&[Seg { vaddr: KERNEL_BASE, flags: 5, data: &both, memsz: both.len() as u64 }], KERNEL_BASE + 0x10);
    assert!(check::kernel(&k2).unwrap_err().contains("two different version marks"));
    two[at + 20] = b'x';
    assert_eq!(check::kernel(&two).unwrap().version, None, "a damaged mark is no number");
    // A bad etc/bitsos/version.
    assert!(check::initrd(&newc(&[("etc/bitsos/version", b"soon\n")])).unwrap_err().contains("no version number"));
    assert_eq!(check::initrd(&newc(&[("etc/bitsos/version", b"1\n"), ("./etc/bitsos/version", b"9\n")])).unwrap().version, Some(9), "the later entry wins");
}

#[test]
fn a_numbered_version_takes_its_own_number_and_only_goes_forward() {
    let gens = [g(103, "O"), g(104, "O"), g(105, "O"), g(106, "O"), g(107, "O")];
    let p = plan::install_versioned(&gens, None, 5, false, false, Some(164)).unwrap();
    assert_eq!((p.number, p.replaced, p.fallback, p.prune.clone()), (164, None, Some(107), vec![]));
    assert_eq!(plan::install_versioned(&gens, None, 3, false, false, Some(164)).unwrap().prune, vec![103, 104], "sys_keep=3");
    // The same number again, a lower one: refused, with the way back named.
    assert_eq!(plan::install_versioned(&gens, None, 5, false, false, Some(107)), Err(PlanError::NotNewer { want: 107, newest: 107 }));
    assert_eq!(plan::install_versioned(&gens, None, 5, false, false, Some(50)), Err(PlanError::NotNewer { want: 50, newest: 107 }));
    assert!(PlanError::NotNewer { want: 107, newest: 107 }.describe().contains("installed already"));
    assert!(PlanError::NotNewer { want: 50, newest: 107 }.describe().contains("Zurück"));
    // An unconfirmed newest (a version that fell back) is replaced by the next number, as before; its own number
    // is not taken again.
    let tried = [g(164, "O"), g(165, "NT")];
    let p = plan::install_versioned(&tried, None, 5, false, false, Some(166)).unwrap();
    assert_eq!((p.number, p.replaced, p.fallback), (166, Some(165), Some(164)));
    assert!(plan::install_versioned(&tried, None, 5, false, false, Some(165)).is_err());
    // A number past the medium's rule.
    assert_eq!(plan::install_versioned(&gens, None, 5, false, false, Some(plan::NUMBER_MAX + 1)), Err(PlanError::NoNumberLeft));
    // A development build keeps the old counting (and `install` is that).
    assert_eq!(plan::install_versioned(&gens, None, 5, false, false, None), plan::install(&gens, None, 5, false, false));
    assert_eq!(plan::install(&gens, None, 5, false, false).unwrap().number, 108);
}

#[test]
fn back_goes_to_the_newest_confirmed_below_the_running_one() {
    let gens = [g(105, "O"), g(106, "O"), g(107, "NT"), g(164, "O"), g(165, "O")];
    assert_eq!(plan::back_target(&gens, Some(165)), Ok(164));
    assert_eq!(plan::back_target(&gens, Some(164)), Ok(106), "an unconfirmed one is never a way back");
    assert_eq!(plan::back_target(&gens, None), Ok(164), "unknown: from the newest");
    assert!(plan::back_target(&gens, Some(105)).unwrap_err().contains("no confirmed generation older than 105"));
}

/// The store migration, on the model of a stick that holds the Gaming-PC's generations 103-107 (all OK, the old
/// counting), proven with bitsboot's own rule: version 164 installs beside them, starts as its one try, falls
/// back to 107 when it never confirms, confirms itself otherwise; "back" (a PIN) starts 107 again; an older kernel
/// that installs a development build afterwards counts on from the highest folder (165) as it always did.
#[test]
fn version_164_on_a_stick_with_103_to_107() {
    let m = Model::new(stick_with(&[(103, "O", 3), (104, "O", 4), (105, "O", 5), (106, "O", 6), (107, "O", 7)]));
    let before = m.st.borrow().clone();
    assert_eq!(boot(&before).map(|b| (b.0, b.1)), Some((107, "NEWEST_OK")));
    let (k, i) = (vkernel(164, 1), vinitrd(164, 1));
    // Refused first: a lower number, a mixed pair; nothing changes.
    for (kk, ii) in [(vkernel(100, 1), vinitrd(100, 1)), (vkernel(164, 1), initrd(1))] {
        assert!(matches!(install_on(&m, &kk, &ii, None, &Opts::default()), Err(InstallError::Refused(_))));
        assert_eq!(*m.st.borrow(), before);
    }
    let r = install_on(&m, &k, &i, None, &Opts::default()).unwrap();
    assert_eq!((r.plan.number, r.version, r.plan.fallback, r.plan.prune.clone()), (164, Some(164), Some(107), vec![]));
    let s = m.st.borrow().clone();
    assert_eq!(s.gens.keys().copied().collect::<Vec<_>>(), vec![103, 104, 105, 106, 107, 164]);
    // Every old generation is untouched (files and markers), so every older kernel and bitsboot read it as before.
    for n in 103..=107 {
        assert_eq!(s.gens[&n], before.gens[&n], "generation {n}");
    }
    let (n, why, bk, _) = boot(&s).unwrap();
    assert_eq!((n, why, bk == k), (164, "NEWEST_TRY", true));
    // Never confirmed: the boot after starts 107.
    let mut tried = s.clone();
    tried.gens.get_mut(&164).unwrap().files.insert("TRY".into(), Vec::new());
    assert_eq!(boot(&tried).map(|b| (b.0, b.1)), Some((107, "FALLBACK_AFTER_TRY")));
    // Confirmed by its kernel: 164 for good.
    let c = Model::new(tried);
    assert!(matches!(confirm_on(&c, 164), Ok(Confirmed::Now { .. })));
    let ok = c.st.borrow().clone();
    assert_eq!(boot(&ok).map(|b| (b.0, b.1)), Some((164, "NEWEST_OK")));
    // "Zurück": the PIN names the newest confirmed one below the running 164.
    let gens: Vec<Gen> = ok.gens.keys().map(|&n| g(n, "O")).collect();
    let back = plan::back_target(&gens, Some(164)).unwrap();
    let mut pinned = ok.clone();
    pinned.pin = Some(back);
    assert_eq!(boot(&pinned).map(|b| (b.0, b.1)), Some((107, "PINNED")));
    // An older kernel on the same stick (the old counting, no version) installs a development build: 165.
    let old = Model::new(ok.clone());
    let r = install_on(&old, &kernel(9), &initrd(9), None, &Opts::default()).unwrap();
    assert_eq!((r.plan.number, r.version), (165, None));
    assert!(r.warnings.iter().any(|w| w.contains("no version number")), "{:?}", r.warnings);
    // The next numbered version must be above it.
    let s2 = old.st.borrow().clone();
    let gens2: Vec<Gen> = s2.gens.keys().map(|&n| g(n, if n == 165 { "N" } else { "O" })).collect();
    assert!(plan::install_versioned(&gens2, None, 5, false, false, Some(165)).is_err());
    assert_eq!(plan::install_versioned(&gens2, None, 5, false, false, Some(166)).unwrap().number, 166);
}

/// A cut after every single change of a numbered install leaves a stick bitsboot starts something good from.
#[test]
fn a_numbered_install_survives_a_cut_anywhere() {
    let base = stick_with(&[(106, "O", 6), (107, "O", 7)]);
    let (k, i) = (vkernel(164, 2), vinitrd(164, 2));
    for cut in 0..20 {
        let mut m = Model::new(base.clone());
        m.cut = Some(cut);
        let _ = install_on(&m, &k, &i, None, &Opts::default());
        let s = m.st.borrow().clone();
        let (n, _, bk, _) = boot(&s).unwrap_or_else(|| panic!("cut {cut}: nothing starts"));
        assert!(n == 107 || (n == 164 && bk == k), "cut {cut}: starts {n}");
    }
}

#[test]
fn the_desktop_confirms_only_after_a_held_screen_and_spread_real_input() {
    const S: u64 = 1_000_000_000;
    let need = 120 * S;
    // Held from t=0, input at 10, 50, 90 s: done at 120 s, not before.
    let mut d = DesktopWatch::new(need);
    let mut input = 5u64; // what came before the hold does not count
    let mut done_at = None;
    for t in (0..=140u64).step_by(2) {
        if [10, 50, 90].contains(&t) {
            input += 4;
        }
        if d.sample(t * S, 1, 0, input) && done_at.is_none() {
            done_at = Some(t);
        }
    }
    assert_eq!(done_at, Some(120));
    // Input only in the first minute's second: too close together.
    let mut d = DesktopWatch::new(need);
    let mut input = 0;
    for t in (0..=200u64).step_by(2) {
        if (10..=20).contains(&t) {
            input += 1;
        }
        assert!(!d.sample(t * S, 1, 0, input), "t={t}: input over 10 s is no proof");
    }
    // Never held (a text console only, the screen given back): never, however much real input.
    let mut d = DesktopWatch::new(need);
    for t in (0..=300u64).step_by(2) {
        assert!(!d.sample(t * S, 4, 4, t), "t={t}");
    }
    // A hold that ends and comes back: the proof starts again.
    let mut d = DesktopWatch::new(need);
    for t in (0..=118u64).step_by(2) {
        assert!(!d.sample(t * S, 1, 0, t));
    }
    // The hold ends at 118 s and a new one starts: everything again from there.
    assert!(!d.sample(120 * S, 2, 1, 200));
    assert!(d.restarts >= 1);
    let mut done = false;
    for t in (122..=240u64).step_by(2) {
        done = d.sample(t * S, 2, 1, 200 + t);
    }
    assert!(done, "a whole new proof time with input: done");
    assert_eq!(DESKTOP_INPUTS, 3);
}

#[test]
fn a_slot_names_its_version_however_the_pieces_fall() {
    let mut k = vkernel(165, 3);
    let at = k.windows(bitsver::PREFIX.len()).position(|w| w == bitsver::PREFIX).unwrap();
    // Pieces of 512 bytes: put the mark across a piece boundary, then at its very start and end.
    for shift in [0usize, 1, 13, 25, 26, 511] {
        let pad = (512 - (at % 512) + 512 - shift) % 512;
        let mut f = vec![0u8; pad];
        f.extend_from_slice(&k);
        let read = |off: u64, b: &mut [u8]| -> Result<usize, i64> {
            let o = (off as usize).min(f.len());
            let take = b.len().min(f.len() - o);
            b[..take].copy_from_slice(&f[o..o + take]);
            Ok(take)
        };
        assert_eq!(check::stream_version(&mut { read }, 512), Ok(165), "shift {shift}");
    }
    // A development build and a kernel from before UPDATE: 0. Two marks: 0 (no number can be trusted).
    let one = |f: &[u8]| {
        let f = f.to_vec();
        check::stream_version(&mut move |off, b| {
            let o = (off as usize).min(f.len());
            let take = b.len().min(f.len() - o);
            b[..take].copy_from_slice(&f[o..o + take]);
            Ok(take)
        }, 4096)
    };
    assert_eq!(one(&vkernel(0, 3)), Ok(0));
    assert_eq!(one(&kernel(2)), Ok(0));
    k.extend_from_slice(&raw_mark(166));
    assert_eq!(one(&k), Ok(0));
    assert_eq!(check::stream_version(&mut |_, _| Err(5), 512), Err(5));
}

/// The desk's development builds keep MCP as their only proof: a held screen and Georg's real input over the proof
/// time do not confirm them (one with a broken MCP falls back by itself); a numbered version in its own slot is
/// confirmed by the same desktop use.
#[test]
fn only_a_numbered_version_in_its_own_slot_is_confirmed_by_the_desktop() {
    const S: u64 = 1_000_000_000;
    assert!(desktop_proof_for(165, 165));
    assert!(!desktop_proof_for(0, 166), "a development build");
    assert!(!desktop_proof_for(165, 166), "a version outside its own slot");
    let run = |version: u32, generation: u32| {
        let mut p = Proofs::new(120 * S, version, generation);
        let mut by = None;
        // No MCP answer at all; the screen held from the start, real input every 10 s.
        for t in (0..=400u64).step_by(2) {
            by = by.or(p.sample(t * S, 0, 1, 0, t / 10));
        }
        by
    };
    assert_eq!(run(0, 166), None, "stays pending: the next boot falls back");
    assert_eq!(run(165, 166), None);
    assert_eq!(run(165, 165), Some("desktop"));
    // MCP still confirms a development build, as before UPDATE.
    let mut p = Proofs::new(120 * S, 0, 166);
    let by = (0..=130u64).step_by(2).find_map(|t| p.sample(t * S, t, 0, 0, 0));
    assert_eq!(by, Some("mcp"));
}

/// UPD-4: a slot installed with its parts holds FILES (written first, read back like the bundle); bitsboot ignores
/// it; a cut while the bundle follows leaves a slot bitsboot refuses or gives one try, never a confirmed slot that
/// runs without its parts.
#[test]
fn a_slot_with_parts_holds_files_first() {
    let files = super::parts_rules::files_render(&[(String::from("bin"), "ab".repeat(32))]);
    let (k, i) = (vkernel(169, 1), vinitrd(169, 1));
    let m = Model::new(stick_with(&[(168, "O", 8)]));
    let r = super::install_with(&m, &k, &i, None, Some(files.as_bytes()), &Opts::default()).unwrap();
    assert_eq!(r.plan.number, 169);
    let s = m.st.borrow().clone();
    assert_eq!(s.gens[&169].files.get("FILES").map(|f| f.as_slice()), Some(files.as_bytes()));
    assert_eq!(boot(&s).map(|b| (b.0, b.1)), Some((169, "NEWEST_TRY")));
    assert!(!s.gens[&168].files.contains_key("FILES"), "the older slot keeps running with the medium's own folders");
    // Cut after FILES and the folder (changes: folder, FILES, then INITRD half written).
    for cut in 1..4 {
        let mut m = Model::new(stick_with(&[(168, "O", 8)]));
        m.cut = Some(cut);
        assert!(super::install_with(&m, &k, &i, None, Some(files.as_bytes()), &Opts::default()).is_err());
        let s = m.st.borrow().clone();
        if let Some(d) = s.gens.get(&169) {
            assert!(!d.files.contains_key("OK") && !d.files.contains_key("NEW"), "cut {cut}: never marked");
        }
        assert_eq!(boot(&s).map(|b| b.0), Some(168), "cut {cut}: the fallback starts");
    }
}

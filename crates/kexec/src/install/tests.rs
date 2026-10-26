//! Host tests of RB-6: the bundle checks, the generation arithmetic, the whole install and confirmation on a
//! model of the stick (with a cut after every single change), the proof rule of the confirmation.

extern crate std;

use super::check;
use super::confirm::{MAX_S, MIN_S, Watch, needs_confirmation};
use super::plan::{self, Gen, PlanError};
use super::{Confirmed, InstallError, Opts, Stick, confirm_on, install_on};
use crate::testkit::{Seg, elf};
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use bootinfo::gen_reason;
use bootinfo::layout::KERNEL_BASE;
use core::cell::RefCell;
use fs_fat::sysapi::Marker;
use std::format;
use uapi::errno::*;

// ---------- bundles ----------

/// A kernel-like ELF with the BitsOS signature in its rodata; `tag` makes kernels differ.
fn kernel(tag: u8) -> Vec<u8> {
    let text: Vec<u8> = (0..6000u32).map(|i| (i as u8).wrapping_mul(7) ^ tag).collect();
    let mut rodata = b"....BitsOS V2 kernel: entry\n....".to_vec();
    rodata.push(tag);
    let data = vec![tag; 64];
    elf(
        &[
            Seg { vaddr: KERNEL_BASE, flags: 5, data: &text, memsz: text.len() as u64 },
            Seg { vaddr: KERNEL_BASE + 0x2000, flags: 4, data: &rodata, memsz: rodata.len() as u64 },
            Seg { vaddr: KERNEL_BASE + 0x3000, flags: 6, data: &data, memsz: 0x2000 },
        ],
        KERNEL_BASE + 0x10,
    )
}

fn newc(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let put = |name: &str, body: &[u8], mode: u32, out: &mut Vec<u8>| {
        let h = format!(
            "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            1,
            mode,
            0,
            0,
            1,
            0,
            body.len(),
            0,
            0,
            0,
            0,
            name.len() + 1,
            0
        );
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(body);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    };
    for (n, b) in entries {
        put(n, b, 0o100644, &mut out);
    }
    put("TRAILER!!!", b"", 0, &mut out);
    out
}

fn initrd(tag: u8) -> Vec<u8> {
    newc(&[("bin/init", &[tag; 300]), ("etc/motd", b"hello\n")])
}

#[test]
fn kernel_checks() {
    let k = kernel(1);
    let f = check::kernel(&k).unwrap();
    assert_eq!(f.entry, KERNEL_BASE + 0x10);
    assert_eq!(f.bytes, k.len() as u64);
    // Every cut-short file is refused (Reload's or bitsboot's rule names it).
    let end = crate::elf::parse_head(&k).unwrap().unwrap().file_end() as usize;
    for len in [0, 3, 63, 64, 200, 0x1000, 0x1000 + 100, end - 1] {
        assert!(check::kernel(&k[..len]).is_err(), "cut at {len} accepted");
    }
    // No signature.
    let base = KERNEL_BASE;
    let plain = elf(&[Seg { vaddr: base, flags: 5, data: &[0x90; 64], memsz: 64 }], base);
    assert!(check::kernel(&plain).unwrap_err().contains("signature"));
    // The signature only outside the loaded segments (appended to the file) does not count.
    let mut tail = plain.clone();
    tail.extend_from_slice(check::SIGNATURE);
    assert!(check::kernel(&tail).unwrap_err().contains("signature"));
    // Linux's layout (image above the BitsOS base) is refused.
    let mut sig = vec![0x90u8; 64];
    sig.extend_from_slice(check::SIGNATURE);
    let linux = elf(&[Seg { vaddr: base + 0x100_0000, flags: 5, data: &sig, memsz: sig.len() as u64 }], base + 0x100_0000);
    assert!(check::kernel(&linux).unwrap_err().contains("laid out"));
    let ok = elf(&[Seg { vaddr: base, flags: 5, data: &sig, memsz: sig.len() as u64 }], base);
    check::kernel(&ok).unwrap();
    // bitsboot's own rule: different rights on one page.
    let wx = elf(
        &[Seg { vaddr: base, flags: 5, data: &sig, memsz: sig.len() as u64 }, Seg { vaddr: base + 0x800, flags: 6, data: &[1; 16], memsz: 16 }],
        base,
    );
    assert!(check::kernel(&wx).unwrap_err().contains("bitsboot"), "{:?}", check::kernel(&wx));
    // Not an ELF at all.
    assert!(check::kernel(b"MZ\x90\x00 this is a PE file, not an ELF").is_err());
}

#[test]
fn kernel_checks_survive_random_damage() {
    let k = kernel(9);
    let mut x: u64 = 0x1234_5678_9abc_def1;
    for _ in 0..3000 {
        let mut b = k.clone();
        for _ in 0..4 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let at = (x as usize) % 0x200.min(b.len());
            b[at] = (x >> 32) as u8;
        }
        let _ = check::kernel(&b);
    }
}

#[test]
fn initrd_checks() {
    let i = initrd(1);
    let f = check::initrd(&i).unwrap();
    assert_eq!(f.entries, 2);
    for len in 0..i.len() {
        assert!(check::initrd(&i[..len]).is_err(), "cut at {len} accepted");
    }
    // Concatenated archives with zero padding between them (the kernel's unpack takes them).
    let mut two = i.clone();
    two.extend_from_slice(&[0; 512]);
    two.extend_from_slice(&initrd(2));
    assert_eq!(check::initrd(&two).unwrap().entries, 4);
    // Trailing zero padding only.
    let mut pad = i.clone();
    pad.extend_from_slice(&[0; 4096]);
    check::initrd(&pad).unwrap();
    // Junk after the trailer.
    let mut junk = i.clone();
    junk.extend_from_slice(&[b'g'; 200]);
    assert!(check::initrd(&junk).is_err());
    // gzip, an old-style cpio, garbage hex.
    assert!(check::initrd(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3].repeat(20)).is_err());
    assert!(check::initrd(&b"070707".repeat(30)).is_err());
    let mut bad = i.clone();
    bad[60] = b'z';
    assert!(check::initrd(&bad).is_err());
}

#[test]
fn config_checks() {
    let old = b"token=abc\nloglevel=info\nstick_write=off\n";
    check::config(Some(old), b"token=abc\nloglevel=debug\nstick_write=off\n").unwrap();
    check::config(Some(old), b"token=other\n").unwrap(); // dropping a guarded line is allowed
    check::config(Some(old), b"token=abc\nwlan=on\n").unwrap(); // wlan is free (R8d, 21.10.)
    assert!(check::config(Some(old), b"token=abc\nstick_write=on\n").is_err());
    assert!(check::config(Some(old), b"token=abc\nstick_write=off\nstick_write=on\n").is_err());
    assert!(check::config(Some(old), b"token=abc\nnvme_write=on\n").is_err());
    assert!(check::config(None, b"nvme_write=on\n").is_err());
    assert!(check::config(Some(old), b"loglevel=info\n").unwrap_err().contains("token"));
    assert!(check::config(Some(old), b"token=\n").unwrap_err().contains("token"));
    assert!(check::config(Some(old), b"#token=abc\n").unwrap_err().contains("token"));
    check::config(None, b"loglevel=info\n").unwrap();
    let huge = vec![b'#'; (fs_fat::protect::CONFIG_MAX + 1) as usize];
    assert!(check::config(None, &huge).is_err());
}

#[test]
fn sys_keep_values() {
    assert_eq!(check::sys_keep(None), Ok(5));
    assert_eq!(check::sys_keep(Some(b"a=b\n")), Ok(5));
    assert_eq!(check::sys_keep(Some(b"sys_keep=3\nsys_keep=7 # more\n")), Ok(7));
    assert_eq!(check::sys_keep(Some(b"sys_keep=0\n")), Ok(1));
    assert!(check::sys_keep(Some(b"sys_keep=ten\n")).is_err());
}

// ---------- plan ----------

fn g(n: u32, marks: &str) -> Gen {
    Gen { n, new: marks.contains('N'), tried: marks.contains('T'), ok: marks.contains('O'), kernel: true, initrd: true, bytes: 1000 }
}

#[test]
fn numbering_and_replacement() {
    // Fresh stick (no generation): needs allow_no_fallback.
    assert_eq!(plan::install(&[], None, 5, false, false), Err(PlanError::NoFallback));
    let p = plan::install(&[], None, 5, false, true).unwrap();
    assert_eq!((p.number, p.replaced, p.fallback), (1, None, None));
    // Confirmed newest: n+1, nothing replaced.
    let p = plan::install(&[g(1, "O"), g(2, "O")], None, 5, false, false).unwrap();
    assert_eq!((p.number, p.replaced, p.fallback), (3, None, Some(2)));
    // Unconfirmed newest (NEW, TRY, both, none) is replaced; the number goes up, never reused.
    for m in ["N", "T", "NT", ""] {
        let p = plan::install(&[g(1, "O"), g(4, m)], None, 5, false, false).unwrap();
        assert_eq!((p.number, p.replaced, p.fallback), (5, Some(4), Some(1)), "{m}");
        assert_eq!(p.freed, 1000);
    }
    // Only an unconfirmed one: no fallback would stay.
    assert_eq!(plan::install(&[g(1, "NT")], None, 5, false, false), Err(PlanError::NoFallback));
    // An OK generation with stale markers is not replaced, and gets tidied.
    let p = plan::install(&[g(1, "OT"), g(2, "ON")], None, 5, false, false).unwrap();
    assert_eq!((p.number, p.replaced), (3, None));
    assert_eq!(p.tidy, vec![(1, plan::Stale::Try), (2, plan::Stale::New)]);
    // The number range ends.
    assert_eq!(plan::install(&[g(1, "O"), g(plan::NUMBER_MAX, "O")], None, 5, false, false), Err(PlanError::NoNumberLeft));
    let p = plan::install(&[g(1, "O"), g(plan::NUMBER_MAX - 1, "O")], None, 5, false, false).unwrap();
    assert_eq!(p.number, plan::NUMBER_MAX);
}

#[test]
fn pin_and_prune() {
    let gens: Vec<Gen> = (1..=6).map(|n| g(n, "O")).collect();
    let p = plan::install(&gens, None, 2, false, false).unwrap();
    assert_eq!(p.prune, vec![1, 2, 3, 4]);
    assert_eq!(p.fallback, Some(6));
    assert_eq!(p.freed, 4000);
    let p = plan::install(&gens, Some(2), 2, false, false).unwrap();
    assert_eq!(p.prune, vec![1, 3, 4]);
    assert_eq!(p.pinned_instead, Some(2));
    // unpin: the pin no longer protects, and no warning.
    let p = plan::install(&gens, Some(2), 2, true, false).unwrap();
    assert_eq!(p.prune, vec![1, 2, 3, 4]);
    assert_eq!(p.pinned_instead, None);
    // A pin on an unconfirmed generation is no valid pin (bitsboot ignores it), but its folder is kept.
    let p = plan::install(&[g(1, "O"), g(2, "T"), g(3, "O")], Some(2), 5, false, false).unwrap();
    assert_eq!(p.pinned_instead, None);
    assert_eq!(p.prune, Vec::<u32>::new());
    // An older unconfirmed generation is never a fallback and goes.
    let p = plan::install(&[g(1, "O"), g(2, "N"), g(3, "O")], None, 5, false, false).unwrap();
    assert_eq!(p.prune, vec![2]);
}

#[test]
fn prune_invariants_over_many_layouts() {
    for len in 0..=7u32 {
        for bits in 0..(1u32 << len) {
            let gens: Vec<Gen> = (0..len).map(|i| g(i + 1, if bits & (1 << i) != 0 { "O" } else { "N" })).collect();
            for keep in 1..=4 {
                for pin in core::iter::once(None).chain((1..=len + 1).map(Some)) {
                    for unpin in [false, true] {
                        let Ok(p) = plan::install(&gens, pin, keep, unpin, true) else { panic!("{bits:b}") };
                        let newest_ok = gens.iter().filter(|x| x.ok).map(|x| x.n).max();
                        assert_eq!(p.number, len + 1);
                        assert!(!p.prune.contains(&p.number));
                        assert!(newest_ok.is_none_or(|n| !p.prune.contains(&n)), "newest OK stays");
                        assert_eq!(p.fallback, newest_ok);
                        let pin_kept = if unpin { None } else { pin };
                        assert!(pin_kept.is_none_or(|q| !p.prune.contains(&q)), "pin stays");
                        // Left: at most `keep` OK ones besides the pin, nothing unconfirmed but the pin.
                        let left: Vec<&Gen> = gens.iter().filter(|x| !p.prune.contains(&x.n) && Some(x.n) != p.replaced).collect();
                        let extra = left.iter().filter(|x| x.ok && Some(x.n) != pin_kept).count();
                        assert!(extra <= keep as usize);
                        assert!(left.iter().all(|x| x.ok || Some(x.n) == pin_kept), "{bits:b} keep {keep} pin {pin:?}");
                        let ok_all = gens.iter().filter(|x| x.ok).count();
                        assert!(left.iter().filter(|x| x.ok).count() >= ok_all.min(keep as usize));
                        if let Some(r) = p.replaced {
                            assert_eq!(r, len);
                            assert!(!gens[len as usize - 1].ok);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn next_boot_follows_bitsboot() {
    let after = [g(1, "O"), g(2, "N")];
    assert_eq!(plan::next_boot(&after, None).unwrap().0, 2);
    assert_eq!(plan::next_boot(&after, None).unwrap().1, "NEWEST_TRY");
    let tried = [g(1, "O"), g(2, "NT")];
    let (n, r, _) = plan::next_boot(&tried, None).unwrap();
    assert_eq!((n, r), (1, "FALLBACK_AFTER_TRY"));
    let (n, r, _) = plan::next_boot(&after, Some(1)).unwrap();
    assert_eq!((n, r), (1, "PINNED"));
    assert!(plan::next_boot(&[], None).is_err());
}

#[test]
fn confirm_plans() {
    use plan::ConfirmPlan;
    assert_eq!(plan::confirm(&[g(1, "O")], 2, None, 5), ConfirmPlan::Missing);
    assert_eq!(plan::confirm(&[g(1, "O"), g(2, "NT")], 2, None, 5), ConfirmPlan::Confirm { prune: vec![], tidy: vec![] });
    // sys_keep=1: the old fallback goes once the new one is confirmed.
    assert_eq!(plan::confirm(&[g(1, "O"), g(2, "NT")], 2, None, 1), ConfirmPlan::Confirm { prune: vec![1], tidy: vec![] });
    // Pinned 1 stays.
    assert_eq!(plan::confirm(&[g(1, "O"), g(2, "NT")], 2, Some(1), 1), ConfirmPlan::Confirm { prune: vec![], tidy: vec![] });
    assert_eq!(plan::confirm(&[g(1, "OT"), g(2, "OT")], 2, None, 5), ConfirmPlan::AlreadyOk { prune: vec![], tidy: vec![(1, plan::Stale::Try)] });
    // An older generation confirming while a newer NEW waits: it becomes the fallback, the new one stays.
    assert_eq!(plan::confirm(&[g(1, "O"), g(2, "NT"), g(3, "N")], 2, None, 1), ConfirmPlan::Confirm { prune: vec![1], tidy: vec![] });
}

// ---------- the proof rule ----------

#[test]
fn watch_needs_answers_over_the_whole_time() {
    const S: u64 = 1_000_000_000;
    let mut w = Watch::new(120 * S);
    assert_eq!(w.gap_ns, 30 * S);
    assert!(!w.sample(2 * S, 0));
    assert!(!w.sample(4 * S, 3)); // first answers
    // An answer every 20 s: within the 30 s gap bound.
    for (i, t) in (24..124).step_by(20).enumerate() {
        assert!(!w.sample(t * S, 4 + i as u64), "{t}");
    }
    assert!(!w.sample(124 * S - 1, 9)); // just short of 120 s after the first
    assert!(w.sample(124 * S, 10));
    assert_eq!(w.covered_ns(), 120 * S);
    assert_eq!(w.restarts, 0);
    assert!(w.sample(500 * S, 10), "a proof once done stays done");
    // Answers only at the start never confirm.
    let mut w = Watch::new(120 * S);
    for t in 0..200 {
        assert!(!w.sample(t * S, if t < 10 { t } else { 9 }));
    }
    // Answers counted before the watch began count from the first sample.
    let mut w = Watch::new(10 * S);
    assert_eq!(w.gap_ns, 6 * S, "a short proof tolerates three samples of silence");
    assert!(!w.sample(S, 50));
    assert!(!w.sample(6 * S, 51));
    assert!(w.sample(11 * S, 52));
}

#[test]
fn two_answers_two_minutes_apart_prove_nothing() {
    const S: u64 = 1_000_000_000;
    // The checker's case: one answer at boot, one stray answer 120 s later, nothing between.
    let mut w = Watch::new(120 * S);
    assert!(!w.sample(2 * S, 1));
    for t in (4..122).step_by(2) {
        assert!(!w.sample(t * S, 1));
    }
    assert!(!w.sample(122 * S, 2), "a stray answer after a long silence starts the proof again");
    assert_eq!(w.restarts, 1);
    assert_eq!(w.covered_ns(), 0);
    // A pause just over the bound in the middle of an otherwise steady run restarts it too.
    let mut w = Watch::new(120 * S);
    let mut c = 0;
    let mut t = 0;
    while t <= 60 {
        c += 1;
        assert!(!w.sample(t * S, c));
        t += 2;
    }
    // Silent from 60 s to 92 s (32 s > 30 s).
    while t <= 92 {
        assert!(!w.sample(t * S, c));
        assert_eq!(w.restarts, u32::from(t >= 92), "{t}: the silence is noticed while it lasts, not before");
        t += 2;
    }
    assert_eq!(w.covered_ns(), 0);
    while t < 94 + 120 {
        c += 1;
        assert!(!w.sample(t * S, c), "{t}");
        t += 2;
    }
    c += 1;
    assert!(w.sample((94 + 120) * S, c), "a fresh 120 s run after the pause confirms");
    // A pause of exactly the bound is still unbroken.
    let mut w = Watch::new(120 * S);
    let mut c = 0;
    for t in (0..=120).step_by(30) {
        c += 1;
        let done = w.sample(t * S, c);
        assert_eq!(done, t == 120, "{t}");
    }
    assert_eq!(w.restarts, 0);
    // Every proof time the CONFIG allows gets a bound between three samples and 30 s.
    for s in [MIN_S, 20, 60, 120, 3600, MAX_S] {
        let g = super::confirm::gap_for(s * S);
        assert!(g >= 6 * S && g <= 30 * S && g <= s * S, "{s}: {g}");
    }
}

#[test]
fn a_replaced_generation_ends_the_watch_and_a_waiting_write() {
    use super::confirm::{Status, after_replace};
    let p = Status::Pending { generation: 4, covered_s: 30, need_s: 120, counter: "mcp.requests" };
    assert_eq!(after_replace(&p, 4), Some(Status::Replaced { generation: 4 }));
    assert_eq!(after_replace(&p, 3), None);
    assert_eq!(after_replace(&Status::Writing { generation: 4 }, 4), Some(Status::Replaced { generation: 4 }));
    assert_eq!(after_replace(&Status::Writing { generation: 4 }, 5), None);
    for done in [Status::Confirmed { generation: 4 }, Status::NotNeeded(String::from("x")), Status::Unknown] {
        assert_eq!(after_replace(&done, 4), None, "{done:?}");
    }
}

#[test]
fn the_test_panic_entry_is_found_in_concatenated_archives() {
    use super::confirm::{TEST_PANIC_ENTRY, has_entry};
    let name = core::str::from_utf8(TEST_PANIC_ENTRY).unwrap();
    let look = |d: &[u8]| has_entry(&mut |off, b: &mut [u8]| {
        let o = off as usize;
        let n = b.len().min(d.len().saturating_sub(o));
        b[..n].copy_from_slice(&d[o..o + n]);
        n == b.len()
    }, d.len() as u64, TEST_PANIC_ENTRY);
    let plain = initrd(1);
    assert!(!look(&plain));
    let mut two = plain.clone();
    two.extend_from_slice(&[0; 508]);
    two.extend_from_slice(&newc(&[("etc/bitsos", b""), (name, b"x")]));
    assert!(look(&two));
    assert!(look(&newc(&[(name, b"")])));
    // A longer or shorter name with the same start is not it; junk ends the walk.
    assert!(!look(&newc(&[(&format!("{name}x"), b"")])));
    assert!(!look(&newc(&[(&name[..name.len() - 1], b"")])));
    let mut junk = plain.clone();
    junk.extend_from_slice(b"garbage-garbage-garbage");
    junk.extend_from_slice(&newc(&[(name, b"")]));
    assert!(!look(&junk));
    assert!(!look(&[0u8; 4096]));
}

#[test]
fn which_boots_confirm_themselves() {
    let rec = |reason| Some(bootinfo::Generation { number: 7, reason, skipped: 0, flags: 0 });
    assert_eq!(needs_confirmation(0, rec(gen_reason::NEWEST_TRY)), Ok(7));
    assert!(needs_confirmation(1, rec(gen_reason::NEWEST_TRY)).unwrap_err().contains("Reload"));
    for r in [gen_reason::NEWEST_OK, gen_reason::FALLBACK_AFTER_TRY, gen_reason::FALLBACK_BAD_FILES, gen_reason::PINNED, gen_reason::NEWEST_NO_RULE] {
        assert!(needs_confirmation(0, rec(r)).is_err(), "{r}");
    }
    assert!(needs_confirmation(0, None).is_err());
}

// ---------- the model stick ----------

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct Dir {
    files: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct State {
    gens: BTreeMap<u32, Dir>,
    pin: Option<u32>,
    config: Option<Vec<u8>>,
}

/// A stick in memory. `cut`: the stick stops answering after this many changes (a pulled stick, a power cut);
/// the change at the cut is half done (a file write leaves its first half, a removal its first file).
struct Model {
    st: RefCell<State>,
    changes: RefCell<usize>,
    cut: Option<usize>,
    flip_read: bool,
    cluster: u64,
    free: u64,
}

impl Model {
    fn new(s: State) -> Model {
        Model { st: RefCell::new(s), changes: RefCell::new(0), cut: None, flip_read: false, cluster: 4096, free: 1 << 30 }
    }
    /// Ok(true) = do it, Ok(false) = do half of it and fail, Err = the stick is gone.
    fn change(&self) -> Result<bool, i64> {
        let mut c = self.changes.borrow_mut();
        *c += 1;
        match self.cut {
            Some(k) if *c > k + 1 => Err(EIO),
            Some(k) if *c == k + 1 => Ok(false),
            _ => Ok(true),
        }
    }
    fn gone(&self) -> bool {
        self.cut.is_some_and(|k| *self.changes.borrow() > k)
    }
}

impl Stick for Model {
    fn generations(&self) -> Result<Vec<Gen>, i64> {
        if self.gone() {
            return Err(EIO);
        }
        Ok(self
            .st
            .borrow()
            .gens
            .iter()
            .map(|(&n, d)| {
                let has = |f: &str| d.files.contains_key(f);
                Gen {
                    n,
                    new: has("NEW"),
                    tried: has("TRY"),
                    ok: has("OK"),
                    kernel: has("KERNEL"),
                    initrd: has("INITRD"),
                    bytes: d.files.values().map(|v| v.len() as u64).sum(),
                }
            })
            .collect())
    }
    fn pin(&self) -> Result<Option<u32>, i64> {
        if self.gone() { Err(EIO) } else { Ok(self.st.borrow().pin) }
    }
    fn create_generation(&self, n: u32) -> Result<(), i64> {
        let whole = self.change()?;
        let mut s = self.st.borrow_mut();
        if s.gens.contains_key(&n) {
            return Err(EEXIST);
        }
        s.gens.insert(n, Dir::default());
        if whole { Ok(()) } else { Err(EIO) }
    }
    fn write_file(&self, n: u32, name: &str, data: &[u8]) -> Result<(), i64> {
        let whole = self.change()?;
        let mut s = self.st.borrow_mut();
        let d = s.gens.get_mut(&n).ok_or(ENOENT)?;
        if d.files.contains_key(name) {
            return Err(EEXIST);
        }
        if whole {
            d.files.insert(name.to_string(), data.to_vec());
            Ok(())
        } else {
            d.files.insert(name.to_string(), data[..data.len() / 2].to_vec());
            Err(EIO)
        }
    }
    fn read_at(&self, n: u32, name: &str, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        if self.gone() {
            return Err(EIO);
        }
        let s = self.st.borrow();
        let f = s.gens.get(&n).and_then(|d| d.files.get(name)).ok_or(ENOENT)?;
        let from = (off as usize).min(f.len());
        let k = (f.len() - from).min(buf.len());
        buf[..k].copy_from_slice(&f[from..from + k]);
        let flip = f.len() / 3;
        if self.flip_read && name == "KERNEL" && (from..from + k).contains(&flip) {
            buf[flip - from] ^= 0x40;
        }
        Ok(k)
    }
    fn set_marker(&self, n: u32, m: Marker, on: bool) -> Result<(), i64> {
        let _ = self.change()?;
        let mut s = self.st.borrow_mut();
        let d = s.gens.get_mut(&n).ok_or(ENOENT)?;
        let name = match m {
            Marker::New => "NEW",
            Marker::Try => "TRY",
            Marker::Ok => "OK",
        };
        if on {
            d.files.insert(name.to_string(), Vec::new());
        } else {
            d.files.remove(name);
        }
        Ok(())
    }
    fn set_pin(&self, n: Option<u32>) -> Result<(), i64> {
        let _ = self.change()?;
        self.st.borrow_mut().pin = n;
        Ok(())
    }
    fn remove_generation(&self, n: u32) -> Result<(), i64> {
        let whole = self.change()?;
        let mut s = self.st.borrow_mut();
        let d = s.gens.get_mut(&n).ok_or(ENOENT)?;
        if whole {
            s.gens.remove(&n);
            Ok(())
        } else {
            // Half: the first file (by name) unlinked.
            let first = d.files.keys().next().cloned();
            if let Some(f) = first {
                d.files.remove(&f);
            }
            Err(EIO)
        }
    }
    fn space(&self) -> Result<(u64, u64), i64> {
        if self.gone() { Err(EIO) } else { Ok((self.cluster, self.free)) }
    }
    fn config(&self) -> Result<Option<Vec<u8>>, i64> {
        if self.gone() { Err(EIO) } else { Ok(self.st.borrow().config.clone()) }
    }
    fn write_config(&self, data: &[u8]) -> Result<(), i64> {
        let whole = self.change()?;
        if whole {
            self.st.borrow_mut().config = Some(data.to_vec());
            Ok(())
        } else {
            Err(EIO) // the rename never happened: the old file stays
        }
    }
}

fn gen_dir(files: &[(&str, &[u8])]) -> Dir {
    Dir { files: files.iter().map(|(n, b)| (n.to_string(), b.to_vec())).collect() }
}

fn stick_with(gens: &[(u32, &str, u8)]) -> State {
    let mut s = State { config: Some(b"token=t\n".to_vec()), ..State::default() };
    for &(n, marks, tag) in gens {
        let (k, i) = (kernel(tag), initrd(tag));
        let mut d = gen_dir(&[("KERNEL", &k), ("INITRD", &i)]);
        for (c, m) in [('N', "NEW"), ('T', "TRY"), ('O', "OK")] {
            if marks.contains(c) {
                d.files.insert(m.to_string(), Vec::new());
            }
        }
        s.gens.insert(n, d);
    }
    s
}

/// bitsboot on a stick state: its marker rule and its file checks (a generation whose files fail is "bad" and
/// the rule is asked again, as `boot/bitsboot/src/load.rs` loops). Returns the started generation and its files.
fn boot(s: &State) -> Option<(u32, &'static str, Vec<u8>, Vec<u8>)> {
    use bitsboot_pure::gens::{Choice, GenDir, Pin};
    let dirs: Vec<GenDir> = s
        .gens
        .iter()
        .map(|(&n, d)| {
            let mut g = GenDir { number: n, ..GenDir::default() };
            for f in d.files.keys() {
                g.note(f, false);
            }
            g
        })
        .collect();
    let pin = s.pin.map_or(Pin::None, Pin::Number);
    let mut bad = Vec::new();
    loop {
        match bitsboot_pure::gens::choose(&dirs, pin, &bad, &[]) {
            Choice::Refuse(_) => return None,
            Choice::Start { record, .. } => {
                let d = &s.gens[&record.number];
                let (k, i) = (d.files["KERNEL"].clone(), d.files["INITRD"].clone());
                if bitsboot_pure::elf::parse(&k).is_err() || bitsboot_pure::gens::check_initrd(&i).is_err() {
                    bad.push(record.number);
                    continue;
                }
                return Some((record.number, bitsboot_pure::gens::reason_name(record.reason), k, i));
            }
        }
    }
}

#[test]
fn install_round_trip_on_the_model() {
    let m = Model::new(stick_with(&[(1, "O", 1), (2, "NT", 2)]));
    let (k, i) = (kernel(3), initrd(3));
    let r = install_on(&m, &k, &i, None, &Opts::default()).unwrap();
    assert_eq!((r.plan.number, r.plan.replaced, r.plan.fallback), (3, Some(2), Some(1)));
    assert!(r.next_boot.starts_with("generation 3 (NEWEST_TRY"), "{}", r.next_boot);
    let s = m.st.borrow().clone();
    assert_eq!(s.gens.keys().copied().collect::<Vec<_>>(), vec![1, 3]);
    assert_eq!(s.gens[&3], gen_dir(&[("KERNEL", &k), ("INITRD", &i), ("NEW", b"")]));
    // The next boot starts 3 (bitsboot writes TRY); a hang, then the boot after starts 1.
    let (n, why, bk, bi) = boot(&s).unwrap();
    assert_eq!((n, why, bk == k, bi == i), (3, "NEWEST_TRY", true, true));
    let mut tried = s.clone();
    tried.gens.get_mut(&3).unwrap().files.insert("TRY".into(), Vec::new());
    assert_eq!(boot(&tried).map(|b| (b.0, b.1)), Some((1, "FALLBACK_AFTER_TRY")));
    // Confirmation by the kernel of 3: OK, no TRY/NEW, and the boot after starts 3 for good.
    let m2 = Model::new(tried);
    assert_eq!(confirm_on(&m2, 3), Ok(Confirmed::Now { pruned: vec![] }));
    let s2 = m2.st.borrow().clone();
    assert_eq!(s2.gens[&3].files.keys().cloned().collect::<Vec<_>>(), vec!["INITRD", "KERNEL", "OK"]);
    assert_eq!(boot(&s2).map(|b| (b.0, b.1)), Some((3, "NEWEST_OK")));
    // Confirming again changes nothing; a replaced generation is "Missing".
    assert_eq!(confirm_on(&m2, 3), Ok(Confirmed::Already { pruned: vec![] }));
    assert_eq!(confirm_on(&m2, 9), Ok(Confirmed::Missing));
}

#[test]
fn install_refusals_change_nothing() {
    let base = stick_with(&[(1, "O", 1)]);
    let refused = |m: &Model, k: &[u8], i: &[u8], c: Option<&[u8]>, want: &str| {
        match install_on(m, k, i, c, &Opts::default()) {
            Err(InstallError::Refused(t)) => assert!(t.contains(want), "{t} (want {want})"),
            other => panic!("{other:?}"),
        }
        assert_eq!(*m.st.borrow(), base);
        assert_eq!(*m.changes.borrow(), 0);
    };
    let m = Model::new(base.clone());
    refused(&m, b"junk", &initrd(2), None, "KERNEL");
    refused(&m, &kernel(2), b"junk", None, "INITRD");
    refused(&m, &kernel(2), &initrd(2), Some(b"token=t\nusb_msd_write=on\n"), "usb_msd_write");
    refused(&m, &kernel(2), &initrd(2), Some(b"nvme_write=on\ntoken=t\n"), "nvme_write");
    refused(&m, &kernel(2), &initrd(2), Some(b"loglevel=info\n"), "token");
    let mut small = Model::new(base.clone());
    small.free = 8192;
    refused(&small, &kernel(2), &initrd(2), None, "KiB free");
    // No fallback: an unconfirmed generation alone.
    let lone = stick_with(&[(1, "NT", 1)]);
    let m = Model::new(lone.clone());
    match install_on(&m, &kernel(2), &initrd(2), None, &Opts::default()) {
        Err(InstallError::Refused(t)) => assert!(t.contains("allow_no_fallback"), "{t}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(*m.st.borrow(), lone);
    let r = install_on(&m, &kernel(2), &initrd(2), None, &Opts { allow_no_fallback: true, ..Opts::default() }).unwrap();
    assert_eq!((r.plan.number, r.plan.replaced, r.plan.fallback), (2, Some(1), None));
    // Dry run: everything checked and planned, nothing written.
    let m = Model::new(base.clone());
    let r = install_on(&m, &kernel(2), &initrd(2), None, &Opts { dry_run: true, ..Opts::default() }).unwrap();
    assert_eq!(r.plan.number, 2);
    assert_eq!(*m.st.borrow(), base);
}

#[test]
fn read_back_mismatch_removes_the_new_generation() {
    let base = stick_with(&[(1, "O", 1)]);
    let mut m = Model::new(base.clone());
    m.flip_read = true;
    match install_on(&m, &kernel(2), &initrd(2), None, &Opts::default()) {
        Err(InstallError::Failed(t)) => assert!(t.contains("read back differs") && t.contains("removed again"), "{t}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(*m.st.borrow(), base);
}

#[test]
fn config_unpin_and_prune_on_the_model() {
    let mut s = stick_with(&[(1, "O", 1), (2, "O", 2), (3, "OT", 3)]);
    s.pin = Some(1);
    let m = Model::new(s);
    let new_cfg = b"token=t\nsys_keep=1\n";
    let r = install_on(&m, &kernel(4), &initrd(4), Some(new_cfg), &Opts { unpin: true, ..Opts::default() }).unwrap();
    assert_eq!(r.sys_keep, Some(1));
    assert_eq!(r.plan.prune, vec![1, 2]);
    assert!(r.config_written);
    let s = m.st.borrow().clone();
    assert_eq!(s.pin, None);
    assert_eq!(s.config.as_deref(), Some(&new_cfg[..]));
    assert_eq!(s.gens.keys().copied().collect::<Vec<_>>(), vec![3, 4]);
    assert!(!s.gens[&3].files.contains_key("TRY"), "stale TRY tidied");
    // Without unpin the pin is kept and named.
    let mut s = stick_with(&[(1, "O", 1), (2, "O", 2)]);
    s.pin = Some(1);
    let m = Model::new(s);
    let r = install_on(&m, &kernel(3), &initrd(3), None, &Opts::default()).unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("PIN names generation 1")), "{:?}", r.warnings);
    assert!(r.next_boot.starts_with("generation 1 (PINNED"), "{}", r.next_boot);
    // An unparsable sys_keep prunes nothing (but still replaces).
    let mut s = stick_with(&[(1, "O", 1), (2, "O", 2), (3, "N", 3)]);
    s.config = Some(b"token=t\nsys_keep=many\n".to_vec());
    let m = Model::new(s);
    let r = install_on(&m, &kernel(4), &initrd(4), None, &Opts::default()).unwrap();
    assert_eq!((r.plan.replaced, r.plan.prune.len()), (Some(3), 0));
    assert!(r.warnings.iter().any(|w| w.contains("sys_keep=many")));
}

/// A cut after every single change of an install (half of the change at the cut done): whatever bitsboot then
/// starts is a whole, earlier-confirmed generation or the new generation with exactly the new files; a newest
/// confirmed generation always remains; and a second install afterwards reaches the planned state.
#[test]
fn a_cut_anywhere_leaves_a_bootable_stick() {
    let mut start = stick_with(&[(1, "O", 1), (2, "O", 2), (3, "OT", 3), (4, "NT", 4)]);
    start.config = Some(b"token=t\nsys_keep=2\n".to_vec());
    start.pin = Some(2);
    let (k, i) = (kernel(7), initrd(7));
    let cfg: &[u8] = b"token=t\nsys_keep=1\n";
    let o = Opts { unpin: true, ..Opts::default() };
    let changes = {
        let m = Model::new(start.clone());
        install_on(&m, &k, &i, Some(cfg), &o).unwrap();
        *m.changes.borrow()
    };
    assert!(changes > 8, "{changes}");
    let complete = |n: u32, s: &State| -> bool {
        let d = &s.gens[&n];
        let pair = |t: u8| d.files.get("KERNEL") == Some(&kernel(t)) && d.files.get("INITRD") == Some(&initrd(t));
        pair(7) || (1..=4).any(pair)
    };
    for cut in 0..changes {
        let mut m = Model::new(start.clone());
        m.cut = Some(cut);
        assert!(install_on(&m, &k, &i, Some(cfg), &o).is_err(), "cut {cut}: the install reported success");
        let s = m.st.borrow().clone();
        let b = boot(&s).unwrap_or_else(|| panic!("cut {cut}: nothing bootable: {s:?}"));
        assert!(complete(b.0, &s), "cut {cut}: started generation {} with partial files", b.0);
        if b.0 == 5 {
            assert_eq!((b.2 == k, b.3 == i), (true, true), "cut {cut}");
        } else {
            assert!(s.gens[&b.0].files.contains_key("OK"), "cut {cut}: started the unconfirmed generation {}", b.0);
        }
        assert!(s.gens.iter().any(|(&n, d)| d.files.contains_key("OK") && complete(n, &s)), "cut {cut}: no fallback left");
        // The stick answers again: a second install finishes the job.
        let m2 = Model::new(s);
        let r2 = install_on(&m2, &k, &i, Some(cfg), &o).unwrap_or_else(|e| panic!("cut {cut}: the rerun failed: {e:?}"));
        let s2 = m2.st.borrow().clone();
        let top = *s2.gens.keys().max().unwrap();
        assert_eq!(top, r2.plan.number);
        assert_eq!(s2.gens[&top], gen_dir(&[("KERNEL", &k), ("INITRD", &i), ("NEW", b"")]), "cut {cut}");
        assert_eq!(s2.gens.len(), 2, "cut {cut}: {:?}", s2.gens.keys());
        assert_eq!(s2.pin, None);
        assert_eq!(s2.config.as_deref(), Some(cfg));
        assert_eq!(boot(&s2).map(|b| b.0), Some(top));
    }
}

/// The same for the confirmation: a cut anywhere leaves the old markers or a confirmed generation.
#[test]
fn a_cut_in_the_confirmation_is_safe() {
    let start = stick_with(&[(1, "O", 1), (2, "NT", 2)]);
    for cut in 0..8 {
        let mut m = Model::new(start.clone());
        m.cut = Some(cut);
        let _ = confirm_on(&m, 2);
        let s = m.st.borrow().clone();
        let b = boot(&s).unwrap();
        if s.gens[&2].files.contains_key("OK") {
            assert_eq!(b.0, 2, "cut {cut}: OK written, so 2 is confirmed");
        } else {
            assert_eq!((b.0, b.1), (1, "FALLBACK_AFTER_TRY"), "cut {cut}");
        }
    }
}

#[test]
fn the_number_of_a_generation_is_never_reused() {
    // Two installs before the reboot: the second replaces the first under a new number, so a kernel waiting to
    // confirm "its" generation finds it gone instead of confirming different files.
    let m = Model::new(stick_with(&[(1, "O", 1)]));
    let a = install_on(&m, &kernel(2), &initrd(2), None, &Opts::default()).unwrap().plan.number;
    let b = install_on(&m, &kernel(3), &initrd(3), None, &Opts::default()).unwrap().plan.number;
    assert_eq!((a, b), (2, 3));
    assert_eq!(confirm_on(&m, a), Ok(Confirmed::Missing));
    let s = m.st.borrow().clone();
    assert_eq!(s.gens.keys().copied().collect::<Vec<_>>(), vec![1, 3]);
}

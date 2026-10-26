//! Host tests of RB-9: pin rules, the next start and the Reload target by bitsboot's rule, the listing text,
//! "which generation ran at", the console argument parser, the early-services check of `install_system`.

extern crate std;

use super::check;
use super::plan::Gen;
use super::system::{self, Listing, Running, SysGen, Want};
use super::system_tool::parse_pin_args;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use bootinfo::{Generation, gen_reason};
use std::format;

fn g(n: u32, marks: &str) -> Gen {
    Gen { n, new: marks.contains('N'), tried: marks.contains('T'), ok: marks.contains('O'), kernel: true, initrd: true, bytes: 1000 }
}

fn rec(number: u32, reason: u32) -> Generation {
    Generation { number, reason, skipped: 0, flags: 0 }
}

#[test]
fn pin_needs_a_confirmed_generation_with_both_files() {
    let gens = [g(1, "O"), g(2, "O"), g(3, "N")];
    let p = system::pin_plan(&gens, None, Want::Pin(1)).unwrap();
    assert_eq!((p.pin, p.write, p.next.0.number, p.next.0.reason), (Some(1), true, 1, gen_reason::PINNED));
    let e = system::pin_plan(&gens, None, Want::Pin(3)).unwrap_err();
    assert!(e.contains("not confirmed") && e.contains("NEW"), "{e}");
    let e = system::pin_plan(&gens, None, Want::Pin(7)).unwrap_err();
    assert!(e.contains("no generation 7") && e.contains("1 (OK), 2 (OK), 3 (NEW)"), "{e}");
    let mut broken = gens;
    broken[0].initrd = false;
    let e = system::pin_plan(&broken, None, Want::Pin(1)).unwrap_err();
    assert!(e.contains("INITRD"), "{e}");
    // The same pin again writes nothing.
    let p = system::pin_plan(&gens, Some(1), Want::Pin(1)).unwrap();
    assert!(!p.write);
    assert!(system::pin_text(&p, false).contains("pinned already"));
}

#[test]
fn off_goes_back_to_bitsboots_rule() {
    let gens = [g(1, "O"), g(2, "O")];
    let p = system::pin_plan(&gens, Some(1), Want::Off).unwrap();
    assert_eq!((p.pin, p.write, p.was), (None, true, Some(1)));
    assert_eq!((p.next.0.number, p.next.0.reason), (2, gen_reason::NEWEST_OK));
    assert!(system::pin_text(&p, false).starts_with("PIN removed (it named generation 1)"));
    // A PIN file without a number reads as None: off still removes it.
    let p = system::pin_plan(&gens, None, Want::Off).unwrap();
    assert!(p.write);
    assert!(system::pin_text(&p, true).starts_with("dry run, nothing written"));
    // Newest on trial (TRY): the next start is the fallback.
    let p = system::pin_plan(&[g(1, "O"), g(2, "T")], Some(1), Want::Off).unwrap();
    assert_eq!((p.next.0.number, p.next.0.reason, p.next.0.skipped), (1, gen_reason::FALLBACK_AFTER_TRY, 2));
}

#[test]
fn now_goes_where_the_next_start_would() {
    let gens = [g(1, "O"), g(2, "O")];
    let running = Some(2);
    let p = system::pin_plan(&gens, None, Want::Pin(1)).unwrap();
    assert_eq!(system::now_target(&p, running).unwrap(), Some(Generation { number: 1, reason: gen_reason::PINNED, skipped: 2, flags: 0 }));
    // Pinning the running generation needs no Reload.
    let p = system::pin_plan(&gens, None, Want::Pin(2)).unwrap();
    assert_eq!(system::now_target(&p, running).unwrap(), None);
    // Back from a pin: the newest OK.
    let p = system::pin_plan(&gens, Some(1), Want::Off).unwrap();
    let t = system::now_target(&p, Some(1)).unwrap().unwrap();
    assert_eq!((t.number, t.reason), (2, gen_reason::NEWEST_OK));
    // Off with a NEW newest: its one try needs a firmware start.
    let p = system::pin_plan(&[g(1, "O"), g(2, "N")], Some(1), Want::Off).unwrap();
    let e = system::now_target(&p, Some(1)).unwrap_err();
    assert!(e.contains("one try") && e.contains("reboot"), "{e}");
    // Not known which kernel runs (after a Reload by an older kernel): the Reload goes ahead.
    let p = system::pin_plan(&gens, None, Want::Pin(2)).unwrap();
    assert_eq!(system::now_target(&p, None).unwrap().map(|r| r.number), Some(2));
}

fn sg(n: u32, marks: &str, installed: i64) -> SysGen {
    SysGen { dir: g(n, marks), kernel_bytes: Some(3 << 20), initrd_bytes: Some(5 << 20), installed: Some(installed), confirmed: None, kernel_id: None }
}

#[test]
fn which_generation_ran_at_a_time() {
    let gens = [sg(1, "O", 1000), sg(2, "O", 2000), sg(3, "N", 3000)];
    assert_eq!(system::at_time(&gens, 999), None);
    assert_eq!(system::at_time(&gens, 1500), Some(1));
    assert_eq!(system::at_time(&gens, 2000), Some(2));
    assert_eq!(system::at_time(&gens, 9000), Some(2), "an unconfirmed generation is no pin target");
}

#[test]
fn listing_names_running_next_and_pin() {
    // 2026-10-23 13:04:05 UTC.
    let t0 = time::tz::days_from_civil(2026, 10, 23) * 86_400 + 13 * 3600 + 4 * 60 + 5;
    let mut gens = vec![sg(4, "O", t0), sg(5, "O", t0 + 3600), sg(6, "NT", t0 + 7200)];
    gens[1].kernel_id = Some(0x0123_4567_89ab_cdef);
    gens[1].confirmed = Some(t0 + 3720);
    let l = Listing {
        gens,
        pin: Some(4),
        writable: true,
        running: Some(Generation { skipped: 6, ..rec(5, gen_reason::FALLBACK_AFTER_TRY) }),
        reload: 2,
        running_kernel: Some(0x0123_4567_89ab_cdef),
    };
    let t = system::text(&l, &|_| 120);
    let line = |n: &str| t.lines().find(|x| x.trim_start().starts_with(n)).map(String::from).unwrap_or_default();
    assert!(line("4 ").contains("OK") && line("4 ").contains("installed 2026-10-23 15:04") && line("4 ").contains("NEXT START, PINNED"), "{t}");
    assert!(line("5 ").contains("confirmed 2026-10-23 16:06") && line("5 ").contains("0123456789abcdef") && line("5 ").contains("RUNNING"), "{t}");
    assert!(line("6 ").contains("NEW+TRY") && line("6 ").contains("(not read yet)"), "{t}");
    assert!(
        t.contains("running: generation 5 (fallback: the newest one was never confirmed; bitsboot: FALLBACK_AFTER_TRY, skipped 6, after Reload 2, kernel id 0123456789abcdef)"),
        "{t}"
    );
    assert!(t.contains("next start: generation 4 (PINNED"), "{t}");
    assert!(t.contains("pin: generation 4"), "{t}");
    // Newest first.
    let order: Vec<&str> = t.lines().skip(1).take(3).map(|x| x.trim_start().split(' ').next().unwrap_or("")).collect();
    assert_eq!(order, ["6", "5", "4"]);
    let empty = system::text(&Listing::default(), &|_| 0);
    assert!(empty.contains("none") && empty.contains("next start: nothing") && empty.contains("running: unknown"), "{empty}");
}

#[test]
fn console_arguments() {
    assert_eq!(parse_pin_args(&["3"]), Ok((Want::Pin(3), false)));
    assert_eq!(parse_pin_args(&["3", "now"]), Ok((Want::Pin(3), true)));
    assert_eq!(parse_pin_args(&["off", "now"]), Ok((Want::Off, true)));
    for bad in [&[][..], &["0"], &["-1"], &["x"], &["3", "later"], &["3", "now", "x"], &["100000000"]] {
        assert!(parse_pin_args(bad).is_err(), "{bad:?}");
    }
}

fn newc(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, mode, body) in entries.iter().copied().chain([("TRAILER!!!", 0, &b""[..])]) {
        let h = format!("070701{:08x}{mode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}", 1, 0, 0, 1, 0, body.len(), 0, 0, 0, 0, name.len() + 1, 0);
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
    }
    out
}

#[test]
fn install_system_needs_init_and_the_early_services() {
    let x = 0o100755;
    let whole = newc(&[("bin/init", x, b"init"), ("./bin/settingsd", x, b"settings"), ("/bin/bitssnap", x, b"snap"), ("etc/motd", 0o100644, b"hi")]);
    let s = check::services(&whole).unwrap();
    assert_eq!(s, vec![("bin/init", 4), ("bin/settingsd", 8), ("bin/bitssnap", 4)]);
    assert_eq!(check::initrd(&whole).unwrap().entries, 4);
    let e = check::services(&newc(&[("bin/init", x, b"init")])).unwrap_err();
    assert!(e.contains("/bin/settingsd (missing)") && e.contains("/bin/bitssnap (missing)"), "{e}");
    let e = check::services(&newc(&[("bin/init", 0o100644, b"init"), ("bin/settingsd", x, b"s"), ("bin/bitssnap", x, b"b")])).unwrap_err();
    assert!(e.contains("/bin/init (mode 100644"), "{e}");
    let e = check::services(&newc(&[("bin/init", 0o120777, b"busybox"), ("bin/settingsd", x, b"s"), ("bin/bitssnap", x, b"")])).unwrap_err();
    assert!(e.contains("/bin/init (mode 120777") && e.contains("/bin/bitssnap (mode 100755, 0 bytes"), "{e}");
    // A later entry of the same name wins (as when it is unpacked).
    let later = newc(&[("bin/init", 0o100644, b"old"), ("bin/settingsd", x, b"s"), ("bin/bitssnap", x, b"b"), ("bin/init", x, b"new!")]);
    assert_eq!(check::services(&later).unwrap()[0], ("bin/init", 4));
    // A cut archive fails the initrd rules first.
    assert!(check::services(&whole[..whole.len() - 8]).unwrap_err().contains("INITRD"));
}

fn listing(gens: Vec<SysGen>, record: Generation, reload: u32, kernel: Option<u64>) -> Listing {
    Listing { gens, pin: None, writable: true, running: Some(record), reload, running_kernel: kernel }
}

fn running_line(t: &str) -> String {
    t.lines().find(|l| l.starts_with("running: ")).map(String::from).unwrap_or_default()
}

/// RB-11 (desk-minipc-w13 FAIL 5): a TRY generation that confirmed itself in this boot is named as `reboot` and
/// `hw_info` name it, from the listing's own OK marker.
#[test]
fn a_try_generation_confirmed_in_this_boot_is_named_so() {
    let gens = vec![sg(20, "O", 1000), sg(21, "TO", 2000)];
    let t = system::text(&listing(gens, rec(21, gen_reason::NEWEST_TRY), 0, None), &|_| 0);
    assert_eq!(running_line(&t), "running: generation 21 (newest, confirmed (OK) in this boot; bitsboot: NEWEST_TRY)", "{t}");
    let gens = vec![sg(20, "O", 1000), sg(21, "T", 2000)];
    let t = system::text(&listing(gens, rec(21, gen_reason::NEWEST_TRY), 0, None), &|_| 0);
    assert_eq!(running_line(&t), "running: generation 21 (newest, on trial (TRY); bitsboot: NEWEST_TRY)", "{t}");
    // The words are mcp::boot's (`reason_name`, `running_reason_with`; RB-11 MC CR: one table).
    let words = [
        (gen_reason::NEWEST_NO_RULE, "newest (loader without the marker rule)"),
        (gen_reason::NEWEST_OK, "newest, confirmed (OK)"),
        (gen_reason::NEWEST_TRY, "newest, on trial (TRY)"),
        (gen_reason::FALLBACK_AFTER_TRY, "fallback: the newest one was never confirmed"),
        (gen_reason::FALLBACK_BAD_FILES, "fallback: the newest one's files were broken"),
        (gen_reason::PINNED, "pinned (SYS\\PIN)"),
        (99, "unknown reason"),
    ];
    for (r, w) in words {
        assert_eq!(mcp::running_reason_with(&rec(1, r), false), w);
        assert_eq!(mcp::reason_name(r), w);
        assert_eq!(mcp::running_reason_with(&rec(1, r), true), if r == gen_reason::NEWEST_TRY { "newest, confirmed (OK) in this boot" } else { w });
    }
}

/// RB-11 (desk-minipc-w13 follow-up 4): after a URL Reload from pinned generation 20 into generation 21's kernel
/// the record still says 20; the handed-on kernel id names 21.
#[test]
fn the_running_kernel_is_named_by_its_kernel_id() {
    let (k20, k21) = (0x2020_2020_2020_2020u64, 0x2121_2121_2121_2121u64);
    let mut gens = vec![sg(20, "O", 1000), sg(21, "O", 2000)];
    gens[0].kernel_id = Some(k20);
    gens[1].kernel_id = Some(k21);
    let t = system::text(&listing(gens.clone(), rec(20, gen_reason::PINNED), 1, Some(k21)), &|_| 0);
    let line = |n: &str| t.lines().find(|x| x.trim_start().starts_with(n)).map(String::from).unwrap_or_default();
    assert!(line("21 ").contains("RUNNING") && !line("20 ").contains("RUNNING"), "{t}");
    assert_eq!(
        running_line(&t),
        "running: generation 21's kernel (from a Reload: kernel id 2121212121212121 = generation 21's KERNEL, after Reload 1); the GENERATION record says generation 20 (pinned (SYS\\PIN); bitsboot: PINNED)",
        "{t}"
    );
    // A kernel no generation has: no RUNNING tag at all.
    let t = system::text(&listing(gens.clone(), rec(20, gen_reason::PINNED), 1, Some(0x77)), &|_| 0);
    assert!(!t.contains("RUNNING"), "{t}");
    assert!(running_line(&t).starts_with("running: a kernel from a URL (kernel id 0000000000000077, after Reload 1): no generation on the stick has this KERNEL;"), "{t}");
    // Ids not read yet: says so.
    let mut unread = gens.clone();
    unread[1].kernel_id = None;
    let t = system::text(&listing(unread, rec(20, gen_reason::PINNED), 1, Some(k21)), &|_| 0);
    assert!(!t.contains("RUNNING") && running_line(&t).starts_with("running: kernel id 2121212121212121 (after Reload 1): not all kernel ids are read yet"), "{t}");
    // After a Reload by an older kernel that handed nothing on: not the pin's generation.
    let t = system::text(&listing(gens.clone(), rec(20, gen_reason::PINNED), 1, None), &|_| 0);
    assert!(!t.contains("RUNNING"), "{t}");
    assert!(running_line(&t).starts_with("running: not known which kernel (after Reload 1 by a kernel that did not hand on its kernel id)"), "{t}");
    // A Reload into generation 20 itself (system_pin now) keeps the usual line.
    let t = system::text(&listing(gens, rec(20, gen_reason::PINNED), 2, Some(k20)), &|_| 0);
    assert_eq!(running_line(&t), "running: generation 20 (pinned (SYS\\PIN); bitsboot: PINNED, after Reload 2, kernel id 2020202020202020)", "{t}");
}

#[test]
fn identify_rules() {
    let r = Some(rec(3, gen_reason::NEWEST_OK));
    assert_eq!(system::identify(r, 0, None, &[]), Running::Gen(3), "a firmware start: bitsboot's record");
    assert_eq!(system::identify(None, 0, None, &[]), Running::NoRecord);
    assert_eq!(system::identify(r, 1, None, &[(3, Some(9))]), Running::Unknown);
    // The same KERNEL in several generations: the record's one, else the newest.
    let ids = [(1, Some(9)), (2, Some(9)), (3, Some(9)), (4, Some(5))];
    assert_eq!(system::identify(r, 1, Some(9), &ids), Running::Gen(3));
    assert_eq!(system::identify(Some(rec(4, gen_reason::NEWEST_OK)), 1, Some(9), &ids), Running::Gen(3));
    assert_eq!(system::identify(r, 1, Some(7), &ids), Running::Other { id: 7 });
    assert_eq!(system::identify(r, 1, Some(7), &[(1, None)]), Running::Pending { id: 7 });
    // Unread ids: only the record's own match is certain; an older match is no answer yet (checker finding 2).
    assert_eq!(system::identify(r, 1, Some(9), &[(1, Some(9)), (3, None)]), Running::Pending { id: 9 });
    assert_eq!(system::identify(r, 1, Some(9), &[(2, Some(9)), (5, None)]), Running::Pending { id: 9 }, "a newer unread one may match");
    assert_eq!(system::identify(r, 1, Some(9), &[(3, Some(9)), (5, None)]), Running::Gen(3));
}

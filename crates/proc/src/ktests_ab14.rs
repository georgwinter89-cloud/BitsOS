//! ktests of AB-14's vDSO (`ab.vdso.*`): the mappings every program gets, the page against the kernel's clock,
//! the user program `abi-vdso` (musl, user/abi-tests/c/vdsotest.h) and `dyn-vdso` (glibc) checking every vDSO
//! function against the syscall, the two compared for 10 s, a seqlock torture (the kernel republishing two
//! equal descriptions of the clock as fast as it can while 16 threads read: a torn read is off by 100 ms), the
//! fallback with the TSC marked unusable, a writer stuck mid-update (the readers take the syscall), a rate
//! change and a wall-clock jump reaching the page, and publishers racing a wall-clock set; INT-W19: the page is
//! the clock's own line and the time crate's hook publishes at once (`publish_hook`, `recheck_gate`).

use crate::ktests::{ended, need, start};
use crate::ktests_ab8::{checks, link_libs, run};
use crate::vdso::{self, Params, layout};
use crate::{ExitStatus, vdso::published};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use hal::entry::test_programs;
use ktest::{TestResult, kassert, ktest};

const MS: u64 = 1_000_000;

/// The monotonic ns the page says for TSC value `tsc` (the vDSO's arithmetic).
fn page_ns(p: &Params, tsc: u64) -> u64 {
    p.base_ns.saturating_add(time::clock::scale(tsc.saturating_sub(p.base_tsc), p.mult))
}

/// Waits (at most 3 s) for the boot's rate re-check after a Reload: until it is done the page sends programs to
/// the syscalls (MODE_NONE). Then publishes, so the page is the clock now.
fn settle() {
    let end = time::now_ns() + 3000 * MS;
    while time::recheck_pending() && time::now_ns() < end {
        sched::sleep_ns(50 * MS);
    }
    vdso::ensure();
    vdso::publish();
}

fn have_program() -> Result<(), TestResult> {
    if vfs::root_ns().lookup("/bin/abi-vdso").is_err() {
        return Err(ktest::skip("the user/abi-tests programs are not in this image (no musl sysroot when it was built)"));
    }
    Ok(())
}

ktest!("ab.vdso.mapping", Safe, mapping);
/// A started program (held before its first instruction) has [vvar] (one read-only page) right before [vdso]
/// (the image, read+exec) at AT_SYSINFO_EHDR; the image bytes are the embedded file, the vvar page is the
/// published one; both are the device's pages (not copies) and nothing is resident before the program runs.
fn mapping() -> TestResult {
    let img = crate::ktests::image(need(test_programs::stack_report())?, true);
    let (p, _out) = start("/ab-ktest/vdso-map", &img, &["vdso-map"], None, true)?;
    let r = (|| -> TestResult {
        let aux = p.saved_auxv.lock().clone();
        let at = aux.chunks(2).find(|c| c.first() == Some(&uapi::auxv::AT_SYSINFO_EHDR)).and_then(|c| c.get(1).copied()).unwrap_or(0);
        kassert!(at != 0 && at % 4096 == 0, "AT_SYSINFO_EHDR {at:#x}");
        let vmas = p.mm.vmas();
        let text = vmas.iter().find(|v| v.start == at).ok_or_else(|| format!("no VMA at {at:#x}: {vmas:?}"))?;
        kassert!(text.prot.read && text.prot.exec && !text.prot.write && text.end - text.start == vdso::image_pages() * 4096, "vdso VMA {text:?}");
        kassert!(text.file.as_ref().is_some_and(vdso::is_vdso) && text.file.as_ref().is_some_and(|f| f.pgoff == 1), "vdso VMA backing {text:?}");
        let vv = vmas.iter().find(|v| v.end == at).ok_or("no VMA right before the vDSO")?;
        kassert!(vv.prot.read && !vv.prot.write && !vv.prot.exec && vv.end - vv.start == 4096, "vvar VMA {vv:?}");
        kassert!(vv.file.as_ref().is_some_and(|f| vdso::is_vdso(f) && f.pgoff == 0), "vvar VMA backing {vv:?}");
        kassert!(vmas.iter().filter(|v| v.file.as_ref().is_some_and(vdso::is_vdso)).count() == 2, "more vDSO VMAs: {vmas:?}");
        let mut b = vec![0u8; vdso::image().len()];
        p.mm.copy_from_user(&mut b, at).map_err(|e| format!("read the vDSO: {e:?}"))?;
        kassert!(b == vdso::image(), "the mapped vDSO differs from the embedded image");
        let mut page = [0u8; 16];
        p.mm.copy_from_user(&mut page, at - 4096).map_err(|e| format!("read the vvar page: {e:?}"))?;
        let (seq, pp) = published().ok_or("nothing published")?;
        let mseq = u32::from_le_bytes([page[0], page[1], page[2], page[3]]);
        let mode = u32::from_le_bytes([page[4], page[5], page[6], page[7]]);
        kassert!(mseq % 2 == 0 && mseq >= seq && mode == pp.mode, "mapped vvar seq {mseq} mode {mode}, published {seq} {pp:?}");
        kassert!(p.mm.copy_to_user(at - 4096, &[1]).is_err(), "a write through the program's mapping of the vvar page went through");
        Ok(())
    })();
    p.kill(9);
    p.release();
    let _ = ended(&p, 30);
    r
}

ktest!("ab.vdso.page", Safe, page);
/// The published page against the kernel's clock for a second: now_ns() before <= the page's answer + 1 <=
/// now_ns() after (the page's base is a point of the clock's line, so it may read 1 ns low), and the wall
/// offset is the syscalls' one.
fn page() -> TestResult {
    settle();
    let (seq, p) = published().ok_or("no vDSO page")?;
    kassert!(seq % 2 == 0, "sequence {seq} odd at rest");
    kassert!(p.mult == time::clock::ns_mult(time::tsc_hz()) && p.real_off == vdso::realtime_offset_ns(), "page {p:?}");
    kassert!(p.mode == layout::MODE_TSC, "mode {} (the TSC should serve the vDSO in this boot: {:?})", p.mode, p);
    let end = time::now_ns() + 1000 * MS;
    let mut n = 0u64;
    while time::now_ns() < end {
        let k1 = time::now_ns();
        let v = page_ns(&p, hal::cpu::rdtsc());
        let k2 = time::now_ns();
        kassert!(k1 <= v + 1 && v <= k2, "kernel {k1} page {v} kernel {k2} (round {n})");
        n += 1;
    }
    kassert!(n > 0);
    Ok(())
}

ktest!("ab.vdso.program", Safe, program);
/// abi-vdso (musl static-PIE), dyn-vdso-static (glibc static-PIE) and dyn-vdso (glibc dynamic): every vDSO function, called directly and through the C
/// library, between two raw syscalls; a bad pointer faults (the fast path answered, as on Linux).
fn program() -> TestResult {
    if let Err(s) = have_program() {
        return s;
    }
    settle();
    checks("/bin/abi-vdso", &["abi-vdso"], "vdso clock_gettime bad pointer = SIGSEGV")?;
    if vfs::root_ns().lookup("/bin/dyn-vdso-static").is_ok() {
        checks("/bin/dyn-vdso-static", &["dyn-vdso-static"], "vdso clock_gettime bad pointer = SIGSEGV")?;
    }
    if link_libs().is_ok() && vfs::root_ns().lookup("/bin/dyn-vdso").is_ok() {
        checks("/bin/dyn-vdso", &["dyn-vdso"], "vdso clock_gettime bad pointer = SIGSEGV")?;
    }
    Ok(())
}

ktest!("ab.vdso.compare", Safe, compare);
/// The vDSO's answer and the syscall's stay in order (syscall <= vDSO <= syscall, MONOTONIC and REALTIME in
/// turn) for 10 s: never more apart than the two syscalls, so within any tick.
fn compare() -> TestResult {
    if let Err(s) = have_program() {
        return s;
    }
    settle();
    let (st, out, err) = run("/bin/abi-vdso", &["abi-vdso", "compare", "10"], b"")?;
    kassert!(st == ExitStatus::Exited(0), "{}: {out} {err}", st.describe());
    kassert!(out.lines().last().is_some_and(|l| l.ends_with(" 0 out of order")), "{out}");
    Ok(())
}

ktest!("ab.vdso.bench", Safe, bench);
/// Cost per call (logged as `ktest vdso.bench`, never judged: timing belongs to the Mini-PC check list): the
/// vDSO's clock_gettime, libc's (through the vDSO), the raw syscall (what libc paid before AB-14), getcpu.
fn bench() -> TestResult {
    if let Err(s) = have_program() {
        return s;
    }
    settle();
    let (st, out, err) = run("/bin/abi-vdso", &["abi-vdso", "bench"], b"")?;
    let line = out.lines().find(|l| l.starts_with("bench: ")).unwrap_or("");
    klog::event!(Info, "ktest", "vdso.bench", result = line);
    kassert!(st == ExitStatus::Exited(0) && !line.is_empty(), "{}: {out} {err}", st.describe());
    Ok(())
}

ktest!("ab.vdso.torture", QemuOnly, torture);
/// Seqlock torture (run it with `-- -smp 16`): a kernel thread republishes two descriptions of the same clock
/// line, 100 ms of TSC apart, back to back for 5 s while 16 threads of abi-vdso read MONOTONIC between two
/// syscalls. A reader mixing the two would be 100 ms off; none may be. QemuOnly: it keeps a CPU busy.
fn torture() -> TestResult {
    if let Err(s) = have_program() {
        return s;
    }
    settle();
    vdso::publish();
    let (_, a) = published().ok_or("no vDSO page")?;
    if a.mode != layout::MODE_TSC {
        return ktest::skip("the vDSO does not read the TSC in this boot");
    }
    let k = time::tsc_hz() / 10;
    sched::sleep_ns(150 * MS);
    kassert!(hal::cpu::rdtsc() > a.base_tsc + k, "the TSC did not move 100 ms in 150 ms of sleep");
    let b = Params { base_tsc: a.base_tsc + k, base_ns: page_ns(&a, a.base_tsc + k), ..a };
    let stop = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicU64::new(0));
    let (s2, w2) = (stop.clone(), writes.clone());
    let h = sched::spawn("vdso-torture", sched::Class::Normal, move || {
        while !s2.load(Ordering::Acquire) {
            for p in [&a, &b] {
                vdso::write(p);
                w2.fetch_add(1, Ordering::Relaxed);
            }
        }
    })
    .map_err(|e| format!("spawn the writer: {e:?}"))?;
    let r = run("/bin/abi-vdso", &["abi-vdso", "torture", "5", "16"], b"");
    stop.store(true, Ordering::Release);
    h.join();
    vdso::publish();
    let (st, out, err) = r?;
    let n = writes.load(Ordering::Relaxed);
    klog::event!(Info, "ktest", "vdso.torture", writes = n);
    kassert!(n > 1000, "only {n} writes");
    kassert!(st == ExitStatus::Exited(0) && out.lines().last().is_some_and(|l| l.ends_with(" 0 wrong readings")), "{}: {out} {err} ({n} writes)", st.describe());
    Ok(())
}

ktest!("ab.vdso.fallback", Safe, fallback);
/// The TSC marked unusable: the page says MODE_NONE and every vDSO call makes the syscall (correct answers,
/// EFAULT for a bad pointer instead of a fault); put back after.
fn fallback() -> TestResult {
    if let Err(s) = have_program() {
        return s;
    }
    settle();
    vdso::set_tsc_usable_for_test(Some(false));
    let mode = published().map(|(_, p)| p.mode);
    let r = run("/bin/abi-vdso", &["abi-vdso", "fallback"], b"");
    vdso::set_tsc_usable_for_test(None);
    kassert!(mode == Some(layout::MODE_NONE), "mode {mode:?} with the TSC marked unusable");
    kassert!(published().is_some_and(|(_, p)| p.mode == layout::MODE_TSC), "the TSC mode did not come back");
    let (st, out, err) = r?;
    kassert!(st == ExitStatus::Exited(0) && !out.lines().any(|l| l.ends_with(": no")), "{}: {out} {err}", st.describe());
    Ok(())
}

ktest!("ab.vdso.rate_change", QemuOnly, rate_change);
/// A rate change (the time crate's recheck correcting a handed-on rate) reaches the page within a refresh, and
/// the vDSO agrees with the syscall afterwards. QemuOnly: it runs the clock 100 ppm fast for a moment.
fn rate_change() -> TestResult {
    settle();
    let Some(c) = time::calibration() else { return ktest::skip("no calibration in this boot") };
    let hz = time::tsc_hz();
    time::set_rate_for_test(hz + hz / 10_000, c.source);
    vdso::refresh();
    let got = published().map(|(_, p)| p.mult);
    let r = if have_program().is_ok() { Some(run("/bin/abi-vdso", &["abi-vdso", "compare", "1"], b"")) } else { None };
    time::set_rate_for_test(hz, c.source);
    vdso::refresh();
    kassert!(got == Some(time::clock::ns_mult(hz + hz / 10_000)), "mult {got:?} after the rate change");
    kassert!(published().is_some_and(|(_, p)| p.mult == time::clock::ns_mult(hz)), "mult not back");
    if let Some(r) = r {
        let (st, out, err) = r?;
        kassert!(st == ExitStatus::Exited(0), "{}: {out} {err}", st.describe());
    }
    Ok(())
}

ktest!("ab.vdso.wall_jump", QemuOnly, wall_jump);
/// A wall-clock set (as a future clock_settime makes it) moves the page's REALTIME offset with it. QemuOnly:
/// it moves the wall clock by an hour for a moment.
fn wall_jump() -> TestResult {
    settle();
    let Some(now) = time::wall::realtime_ns() else { return ktest::skip("no wall clock in this boot") };
    let before = published().map(|(_, p)| p.real_off).unwrap_or(0);
    time::wall::set_realtime(now + 3600 * 1000 * MS);
    vdso::refresh();
    let after = published().map(|(_, p)| p.real_off).unwrap_or(0);
    let want = vdso::realtime_offset_ns();
    let r = if have_program().is_ok() { Some(run("/bin/abi-vdso", &["abi-vdso", "compare", "1"], b"")) } else { None };
    let back = time::wall::realtime_ns().unwrap_or(now).saturating_sub(3600 * 1000 * MS);
    time::wall::set_realtime(back);
    vdso::refresh();
    kassert!(after == want, "page offset {after}, clock offset {want}");
    let moved = after - before;
    kassert!((3600 * 1000 * MS as i64 - 1000 * MS as i64..3600 * 1000 * MS as i64 + 1000 * MS as i64).contains(&moved), "offset moved {moved} ns");
    if let Some(r) = r {
        let (st, out, err) = r?;
        kassert!(st == ExitStatus::Exited(0), "{}: {out} {err}", st.describe());
    }
    Ok(())
}

ktest!("ab.vdso.stuck", QemuOnly, stuck);
/// A writer stuck mid-update (the page's count held odd for the whole run of `abi-vdso fallback`): every vDSO
/// call ends in the syscall after its bounded wait (right answers, EFAULT for a bad pointer), none hangs; after
/// the hold the fast path answers again. QemuOnly: other publishers wait for the hold.
fn stuck() -> TestResult {
    if let Err(s) = have_program() {
        return s;
    }
    settle();
    let r = vdso::hold_odd_for_test(|| {
        let odd = published().map(|(seq, _)| seq % 2 == 1);
        (odd, run("/bin/abi-vdso", &["abi-vdso", "fallback"], b""))
    });
    let (odd, r) = r.ok_or("no vDSO page")?;
    kassert!(odd == Some(true), "the page's count was not odd during the hold: {odd:?}");
    kassert!(published().is_some_and(|(seq, p)| seq % 2 == 0 && p.mode == layout::MODE_TSC), "page after the hold: {:?}", published());
    let (st, out, err) = r?;
    kassert!(st == ExitStatus::Exited(0) && !out.lines().any(|l| l.ends_with(": no")), "{}: {out} {err}", st.describe());
    checks("/bin/abi-vdso", &["abi-vdso"], "vdso clock_gettime bad pointer = SIGSEGV")?;
    Ok(())
}

ktest!("ab.vdso.publish_race", QemuOnly, publish_race);
/// Publishers racing a wall-clock set: a publisher reads the clock and is held 20 ms before it writes (as if
/// preempted there); meanwhile the wall clock moves by an hour and `publish` runs. The page must carry the new
/// offset afterwards: the older reading never lands over the newer one. QemuOnly: it moves the wall clock.
fn publish_race() -> TestResult {
    let Some(_) = time::wall::realtime_ns() else { return ktest::skip("no wall clock in this boot") };
    settle();
    let h = sched::spawn("vdso-race", sched::Class::Normal, || vdso::publish_slow_for_test(20 * MS)).map_err(|e| format!("spawn the publisher: {e:?}"))?;
    sched::sleep_ns(5 * MS);
    const HOUR: u64 = 3600 * 1000 * MS;
    let now = time::wall::realtime_ns().unwrap_or(0);
    time::wall::set_realtime(now + HOUR);
    vdso::publish();
    h.join();
    let (got, want) = (published().map(|(_, p)| p.real_off), vdso::realtime_offset_ns());
    let back = time::wall::realtime_ns().unwrap_or(now + HOUR).saturating_sub(HOUR);
    time::wall::set_realtime(back);
    vdso::publish();
    kassert!(got == Some(want), "page offset {got:?}, clock offset {want} (the held publisher's old reading landed last)");
    Ok(())
}

ktest!("ab.vdso.publish_hook", QemuOnly, publish_hook);
/// INT-W19 (AB-14 CR 1): the page is the clock's own line (`time::snapshot`), and a rate change or a wall-clock
/// set reaches it through the time crate's hook at once, without `refresh`. QemuOnly: it runs the clock 100 ppm
/// fast and moves the wall clock by an hour for a moment.
fn publish_hook() -> TestResult {
    settle();
    let Some(c) = time::calibration() else { return ktest::skip("no calibration in this boot") };
    let s = time::snapshot();
    let p = published().map(|(_, p)| p);
    kassert!(p.is_some_and(|p| (p.base_tsc, p.base_ns, p.mult) == (s.base_tsc, s.base_ns, s.ns_mult)), "page {p:?}, clock {s:?}");
    let hz = time::tsc_hz();
    time::set_rate_for_test(hz + hz / 10_000, c.source);
    let fast = published().map(|(_, p)| p.mult);
    time::set_rate_for_test(hz, c.source);
    let back = published().map(|(_, p)| p.mult);
    kassert!(fast == Some(time::clock::ns_mult(hz + hz / 10_000)), "mult {fast:?} right after the rate change");
    kassert!(back == Some(time::clock::ns_mult(hz)), "mult {back:?} right after the rate went back");
    if let Some(now) = time::wall::realtime_ns() {
        time::wall::set_realtime(now + 3600 * 1000 * MS);
        let off = published().map(|(_, p)| p.real_off);
        let want = vdso::realtime_offset_ns();
        let back = time::wall::realtime_ns().unwrap_or(now).saturating_sub(3600 * 1000 * MS);
        time::wall::set_realtime(back);
        kassert!(off == Some(want), "page offset {off:?} right after the wall-clock set, clock offset {want}");
        kassert!(published().is_some_and(|(_, p)| p.real_off == vdso::realtime_offset_ns()), "offset not back");
    }
    Ok(())
}

ktest!("ab.vdso.recheck_gate", QemuOnly, recheck_gate);
/// INT-W19 (AB-14 CR 1): while the boot's rate re-check is pending the page says MODE_NONE (programs take the
/// syscall), and MODE_TSC is back as soon as it ends, both published by the time crate's hook. QemuOnly: it
/// marks the re-check pending for a moment.
fn recheck_gate() -> TestResult {
    settle();
    if published().map(|(_, p)| p.mode) != Some(layout::MODE_TSC) {
        return ktest::skip("the page does not use the TSC in this boot");
    }
    let Some(during) = time::hold_recheck_pending_for_test(|| published().map(|(_, p)| p.mode)) else {
        return ktest::skip("a real re-check is pending");
    };
    kassert!(during == Some(layout::MODE_NONE), "mode {during:?} while the re-check is pending");
    kassert!(published().is_some_and(|(_, p)| p.mode == layout::MODE_TSC), "the TSC mode did not come back after the re-check");
    Ok(())
}

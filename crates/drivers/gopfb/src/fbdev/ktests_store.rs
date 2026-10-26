//! In-kernel tests of DP-2's screen claims (`dp.fbdev.*`, fixer round): the stores a compositor or an SDL program
//! makes (`abi-fbdev store <seed> <how>`: after munmap, across mprotect, from a second thread, from a forked
//! child), a claim refused while a kernel owner holds the screen and retried once it is free, and KD_GRAPHICS
//! handing the screen over before the first store. All Safe and rerunnable: every program ends before the test
//! returns and the console has the screen again.

use super::ktests::{check_picture, console_back, drawn, handed_back, fb, need_program, start, text, wait_for, Drawn, Started, SEC};
use super::{fix_node_owner, stats, Fb};
use alloc::format;
use alloc::string::String;
use ktest::{kassert, ktest, TestResult};

/// Ends the program and every other live `abi-fbdev` (a forked child; the tests run one at a time).
fn kill(s: &Started) {
    s.p.kill(uapi::ab::signal::SIGKILL);
    let _ = s.p.wait(Some(10 * SEC));
    for p in proc::processes() {
        if !p.has_exited() && p.name.contains("abi-fbdev") {
            p.kill(uapi::ab::signal::SIGKILL);
            let _ = p.wait(Some(10 * SEC));
        }
    }
}

/// The pid in a "drawn ... pid=<n>" line (the process that drew: the child after a fork).
fn drew_pid(out: &str) -> Option<u32> {
    out.lines().find(|l| l.starts_with("drawn "))?.split_whitespace().find_map(|w| w.strip_prefix("pid="))?.parse().ok()
}

/// What `abi-fbdev store` printed: the drawing, or None when the case is not supported here (no threads or no
/// fork before AB-4).
fn wait_drawn(s: &Started, how: &str) -> Result<Option<(Drawn, u32)>, String> {
    let unsupported = format!("{how}: unsupported");
    kassert!(
        wait_for(30 * SEC, || {
            let o = text(&s.out);
            // After a fork the parent ends on purpose; the child prints.
            drawn(&o).is_some() || o.contains(&unsupported) || (s.p.has_exited() && how != "fork")
        }),
        "no 'drawn' line:\n{}{}",
        text(&s.out),
        text(&s.err)
    );
    let o = text(&s.out);
    if o.contains(&unsupported) {
        return Ok(None);
    }
    let d = drawn(&o).ok_or_else(|| format!("ended early: {:?}\n{o}{}", s.p.status(), text(&s.err)))?;
    let pid = drew_pid(&o).ok_or_else(|| format!("no pid in:\n{o}"))?;
    Ok(Some((d, pid)))
}

/// The screen shows `pid`'s picture (deferred io: within 1 s).
fn shows(fb: &Fb, d: &Drawn, pid: u32) -> TestResult {
    let mut last = String::new();
    let ok = wait_for(SEC, || {
        if fb.holder_pid() != pid || display::scanout_holder() != Some(("fbdev", pid)) {
            last = format!("holder {} / {:?}, want {pid}", fb.holder_pid(), display::scanout_holder());
            return false;
        }
        match display::snapshot() {
            Some((w, h, px)) => match check_picture(d, w, h, &px) {
                Ok(()) => true,
                Err(e) => {
                    last = e;
                    false
                }
            },
            None => {
                last = "no snapshot".into();
                false
            }
        }
    });
    kassert!(ok, "the screen does not show what pid {pid} drew within 1 s: {last}");
    Ok(())
}

/// Runs `abi-fbdev store <seed> <how>`, checks the screen shows it, ends every process it started.
fn store_case(seed: u32, how: &str) -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let seed_s = format!("{seed}");
    let s = start(&["abi-fbdev", "store", &seed_s, how])?;
    let r = (|| -> Result<Option<u32>, String> {
        let Some((d, pid)) = wait_drawn(&s, how)? else { return Ok(None) };
        shows(fb, &d, pid).map(|_| Some(pid))
    })();
    kill(&s);
    let drew = r?;
    handed_back(fb, "the kill")?;
    match drew {
        Some(pid) => {
            klog::event!(Info, "dp", "ktest.fbdev_store", case = how, pid = pid, program = s.p.pid);
            Ok(())
        }
        None => ktest::skip("the kernel has no threads/fork yet (AB-4): abi-fbdev says unsupported"),
    }
}

/// Stores, then munmap at once: the dirty pages still reach the screen.
fn unmap() -> TestResult {
    store_case(21, "unmap")
}
ktest!("dp.fbdev.unmap", Safe, unmap);

/// Stores, fsync, PROT_READ, PROT_READ|WRITE, stores again: the second stores are still tracked (device pages
/// stay read-only across mprotect until a store faults) and reach the screen without an fsync.
fn mprotect() -> TestResult {
    store_case(23, "mprotect")
}
ktest!("dp.fbdev.mprotect", Safe, mprotect);

/// A second thread's stores take the screen (`proc::current` names the process for any thread).
fn thread() -> TestResult {
    store_case(25, "thread")
}
ktest!("dp.fbdev.thread", Safe, thread);

/// The parent drew and ended; its forked child's stores take the screen again.
fn fork() -> TestResult {
    store_case(27, "fork")
}
ktest!("dp.fbdev.fork", Safe, fork);

/// A kernel owner holds the screen while a program stores (its claim is refused); once the owner lets go, the
/// program's next store claims again (the pages were write-protected anew) and the screen shows it.
fn rearm() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let g = display::take_scanout("dp.fbdev.rearm").map_err(|e| format!("{e:?}"))?;
    let before = stats();
    let s = match start(&["abi-fbdev", "store", "29", "loop"]) {
        Ok(s) => s,
        Err(e) => {
            drop(g);
            return Err(e);
        }
    };
    let r = (|| -> TestResult {
        let (d, pid) = wait_drawn(&s, "loop")?.ok_or("loop is always supported")?;
        // The program keeps storing while the kernel owner has the screen: refused claims, re-armed pages.
        kassert!(wait_for(2 * SEC, || stats().rearms > before.rearms), "no re-arm while the screen was held");
        kassert!(fb.holder_pid() == 0 && display::scanout_holder() == Some(("dp.fbdev.rearm", 0)), "holder {} / {:?}", fb.holder_pid(), display::scanout_holder());
        drop(g);
        shows(fb, &d, pid)
    })();
    kill(&s);
    r?;
    handed_back(fb, "the kill")?;
    klog::event!(Info, "dp", "ktest.fbdev_rearm", rearms = stats().rearms - before.rearms, takes = stats().takes - before.takes);
    Ok(())
}
ktest!("dp.fbdev.rearm", Safe, rearm);

/// KD_GRAPHICS (`display::request_scanout_for`): a program that opened fb0 but never stored gets the screen at
/// once; the console stops presenting; it comes back when the program ends. Ended pids are refused.
fn graphics() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let s = start(&["abi-fbdev", "idle"])?;
    let r = (|| -> TestResult {
        kassert!(wait_for(30 * SEC, || text(&s.out).contains("idle pid=") || s.p.has_exited()), "no 'idle' line: {}{}", text(&s.out), text(&s.err));
        kassert!(text(&s.out).contains("fd=1"), "fb0 did not open: {}", text(&s.out));
        kassert!(fb.holder_pid() == 0, "the program holds the screen before KD_GRAPHICS (holder {})", fb.holder_pid());
        let takes = stats().wakes_take;
        kassert!(display::request_scanout_for(s.p.pid), "KD_GRAPHICS refused (holder {:?})", display::scanout_holder());
        // DP-2f: the flake's cause; without this wake the thread finishes its 1 s idle wait before it watches.
        kassert!(stats().wakes_take > takes, "the KD_GRAPHICS take did not wake the fbdev thread");
        kassert!(fb.holder_pid() == s.p.pid && display::scanout_holder() == Some(("fbdev", s.p.pid)), "holder {} / {:?}", fb.holder_pid(), display::scanout_holder());
        kassert!(display::request_scanout_for(s.p.pid), "a second KD_GRAPHICS for the holder");
        // GX-4 CR 1: a mode switch's software side is refused while a program holds the screen; nothing changes.
        let g0 = fb.geometry();
        kassert!(super::can_resize(g0.width, g0.height).is_err(), "can_resize while held");
        kassert!(super::resize(g0.width, g0.height).is_err(), "resize while held");
        kassert!(fb.geometry() == g0, "geometry changed while held: {:?}", fb.geometry());
        let skips = display::stats().held_skips;
        display::with_frame(|f| f.mark_all());
        display::present();
        kassert!(display::stats().held_skips > skips, "the console presented while the program had the screen");
        Ok(())
    })();
    kill(&s);
    r?;
    handed_back(fb, "the kill")?;
    kassert!(!display::request_scanout_for(s.p.pid), "KD_GRAPHICS for an ended program was accepted");
    kassert!(console_back(fb, 0), "holder {} after the refused request", fb.holder_pid());
    Ok(())
}
ktest!("dp.fbdev.graphics", Safe, graphics);

//! DS-4 ktests: the live step list (fed by the real log since the console began) and the boot picture drawn
//! in the kernel into a private frame (never the screen), with its cost (numbers only; timing is judged on the
//! Mini-PC). Both are Safe and rerunnable.

use crate::bootdraw::{Scene, View};
use crate::bootsteps::{Key, State, Steps};
use ktest::{TestResult, kassert, ktest};

/// `/proc/bitsos/boot` answers with the steps the start has reached by now (ktests run before `kernel ready`).
fn live_steps() -> TestResult {
    let text = vfs::root_ns().read_file("/proc/bitsos/boot", 64 * 1024).map_err(|e| alloc::format!("read /proc/bitsos/boot: {e:?}"))?;
    let text = core::str::from_utf8(&text).map_err(|_| "not UTF-8")?;
    let s = Steps::from_proc(text).ok_or("does not parse")?;
    kassert!(text.starts_with("bitsos-boot 1\n"), "{text}");
    for k in [Key::Kernel, Key::Memory] {
        kassert!(s.get(k).state == State::Done, "{k:?} not done: {text}");
    }
    kassert!(s.get(Key::Cpus).state.is_final(), "cpus: {text}");
    kassert!(s.seq > 0 && crate::bootscreen::steps().seq >= s.seq, "seq {}", s.seq);
    Ok(())
}
ktest!("in.boot.steps", Safe, live_steps);

/// The whole picture and one changed row, in a private 2560x1440 frame: the same pixels as the host model at the
/// probe points, and what it costs here.
fn draw_private() -> TestResult {
    if !crate::bootart::ready() {
        return ktest::fail("built without the boot screen fonts");
    }
    let mut f = display::RamFrame::try_new(2560, 1440).ok_or("no memory for a 2560x1440 frame")?;
    let scene = Scene::new(2560, 1440).ok_or("scene does not fit 2560x1440")?;
    // A fresh list (the live one may have the desktop step settled already): the start, then one more step.
    let mut s = Steps::new();
    s.feed("[    0.010000] I kernel start version=0.1.0 generation=0");
    let v = View::of(&s);
    let t0 = time::now_ns();
    scene.draw_all(&mut f, &v);
    let t1 = time::now_ns();
    s.feed("[    9.000000] I console desktop.start program=/bin/bitscomp attempt=1");
    let v2 = View::of(&s);
    scene.draw_changes(&mut f, &v, &v2);
    let t2 = time::now_ns();
    let core = f.pixels()[crate::bootdraw::RING_CENTRE.1 as usize * 2560 + crate::bootdraw::RING_CENTRE.0 as usize];
    kassert!((core >> 8) & 0xFF > 0xB0, "ring core {core:06x}");
    let corner = f.pixels()[2 * 2560 + 2];
    kassert!((corner >> 8) & 0xFF <= 0x12, "corner {corner:06x}");
    klog::event!(Info, "console", "ktest.boot_draw", full_us = (t1 - t0) / 1000, row_us = (t2 - t1) / 1000);
    Ok(())
}
ktest!("in.boot.draw", Safe, draw_private);

/// The panic path (`vt::write_emergency`, what klog's emergency lines reach) while the boot screen is up: it ends
/// the boot screen without a lock or an allocation and paints the text console with the line over the picture.
/// The first line finds the VT busy (as when another CPU held it at the panic): the repaint is not lost, the next
/// line paints it. QemuOnly (it ends the boot screen and writes over the screen); without a real boot screen
/// (QEMU's default desktop=auto -> off) the state is pretended, so every QEMU run covers it.
fn emergency_shows_text() -> TestResult {
    let real = crate::bootscreen::active();
    if !real {
        crate::bootscreen::pretend_up();
    }
    const MARK: u32 = 0x0012_3456;
    let corner = |f: &display::RamFrame| f.width as usize - 1;
    display::with_frame(|f| {
        let i = corner(f);
        f.pixels_mut()[i] = MARK;
    })
    .ok_or("no frame")?;
    let line = "in.boot.emergency: the text console comes back on a panic";
    let fg = crate::screen::index_of(crate::FG_ERR);
    crate::vt::with_vt_held(|| crate::vt::write_emergency(line, fg)).ok_or("no VT")?;
    kassert!(!crate::bootscreen::active(), "the boot screen is still up");
    kassert!(crate::bootscreen::panic_repaint_pending(), "the repaint was dropped while the VT was busy");
    let held = display::with_frame(|f| f.pixels()[corner(f)]).ok_or("no frame")?;
    kassert!(held == MARK, "painted although the VT was busy: {held:06x}");
    crate::vt::write_emergency(line, fg);
    kassert!(!crate::bootscreen::panic_repaint_pending(), "the second line did not repaint");
    let after = display::with_frame(|f| f.pixels()[corner(f)]).ok_or("no frame")?;
    let bg = crate::screen::PALETTE[crate::screen::BG_INDEX as usize];
    kassert!(after == bg, "top-right pixel {after:06x}, want the console's {bg:06x} (real boot screen: {real})");
    kassert!(crate::bootscreen::steps().seq > 0, "step list empty");
    Ok(())
}
ktest!("in.boot.emergency", QemuOnly, emergency_shows_text);

/// Puts the level settings back on drop (also on an early return), but only when they are still the test's own:
/// a `settings_set kernel.loglevel` that came in between wins.
struct Restore {
    before: alloc::string::String,
    ours: alloc::string::String,
}
impl Drop for Restore {
    fn drop(&mut self) {
        if klog::levels() == self.ours {
            let _ = klog::set_levels(&self.before);
        }
    }
}

/// IN-9: below the level a step event (the test's own, which the list ignores) reaches the step list through
/// klog's watch, readable, and not the ring. Only the test's own subsystem goes to error for that moment
/// (`ktest=error` over the current settings): every other subsystem keeps its level; rerunnable. And on a start
/// whose level drops Info (`loglevel=error|warn`), the steps that only Info lines settle are settled anyway.
fn watch_below_level() -> TestResult {
    if !klog::metrics::snapshot().any(|(n, _)| n == "klog.watched") {
        return ktest::fail("klog has no watch yet (change request IN-9 CR-1)");
    }
    let (n0, bad0) = crate::bootscreen::watched_lines();
    let before = klog::levels();
    klog::set_levels(&alloc::format!("{before},ktest=error")).map_err(|e| alloc::format!("set_levels: {e}"))?;
    let guard = Restore { before, ours: klog::levels() };
    let seq0 = klog::next_seq();
    klog::event!(Info, "ktest", "in.boot.watch", nonce = time::now_ns());
    let (n1, bad1) = crate::bootscreen::watched_lines();
    // Read while `ktest` is still at error: the record was written (or not) inside `event!`.
    let page = klog::read(seq0, 4096, &klog::Filter::ALL);
    drop(guard);
    kassert!(n1 > n0, "the watch did not get the line (kernel without klog::set_watch? change request IN-9 CR-2)");
    kassert!(bad1 == bad0, "the watched line does not parse as a step line");
    kassert!(!page.records.iter().any(|r| r.name == "in.boot.watch"), "the dropped line is in the ring");
    let steps = crate::bootscreen::steps();
    for (sub, key) in [("kernel", Key::Kernel), ("mm", Key::Memory)] {
        if klog::level_for(sub) < klog::Level::Info {
            let st = steps.get(key);
            kassert!(st.state == State::Done, "{sub} logs no Info, and {key:?} is {:?} ({})", st.state, st.detail);
        }
    }
    Ok(())
}
ktest!("in.boot.watch", Safe, watch_below_level);

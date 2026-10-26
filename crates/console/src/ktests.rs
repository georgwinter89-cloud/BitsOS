//! ktests of the VT (IN-3). Input goes through the real sink and VT thread: injected events as MCP sends them, and
//! fake "physical" devices registered like a USB keyboard or mouse (QemuOnly where they type).

use crate::evdev::{self, Caps, DeviceInfo, EV_KEY, EV_REL, EV_SYN, InputEvent, SYN_REPORT};
use crate::keymap::code;
use crate::render;
use crate::{Console, monitor, vt};
use alloc::string::{String, ToString};
use ktest::{TestResult, kassert, ktest};

const INJECT_KEYBOARD: u32 = 0xFFFF_FF00;
const WAIT_NS: u64 = 5_000_000_000;

fn push(code: u16, value: i32) {
    let s = evdev::sink();
    let t = time::now_ns();
    s.push(InputEvent { time_ns: t, kind: EV_KEY, code, value, device: INJECT_KEYBOARD, injected: true });
    s.push(InputEvent { time_ns: t, kind: EV_SYN, code: SYN_REPORT, value: 0, device: INJECT_KEYBOARD, injected: true });
}
fn stroke(mods: &[u16], key: u16) {
    for &m in mods {
        push(m, 1);
    }
    push(key, 1);
    push(key, 0);
    for &m in mods.iter().rev() {
        push(m, 0);
    }
}
/// Waits until the VT took everything pushed so far (and a little longer for its drawing).
fn settle() -> Result<(), String> {
    let end = time::now_ns() + WAIT_NS;
    while vt::input_pending() > 0 {
        if time::now_ns() > end {
            return Err(String::from("the VT did not take the events within 5 s"));
        }
        sched::sleep_ns(5_000_000);
    }
    sched::sleep_ns(50_000_000);
    Ok(())
}

fn skip_without_display() -> Option<TestResult> {
    if display::device().is_none() {
        return Some(ktest::skip("no display device (no GOP framebuffer)"));
    }
    None
}

/// Text written to the console is in the VT's grid and its pixels are exactly the model's (`render::cell_pixel`).
fn grid_and_pixels() -> TestResult {
    if let Some(s) = skip_without_display() {
        return s;
    }
    let marker = alloc::format!("in.console.grid Grüße {{@}} [x] € {}", time::now_ns() % 1_000_003);
    crate::console().write_str(&alloc::format!("\n{marker}\n"), crate::FG);
    let (cols, rows, lines, _) = crate::text_grid().ok_or("no grid")?;
    kassert!(cols > 0 && rows > 0);
    let row = lines.iter().rposition(|l| l == &marker).ok_or_else(|| alloc::format!("marker not in the grid: {lines:?}"))? as u32;
    let font = vt::font().ok_or("no font")?;
    let (w, _h, px) = display::snapshot().ok_or("no frame")?;
    let p = vt::pointer().ok_or("no pointer")?;
    let scale = (font.h / 16).max(1);
    for (i, ch) in marker.chars().enumerate() {
        let cell = crate::screen::Cell { ch, attr: crate::screen::FG_INDEX };
        for y in 0..font.h {
            for x in 0..font.w {
                let (fx, fy) = (i as u32 * font.w + x, row * font.h + y);
                let under_pointer =
                    p.2 && fx >= p.0 && fy >= p.1 && fx < p.0 + render::ARROW_W * scale && fy < p.1 + render::ARROW_H * scale;
                if under_pointer {
                    continue;
                }
                let want = render::cell_pixel(&font, cell, false, x, y);
                let got = px[(fy * w + fx) as usize];
                kassert!(got == want, "pixel ({fx},{fy}) of {ch:?}: {got:#08x} != model {want:#08x}");
            }
        }
    }
    Ok(())
}
ktest!("in.console.grid", Safe, grid_and_pixels);

/// reboot and poweroff refuse injected lines, directly and typed through the VT and the monitor thread.
fn injected_reboot_refused() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let (ok, out) = monitor::execute("reboot", true, false);
    kassert!(!ok && out.contains("refused"), "reboot from injected input: {out}");
    let (ok, out) = monitor::execute("poweroff", true, false);
    kassert!(!ok && out.contains("refused"), "poweroff from injected input: {out}");
    // INT-A1: poweroff is CP-5's real S5 path now, so a physical line is not executed here (it would switch the
    // machine off); its registration must stay keyboard-only.
    kassert!(monitor::COMMANDS.entries().iter().any(|c| c.name == "poweroff" && c.physical_only), "poweroff is not a physical_only command");
    let (ok, out) = monitor::execute("echo a \"b c\"", true, false);
    kassert!(ok && out == "a b c\n", "echo: {out:?}");
    // The whole path: Ctrl+U (drop whatever is half typed), "reboot", Enter, all injected.
    let since = klog::next_seq();
    stroke(&[code::LEFTCTRL], 22);
    for k in [19u16, 18, 48, 24, 24, 20] {
        stroke(&[], k); // r e b o o t
    }
    stroke(&[], code::ENTER);
    let end = time::now_ns() + WAIT_NS;
    loop {
        let page = klog::read(since, 500, &klog::Filter { level: None, subsystem: Some("console"), name: Some("monitor.refused") });
        if page.records.iter().any(|r| r.text.contains("reboot")) {
            break;
        }
        kassert!(time::now_ns() < end, "no monitor.refused for an injected 'reboot' line within 5 s");
        sched::sleep_ns(20_000_000);
    }
    Ok(())
}
ktest!("in.console.injected_reboot_refused", Safe, injected_reboot_refused);

fn fake(name: &str, keys: &[u16], rel: &[u16]) -> alloc::sync::Arc<evdev::InputDevice> {
    let mut caps = Caps::default();
    caps.keys.extend(keys.iter().copied());
    caps.rel.extend(rel.iter().copied());
    evdev::register(evdev::sink(), DeviceInfo { name: name.to_string(), bus: evdev::BUS_VIRTUAL, caps, ..DeviceInfo::default() })
}

/// A held key repeats by the VT's timer (500 ms, then every 33 ms) and stops at its release. Functional only
/// (generous bounds; timing is judged on the Mini-PC). QemuOnly: the fake keyboard types into the console.
fn repeat() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let k = fake("ktest repeat kbd", &[45, 22, code::LEFTCTRL], &[]);
    let t = crate::tty();
    let clear = |k: &evdev::InputDevice| {
        k.emit(time::now_ns(), &[(EV_KEY, code::LEFTCTRL, 1), (EV_KEY, 22, 1), (EV_SYN, SYN_REPORT, 0)]);
        k.emit(time::now_ns(), &[(EV_KEY, 22, 0), (EV_KEY, code::LEFTCTRL, 0), (EV_SYN, SYN_REPORT, 0)]);
    };
    clear(&k);
    let r = (|| -> TestResult {
        settle()?;
        kassert!(t.editing().is_empty(), "line not empty after Ctrl+U: {:?}", t.editing());
        k.emit(time::now_ns(), &[(EV_KEY, 45, 1), (EV_SYN, SYN_REPORT, 0)]);
        sched::sleep_ns(200_000_000);
        let early = t.editing().chars().filter(|&c| c == 'x').count();
        sched::sleep_ns(900_000_000);
        k.emit(time::now_ns(), &[(EV_KEY, 45, 0), (EV_SYN, SYN_REPORT, 0)]);
        settle()?;
        let held = t.editing().chars().filter(|&c| c == 'x').count();
        sched::sleep_ns(300_000_000);
        let after = t.editing().chars().filter(|&c| c == 'x').count();
        kassert!(early == 1, "{early} x before the 500 ms delay (want 1)");
        kassert!(held >= 4, "only {held} x after 1.1 s held (want 1 + repeats)");
        kassert!(after == held, "repeat went on after the release: {held} -> {after}");
        Ok(())
    })();
    clear(&k);
    k.unregister(time::now_ns());
    r
}
ktest!("in.console.repeat", QemuOnly, repeat);

/// A newer key that types nothing takes the repeat over. Drives an own `vt::Input` with physical-looking events
/// and made-up times (no waiting). QemuOnly: it types into the real console's line.
fn repeat_takeover() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    if let Some(s) = skip_without_display() {
        return s;
    }
    const DEV: u32 = 0xFFFF_FE00;
    let ev = |code: u16, value: i32| InputEvent { time_ns: 0, kind: EV_KEY, code, value, device: DEV, injected: false };
    let t = crate::tty();
    let mut inp = vt::Input::new();
    let base = time::now_ns();
    let r = (|| -> TestResult {
        // x held, then F13 (types nothing) is the newest key: nothing repeats any more.
        inp.event(&ev(45, 1), base);
        kassert!(inp.repeat.key() == Some(45), "x does not repeat");
        inp.event(&ev(183, 1), base + 1_000_000);
        kassert!(inp.repeat.key().is_none(), "x still repeats after a newer key that types nothing");
        inp.event(&ev(183, 0), base + 2_000_000);
        inp.event(&ev(45, 0), base + 3_000_000);
        Ok(())
    })();
    t.flush();
    r
}
ktest!("in.console.repeat_takeover", QemuOnly, repeat_takeover);

/// A relative mouse moves the pointer, clamped to the screen; the arrow is drawn there.
fn pointer() -> TestResult {
    if let Some(s) = skip_without_display() {
        return s;
    }
    let m = fake("ktest pointer mouse", &[0x110], &[0, 1]);
    let r = (|| -> TestResult {
        m.emit(time::now_ns(), &[(EV_REL, 0, -100_000), (EV_REL, 1, -100_000), (EV_SYN, SYN_REPORT, 0)]);
        wait_pointer(0, 0)?;
        m.emit(time::now_ns(), &[(EV_REL, 0, 40), (EV_REL, 1, 30), (EV_SYN, SYN_REPORT, 0)]);
        wait_pointer(40, 30)?;
        let (w, h, px) = display::snapshot().ok_or("no frame")?;
        let p = vt::pointer().ok_or("no pointer")?;
        kassert!(p.2, "pointer not shown");
        let scale = vt::font().map(|f| (f.h / 16).max(1)).unwrap_or(1);
        let at = |x: u32, y: u32| px[((p.1 + y) * w + p.0 + x) as usize];
        kassert!(at(0, 0) == render::POINTER_OUTLINE, "tip is not the outline colour");
        kassert!(at(scale, 3 * scale) == render::POINTER_FILL, "inside is not the fill colour");
        m.emit(time::now_ns(), &[(EV_REL, 0, 100_000), (EV_REL, 1, 100_000), (EV_SYN, SYN_REPORT, 0)]);
        wait_pointer(w - 1, h - 1)?;
        Ok(())
    })();
    m.unregister(time::now_ns());
    r
}
ktest!("in.console.pointer", Safe, pointer);

fn wait_pointer(x: u32, y: u32) -> TestResult {
    let end = time::now_ns() + WAIT_NS;
    loop {
        let p = vt::pointer().ok_or("no pointer")?;
        if (p.0, p.1) == (x, y) {
            return Ok(());
        }
        kassert!(time::now_ns() < end, "pointer at ({}, {}), want ({x}, {y})", p.0, p.1);
        sched::sleep_ns(10_000_000);
    }
}

/// Waits until the console's grid has a line containing `needle` (the console is a deferred sink: klogd draws).
fn wait_grid(needle: &str) -> Result<alloc::vec::Vec<String>, String> {
    let end = time::now_ns() + WAIT_NS;
    loop {
        let (_, _, lines, _) = crate::text_grid().ok_or("no grid")?;
        if lines.iter().any(|l| l.contains(needle)) {
            return Ok(lines);
        }
        if time::now_ns() > end {
            return Err(alloc::format!("{needle:?} not on the console within 5 s: {lines:?}"));
        }
        sched::sleep_ns(10_000_000);
    }
}

/// IN-quiet: at the default level `warn` the console stays free of Info noise (it is in the ring all the same),
/// a warning still shows, and it goes above the line being typed, which stays whole at the bottom. At `info` the
/// same noise shows. Safe and rerunnable: the console level and klog's levels are restored, the typed word is
/// erased again (Ctrl+U, injected like MCP's `send_keys`).
fn quiet_console() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    if let Some(s) = skip_without_display() {
        return s;
    }
    let level = crate::log_level();
    let levels = klog::levels();
    // Whatever CONFIG `loglevel=` says, the test's own noise reaches the ring.
    klog::set_levels(&alloc::format!("{levels},console.quiet=info"))?;
    crate::set_log_level(crate::logfilter::WARN, "ktest");
    let r = (|| -> TestResult {
        let mark = time::now_ns() % 1_000_000_007;
        let since = klog::next_seq();
        stroke(&[code::LEFTCTRL], 22); // Ctrl+U: whatever was half typed goes
        for k in [16u16, 22, 23, 18, 20] {
            stroke(&[], k); // q u i e t
        }
        settle()?;
        for i in 0..5 {
            klog::event!(Info, "console.quiet", "ktest.noise", mark = mark, i = i; "Info noise the quiet console must not show");
        }
        klog::event!(Warn, "console.quiet", "ktest.warn", mark = mark; "expected: the console filter test shows a warning");
        let warn = alloc::format!("ktest.warn mark={mark}");
        let lines = wait_grid(&warn)?;
        let noise = alloc::format!("ktest.noise mark={mark}");
        kassert!(!lines.iter().any(|l| l.contains(&noise)), "Info noise on the quiet console: {lines:?}");
        let page = klog::read(since, 500, &klog::Filter { level: None, subsystem: Some("console.quiet"), name: Some("ktest.noise") });
        kassert!(page.records.iter().filter(|r| r.text.contains(&alloc::format!("mark={mark} "))).count() == 5, "the noise is not in the ring");
        let w = lines.iter().rposition(|l| l.contains(&warn)).unwrap_or(0);
        kassert!(lines.last().is_some_and(|l| l.ends_with("quiet")) && w + 1 < lines.len(), "the typed line is not whole below the warning: {lines:?}");

        // The same noise at `info` shows (the filter, not a dead sink, kept it away).
        crate::set_log_level(crate::logfilter::INFO, "ktest");
        klog::event!(Info, "console.quiet", "ktest.noise", mark = mark, i = 5; "Info noise, shown at info");
        let lines = wait_grid(&alloc::format!("ktest.noise mark={mark} i=5"))?;
        kassert!(lines.last().is_some_and(|l| l.ends_with("quiet")), "the typed line is not whole at the bottom: {lines:?}");
        Ok(())
    })();
    crate::set_log_level(level, "ktest");
    let restored = klog::set_levels(&levels);
    stroke(&[code::LEFTCTRL], 22);
    settle()?;
    r?;
    restored.map_err(|e| alloc::format!("klog levels not restored: {e}"))
}
ktest!("in.console.quiet", Safe, quiet_console);

/// The line discipline on its own instance: cooked editing, Ctrl+C, Ctrl+D, raw bytes.
fn tty_cooked_raw() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    use crate::keymap::Key;
    let t = crate::tty::Tty::new();
    for c in "lsx".chars() {
        t.key(Key::Char(c), false);
    }
    kassert!(t.key(Key::Backspace, false) == "\u{8} \u{8}");
    t.key(Key::Enter, false);
    let mut buf = [0u8; 2];
    let none = || false;
    kassert!(t.read(&mut buf, true, &none, false) == Ok(2) && &buf == b"ls");
    kassert!(t.read(&mut buf, true, &none, false) == Ok(1) && buf[0] == b'\n');
    kassert!(t.read(&mut buf, true, &none, false) == Err(uapi::errno::EAGAIN));
    let n = t.interrupts();
    kassert!(t.key(Key::Ctrl(3), false) == "^C" && t.interrupts() == n + 1);
    t.key(Key::Ctrl(4), true);
    let l = t.read_line(0).ok_or("no EOF line")?;
    kassert!(l.eof && l.injected);
    t.set_raw(true);
    t.key(Key::Up, false);
    let mut b3 = [0u8; 8];
    kassert!(t.read(&mut b3, true, &none, false) == Ok(3) && &b3[..3] == b"\x1b[A");
    Ok(())
}
ktest!("in.console.tty", Safe, tty_cooked_raw);

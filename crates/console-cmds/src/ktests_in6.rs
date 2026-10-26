//! IN-6 ktest with a real fullscreen program (here because the console cannot start processes): `abi-fbdev
//! draw 7 3 hold` takes the screen through fbdev (KD_GRAPHICS), draws and then hangs. The pointer still moves,
//! drawn by the kernel over the program's picture, which itself stays untouched.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use console::evdev::{self, Caps, DeviceInfo, EV_REL, EV_SYN, SYN_REPORT};
use console::latency::{self, Path};
use console::render::{ARROW_W, POINTER_OUTLINE};
use console::{pointer_over, vt};
use ktest::{TestResult, kassert, ktest};
use proc::SpawnOptions;
use proc::stdio::{Capture, Input};

const SEC: u64 = 1_000_000_000;
const PROGRAM: &str = "/bin/abi-fbdev";

fn wait_for(timeout_ns: u64, mut cond: impl FnMut() -> bool) -> bool {
    let end = time::now_ns() + timeout_ns;
    while !cond() {
        if time::now_ns() >= end {
            return false;
        }
        sched::sleep_ns(5_000_000);
    }
    true
}

fn at_pointer(x: u32, y: u32) -> bool {
    vt::pointer().is_some_and(|p| (p.0, p.1) == (x, y))
}

fn patch(r: display::Rect) -> Result<Vec<u32>, String> {
    pointer_over::last_patches().into_iter().find(|(x, _)| *x == r).map(|(_, p)| p).ok_or_else(|| format!("no patch for {r:?}"))
}

/// The frame over the program that shows the report stamped `t` (left after it; arrival `t` or earlier when
/// other input was pending in the same batch).
fn newest(t: u64) -> Option<latency::Sample> {
    latency::report().1.into_iter().rev().find(|s| s.path == Path::PointerOverProgram && s.start_ns >= t && s.arrival_ns <= t)
}

fn pointer_over_fbdev() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    if gopfb::fbdev::device().is_none() || display::device().is_none() {
        return ktest::skip("no fbdev device (no GOP framebuffer)");
    }
    if vfs::root_ns().lookup(PROGRAM).is_err() {
        return ktest::skip("/bin/abi-fbdev is not in this image (user/abi-tests need the musl sysroot)");
    }
    if vt::graphics() || display::scanout_holder().is_some() {
        return ktest::skip("a program has the screen");
    }
    gopfb::fbdev::fix_node_owner();
    let mut caps = Caps::default();
    caps.keys.insert(0x110);
    caps.rel.extend([0u16, 1]);
    let m = evdev::register(evdev::sink(), DeviceInfo { name: "ktest latency fbdev".to_string(), bus: evdev::BUS_VIRTUAL, caps, ..DeviceInfo::default() });
    m.emit(time::now_ns(), &[(EV_REL, 0, -100_000), (EV_REL, 1, -100_000), (EV_SYN, SYN_REPORT, 0)]);
    if !wait_for(5 * SEC, || at_pointer(0, 0)) {
        m.unregister(time::now_ns());
        return Err("the pointer did not go to (0, 0)".to_string());
    }
    let (out, err) = (Capture::new(1 << 16), Capture::new(1 << 16));
    let mut o = SpawnOptions::new(&["abi-fbdev", "draw", "7", "3", "hold"], &["HOME=/", "PATH=/bin"], policy::ProgramId(1));
    o.stdin = Some(Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(err.clone() as Arc<dyn vfs::File>);
    let p = match proc::spawn_with(PROGRAM, &o) {
        Ok(p) => p,
        Err(e) => {
            m.unregister(time::now_ns());
            return Err(format!("spawn {PROGRAM}: {e}"));
        }
    };
    let text = |c: &Capture| String::from_utf8_lossy(&c.contents()).into_owned();
    pointer_over::set_record(true);
    let r = (|| -> TestResult {
        kassert!(wait_for(120 * SEC, || text(&out).contains("drawn ") || p.has_exited()), "no 'drawn' line:\n{}", text(&out));
        kassert!(!p.has_exited(), "the program ended: {:?}\n{}{}", p.status(), text(&out), text(&err));
        kassert!(wait_for(5 * SEC, vt::graphics), "the program did not get KD_GRAPHICS");
        let (w, h, before) = display::snapshot().ok_or("no snapshot of the program's picture")?;
        let scale = vt::font().map(|f| (f.h / 16).max(1)).unwrap_or(1);
        let t = time::now_ns();
        m.emit(t, &[(EV_REL, 0, 40 * scale as i32), (EV_REL, 1, 30 * scale as i32), (EV_SYN, SYN_REPORT, 0)]);
        let (x, y) = (40 * scale, 30 * scale);
        kassert!(wait_for(5 * SEC, || at_pointer(x, y)), "the pointer did not move over the hanging program");
        let at = pointer_over::arrow_rect(x, y, scale, w, h);
        kassert!(wait_for(5 * SEC, || pointer_over::drawn_at() == Some(at)), "arrow at {:?}, want {at:?}", pointer_over::drawn_at());
        let pic = |r: display::Rect| pointer_over::copy_out_vec(&before, w, h, r).ok_or("picture patch");
        let (p1, under) = (patch(at)?, pic(at)?);
        kassert!(p1[0] == POINTER_OUTLINE, "no arrow tip over the program");
        let beside = (ARROW_W * scale - 1) as usize;
        kassert!(p1[beside] == under[beside], "beside the arrow is not the program's picture");
        kassert!(wait_for(5 * SEC, || newest(t).is_some()), "the move over the program was not measured");
        let s = newest(t).ok_or("sample")?;
        kassert!(s.arrival_ns <= t && s.arrival_ns <= s.picked_ns && s.picked_ns <= s.start_ns && s.start_ns <= s.done_ns, "sample {s:?}");

        let (x2, y2) = (x + 100 * scale, y);
        m.emit(time::now_ns(), &[(EV_REL, 0, 100 * scale as i32), (EV_SYN, SYN_REPORT, 0)]);
        let at2 = pointer_over::arrow_rect(x2, y2, scale, w, h);
        kassert!(wait_for(5 * SEC, || pointer_over::drawn_at() == Some(at2)), "arrow at {:?}, want {at2:?}", pointer_over::drawn_at());
        kassert!(patch(at)? == under, "the old spot did not get the program's picture back");
        let (_, _, after) = display::snapshot().ok_or("no second snapshot")?;
        kassert!(after == before, "the program's picture changed under the kernel's pointer");
        Ok(())
    })();
    pointer_over::set_record(false);
    p.kill(uapi::ab::signal::SIGKILL);
    let ended = p.wait(Some(10 * SEC)).is_some();
    let back = wait_for(5 * SEC, || !vt::graphics() && display::scanout_holder().is_none());
    m.unregister(time::now_ns());
    r?;
    kassert!(ended, "{PROGRAM} did not end after SIGKILL");
    kassert!(back, "the console did not get the screen back");
    kassert!(pointer_over::drawn_at().is_none(), "the arrow over the program is still remembered after the program ended");
    Ok(())
}
ktest!("in.latency.pointer_over_fbdev", Safe, pointer_over_fbdev);

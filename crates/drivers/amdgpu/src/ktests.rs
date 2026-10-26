//! GX-2/GX-3/GX-4 in-kernel tests. All Safe and rerunnable (CPU 0 over MCP on the Mini-PC): the GX-2 ones only read registers,
//! gx.flip.frames flips the unchanged console picture eight times and gives the screen back. QEMU has
//! no AMD display function: there they check the "no engine" answers.

use crate::attach;
use crate::dcn::{self, DcnRegs};
use crate::gpu::Gpu;
use crate::{kms, snapshot};
use alloc::format;
use alloc::string::String;
use ktest::kassert;
use mcp::{Json, ToolContext, ToolError, ToolOutput};

fn call(name: &str, args: &str) -> Result<String, ToolError> {
    let t = mcp::find(name).ok_or_else(|| ToolError::Failed(format!("tool {name} not registered")))?;
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
    let a = Json::parse(args).map_err(|e| ToolError::Failed(format!("{e:?}")))?;
    match (t.handler)(&ctx, &a)?.first() {
        Some(ToolOutput::Text(s)) => Ok(s.clone()),
        _ => Err(ToolError::Failed(String::from("no text answer"))),
    }
}

/// Both tools registered as GX's, safety Read.
fn tools_registered() -> ktest::TestResult {
    for n in ["dcn_snapshot", "dcn_writes"] {
        let t = mcp::find(n).ok_or("tool not registered")?;
        kassert!(t.owner == "GX" && t.safety == mcp::Safety::Read, "{n}: owner {} safety {:?}", t.owner, t.safety);
    }
    let w = call("dcn_writes", "{}").map_err(|e| format!("{e:?}"))?;
    kassert!(w.starts_with("writes "), "dcn_writes answer: {w}");
    Ok(())
}
ktest::ktest!("gx.tools", Safe, tools_registered);

/// The argument checks and the `compare` file read run before the engine is looked at, so QEMU covers them:
/// every bad argument is BadArgument or Failed (never a panic), a readable UTF-8 file reaches the engine check.
fn tools_args() -> ktest::TestResult {
    let engine = Gpu::get().is_some();
    let bad = |args: &str| -> ktest::TestResult {
        let e = call("dcn_snapshot", args);
        kassert!(matches!(e, Err(ToolError::BadArgument(_))), "{args}: {e:?}");
        Ok(())
    };
    bad(r#"{"compare":"/tmp/gx2-a","reference":"x"}"#)?;
    bad(r#"{"compare":5}"#)?;
    bad(r#"{"reference":[]}"#)?;
    bad(r#"{"wide":"yes"}"#)?;
    bad(r#"{"compare":"relative/path"}"#)?;
    let failed = |args: &str, part: &str| -> ktest::TestResult {
        let e = call("dcn_snapshot", args);
        kassert!(
            matches!(&e, Err(ToolError::Failed(s) | ToolError::BadArgument(s) | ToolError::Refused(s)) if s.contains(part)),
            "{args}: want an error naming {part:?}, got {e:?}"
        );
        kassert!(!matches!(&e, Err(ToolError::Failed(s)) if s.starts_with("no display engine")), "{args}: {e:?}");
        Ok(())
    };
    let write = |path: &str, field: &str, value: &str| {
        call("write_file", &format!("{{\"path\":\"{path}\",\"{field}\":{}}}", Json::str(value).to_string()))
            .map_err(|e| format!("write_file {path}: {e:?}"))
    };
    // A leftover of an interrupted run (the test is rerun on the Mini-PC); no delete of a missing path (it logs).
    if call("stat", r#"{"path":"/tmp/gx2-args"}"#).is_ok() {
        let _ = call("delete", r#"{"path":"/tmp/gx2-args","recursive":true}"#);
    }
    call("write_file", r#"{"path":"/tmp/gx2-args/ref.json","text":"{}","parents":true}"#)
        .map_err(|e| format!("mkdir: {e:?}"))?;
    let result = (|| -> ktest::TestResult {
        failed(r#"{"compare":"/tmp/gx2-args/missing.json"}"#, "missing.json")?;
        failed(r#"{"compare":"/tmp/gx2-args"}"#, "gx2-args")?;
        write("/tmp/gx2-args/bin", "data", "/w==")?;
        failed(r#"{"compare":"/tmp/gx2-args/bin"}"#, "not UTF-8")?;
        let big = "0x0EACC 0x0\n".repeat((1 << 20) / 12 + 1);
        write("/tmp/gx2-args/big", "text", &big)?;
        failed(r#"{"compare":"/tmp/gx2-args/big"}"#, "bytes")?;
        // A readable reference: QEMU stops at the engine check, the Mini-PC parses it (an empty object has no regs).
        let e = call("dcn_snapshot", r#"{"compare":"/tmp/gx2-args/ref.json"}"#);
        if engine {
            kassert!(matches!(&e, Err(ToolError::Failed(s)) if s.contains("no \"regs\"")), "{e:?}");
        } else {
            kassert!(matches!(&e, Err(ToolError::Failed(s)) if s.starts_with("no display engine")), "{e:?}");
        }
        Ok(())
    })();
    let _ = call("delete", r#"{"path":"/tmp/gx2-args","recursive":true}"#);
    result
}
ktest::ktest!("gx.tools.args", Safe, tools_args);

/// QEMU: no engine, the snapshot says so. Mini-PC: the device object exists, the gates give a verdict, and two
/// snapshots compare equal (the volatile bits masked) with not one register written in between by the tool.
fn snapshot_rerun() -> ktest::TestResult {
    let Some(g) = Gpu::get() else {
        kassert!(DcnRegs::attached().is_none(), "engine attached but no device object");
        let e = call("dcn_snapshot", "{}");
        kassert!(
            matches!(&e, Err(ToolError::Failed(s)) if s.starts_with("no display engine")),
            "snapshot without engine: {e:?}"
        );
        return Ok(());
    };
    // The tool never writes; a write by another user (DP audio) between two reads may change a register, so
    // equality is only demanded over a span in which the write count stayed the same.
    let (w0, _) = dcn::write_counts();
    let a = snapshot::take(g.regs, false);
    let b = snapshot::take(g.regs, false);
    let reference = snapshot::Reference { regs: a.regs.clone(), scope: snapshot::Scope::Snapshot { wide: false } };
    let r = snapshot::compare(&reference, &b.regs, false);
    if dcn::write_counts().0 == w0 {
        kassert!(r.equal(), "two snapshots differ:\n{}", snapshot::render(&r));
    }
    kassert!(a.regs.len() > 150, "only {} registers read", a.regs.len());
    kassert!(a.regs.get(&dcn::GATE_REG).is_none_or(|&v| v == dcn::GATE_VALUE), "codec id");
    let (w1, _) = dcn::write_counts();
    let text = call("dcn_snapshot", "{}").map_err(|e| format!("{e:?}"))?;
    let json = text.lines().last().ok_or("empty answer")?;
    let again = call("dcn_snapshot", &format!("{{\"reference\":{}}}", Json::str(json).to_string()))
        .map_err(|e| format!("{e:?}"))?;
    if dcn::write_counts().0 == w1 {
        kassert!(again.starts_with("equal"), "self-compare: {again}");
    }
    Ok(())
}
ktest::ktest!("gx.snapshot.rerun", Safe, snapshot_rerun);

/// Mini-PC: the gates' verdict is stable across two evaluations, and an attached display has a running CRTC,
/// a visible plane on it, and the GOP framebuffer reserved in the BO manager.
fn attach_stable() -> ktest::TestResult {
    let Some(g) = Gpu::get() else {
        return ktest::skip("no AMD display function (QEMU)");
    };
    let (a, b) = (g.check_display(), g.check_display());
    kassert!(a.as_ref().map_err(|e| e.why) == b.as_ref().map_err(|e| e.why), "verdict changed: {a:?} / {b:?}");
    let Ok(at) = a else { return Ok(()) };
    let t = kms::read(g.regs, false);
    for p in &at.pipes {
        kassert!(
            t.crtcs.iter().any(|c| c.id == p.otg && c.enabled),
            "pipe {} on OTG{} but no running crtc",
            p.hubp,
            p.otg
        );
        kassert!(
            t.planes.iter().any(|q| q.id == p.hubp && q.visible && q.crtc == p.otg),
            "no visible plane for pipe {}",
            p.hubp
        );
    }
    let bos = g.bos.as_ref().ok_or("attached display without a BO manager")?;
    let m = bos.lock();
    kassert!(
        m.reserved().iter().any(|r| r.name == "gop" && r.range.start == at.a0_offset),
        "GOP framebuffer not reserved"
    );
    Ok(())
}
ktest::ktest!("gx.attach.stable", Safe, attach_stable);

/// GX-3: the flip engine is installed into gopfb's fbdev, `dcn_flip` is GX's (safety Change) and checks its
/// arguments before anything else; without an AMD display (QEMU) nothing flips and fbdev copies.
fn flip_installed() -> ktest::TestResult {
    kassert!(gopfb::fbdev::flip::get().is_some(), "amdgpu's flip engine is not installed in fbdev");
    let t = mcp::find("dcn_flip").ok_or("dcn_flip not registered")?;
    kassert!(t.owner == "GX" && t.safety == mcp::Safety::Change, "dcn_flip: owner {} safety {:?}", t.owner, t.safety);
    for bad in [r#"{"test":"warp"}"#, r#"{"test":3}"#, r#"{"flips":-1}"#, r#"{"copy":"yes"}"#] {
        let e = call("dcn_flip", bad);
        kassert!(matches!(e, Err(ToolError::BadArgument(_))), "{bad}: {e:?}");
    }
    let s = call("dcn_flip", "{}").map_err(|e| format!("{e:?}"))?;
    kassert!(s.starts_with("flip: "), "state answer: {s}");
    if DcnRegs::attached().is_none() {
        kassert!(crate::live::get().is_none(), "a scanout without a display engine");
        kassert!(gopfb::fbdev::flip::active().is_none(), "fbdev would flip without an engine");
        kassert!(s.starts_with("flip: off"), "state answer: {s}");
        let e = call("dcn_flip", r#"{"test":"pace","flips":1}"#);
        kassert!(matches!(e, Err(ToolError::Failed(_))), "pace without an engine: {e:?}");
    }
    Ok(())
}
ktest::ktest!("gx.flip.installed", Safe, flip_installed);

/// GX-3 on the Mini-PC: eight flips of the console picture (it looks unchanged), each lands within its 50 ms,
/// the OTG lock is never left set, no write reads back different; the console gets the screen back.
fn flip_frames() -> ktest::TestResult {
    if DcnRegs::attached().is_none() {
        return ktest::skip("no AMD display function (QEMU)");
    }
    let s = crate::live::attach().ok_or("display attached but not taken over for flips (see dcn flip.off)")?;
    if !s.active() && !s.rearm() {
        return Err(String::from("the surface fell back and A0 is not settled").into());
    }
    let Some((w, h, base)) = display::snapshot() else {
        return ktest::skip("no console picture");
    };
    let Ok(guard) = display::take_scanout("gx.ktest.flip") else {
        return ktest::skip("the screen is held by a program");
    };
    let (_, bad0) = dcn::write_counts();
    let before = s.state();
    let mut res = Ok(());
    for i in 0..8 {
        let r = s.begin(&base, w).map_err(|e| format!("begin {i}: {e:?}")).and_then(|b| {
            let _ = devmodel::display::DisplayDevice::present(b, &base, w, (0, 0, w, h));
            s.commit().and_then(|q| s.wait(q)).map_err(|e| format!("flip {i}: {e:?}"))
        });
        if r.is_err() {
            res = r;
            break;
        }
    }
    drop(guard);
    res?;
    let after = s.state();
    kassert!(after.ordered == before.ordered + 8 && after.landed == after.ordered, "{before:?} -> {after:?}");
    kassert!(after.late == before.late && after.flipping, "late flips: {after:?}");
    let lock = attach::Regs::read(s.regs().0, crate::layout::otg(s.otg(), crate::layout::OTG_LOCK)).unwrap_or(u32::MAX);
    kassert!(lock & crate::layout::LOCK == 0, "OTG lock left set: {lock:#x}");
    kassert!(dcn::write_counts().1 == bad0, "a flip write read back different (dcn_writes)");
    Ok(())
}
ktest::ktest!("gx.flip.frames", Safe, flip_frames);

/// GX-4: `display_mode` is GX's (safety Change), checks its arguments and the request text before anything else;
/// the console command `mode` is registered. QEMU: no engine, so every switch is refused with that reason and the
/// GOP mode stays (nothing written). Mini-PC: the state answer names the link's timing.
fn mode_tool() -> ktest::TestResult {
    let t = mcp::find("display_mode").ok_or("display_mode not registered")?;
    kassert!(t.owner == "GX" && t.safety == mcp::Safety::Change, "display_mode: owner {} safety {:?}", t.owner, t.safety);
    for bad in [r#"{"set":5}"#, r#"{"confirm":"yes"}"#, r#"{"set":"1920x1080","undo":true}"#] {
        let e = call("display_mode", bad);
        kassert!(matches!(e, Err(ToolError::BadArgument(_))), "{bad}: {e:?}");
    }
    for bad in ["banana", "1920x1080@0", "99999x2", "1920x1080@60@1"] {
        let e = call("display_mode", &format!("{{\"set\":{}}}", Json::str(bad).to_string()));
        kassert!(matches!(&e, Err(ToolError::Failed(s)) if !s.contains("no display engine")), "{bad}: {e:?}");
    }
    kassert!(console::monitor::find("mode").is_some(), "console command mode not registered");
    let (w0, _) = dcn::write_counts();
    if DcnRegs::attached().is_none() {
        let e = call("display_mode", "{}");
        kassert!(matches!(&e, Err(ToolError::Failed(s)) if s.starts_with("no display engine")), "state: {e:?}");
        let e = call("display_mode", r#"{"set":"1920x1080@60"}"#);
        kassert!(matches!(&e, Err(ToolError::Failed(s)) if s.starts_with("no display engine")), "set: {e:?}");
        let (ok, out) = console::monitor::execute("mode 1024x768", false, false);
        kassert!(!ok && out.contains("no display engine"), "mode command: {ok} {out}");
        kassert!(dcn::write_counts().0 == w0, "a display register was written");
        let m = display::device().map(|d| d.current_mode());
        kassert!(display::snapshot().is_none_or(|(w, h, _)| m.is_none_or(|m| (m.width, m.height) == (w, h))), "GOP size changed");
        return Ok(());
    }
    let s = call("display_mode", "{}").map_err(|e| format!("{e:?}"))?;
    kassert!(s.starts_with("link ") && s.contains("modes ("), "state answer: {s}");
    Ok(())
}
ktest::ktest!("gx.mode.tool", Safe, mode_tool);

/// GX-4 on the Mini-PC (reads only): the gates of a switch pass on the pipe as it is now (the first switch's
/// scaler check only while no switch has written).
fn mode_gates() -> ktest::TestResult {
    if DcnRegs::attached().is_none() {
        return ktest::skip("no AMD display function (QEMU)");
    }
    let s = crate::live::attach().ok_or("display attached but not taken over (see dcn flip.off)")?;
    let boot = s.mode_boot();
    let home = boot.map_or_else(|| { let (w, h) = s.size(); crate::mode::Size::new(w, h) }, |(_, h)| h);
    let k = s.otg();
    let crtc = kms::read(s.regs(), false).crtcs.into_iter().find(|c| u32::from(c.id) == k).ok_or("no CRTC for the pipe's OTG")?;
    let link = crate::mode::Size::new(crtc.h_active, crtc.v_active);
    let first = boot.is_none();
    match crate::mode::gates(s.regs(), s.pipes(), link, home, first) {
        Ok(p) => {
            kassert!(p.known() || !first, "{p:?}");
            Ok(())
        }
        Err(g) => Err(format!("a switch would be refused: {g}").into()),
    }
}
ktest::ktest!("gx.mode.gates", Safe, mode_gates);

/// GX-4 on the Mini-PC: one switch to a listed size below the link's (`mode::trial`: the largest with the link's
/// aspect, else the largest) and straight back (`undo`): both land, the viewport reads the size each time, every
/// boot mode register (viewports, scaler, ratios, taps) is back, no write read back different.
/// Skipped while a program holds the screen or a switch waits for a confirmation (rerunnable).
fn mode_roundtrip() -> ktest::TestResult {
    if DcnRegs::attached().is_none() {
        return ktest::skip("no AMD display function (QEMU)");
    }
    let s = crate::live::attach().ok_or("display attached but not taken over (see dcn flip.off)")?;
    if display::scanout_holder().is_some() {
        return ktest::skip("the screen is held by a program");
    }
    let state = call("display_mode", "{}").map_err(|e| format!("{e:?}"))?;
    if state.contains("waiting for a confirmation") || state.contains("BLOCKED") {
        return ktest::skip("a switch waits for a confirmation or the switch is blocked");
    }
    let (w, h) = s.size();
    let home = s.mode_boot().map_or(crate::mode::Size::new(w, h), |(_, h)| h);
    if (w, h) != (home.w, home.h) {
        return ktest::skip("a confirmed switch shows another size than the boot one");
    }
    let (list, src) = crate::modeset::offered(home);
    let p = *s.pipes().first().ok_or("no pipe")?;
    let link = crate::mode::Size::of_dim(crate::mode::read(s.regs(), &p).map_err(|e| format!("{e:?}"))?.recout);
    let Some(to) = crate::mode::trial(&list, home, link) else {
        return ktest::skip("no smaller mode on offer");
    };
    klog::event!(Info, "dcn", "mode.trial", size = format!("{to}"), source = src.name(); "round trip through a listed mode");
    let before = crate::mode::read(s.regs(), &p).map_err(|e| format!("{e:?}"))?;
    let (_, bad0) = dcn::write_counts();
    let set = crate::modeset::set(&format!("{to}"));
    let shown = crate::mode::shown(s.regs(), &p).map_err(|e| format!("{e:?}"))?;
    let undo = crate::modeset::undo();
    set.map_err(|e| format!("set {to}: {e}"))?;
    kassert!(shown == to, "viewport {shown} after the switch to {to}");
    undo.map_err(|e| format!("undo: {e}"))?;
    let after = crate::mode::read(s.regs(), &p).map_err(|e| format!("{e:?}"))?;
    kassert!(after.same_checked(&before), "mode registers {before:?} -> {after:?}");
    kassert!(s.size() == (w, h), "scanout size {:?}", s.size());
    kassert!(dcn::write_counts().1 == bad0, "a mode write read back different (dcn_writes)");
    Ok(())
}
ktest::ktest!("gx.mode.roundtrip", Safe, mode_roundtrip);

struct NoUser;
impl gopfb::fbdev::abi::UserIo for NoUser {
    fn read(&self, _: u64, _: &mut [u8]) -> bool {
        false
    }
    fn write(&self, _: u64, _: &[u8]) -> bool {
        false
    }
}

/// fb0's FBIOGET_VSCREENINFO: xres, yres, xres_virtual, yres_virtual.
fn fb0_var() -> Result<[u32; 4], String> {
    let fb = gopfb::fbdev::device().ok_or("no fb0")?;
    let b = fb.ioctl(gopfb::fbdev::abi::FBIOGET_VSCREENINFO, 0, &NoUser, None).map_err(|e| format!("GET_VSCREENINFO: {e:?}"))?;
    let w = |i: usize| b.get(i * 4..i * 4 + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    Ok([w(0), w(1), w(2), w(3)])
}

/// GX-4 CR 1 in QEMU (no engine: the software side alone): the follower takes the screen to 3/4 of its size and
/// back; each time the console frame (MCP `screen`), the GopFb's mode, fb0's geometry and FBIOGET_VSCREENINFO
/// and the VT grid show the new size. QemuOnly: on the Mini-PC the software side must not move without the engine.
fn mode_follow() -> ktest::TestResult {
    let Some(f) = crate::modeset::follower() else {
        return ktest::skip("no follower installed (GX-4 change request 1 not applied)");
    };
    if DcnRegs::attached().is_some() {
        return ktest::skip("a display engine runs: the software side follows it only");
    }
    let Some((w0, h0, _)) = display::snapshot() else {
        return ktest::skip("no display or the screen is held");
    };
    if display::scanout_holder().is_some() {
        return ktest::skip("the screen is held by a program");
    }
    let (w1, h1) = ((w0 * 3 / 4) & !7, (h0 * 3 / 4) & !7);
    kassert!(w1 >= crate::mode::MIN.w && h1 >= crate::mode::MIN.h, "boot size {w0}x{h0} too small for the test");
    let check = |w: u32, h: u32| -> ktest::TestResult {
        let (sw, sh, _) = display::snapshot().ok_or("no picture")?;
        kassert!((sw, sh) == (w, h), "console frame {sw}x{sh}, want {w}x{h}");
        let m = display::device().ok_or("no device")?.current_mode();
        kassert!((m.width, m.height) == (w, h), "device mode {}x{}", m.width, m.height);
        if let Some(fb) = gopfb::fbdev::device() {
            let g = fb.geometry();
            kassert!((g.width, g.height, g.line_length()) == (w, h, w * 4), "fb0 geometry {g:?}");
            let v = fb0_var()?;
            kassert!(v[0] == w && v[1] == h && v[2] == w && (v[3] == h || v[3] == 2 * h), "fb0 var {v:?}");
        }
        let font = console::vt::font().ok_or("no VT font")?;
        let (cols, rows) = console::vt::size();
        kassert!((cols, rows) == (w / font.w, h / font.h), "VT grid {cols}x{rows} for {w}x{h} with {}x{} cells", font.w, font.h);
        // TIOCGWINSZ on the console tty answers the new grid (the VT's refit stores it; SIGWINCH goes out).
        let ws = console::ttyioctl::winsize(console::tty());
        kassert!((u32::from(ws.cols), u32::from(ws.rows)) == (cols, rows), "TIOCGWINSZ {}x{} for the grid {cols}x{rows}", ws.cols, ws.rows);
        Ok(())
    };
    (f.check)(w1, h1).map_err(|e| format!("check: {e}"))?;
    let r = (f.resize)(w1, h1).map_err(|e| format!("resize: {e}")).and_then(|()| check(w1, h1).map_err(|e| format!("{e:?}")));
    let back = (f.resize)(w0, h0);
    r?;
    back.map_err(|e| format!("back: {e}"))?;
    check(w0, h0)?;
    kassert!((f.resize)(w0 + 8, h0).is_err(), "a size beyond the framebuffer is refused");
    check(w0, h0)
}
ktest::ktest!("gx.mode.follow", QemuOnly, mode_follow);

/// GX-4 fixer: the confirmation timer (`switch::run_timer`) as a real `Normal` thread with real sleeps, over a
/// scripted switch (no engine needed, so QEMU covers it; Safe and rerunnable: no register is touched): it rolls
/// back once at or after the deadline, a confirmation ends it without a rollback, a later deadline (a second
/// `set`) moves it. Each run ends within 2 s.
mod timer {
    use crate::mode::Size;
    use crate::switch::{SwitchError, TimerHost, Wake, run_timer};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst};
    use ktest::kassert;

    #[derive(Default)]
    struct Scripted {
        deadline: AtomicU64,
        confirmed: AtomicBool,
        expired: AtomicU32,
        expired_at: AtomicU64,
    }

    impl TimerHost for Scripted {
        fn now_ns(&self) -> u64 {
            time::now_ns()
        }
        fn sleep_ns(&self, ns: u64) {
            sched::sleep_ns(ns);
        }
        fn wake(&self, now_ns: u64) -> Option<Wake> {
            let d = self.deadline.load(SeqCst);
            Some(if self.confirmed.load(SeqCst) || self.expired.load(SeqCst) > 0 {
                Wake::Done
            } else if now_ns >= d {
                Wake::Expire(1)
            } else {
                Wake::Sleep(d - now_ns)
            })
        }
        fn expire(&self, _serial: u64) -> Option<Result<Size, SwitchError>> {
            let now = time::now_ns();
            if now < self.deadline.load(SeqCst) || self.confirmed.load(SeqCst) {
                return None;
            }
            self.expired_at.store(now, SeqCst);
            self.expired.fetch_add(1, SeqCst);
            Some(Ok(Size::new(2560, 1440)))
        }
        fn tick(&self) -> bool {
            true
        }
        fn rolled_back(&self, _to: Size) {}
    }

    fn run(h: &Arc<Scripted>, after: impl FnOnce(&Scripted)) -> ktest::TestResult {
        let t = h.clone();
        let j = sched::spawn("gx-mode-ktest", sched::Class::Normal, move || run_timer(&*t)).map_err(|e| alloc::format!("spawn: {e:?}"))?;
        after(h);
        kassert!(j.join_timeout(2_000_000_000), "the timer thread did not end within 2 s");
        Ok(())
    }

    fn timer_thread() -> ktest::TestResult {
        let h = Arc::new(Scripted::default());
        h.deadline.store(time::now_ns() + 30_000_000, SeqCst);
        run(&h, |_| {})?;
        kassert!(h.expired.load(SeqCst) == 1, "rollbacks: {}", h.expired.load(SeqCst));
        kassert!(h.expired_at.load(SeqCst) >= h.deadline.load(SeqCst), "rolled back before the deadline");

        let h = Arc::new(Scripted::default());
        h.deadline.store(time::now_ns() + 300_000_000, SeqCst);
        run(&h, |h| {
            sched::sleep_ns(20_000_000);
            h.confirmed.store(true, SeqCst);
        })?;
        kassert!(h.expired.load(SeqCst) == 0, "a confirmed switch was rolled back");

        let h = Arc::new(Scripted::default());
        h.deadline.store(time::now_ns() + 40_000_000, SeqCst);
        run(&h, |h| {
            sched::sleep_ns(10_000_000);
            h.deadline.store(time::now_ns() + 80_000_000, SeqCst);
        })?;
        kassert!(h.expired.load(SeqCst) == 1, "rollbacks: {}", h.expired.load(SeqCst));
        kassert!(h.expired_at.load(SeqCst) >= h.deadline.load(SeqCst), "the moved deadline did not count");
        Ok(())
    }
    ktest::ktest!("gx.mode.timer", Safe, timer_thread);
}

/// GX-5a: the hardware cursor's refusals, nothing written: without an AMD display (QEMU) image, move and hide
/// answer NoDevice (fbdev's cursor ioctl answers ENODEV, the compositor keeps its software pointer); a bad size,
/// hotspot or pixel count is refused before a pixel or register is written; `dcn_cursor` is GX's (Change) and
/// checks its arguments first. Safe and rerunnable on the Mini-PC (it never shows or moves the cursor).
fn cursor_refusals() -> ktest::TestResult {
    use crate::cursor::{Alpha, CursorError, Shape};
    for (w, h, hx, hy) in [(0, 8, 0, 0), (8, 0, 0, 0), (257, 8, 0, 0), (8, 257, 0, 0), (8, 8, 8, 0), (8, 8, 0, 8)] {
        let r = Shape::new(w, h, hx, hy, Alpha::Premultiplied);
        kassert!(matches!(r, Err(CursorError::Invalid(_))), "{w}x{h} hotspot {hx},{hy}: {r:?}");
    }
    let sh = Shape::new(64, 64, 0, 0, Alpha::Premultiplied).map_err(|e| format!("{e:?}"))?;
    let short = crate::live::cursor_image(sh, &[0u32; 16], (0, 0));
    if crate::live::get().is_none() {
        kassert!(matches!(short, Err(CursorError::NoDevice(_))), "image without an engine: {short:?}");
        let m = crate::live::cursor_move((1, 1));
        kassert!(matches!(m, Err(CursorError::NoDevice(_))), "move without an engine: {m:?}");
        let h = crate::live::cursor_hide();
        kassert!(matches!(h, Err(CursorError::NoDevice(_))), "hide without an engine: {h:?}");
        let e = call("dcn_cursor", r#"{"op":"show"}"#);
        kassert!(matches!(e, Err(ToolError::Failed(_))), "dcn_cursor show without an engine: {e:?}");
    } else {
        kassert!(matches!(short, Err(CursorError::Invalid(_) | CursorError::NoDevice(_))), "16 pixels for 64x64: {short:?}");
    }
    let t = mcp::find("dcn_cursor").ok_or("dcn_cursor not registered")?;
    kassert!(t.owner == "GX" && t.safety == mcp::Safety::Change, "dcn_cursor: owner {} safety {:?}", t.owner, t.safety);
    for bad in [r#"{"op":"warp"}"#, r#"{"op":3}"#, r#"{"op":"show","size":65}"#, r#"{"op":"sweep","frames":0}"#, r#"{"op":"move","x":"a"}"#] {
        let e = call("dcn_cursor", bad);
        kassert!(matches!(e, Err(ToolError::BadArgument(_))), "{bad}: {e:?}");
    }
    Ok(())
}
ktest::ktest!("gx.cursor.refusals", Safe, cursor_refusals);

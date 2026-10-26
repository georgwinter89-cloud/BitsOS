//! In-kernel tests of MC-3: `screen` over the kernel loopback (PNG equals the RAM frame pixel for pixel, scale and
//! region sizes, size and time recorded; text mode finds a line just written), and the input tools (every event
//! injected, so none counts as a physical key; R8c).

use super::input::{self, INJECT_KEYBOARD, INJECT_POINTER};
use super::ktests::{Client, with_server};
use crate::{Json, TOOLS, b64, png};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use console::Console;
use console::evdev::{EvdevSink, InputEvent};
use ktest::{TestResult, kassert, ktest};

const SCREEN_PORT: u16 = 25005;
/// Generous bound for one shot under QEMU (timing is judged on the Mini-PC: 1440p < 100 ms).
const SHOT_MS_MAX: u64 = 5000;

fn rgb_equals_frame(rgb: &[u8], px: &[u32]) -> bool {
    rgb.len() == px.len() * 3 && px.iter().zip(rgb.as_chunks::<3>().0).all(|(p, c)| [(p >> 16) as u8, (p >> 8) as u8, *p as u8] == [c[0], c[1], c[2]])
}

fn image(r: &crate::tools::ktests::Reply, what: &str) -> Result<(u32, u32, Vec<u8>), String> {
    if r.error || r.rpc_error.is_some() || r.images.len() != 1 {
        return Err(format!("{what}: error {:?} isError {} images {} {}", r.rpc_error, r.error, r.images.len(), r.text));
    }
    let data = b64::decode(&r.images[0]).ok_or_else(|| format!("{what}: image is no base64"))?;
    png::decode_rgb(&data).map_err(|e| format!("{what}: {e}"))
}

/// The number after `key` in the answer's first line ("encode 12 ms").
fn field(text: &str, key: &str) -> Option<u64> {
    let at = text.find(key)? + key.len();
    text[at..].trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// One shot at a time against the frame before and after it (the console keeps drawing log lines meanwhile).
fn shot_matches(c: &mut Client, args: &str, cut: impl Fn(&[u32], u32) -> Vec<u32>) -> Result<String, String> {
    let mut last = String::new();
    for _ in 0..8 {
        let (fw, _, before) = display::snapshot().ok_or("no frame")?;
        let t = time::now_ns();
        let r = c.call("screen", args)?;
        let call_ms = (time::now_ns() - t) / 1_000_000;
        let (_, _, after) = display::snapshot().ok_or("no frame")?;
        let (w, h, rgb) = image(&r, args)?;
        let (b, a) = (cut(&before, fw), cut(&after, fw));
        kassert!(b.len() == (w * h) as usize, "{args}: PNG {w}x{h} but the region has {} pixels", b.len());
        if rgb_equals_frame(&rgb, &b) || rgb_equals_frame(&rgb, &a) {
            return Ok(format!("{} (call {call_ms} ms)", r.text));
        }
        last = r.text;
        sched::sleep_ns(20_000_000);
    }
    Err(format!("{args}: the PNG never matched the frame in 8 tries ({last})"))
}

fn screen_png() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display device (no GOP framebuffer)");
    }
    console::console().write_str("mcp.screen.png: this line is in the picture\n", console::FG);
    let (fw, fh) = display::with_frame(|f| (f.width, f.height)).ok_or("no frame")?;
    with_server(SCREEN_PORT, TOOLS.entries(), |c| {
        let full = shot_matches(c, "{}", |px, _| px.to_vec())?;
        let bytes = field(&full, "): ").unwrap_or(0);
        let encode_ms = field(&full, "encode ").ok_or_else(|| format!("no time in {full}"))?;
        let copy_ms = field(&full, "copy ").unwrap_or(0);
        klog::event!(Info, "mcp", "screen.ktest", width = fw, height = fh, bytes = bytes, copy_ms = copy_ms, encode_ms = encode_ms; "full-frame PNG equals the RAM frame");
        kassert!(full.starts_with(&format!("screen png {fw}x{fh} ")), "{full}");
        kassert!(bytes > 0 && (bytes as usize) < super::screen::PNG_LIMIT, "{full}");
        kassert!(copy_ms + encode_ms < SHOT_MS_MAX, "a {fw}x{fh} shot took {copy_ms} + {encode_ms} ms (generous QEMU bound {SHOT_MS_MAX} ms): {full}");

        // A region, compared pixel for pixel.
        let (rx, ry, rw, rh) = (fw / 4, fh / 3, fw / 2, 50.min(fh / 3));
        let args = format!(r#"{{"x":{rx},"y":{ry},"width":{rw},"height":{rh}}}"#);
        shot_matches(c, &args, |px, w| {
            let mut v = Vec::new();
            for y in ry..ry + rh {
                v.extend_from_slice(&px[(y * w + rx) as usize..(y * w + rx + rw) as usize]);
            }
            v
        })?;

        // Scaled: the size (the averaging itself is host-tested against the same frame arithmetic).
        let r = c.call("screen", r#"{"scale":3}"#)?;
        let (w, h, _) = image(&r, "scale 3")?;
        kassert!((w, h) == (fw.div_ceil(3), fh.div_ceil(3)), "scale 3 of {fw}x{fh} gave {w}x{h}");

        // Refusals: outside the frame, no image.
        let r = c.call("screen", &format!(r#"{{"x":{fw}}}"#))?;
        kassert!(r.rpc_error == Some(-32602) && r.images.is_empty(), "x outside: {:?} {}", r.rpc_error, r.text);
        Ok(())
    })
}
ktest!("mcp.screen.png", Safe, screen_png);

fn screen_text() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display device (no GOP framebuffer)");
    }
    let marker = format!("mcp.screen.text {{marker}} {}", time::now_ns() % 1_000_000_007);
    console::console().write_str(&marker, console::FG);
    console::console().write_str("\n", console::FG);
    with_server(SCREEN_PORT, TOOLS.entries(), |c| {
        let t = c.ok("screen", r#"{"format":"text"}"#)?;
        kassert!(t.starts_with("console ") && t.contains(&marker), "the line just written is not in the text grid:\n{t}");
        let r = c.call("screen", r#"{"format":"text","scale":2}"#)?;
        kassert!(r.rpc_error == Some(-32602), "scale with text: {:?}", r.rpc_error);
        Ok(())
    })
}
ktest!("mcp.screen.text", Safe, screen_text);

/// The worst picture end to end: the whole frame filled with noise, `screen` at scale 1 is refused readably (no
/// answer past 4 MB, no panic) within the generous bound, scale 4 is answered. QemuOnly: it paints over the real
/// screen (restored afterwards from a snapshot).
fn screen_too_big() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display device (no GOP framebuffer)");
    }
    let (fw, fh, saved) = display::snapshot().ok_or("no frame")?;
    let paint = |src: &dyn Fn(u32, u32) -> u32| {
        display::with_frame(|f| {
            if (f.width, f.height) == (fw, fh) {
                for y in 0..fh {
                    for x in 0..fw {
                        f.set(x, y, src(x, y));
                    }
                }
                f.mark(display::Rect { x: 0, y: 0, w: fw, h: fh });
            }
        });
        display::present();
    };
    paint(&|x, y| (x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA6B)).wrapping_mul(0xC2B2_AE35) >> 8);
    let r = with_server(SCREEN_PORT, TOOLS.entries(), |c| {
        let t = time::now_ns();
        let r = c.call("screen", "{}")?;
        let ms = (time::now_ns() - t) / 1_000_000;
        kassert!(r.error && r.images.is_empty() && r.text.starts_with("Refused: ") && r.text.contains("scale 2"), "scale 1 of noise: {:?} {}", r.rpc_error, r.text);
        kassert!(ms < SHOT_MS_MAX, "the refusal took {ms} ms (generous QEMU bound {SHOT_MS_MAX} ms)");
        let r = c.call("screen", r#"{"scale":4}"#)?;
        let (w, h, _) = image(&r, "scale 4 of noise")?;
        kassert!((w, h) == (fw.div_ceil(4), fh.div_ceil(4)), "scale 4 gave {w}x{h}");
        klog::event!(Info, "mcp", "screen.ktest_noise", refusal_ms = ms; "noise frame: scale 1 refused, scale 4 answered");
        Ok(())
    });
    paint(&|x, y| saved[(y * fw + x) as usize]);
    r
}
ktest!("mcp.screen.too_big", QemuOnly, screen_too_big);

/// R8c on a private sink (safe on hardware: nothing reaches the machine's real input): every event the input
/// tools build is injected.
fn injected_flag() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let sink = EvdevSink::new();
    let keys = Json::parse(r#"{"text":"{}","keys":["ctrl+alt+shift+d","altgr+q","ctrl+alt+f1"]}"#).map_err(|_| "json")?;
    let (kev, _) = input::send_keys_events(&keys, time::now_ns()).map_err(|e| format!("{e:?}"))?;
    let mouse = Json::parse(r#"{"x":5,"y":5,"button":"left","action":"double"}"#).map_err(|_| "json")?;
    let mev = input::send_mouse_events(&mouse, time::now_ns(), None).map_err(|e| format!("{e:?}"))?;
    kassert!(input::inject(&sink, &kev) == 0 && input::inject(&sink, &mev) == 0, "events dropped");
    let mut got: Vec<InputEvent> = Vec::new();
    while let Some(e) = sink.pop() {
        got.push(e);
    }
    kassert!(got.len() == kev.len() + mev.len(), "{} events arrived, {} sent", got.len(), kev.len() + mev.len());
    for e in &got {
        kassert!(e.injected, "event without the injected flag: {e:?}");
        kassert!(e.device == INJECT_KEYBOARD || e.device == INJECT_POINTER, "device {:#x}", e.device);
    }
    for a in [r#"{"text":"a","injected":false}"#, r#"{"keys":["enter"],"device":0}"#] {
        let j = Json::parse(a).map_err(|_| "json")?;
        kassert!(input::send_keys_events(&j, 0).is_err(), "{a} was accepted");
    }
    Ok(())
}
ktest!("mcp.input.injected_flag", Safe, injected_flag);

/// The whole path over MCP into the kernel's real input ring. QemuOnly: on a machine with a keyboard reader
/// the keys would reach the console or a program.
fn send_over_mcp() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let sink = console::evdev::sink();
    while sink.pop().is_some() {}
    let dropped_before = sink.dropped();
    let keys = r#"{"text":"{","keys":["ctrl+alt+shift+d"]}"#;
    let mouse = r#"{"dx":3,"dy":-4,"button":"left"}"#;
    let (want_k, _) = input::send_keys_events(&Json::parse(keys).map_err(|_| "json")?, 0).map_err(|e| format!("{e:?}"))?;
    let want_m = input::send_mouse_events(&Json::parse(mouse).map_err(|_| "json")?, 0, None).map_err(|e| format!("{e:?}"))?;
    with_server(SCREEN_PORT, TOOLS.entries(), |c| {
        let t = c.ok("send_keys", keys)?;
        kassert!(t.starts_with("sent key strokes: 2,") && t.contains("injected"), "{t}");
        let t = c.ok("send_mouse", mouse)?;
        kassert!(t.starts_with("sent pointer reports:"), "{t}");
        let r = c.call("send_keys", r#"{"text":"x","injected":false}"#)?;
        kassert!(r.rpc_error == Some(-32602), "injected:false was not refused: {:?} {}", r.rpc_error, r.text);
        Ok(())
    })?;
    let mut got: Vec<InputEvent> = Vec::new();
    while let Some(e) = sink.pop() {
        got.push(e);
    }
    if got.is_empty() && sink.dropped() == dropped_before {
        return ktest::skip("an input reader took the events before the test could look (IN-3)");
    }
    let strip = |v: &[InputEvent]| v.iter().map(|e| (e.device, e.kind, e.code, e.value, e.injected)).collect::<Vec<_>>();
    let want: Vec<InputEvent> = want_k.iter().chain(want_m.iter()).copied().collect();
    let mine: Vec<InputEvent> = got.iter().filter(|e| e.device == INJECT_KEYBOARD || e.device == INJECT_POINTER).copied().collect();
    kassert!(strip(&mine) == strip(&want), "events differ: got {:?}", strip(&mine));
    kassert!(mine.iter().all(|e| e.injected), "an event without the injected flag");
    Ok(())
}
ktest!("mcp.input.send_over_mcp", QemuOnly, send_over_mcp);

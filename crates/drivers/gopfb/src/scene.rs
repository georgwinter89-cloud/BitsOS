//! `dp.scene.screendump` (QemuOnly): the screen compared with the RAM frame from outside, by QEMU's own
//! screendump (host/tests/dp/10-screendump.sh with scene_check.py). Runs only when the scenario attaches a
//! second UART (COM2, 0x2F8) as a socket; otherwise it skips.
//!
//! Protocol on COM2 (ours alone; COM1 carries klog and may interleave): for each step the kernel writes
//! `DPSCENE step=<name> phys=<hex> width= height= stride= masks=<r>,<g>,<b>,<x> bytes=<n> hidden=<x>:<y>:<old>;...`
//! (`stride` in pixels of the step's layout, so the row pitch is `stride * bytes`)
//! and waits for one byte: `g` = the host has taken its screendump and saved the RAM frame (QMP `pmemsave`
//! at `phys`, which is the RAM frame's direct-map address, so physically contiguous), anything else = abort.
//! Hidden pixels changed in the RAM frame without being marked; the screen must still show `<old>`.
//! Steps: the console picture plus colour ramps at the boot layout (full present), a damage-only present,
//! then the same frame through an RGB layout and two 32-bit bit-mask layouts over the same scanout, then
//! through a 16-bit 565 and a 24-bit layout (the `write_bytes` path) onto a scanout cleared to black first:
//! QEMU still reads the scanout as 32-bit pixels, and the host models the packed bytes it then shows. Those
//! two steps need a scanout without line padding (QEMU's) and are left out otherwise.

use crate::{live, GopFb, Layout};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use display::{RamFrame, Rect};
use ktest::{ktest, TestResult};

const COM2: u16 = 0x2F8;
const ANSWER_NS: u64 = 120_000_000_000;

pub(crate) fn com2_present() -> bool {
    // The scratch register keeps what is written to a real 16550; a missing port reads 0xFF.
    [0xA5u8, 0x5A].iter().all(|&v| hal::portio::outb(COM2 + 7, v).is_ok() && hal::portio::inb(COM2 + 7) == Ok(v))
}

pub(crate) fn com2_write(s: &str) {
    for &b in s.as_bytes() {
        for _ in 0..100_000 {
            if hal::portio::inb(COM2 + 5).is_ok_and(|l| l & 0x20 != 0) {
                break;
            }
            core::hint::spin_loop();
        }
        let _ = hal::portio::outb(COM2, b);
    }
}

fn com2_wait(timeout_ns: u64) -> Option<u8> {
    let end = time::now_ns().saturating_add(timeout_ns);
    while time::now_ns() < end {
        if hal::portio::inb(COM2 + 5).is_ok_and(|l| l & 1 != 0) {
            return hal::portio::inb(COM2).ok();
        }
        sched::sleep_ns(2_000_000);
    }
    None
}

fn step(name: &str, f: &RamFrame, l: Layout, stride: u32, hidden: &[(u32, u32, u32)]) -> TestResult {
    step_px(name, f.pixels(), f.width, f.height, l, stride, hidden)
}

/// One step over any picture in RAM (DP-2: a copy of fbdev's frame as MCP `screen` gets it); `px` must be one
/// heap block (large allocations are page runs of the direct map, physically contiguous).
pub(crate) fn step_px(name: &str, px: &[u32], width: u32, height: u32, l: Layout, stride: u32, hidden: &[(u32, u32, u32)]) -> TestResult {
    let virt = px.as_ptr() as u64;
    let phys = hal::virt_to_phys(virt).ok_or_else(|| format!("RAM frame at {virt:#x} is not in the direct map"))?;
    let m = l.masks();
    let mut h = String::new();
    for (x, y, old) in hidden {
        h.push_str(&format!("{x}:{y}:{old:#x};"));
    }
    klog::event!(Info, "dp", "scene.step", step = name, layout = l.name());
    com2_write(&format!(
        "DPSCENE step={name} phys={phys:#x} width={width} height={height} stride={stride} masks={:#x},{:#x},{:#x},{:#x} bytes={} hidden={h}\n",
        m[0], m[1], m[2], m[3], l.bytes_per_pixel()
    ));
    match com2_wait(ANSWER_NS) {
        Some(b'g') => Ok(()),
        Some(c) => Err(format!("step {name}: the host answered {:?}", c as char)),
        None => Err(format!("step {name}: no answer on COM2 within {} s", ANSWER_NS / 1_000_000_000)),
    }
}

/// Horizontal ramps of red, green, blue and grey over the lower quarter (every channel value appears).
fn ramps(f: &mut RamFrame) {
    let (w, h) = (f.width, f.height);
    let band = (h / 16).max(1);
    let y0 = h.saturating_sub(4 * band);
    for (i, shift) in [Some(16u32), Some(8), Some(0), None].into_iter().enumerate() {
        for y in y0 + i as u32 * band..(y0 + (i as u32 + 1) * band).min(h) {
            for x in 0..w {
                let v = x * 256 / w.max(1);
                let p = match shift {
                    Some(s) => v << s,
                    None => v * 0x01_0101,
                };
                f.set(x, y, p);
            }
        }
    }
    f.mark(Rect::new(0, y0, w, h - y0));
}

fn scene() -> TestResult {
    if !com2_present() {
        return ktest::skip("no COM2 handshake port: only host/tests/dp/10-screendump.sh runs this scene");
    }
    let Some((fb, target)) = live() else { return ktest::skip("no GOP framebuffer") };
    let boot = Layout::from_bootinfo(&fb).map_err(|e| format!("{e:?}"))?;
    let mut g = display::take_scanout("dp.scene").map_err(|e| format!("{e:?}"))?;
    let (w, h) = (fb.width, fb.height);
    let mut f = RamFrame::new(w, h);
    if let Some((sw, sh, px)) = display::snapshot()
        && (sw, sh) == (w, h)
    {
        f.pixels_mut().copy_from_slice(&px);
    }
    ramps(&mut f);
    f.mark_all();
    g.present(&mut f);
    step("boot-full", &f, boot, fb.stride, &[])?;

    // Damage only: two far rectangles and a glyph; three pixels change unmarked (one between the rectangles).
    let a = Rect::new(w / 16, h / 16, w / 8, h / 8);
    let b = Rect::new(w * 3 / 4, h * 3 / 4 - h / 8, w / 10, h / 10);
    f.fill(a, 0x00FF_8000);
    f.fill(b, 0x0000_80FF);
    display::font::draw(&mut f, w / 3, h / 3, 'Ä', 3, 0x00FF_FFFF, 0x0020_2020);
    let mut hidden = Vec::new();
    for (x, y) in [(w / 2, h / 2), (w - 1, 0), (0, h / 2)] {
        hidden.push((x, y, f.pixels()[(y * w + x) as usize]));
        f.set(x, y, 0x00FF_00FF);
    }
    g.present(&mut f);
    step("boot-damage", &f, boot, fb.stride, &hidden)?;

    let others = [
        ("rgbx", Ok(Layout::Rgbx)),
        ("mask-perm", Layout::from_masks([0xFF, 0xFF_0000, 0xFF00, 0xFF00_0000])),
        ("mask-666", Layout::from_masks([0xFC_0000, 0xFC00, 0xFC, 0xFF00_0000])),
    ];
    for (name, l) in others {
        let l = l.map_err(|e| format!("{name}: {e:?}"))?;
        let dev = GopFb::new(target.clone(), w, h, fb.stride, l, "scene").map_err(|e| format!("{name}: {e:?}"))?;
        f.mark_all();
        g.present_via(&dev, &mut f);
        step(name, &f, l, fb.stride, &[])?;
    }

    // Packed layouts, same pixel stride: the rows land at stride * bytes, inside the black scanout.
    if fb.stride == w && boot.bytes_per_pixel() == 4 {
        let packed = [("mask-565", Layout::from_masks([0xF800, 0x07E0, 0x001F, 0])), ("mask-888", Layout::from_masks([0xFF_0000, 0xFF00, 0xFF, 0]))];
        let mut black = RamFrame::new(w, h);
        for (name, l) in packed {
            let l = l.map_err(|e| format!("{name}: {e:?}"))?;
            black.mark_all();
            g.present(&mut black);
            let dev = GopFb::new(target.clone(), w, h, fb.stride, l, "scene").map_err(|e| format!("{name}: {e:?}"))?;
            f.mark_all();
            g.present_via(&dev, &mut f);
            step(name, &f, l, fb.stride, &[])?;
        }
    } else {
        klog::event!(Info, "dp", "scene.packed_skipped", stride = fb.stride, width = w; "the packed steps need a scanout without line padding");
    }
    drop(g);
    com2_write("DPSCENE done\n");
    Ok(())
}
ktest!("dp.scene.screendump", QemuOnly, scene);

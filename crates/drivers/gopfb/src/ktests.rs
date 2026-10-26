//! In-kernel tests of DP-1 (`dp.*`). The damage-copy checks run against RAM targets (the framebuffer is never
//! read); the live framebuffer is checked through its page tables, timed, and compared from outside by the
//! screendump scenario (`scene.rs`, host/tests/dp/).

use crate::{check, live, GopFb, Layout, RamTarget};
use alloc::format;
use alloc::sync::Arc;
use devmodel::display::DisplayDevice;
use display::{RamFrame, Rect};
use hal::paging::{self, CacheType};
use ktest::{kassert, ktest, TestResult};

fn layouts() -> TestResult {
    check::all_layouts(97, 61)?;
    check::all_layouts(641, 3)
}
ktest!("dp.gopfb.layouts", Safe, layouts);

fn bad_input() -> TestResult {
    check::bad_damage_refused()?;
    check::geometry_refused()
}
ktest!("dp.gopfb.bad_input", Safe, bad_input);

/// The live window is MM-2's vmap mapping with PAT write-combining, over the whole framebuffer.
fn wc_window() -> TestResult {
    let Some((fb, target)) = live() else { return ktest::skip("no GOP framebuffer (headless)") };
    let Some((virt, phys)) = target.window() else {
        return Err("the framebuffer is drawn through bitsboot's mapping, not a vmap WC window (see the gopfb map.fallback event)".into());
    };
    kassert!(phys == fb.phys && target.len() as u64 == fb.size, "window {phys:#x}+{:#x}, framebuffer {:#x}+{:#x}", target.len(), fb.phys, fb.size);
    let pat = hal::pat::read_pat();
    kassert!(hal::pat::pat_entry(pat, 1) == hal::pat::MT_WC, "PAT entry 1 is {} (PAT {pat:#x})", hal::pat::pat_entry(pat, 1));
    let root = paging::kernel_root().unwrap_or_else(paging::current_root);
    let last = fb.size.saturating_sub(1);
    let mut pages = 0u64;
    let mut off = 0u64;
    while off <= last {
        let l = paging::translate_leaf(root, virt + off).ok_or_else(|| format!("framebuffer byte {off:#x} unmapped"))?;
        kassert!(l.phys == phys + off, "byte {off:#x} maps {:#x}", l.phys);
        kassert!(l.flags.cache() == Some(CacheType::WriteCombining), "byte {off:#x}: cache {:?}", l.flags.cache());
        pages += 1;
        // Next leaf (2 MiB pages where the vmap could use them).
        off = l.page_virt + l.size.bytes() - virt;
    }
    klog::event!(Info, "dp", "ktest.wc_window", virt = klog::Hex(virt), phys = klog::Hex(phys), size = fb.size, leaves = pages);
    Ok(())
}
ktest!("dp.gopfb.wc_window", Safe, wc_window);

/// Full-frame presents on the real screen (the Mini-PC check: about 1.7 ms at 1440p). Timing is logged,
/// judged only with a generous limit here.
fn present_full() -> TestResult {
    let Some(dev) = display::device() else { return ktest::skip("no display") };
    let m = dev.current_mode();
    let mut g = display::take_scanout("dp.gopfb.present_full").map_err(|e| format!("{e:?}"))?;
    let mut f = RamFrame::new(m.width, m.height);
    for (i, p) in f.pixels_mut().iter_mut().enumerate() {
        *p = check::pattern(i as u32 % m.width, i as u32 / m.width);
    }
    let rounds = 8u64;
    let (mut min, mut max, mut sum) = (u64::MAX, 0u64, 0u64);
    for _ in 0..rounds {
        f.mark_all();
        let t0 = time::now_ns();
        let st = g.present(&mut f);
        let dt = time::now_ns() - t0;
        kassert!(st.errors == 0 && st.pixels == m.width as u64 * m.height as u64, "{st:?}");
        min = min.min(dt);
        max = max.max(dt);
        sum += dt;
    }
    drop(g);
    let bytes = m.width as u64 * m.height as u64 * 4;
    let avg = sum / rounds;
    klog::event!(Info, "dp", "present.full", width = m.width, height = m.height, bytes = bytes, us_min = min / 1000, us_avg = avg / 1000,
        us_max = max / 1000, mb_per_s = bytes * 1000 / avg.max(1); "full-frame present (Mini-PC target ~1.7 ms at 2560x1440)");
    kassert!(max < 5_000_000_000, "a full present took {} ms", max / 1_000_000);
    Ok(())
}
ktest!("dp.gopfb.present_full", Safe, present_full);

/// The timer estimate: 60 Hz by default, a known refresh replaces it, the sequence never goes back, and
/// waiting ends at the estimated vblank.
fn vblank() -> TestResult {
    let t = Arc::new(RamTarget::new(64 * 4, 0));
    let d = GopFb::new(t, 8, 8, 8, Layout::Bgrx, "ram").map_err(|e| format!("{e:?}"))?;
    kassert!(d.vblank_period_ns() == 16_666_666 && d.current_mode().refresh_mhz == 0, "default 60 Hz, refresh unknown");
    let a = d.last_vblank();
    let ev = d.wait_vblank(1_000_000_000).ok_or("next vblank further than 1 s")?;
    let now = time::now_ns();
    kassert!(now >= ev.time_ns && ev.sequence > a.sequence, "woke at {now} for vblank {} at {}", ev.sequence, ev.time_ns);
    kassert!(d.last_vblank().sequence >= ev.sequence);
    kassert!(d.wait_vblank(0).is_none(), "a zero timeout must not sleep a frame");
    d.set_refresh_mhz(59_950);
    kassert!(d.vblank_period_ns() == 16_680_567 && d.current_mode().refresh_mhz == 59_950);
    let b = d.last_vblank();
    kassert!(b.sequence >= ev.sequence, "sequence went back across the refresh change");
    if let Some(live) = display::device() {
        let x = live.last_vblank();
        sched::sleep_ns(40_000_000);
        let y = live.last_vblank();
        kassert!(y.sequence >= x.sequence + 2 && y.time_ns > x.time_ns, "live estimate {x:?} -> {y:?} over 40 ms");
    }
    Ok(())
}
ktest!("dp.gopfb.vblank", Safe, vblank);

/// The console stops presenting while another owner holds the screen and gets it back whole.
fn handover() -> TestResult {
    let Some((w, h)) = display::with_frame(|f| (f.width, f.height)) else { return ktest::skip("no display") };
    let s0 = display::stats();
    let g = display::take_scanout("dp.display.handover").map_err(|e| format!("{e:?}"))?;
    kassert!(display::scanout_owner() == Some("dp.display.handover"));
    kassert!(matches!(display::take_scanout("second"), Err(display::ScanoutError::Busy { .. })), "two owners at once");
    display::with_frame(|f| f.fill(Rect::new(0, 0, 8, 8), 0x0012_3456));
    display::present();
    let s1 = display::stats();
    kassert!(s1.held_skips > s0.held_skips, "console present not held back");
    drop(g);
    let s2 = display::stats();
    kassert!(s2.pixels - s1.pixels >= w as u64 * h as u64, "the console frame was not shown whole again");
    kassert!(display::scanout_owner().is_none());
    Ok(())
}
ktest!("dp.display.handover", Safe, handover);

/// MCP's frame copy: allocated outside `display.frame`, the size of the device's mode; a second display
/// device is refused and leaves the installed one in place.
fn snapshot_and_device() -> TestResult {
    let Some(dev) = display::device() else { return ktest::skip("no display (headless)") };
    let m = dev.current_mode();
    let (w, h, px) = display::snapshot().ok_or("snapshot returned None with a display installed")?;
    kassert!((w, h) == (m.width, m.height), "snapshot {w}x{h}, mode {}x{}", m.width, m.height);
    kassert!(px.len() == w as usize * h as usize && px.capacity() == px.len(), "snapshot buffer sized exactly");
    let t = Arc::new(RamTarget::new(4 * 16 * 16, 0));
    let other: &'static GopFb = alloc::boxed::Box::leak(alloc::boxed::Box::new(GopFb::new(t, 16, 16, 16, Layout::Bgrx, "test").map_err(|e| format!("{e:?}"))?));
    display::set_device(other);
    kassert!(core::ptr::addr_eq(display::device().unwrap(), dev), "the installed device stays");
    kassert!(display::with_frame(|f| (f.width, f.height)) == Some((w, h)), "the console frame stays");
    Ok(())
}
ktest!("dp.display.snapshot", Safe, snapshot_and_device);

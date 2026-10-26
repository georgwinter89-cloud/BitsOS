//! `dp.fbscene.screendump` (QemuOnly): an fbdev program's picture equals MCP `screen`'s picture equals what
//! QEMU shows (host/tests/dp/20-fbdev.sh with DP-1's scene_check.py). Runs only when the scenario attaches COM2
//! (the DPSCENE protocol of `crate::scene`); otherwise it skips.
//!
//! `abi-fbdev draw` paints double-buffered frames, pans, then stores into the shown buffer and writes a band with
//! pwrite. The kernel checks `display::snapshot()` (MCP `screen`'s source) against the program's pattern, then
//! names that copy on COM2: the host compares QEMU's screendump with it pixel by pixel. Then the program is
//! killed and the console must have the screen again.

use super::ktests::{check_picture, handed_back, drawn, fb, need_program, start, text, wait_for, SEC};
use crate::{live, Layout};
use alloc::format;
use ktest::{kassert, ktest, TestResult};

fn scene() -> TestResult {
    if !crate::scene::com2_present() {
        return ktest::skip("no COM2 handshake port: only host/tests/dp/20-fbdev.sh runs this scene");
    }
    let Some((fbi, _)) = live() else { return ktest::skip("no GOP framebuffer") };
    let fb = fb()?;
    need_program()?;
    super::fix_node_owner();
    let boot = Layout::from_bootinfo(&fbi).map_err(|e| format!("{e:?}"))?;
    let s = start(&["abi-fbdev", "draw", "21", "3", "hold"])?;
    let r = (|| -> TestResult {
        kassert!(wait_for(120 * SEC, || drawn(&text(&s.out)).is_some() || s.p.has_exited()), "no 'drawn' line:\n{}", text(&s.out));
        let d = drawn(&text(&s.out)).ok_or_else(|| format!("ended early: {:?}\n{}{}", s.p.status(), text(&s.out), text(&s.err)))?;
        let (w, h, px) = display::snapshot().ok_or("no snapshot")?;
        check_picture(&d, w, h, &px)?;
        crate::scene::step_px("fbdev-program", &px, w, h, boot, fbi.stride, &[])
    })();
    s.p.kill(uapi::ab::signal::SIGKILL);
    let _ = s.p.wait(Some(10 * SEC));
    r?;
    handed_back(fb, "the scene program ended")?;
    crate::scene::com2_write("DPSCENE done\n");
    Ok(())
}
ktest!("dp.fbscene.screendump", QemuOnly, scene);

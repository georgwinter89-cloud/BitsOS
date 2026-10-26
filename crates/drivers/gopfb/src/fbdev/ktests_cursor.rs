//! In-kernel tests of the hardware cursor's door (`dp.fbdev.cursor`, GX-5a change request): the errors of
//! `cursor` in their order, as far as a kernel caller reaches them. Without a hardware cursor (QEMU) every valid
//! request answers ENODEV (the compositor keeps its software pointer); with one, a kernel caller is not the
//! screen's holder (EPERM). Nothing is shown or moved: Safe and rerunnable. `dp.fbdev.cursor.program` (GX-5a
//! CR 4, INT-W15) makes the same refusals from a program through sys_ioctl (`abi-fbcursor v2`).

use super::cursor::{Arg, ARG_BYTES, FBIO_BITSOS_CURSOR, OP_HIDE, OP_IMAGE, OP_MOVE};
use super::abi::UserIo;
use super::flip;
use super::ktests::{fb, finish, handed_back, start_at, text, SEC};
use alloc::format;
use ktest::{kassert, ktest, TestResult};
use uapi::errno::{EFAULT, EINVAL, ENODEV, ENOTTY, EPERM};

/// One argument at a fake address; any other address faults.
struct One([u8; ARG_BYTES]);
const AT: u64 = 0x7200_0000;

impl UserIo for One {
    fn read(&self, addr: u64, out: &mut [u8]) -> bool {
        if addr != AT || out.len() > ARG_BYTES {
            return false;
        }
        out.copy_from_slice(&self.0[..out.len()]);
        true
    }
    fn write(&self, _: u64, _: &[u8]) -> bool {
        false
    }
}

fn arg(op: u32, flags: u32) -> Arg {
    Arg { op, flags, x: -5, y: 7, width: 64, height: 64, hot_x: 0, hot_y: 0, pixels: AT + 0x1000 }
}

fn refusals() -> TestResult {
    let fb = fb()?;
    let call = |a: Arg, at: u64| fb.ioctl(FBIO_BITSOS_CURSOR, at, &One(a.to_bytes()), None);
    let a = arg(OP_IMAGE, 1);
    kassert!(Arg::from_bytes(&a.to_bytes()) == a, "struct round trip");
    kassert!(FBIO_BITSOS_CURSOR == (1 << 30) | ((ARG_BYTES as u64) << 16) | (u64::from(b'F') << 8) | 0xB2, "ioctl number");
    kassert!(call(arg(OP_MOVE, 0), AT + 4) == Err(EFAULT), "unreadable struct");
    kassert!(call(arg(3, 0), AT) == Err(EINVAL), "unknown op");
    kassert!(call(arg(OP_MOVE, 2), AT) == Err(EINVAL), "unknown flag");
    let r = fb.ioctl(FBIO_BITSOS_CURSOR + (8 << 16), AT, &One(arg(OP_HIDE, 0).to_bytes()), None);
    kassert!(r == Err(ENOTTY), "48-byte size: {r:?}");
    let want = if flip::get().is_some_and(|f| f.cursor_ready()) { EPERM } else { ENODEV };
    for op in [OP_HIDE, OP_IMAGE, OP_MOVE] {
        let r = call(arg(op, 0), AT);
        kassert!(r == Err(want), "{}", format!("op {op}: {r:?}, want {want}"));
    }
    Ok(())
}
ktest!("dp.fbdev.cursor", Safe, refusals);

const CURSOR_PROGRAM: &str = "/bin/abi-fbcursor";

/// `abi-fbcursor v2`: the refusals in order from a program (sys_ioctl, copy_from_user). The expected line holds
/// only without a hardware cursor; with one the test skips (the program would take the screen from bitscomp).
fn program() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    if flip::get().is_some_and(|f| f.cursor_ready()) {
        return ktest::skip("a hardware cursor is present: the expected line is the one without (QEMU)");
    }
    let fb = fb()?;
    if vfs::root_ns().lookup(CURSOR_PROGRAM).is_err() {
        return ktest::skip("/bin/abi-fbcursor is not in this image (user/abi-tests need the musl sysroot)");
    }
    super::fix_node_owner();
    let s = start_at(CURSOR_PROGRAM, &["abi-fbcursor", "v2"])?;
    let code = finish(&s, 60 * SEC)?;
    let got = text(&s.out);
    let want = "EFAULT EINVAL EINVAL ENODEV ENODEV ENOTTY\n";
    kassert!(got == want, "{}", format!("abi-fbcursor v2: got '{got}', want '{want}', exit {code}, stderr: {}", text(&s.err)));
    kassert!(code == 0, "exit {code}");
    handed_back(fb, "abi-fbcursor v2 ended")?;
    Ok(())
}
ktest!("dp.fbdev.cursor.program", Safe, program);

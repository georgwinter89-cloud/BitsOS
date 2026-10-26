//! In-kernel tests of the compositor grant (`dp.fbdev.compositor`, INT-W4 for WY-1a change request 4): only
//! fb0's screen holder gets the Interactive grant, focus needs the grant, revoke and the screen's return end
//! both, and the errors match the header in `compositor`. Safe and rerunnable: the program ends, the console has
//! the screen again and the focus is put back as it was.

use super::compositor::{self, FBIO_BITSOS_COMPOSITOR, OP_FOCUS, OP_GRANT, OP_REVOKE};
use super::ktests::{console_back, fb, handed_back, need_program, start, text, wait_for, Started, SEC};
use super::{abi::UserIo, fix_node_owner, Fb};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use ktest::{kassert, ktest, TestResult};
use sched::interactive::{self, Grant};
use uapi::errno::{EFAULT, EINVAL, ENOTTY, EPERM, ESRCH};

/// One 8-byte argument at a fake address; any other address faults.
struct Arg(Cell<[u8; 8]>);
const AT: u64 = 0x7100_0000;

impl UserIo for Arg {
    fn read(&self, addr: u64, out: &mut [u8]) -> bool {
        if addr != AT || out.len() > 8 {
            return false;
        }
        out.copy_from_slice(&self.0.get()[..out.len()]);
        true
    }
    fn write(&self, _: u64, _: &[u8]) -> bool {
        false
    }
}

fn call(fb: &Fb, s: Option<&Started>, op: u32, pid: u32) -> Result<Vec<u8>, i64> {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&op.to_le_bytes());
    b[4..].copy_from_slice(&pid.to_le_bytes());
    fb.ioctl(FBIO_BITSOS_COMPOSITOR, AT, &Arg(Cell::new(b)), s.map(|s| &s.p))
}

fn has_grant(pid: u32) -> bool {
    interactive::grants_of(pid as u64) & Grant::Compositor as u8 != 0
}

fn steps(fb: &Fb, s: &Started) -> Result<(), String> {
    let me = s.p.pid;
    kassert!(call(fb, None, OP_GRANT, 0) == Err(EPERM), "a kernel caller got the grant");
    kassert!(call(fb, Some(s), OP_FOCUS, me) == Err(EPERM), "focus without the grant");
    kassert!(call(fb, Some(s), OP_REVOKE, 0) == Err(EPERM), "revoke without the grant");
    // A kernel owner has the screen: the program is not its holder and cannot take it.
    {
        let g = display::take_scanout("dp.fbdev.compositor").map_err(|e| format!("{e:?}"))?;
        let r = call(fb, Some(s), OP_GRANT, 0);
        drop(g);
        kassert!(r == Err(EPERM), "grant while a kernel owner holds the screen: {r:?}");
        kassert!(!has_grant(me), "grant bit set after a refusal");
    }
    kassert!(console_back(fb, SEC), "the console did not get the screen back (holder {})", fb.holder_pid());
    // The console has the screen: the program with fb0 open takes it with the grant.
    kassert!(call(fb, Some(s), OP_GRANT, 0) == Ok(Vec::new()), "grant refused");
    kassert!(fb.holder_pid() == me, "the grant did not take the screen (holder {})", fb.holder_pid());
    kassert!(has_grant(me) && compositor::granted() == me, "grants {:#x}, granted {}", interactive::grants_of(me as u64), compositor::granted());
    kassert!(call(fb, Some(s), 7, 0) == Err(EINVAL), "unknown op");
    kassert!(fb.ioctl(FBIO_BITSOS_COMPOSITOR, AT + 8, &Arg(Cell::new([0; 8])), Some(&s.p)) == Err(EFAULT), "bad address");
    kassert!(fb.ioctl(FBIO_BITSOS_COMPOSITOR + (8 << 16), AT, &Arg(Cell::new([0; 8])), Some(&s.p)) == Err(ENOTTY), "16-byte size");
    kassert!(call(fb, Some(s), OP_FOCUS, u32::MAX >> 1) == Err(ESRCH), "focus on no process");
    kassert!(call(fb, Some(s), OP_FOCUS, me) == Ok(Vec::new()), "focus refused");
    kassert!(interactive::focus() == Some(me as u64), "focus {:?}, want {me}", interactive::focus());
    kassert!(call(fb, Some(s), OP_FOCUS, 0) == Ok(Vec::new()) && interactive::focus().is_none(), "focus 0: {:?}", interactive::focus());
    kassert!(call(fb, Some(s), OP_FOCUS, me) == Ok(Vec::new()), "focus again");
    kassert!(call(fb, Some(s), OP_REVOKE, 0) == Ok(Vec::new()), "revoke refused");
    kassert!(!has_grant(me) && compositor::granted() == 0 && interactive::focus().is_none(), "after revoke: grants {:#x}, focus {:?}", interactive::grants_of(me as u64), interactive::focus());
    // Granted again, the screen's return takes grant and focus with it.
    kassert!(call(fb, Some(s), OP_GRANT, 0) == Ok(Vec::new()) && call(fb, Some(s), OP_FOCUS, me) == Ok(Vec::new()), "second grant");
    display::request_scanout_back();
    kassert!(wait_for(SEC, || compositor::granted() == 0), "the grant outlived the screen's return (holder {})", fb.holder_pid());
    // INT-W9: `handed_back` clears GRANTED first and then the grant bit and the focus (on the CPU that settled the
    // screen), so at -smp 16 the test may look in between: wait for all of it.
    kassert!(
        wait_for(SEC, || !has_grant(me) && interactive::focus().is_none()),
        "after the return: grants {:#x}, focus {:?}",
        interactive::grants_of(me as u64),
        interactive::focus()
    );
    Ok(())
}

fn grant() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let prev = interactive::focus().unwrap_or(0);
    let _ = interactive::set_focus(0);
    let s = start(&["abi-fbdev", "idle"])?;
    let r = (|| -> TestResult {
        kassert!(wait_for(30 * SEC, || text(&s.out).contains("idle pid=") || s.p.has_exited()), "no 'idle' line: {}{}", text(&s.out), text(&s.err));
        steps(fb, &s)
    })();
    s.p.kill(uapi::ab::signal::SIGKILL);
    let _ = s.p.wait(Some(10 * SEC));
    let _ = interactive::set_focus(prev).or_else(|_| interactive::set_focus(0));
    r?;
    handed_back(fb, "the kill")?;
    kassert!(compositor::granted() == 0 && !has_grant(s.p.pid), "a grant outlived the program");
    Ok(())
}
ktest!("dp.fbdev.compositor", Safe, grant);

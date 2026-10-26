//! The compositor's door to the Interactive class (SC-7a, B4; WY-1a change request 4, built by INT-W4): one
//! BitsOS ioctl on /dev/fb0 in DP-3's private 'F' range (ENOTTY on Linux, so bitscomp stays portable):
//!
//! ```c
//! struct bitsos_fb_compositor { __u32 op; __u32 pid; };
//! #define FBIO_BITSOS_COMPOSITOR _IOW('F', 0xB1, struct bitsos_fb_compositor)
//! ```
//!
//! op 1 (grant): the caller's process gets `Grant::Compositor`. Only fb0's scanout holder may ask (a caller with
//! fb0 open while nobody holds the screen takes it first, as KD_GRAPHICS would), and only when B3's
//! `policy::check(Access::Scheduling)` allows it. op 2 (focus): the grant holder names the program in front
//! (`pid`, 0 = none). op 0 (revoke): the grant holder gives the grant (and a focus it set) back. Anything else
//! in `op` is EINVAL. No other way leads here: the grant follows the screen, never the caller's wish alone, so a
//! program cannot raise itself without taking the whole screen from the console (and the policy saying yes).
//! When the screen goes back (`handed_back`: the holder ended, KD_TEXT, a kernel owner) the grant and the focus
//! the compositor set end with it; the grant also ends with the process's last thread (sched).

use super::abi::UserIo;
use super::Fb;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use proc::Process;
use sched::interactive::{self, Grant, GrantError};
use uapi::errno::{EACCES, EFAULT, EINVAL, ENOMEM, EPERM, ESRCH};
use vfs::Errno;

/// _IOW('F', 0xB1, 8 bytes).
pub const FBIO_BITSOS_COMPOSITOR: u64 = 0x4008_46B1;
pub const OP_REVOKE: u32 = 0;
pub const OP_GRANT: u32 = 1;
pub const OP_FOCUS: u32 = 2;

/// The process holding the grant through this ioctl (0: none).
static GRANTED: AtomicU32 = AtomicU32::new(0);
/// The group this ioctl put in front (0: none); cleared with the grant only while it is still in front, so a
/// focus set elsewhere since (MCP `sched_focus`) is left alone.
static FOCUSED: AtomicU32 = AtomicU32::new(0);

klog::counter!(CALLS, "fbdev.compositor.calls");
klog::counter!(REFUSED, "fbdev.compositor.refused");

fn grant_err(e: GrantError) -> Errno {
    match e {
        GrantError::NoGroup => ESRCH,
        GrantError::NoMemory => ENOMEM,
    }
}

fn refuse(pid: u32, op: u32, why: &'static str, e: Errno) -> Result<Vec<u8>, Errno> {
    REFUSED.inc();
    klog::event!(Info, "fbdev", "compositor.refused", pid = pid, op = op, why = why);
    Err(e)
}

/// `FBIO_BITSOS_COMPOSITOR` from `who` (None: a kernel caller, refused: it has no process to grant).
pub(super) fn ioctl(fb: &Fb, arg: u64, io: &dyn UserIo, who: Option<&Arc<Process>>) -> Result<Vec<u8>, Errno> {
    CALLS.inc();
    let mut b = [0u8; 8];
    if !io.read(arg, &mut b) {
        return Err(EFAULT);
    }
    let op = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let pid = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let Some(me) = who else { return refuse(0, op, "no calling process", EPERM) };
    match op {
        OP_GRANT => {
            // The screen's holder only; a caller with fb0 open while the console has the screen takes it now.
            if fb.holder_pid() != me.pid && !(fb.holder_pid() == 0 && fb.take_for(me.pid)) {
                return refuse(me.pid, op, "not the screen's holder", EPERM);
            }
            if policy::check(&me.subject(), &policy::Access::Scheduling { grant: "compositor" }).is_err() {
                return refuse(me.pid, op, "policy", EACCES);
            }
            let prev = GRANTED.swap(me.pid, Ordering::AcqRel);
            if prev != 0 && prev != me.pid {
                drop_grant(prev, "replaced");
            }
            let n = interactive::set_grant(me.pid as u64, Grant::Compositor, true).map_err(grant_err)?;
            klog::event!(Info, "fbdev", "compositor.granted", pid = me.pid, threads = n);
            Ok(Vec::new())
        }
        OP_FOCUS => {
            if GRANTED.load(Ordering::Acquire) != me.pid || fb.holder_pid() != me.pid {
                return refuse(me.pid, op, "no compositor grant", EPERM);
            }
            if pid != 0 && !proc::find(pid).is_some_and(|p| !p.has_exited()) {
                return Err(ESRCH);
            }
            interactive::set_focus(pid as u64).map_err(grant_err)?;
            FOCUSED.store(pid, Ordering::Release);
            Ok(Vec::new())
        }
        OP_REVOKE => {
            if GRANTED.compare_exchange(me.pid, 0, Ordering::AcqRel, Ordering::Acquire).is_err() {
                return refuse(me.pid, op, "no compositor grant", EPERM);
            }
            drop_grant(me.pid, "revoked");
            Ok(Vec::new())
        }
        _ => Err(EINVAL),
    }
}

/// Ends `pid`'s compositor grant and the focus it set (`GRANTED` already cleared or replaced by the caller).
fn drop_grant(pid: u32, why: &'static str) {
    let _ = interactive::set_grant(pid as u64, Grant::Compositor, false);
    let f = FOCUSED.swap(0, Ordering::AcqRel);
    if f != 0 && interactive::focus() == Some(f as u64) {
        let _ = interactive::set_focus(0);
    }
    klog::event!(Info, "fbdev", "compositor.revoked", pid = pid, why = why);
}

/// The screen went back to the console from `pid` (`Fb::settle`, outside `fbdev.screen`): its grant ends too.
pub(super) fn handed_back(pid: u32) {
    if pid != 0 && GRANTED.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        drop_grant(pid, "screen returned");
    }
}

/// The process holding the grant (0: none), for the ktests.
pub fn granted() -> u32 {
    GRANTED.load(Ordering::Acquire)
}

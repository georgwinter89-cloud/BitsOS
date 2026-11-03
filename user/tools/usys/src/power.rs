//! UL-7: what init needs to end the system: sync, reboot(2), the signals it waits for (signalfd) and who is on
//! the other end of its power socket (SO_PEERCRED). linux-6.6/kernel/reboot.c, fs/signalfd.c, net/core/sock.c.

use super::{SysResult, ints, raw, ret};

pub const NR_SYNC: usize = 162;
pub const NR_REBOOT: usize = 169;
pub const NR_SIGNALFD4: usize = 289;

pub const MAGIC1: usize = 0xfee1_dead;
pub const MAGIC2: usize = 672_274_793;
pub const CMD_RESTART: u32 = 0x0123_4567;
pub const CMD_POWER_OFF: u32 = 0x4321_FEDC;
pub const SFD_NONBLOCK: usize = 0o4000;
pub const SFD_CLOEXEC: usize = 0o2000000;
const SOL_SOCKET: usize = 1;
const SO_PEERCRED: usize = 17;

pub fn sync() -> SysResult {
    ints(NR_SYNC, &[])
}

/// reboot(MAGIC1, MAGIC2, cmd, NULL): returns only when refused or when the change did not happen.
pub fn reboot(cmd: u32) -> SysResult {
    ints(NR_REBOOT, &[MAGIC1, MAGIC2, cmd as usize, 0])
}

/// signalfd4(-1, &mask, 8, flags): a new descriptor for the signals in `mask` (bit n-1 = signal n).
pub fn signalfd(mask: u64, flags: usize) -> SysResult {
    // SAFETY: the kernel reads the 8-byte mask from a local.
    ret(unsafe { raw(NR_SIGNALFD4, [usize::MAX, &mask as *const u64 as usize, 8, flags, 0, 0]) })
}

/// The parts of a `struct signalfd_siginfo` init looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigInfo {
    pub signo: u32,
    pub code: i32,
    pub pid: u32,
    pub uid: u32,
}

/// One signal from a signalfd (None: nothing pending on a non-blocking one).
pub fn read_signal(fd: usize) -> SysResult<Option<SigInfo>> {
    let mut b = [0u8; 128];
    match super::read(fd, &mut b) {
        Ok(128) => {
            let at = |o: usize| u32::from_ne_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
            Ok(Some(SigInfo { signo: at(0), code: at(8) as i32, pid: at(12), uid: at(16) }))
        }
        Ok(_) => Ok(None),
        Err(super::EAGAIN) => Ok(None),
        Err(e) => Err(e),
    }
}

/// SO_PEERCRED of a connected Unix socket: (pid, uid, gid).
pub fn peer_cred(fd: usize) -> SysResult<(u32, u32, u32)> {
    let mut c = [0u32; 3];
    let mut len: u32 = 12;
    // SAFETY: the kernel writes at most 12 bytes (struct ucred) into c and the length into len.
    ret(unsafe { raw(super::nr::GETSOCKOPT, [fd, SOL_SOCKET, SO_PEERCRED, c.as_mut_ptr() as usize, &mut len as *mut u32 as usize, 0]) })?;
    Ok((c[0], c[1], c[2]))
}

/// Makes `cmd`'s child start with the signals in `mask` unblocked. Init blocks its power signals for the
/// signalfd; the blocked mask is inherited through fork and exec (linux-6.6/kernel/fork.c copies it, execve
/// keeps it), and the spawned programs (busybox ash, the services) expect a clean one.
pub fn unblock_in_child(cmd: &mut std::process::Command, mask: u64) {
    use std::os::unix::process::CommandExt;
    let hook = move || -> std::io::Result<()> {
        let set = [mask];
        // SAFETY: rt_sigprocmask reads 8 bytes from a local and writes nothing (no old set); a raw syscall is
        // async-signal-safe between fork and exec.
        let r = unsafe { raw(super::nr::RT_SIGPROCMASK, [super::SIG_UNBLOCK, set.as_ptr() as usize, 0, 8, 0, 0]) };
        if r < 0 { Err(std::io::Error::from_raw_os_error(-r as i32)) } else { Ok(()) }
    };
    // SAFETY: the hook runs in the forked child before exec and makes one raw system call on a local; it
    // touches no lock, allocator or state shared with the parent.
    unsafe {
        cmd.pre_exec(hook);
    }
}

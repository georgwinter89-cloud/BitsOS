//! The Interactive scheduling class for bitscomp on V2 (V2-LANES WY-4, SC-7a): `FBIO_BITSOS_COMPOSITOR =
//! _IOW('F', 0xB1, struct { u32 op; u32 pid; })` on /dev/fb0 (V2 since INT-W4, kernel gopfb fbdev/compositor.rs),
//! allowed only for the scanout's holder. A probe at start: any error (ENOTTY on Linux, EPERM/EACCES when V2
//! refuses) leaves bitscomp in the Normal class with one line on stderr; once the kernel answers, bitscomp holds
//! the grant and names the focused client's pid on every focus change.

use std::fs::File;
use std::os::fd::AsRawFd;

/// _IOW('F', 0xB1, 8 bytes): dir write (1) << 30 | size 8 << 16 | 'F' << 8 | 0xB1.
const FBIO_BITSOS_COMPOSITOR: u64 = 0x4008_46B1;
const OP_GRANT: u32 = 1;
const OP_FOCUS: u32 = 2;

pub struct Grant {
    fb: File,
    /// The pid last named, so a focus change within one program costs no ioctl.
    last: Option<i32>,
}

fn call(fd: i32, op: u32, pid: u32) -> std::io::Result<()> {
    let arg: [u32; 2] = [op, pid];
    // SAFETY: the ioctl reads 8 bytes from `arg`, a local that outlives the call.
    let r = unsafe { libc::ioctl(fd, FBIO_BITSOS_COMPOSITOR as libc::Ioctl, arg.as_ptr()) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

impl Grant {
    /// Asks for the compositor grant on the framebuffer `path`; `None` (and a line) when it is not offered.
    pub fn request(path: &str) -> Option<Grant> {
        let fb = std::fs::OpenOptions::new().read(true).write(true).open(path).ok()?;
        match call(fb.as_raw_fd(), OP_GRANT, std::process::id()) {
            Ok(()) => {
                eprintln!("bitscomp: scheduling class Interactive (compositor grant on {path})");
                Some(Grant { fb, last: None })
            }
            Err(e) => {
                eprintln!("bitscomp: scheduling class stays Normal: the compositor grant is not offered ({e})");
                None
            }
        }
    }

    /// The focused client's pid (0: none). An error means the grant is gone (the scanout was handed back, or
    /// the policy says no): the caller drops the grant after this one line, so the input path never retries it.
    pub fn focus(&mut self, pid: i32) -> Result<(), ()> {
        let pid = pid.max(0);
        if self.last == Some(pid) {
            return Ok(());
        }
        match call(self.fb.as_raw_fd(), OP_FOCUS, pid as u32) {
            Ok(()) => {
                self.last = Some(pid);
                Ok(())
            }
            Err(e) => {
                eprintln!("bitscomp: focus {pid} for the Interactive class refused ({e}): grant dropped, class Normal from now on");
                Err(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ioctl_number_is_iow_f_b1_of_eight_bytes() {
        let iow = |ty: u64, nr: u64, size: u64| (1u64 << 30) | (size << 16) | (ty << 8) | nr;
        assert_eq!(super::FBIO_BITSOS_COMPOSITOR, iow(b'F' as u64, 0xB1, 8));
    }
}

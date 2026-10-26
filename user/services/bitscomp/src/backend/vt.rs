//! The console's side of the screen (linux-6.6/drivers/tty/vt/vt_ioctl.c KDSETMODE; V2 crates/console
//! ttyioctl.rs and vt.rs): bitscomp puts its terminal (or /dev/tty0) into KD_GRAPHICS, so the VT draws and types
//! nothing and fbdev shows bitscomp's picture at once, and puts it back to KD_TEXT when it ends (normally, on
//! SIGTERM/SIGINT/SIGHUP, or from the panic hook). V2 allows KDSETMODE to the terminal's session or root only
//! (`vt_perm`); started by MCP `run` bitscomp has no terminal, the ioctl answers EPERM and the first store into
//! the fb0 mapping takes the screen instead (fbdev's "who has the screen"). A killed or crashed bitscomp needs
//! nothing from here: the kernel takes the screen back when the KD_GRAPHICS owner or fb0's holder ends
//! (vt.rs KD_OWNER `reclaim`, fbdev's 100 ms owner check).
//!
//! WY-7: a root start that drops to the user (`drop_root`) switches to KD_GRAPHICS as root and back to KD_TEXT
//! as the user, without CAP_SYS_TTY_CONFIG. That works on the controlling terminal (a foreground start from the
//! console shell); a `setsid` start (/dev/tty0, no controlling terminal) gets EPERM, as on Linux
//! (linux-6.6/drivers/tty/vt/vt_ioctl.c:746 perm), and the console comes back only through V2's reclaim above. Linux has no such
//! reclaim: there the VT would stay in KD_GRAPHICS, so a dropping start on Linux must be a foreground one.

use std::sync::atomic::{AtomicI32, Ordering};

const KDSETMODE: u64 = 0x4B3A;
const KD_TEXT: u64 = 0;
const KD_GRAPHICS: u64 = 1;

/// The terminal fd in KD_GRAPHICS (-1: none), for the panic hook.
static GRAPHICS_FD: AtomicI32 = AtomicI32::new(-1);

pub struct Console {
    fd: i32,
    own_fd: bool,
}

fn kdsetmode(fd: i32, mode: u64) -> std::io::Result<()> {
    // SAFETY: KDSETMODE takes its argument by value; no memory is passed.
    let r = unsafe { libc::ioctl(fd, KDSETMODE as libc::Ioctl, mode as libc::c_ulong) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

impl Console {
    /// Asks for KD_GRAPHICS on stdin when it is a terminal, else on /dev/tty0 (O_NOCTTY: never becomes bitscomp's
    /// controlling terminal). A refusal is a note: drawing takes the screen anyway.
    pub fn take() -> Console {
        // SAFETY: isatty only looks at fd 0.
        let (fd, own_fd) = if unsafe { libc::isatty(0) } == 1 {
            (0, false)
        } else {
            // SAFETY: a plain open of a NUL-terminated path.
            (unsafe { libc::open(c"/dev/tty0".as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) }, true)
        };
        if fd < 0 {
            eprintln!("bitscomp: console: no terminal to switch ({}); the first frame takes the screen", std::io::Error::last_os_error());
            return Console { fd: -1, own_fd: false };
        }
        match kdsetmode(fd, KD_GRAPHICS) {
            Ok(()) => {
                eprintln!("bitscomp: console: KD_GRAPHICS (the console is back on exit)");
                GRAPHICS_FD.store(fd, Ordering::Release);
                install_panic_hook();
                Console { fd, own_fd }
            }
            Err(e) => {
                eprintln!("bitscomp: console: KDSETMODE refused ({e}); the first frame takes the screen, the kernel gives it back when bitscomp ends");
                if own_fd {
                    // SAFETY: our own fd from the open above, not used again.
                    unsafe { libc::close(fd) };
                }
                Console { fd: -1, own_fd: false }
            }
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        if self.fd < 0 {
            return;
        }
        GRAPHICS_FD.store(-1, Ordering::Release);
        match kdsetmode(self.fd, KD_TEXT) {
            Ok(()) => eprintln!("bitscomp: console: KD_TEXT, the console has the screen again"),
            Err(e) => eprintln!("bitscomp: console: KD_TEXT refused ({e}); the kernel takes the screen back when bitscomp ends"),
        }
        if self.own_fd {
            // SAFETY: our own fd, closed once.
            unsafe { libc::close(self.fd) };
        }
    }
}

fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let fd = GRAPHICS_FD.swap(-1, Ordering::AcqRel);
        if fd >= 0 {
            let _ = kdsetmode(fd, KD_TEXT);
        }
        prev(info);
    }));
}

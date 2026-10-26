//! ioctl (linux-6.6/fs/ioctl.c `SYSCALL_DEFINE3(ioctl)` and `do_vfs_ioctl`): the generic commands every file
//! has (FIOCLEX, FIONCLEX, FIONBIO, FIONREAD on regular files), then the file's own ioctl. Terminals (the
//! console fds) answer TCGETS and TIOCGWINSZ through FS's tty nodes (IN's console behind them); every other
//! file answers ENOTTY (`File::ioctl`'s default), as Linux does for files without the command.

use crate::SysCtx;
use uapi::ab::ioctl_more::*;
use uapi::errno::*;

/// O_NONBLOCK in the file status flags.
const O_NONBLOCK: u64 = 0o4000;
/// `_IOR('T', 0x41, int)`: open the pty's slave (linux-6.6/include/uapi/asm-generic/ioctls.h).
const TIOCGPTPEER: u64 = 0x5441;

fn sys_ioctl(c: &mut SysCtx<'_>) -> i64 {
    let r = ioctl(c);
    crate::note_gap(0, c.args[1] as u32 as u64, r, c.proc.name.as_str());
    r
}

fn ioctl(c: &mut SysCtx<'_>) -> i64 {
    let [fd, cmd, arg, ..] = c.args;
    // `unsigned int fd, unsigned int cmd`: upper halves do not count.
    let (fd, cmd) = (fd as u32 as usize, cmd as u32 as u64);
    let f = match c.proc.fds.get(fd) {
        // fdget() does not see O_PATH files: EBADF.
        Ok(f) if !f.is_path_only() => f,
        _ => return -EBADF,
    };
    match cmd {
        FIOCLEX | FIONCLEX => return c.proc.fds.set_cloexec(fd, cmd == FIOCLEX).map(|_| 0).unwrap_or_else(|e| -e),
        FIONBIO => {
            let mut b = [0u8; 4];
            if c.proc.mm.copy_from_user(&mut b, arg).is_err() {
                return -EFAULT;
            }
            let on = i32::from_le_bytes(b) != 0;
            let fl = f.status_flags();
            f.set_status_flags(if on { fl | O_NONBLOCK } else { fl & !O_NONBLOCK });
            return 0;
        }
        uapi::ioctl::FIONREAD if f.file.mode() & 0o170000 == 0o100000 => {
            // Regular files: bytes from the position to the end (an int; negative past the end, as Linux).
            let n = f.file.size() as i64 - f.pos() as i64;
            return c.proc.mm.copy_to_user(arg, &(n as i32).to_le_bytes()).map(|_| 0).unwrap_or(-EFAULT);
        }
        // VS-2 CR5: the generic freeze commands, before the file's own ioctl (fs/ioctl.c:804/807).
        crate::fsfreeze::FIFREEZE => return crate::fsfreeze::freeze(c, &f),
        crate::fsfreeze::FITHAW => return crate::fsfreeze::thaw(c, &f),
        // AB-5: timerfd's own command (`timerfd_ioctl`).
        uapi::ab::timerfd::TFD_IOC_SET_TICKS => {
            if let Some(sh) = crate::timerfd::of(&f) {
                return crate::timerfd::set_ticks(c, &sh, arg);
            }
        }
        _ => {}
    }
    // IN-7 CR2: TIOCGPTPEER opens the master's slave as the kernel (Linux opens it through the master's devpts
    // mount without a path walk: linux-6.6/drivers/tty/pty.c:602 `ptm_open_peer`) and answers the new fd. A still
    // locked slave answers the open's EIO. Files that are not a pty master go on below (ENOTTY).
    if cmd == TIOCGPTPEER {
        if let Some(path) = console::pty::peer_path(&*f.file) {
            const O_ACCMODE: u64 = 3;
            const O_NOCTTY: u64 = 0o400;
            const O_CLOEXEC: u64 = 0o2000000;
            let flags = arg & (O_ACCMODE | O_NOCTTY | O_NONBLOCK | O_CLOEXEC);
            return match vfs::root_ns().open_flags(path.as_bytes(), flags, 0) {
                Ok(peer) => c.proc.fds.install_flags(peer, 0, flags & O_CLOEXEC != 0).map(|fd| fd as i64).unwrap_or_else(|e| -e),
                Err(e) => -e,
            };
        }
    }
    // NW-6: a socket's interface ioctls (SIOCGIF*, SIOCGIFCONF, the 0x89xx device commands) read and write the
    // program's `struct ifreq` themselves (`net::sock::sys::dev_ioctl`); every other command goes on below.
    if let Some(r) = crate::socket::dev_ioctl(c, fd, cmd, arg) {
        return r;
    }
    // IN-4: devices that read their argument or answer a value of their own (terminals, evdev).
    if let Some(r) = f.file.ioctl_user(cmd, arg, &ProcMem(c.proc, Some(f.status_flags()))) {
        return r.unwrap_or_else(|e| -e);
    }
    match f.file.ioctl(cmd, arg) {
        Ok(mut out) => {
            // The kernel's structs, whatever the device produced: TCGETS copies `struct termios` (36 bytes),
            // TIOCGWINSZ `struct winsize` (8).
            match cmd {
                uapi::ioctl::TCGETS => out.truncate(TERMIOS_SIZE),
                uapi::ioctl::TIOCGWINSZ => out.truncate(WINSIZE_SIZE),
                _ => {}
            }
            if !out.is_empty() && c.proc.mm.copy_to_user(arg, &out).is_err() {
                return -EFAULT;
            }
            0
        }
        Err(e) => -e,
    }
}
crate::syscall!(IOCTL, sys_ioctl);

/// The calling process's memory for `File::ioctl_user` (IN-4).
/// The caller's memory and, for a device ioctl, the open file's status flags.
struct ProcMem<'a>(&'a proc::Process, Option<u64>);

impl vfs::sys::UserMem for ProcMem<'_> {
    fn file_status(&self) -> Option<u64> {
        self.1
    }
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        self.0.mm.copy_from_user(buf, addr).is_ok()
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.0.mm.copy_to_user(addr, data).is_ok()
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<alloc::vec::Vec<u8>, vfs::Errno> {
        let mut v = alloc::vec::Vec::new();
        match hal::usercopy::strncpy_from_user(self.0.mm.root(), addr, &mut v, max) {
            Ok(_) => Ok(v),
            Err(hal::usercopy::StrError::Fault) => Err(EFAULT),
            Err(hal::usercopy::StrError::TooLong) => Err(ENAMETOOLONG),
        }
    }
}

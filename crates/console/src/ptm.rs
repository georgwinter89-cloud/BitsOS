//! The pty master (IN-7): reads what the slave wrote and echoed, writes the slave's input, and answers the
//! master's ioctls (`pty_unix98_ioctl`, linux-6.6/drivers/tty/pty.c:642, then `tty_ioctl` with the slave as
//! `real_tty`, tty_io.c:2652). See `pty` for the whole picture.

use crate::jobs::{self, Procs};
use crate::pty::{Pair, PTMX_DEV, SLICE_NS, hung_mask, ended, put, get_i32, sets_termios, SIG_MAX};
use crate::termios::ioc::*;
use crate::ttyioctl;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::poll::PollQueue;
use vfs::sys::UserMem;
use vfs::{Errno, Events, File};
pub struct MasterFile {
    pair: Arc<Pair>,
}

impl MasterFile {
    pub(crate) fn new(pair: Arc<Pair>) -> MasterFile {
        MasterFile { pair }
    }

    /// Opens a slave end as `/dev/pts/N` would (`ptm_open_peer` without the descriptor; ktests and tests).
    pub fn open_slave(&self, flags: u64) -> Result<Arc<crate::pts::SlaveFile>, Errno> {
        crate::pts::PtsNode::new(&self.pair).open(flags)
    }

    pub fn pair(&self) -> &Arc<Pair> {
        &self.pair
    }

    fn hung(&self) -> bool {
        self.pair.st.lock().master_hung
    }

    /// The master's read (`n_tty_read` in raw mode, VMIN 1): what is there; EIO once the slave is closed and
    /// everything was read.
    pub fn read(&self, buf: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        if self.hung() {
            return Ok(0);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let out = &self.pair.out;
        loop {
            let n = {
                let mut b = out.buf.lock();
                let n = buf.len().min(b.len());
                for (i, x) in b.drain(..n).enumerate() {
                    buf[i] = x;
                }
                n
            };
            if n > 0 {
                out.room.wake_all();
                return Ok(n);
            }
            if self.pair.st.lock().slave_closed {
                return Err(EIO);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            if abort() || self.hung() {
                return Err(EINTR);
            }
            out.rq.wait_until_timeout(|| !out.buf.lock().is_empty() || self.pair.st.lock().slave_closed, SLICE_NS);
        }
    }

    /// The master's write: the slave's input, waiting for room in the pty's buffer.
    pub fn write(&self, data: &[u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        if self.hung() {
            return Err(EIO);
        }
        let pair = &self.pair;
        let mut done = 0;
        while done < data.len() {
            let n = pair.push_pending(&data[done..]);
            if n == 0 {
                pair.feed();
                if pair.pending_room() > 0 {
                    continue;
                }
                if done > 0 {
                    break;
                }
                if nonblock {
                    return Err(EAGAIN);
                }
                if abort() || self.hung() {
                    return Err(EINTR);
                }
                pair.in_room.wait_until_timeout(|| pair.pending_room() > 0, SLICE_NS);
                continue;
            }
            done += n;
            pair.feed();
        }
        Ok(done)
    }

    /// poll on the master (`n_tty_poll`).
    pub fn poll_events(&self) -> Events {
        if self.hung() {
            return Events(hung_mask());
        }
        let mut m = 0;
        if !self.pair.out.buf.lock().is_empty() {
            m |= Events::IN | Events::RDNORM;
        }
        if self.pair.st.lock().slave_closed {
            m |= Events::HUP;
        }
        if self.pair.pending_room() > 0 {
            m |= Events::OUT | Events::WRNORM;
        }
        Events(m)
    }

    /// The master's ioctls: `pty_unix98_ioctl`, then `tty_ioctl`'s with the slave as `real_tty` (window size,
    /// TIOCGDEV, the job-control calls, the termios calls of `tty_mode_ioctl`), then n_tty's on the master itself.
    pub fn ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem, p: &dyn Procs) -> Result<i64, Errno> {
        let pair = &self.pair;
        if self.hung() {
            // `hung_up_tty_ioctl`.
            return Err(if cmd == TIOCSPGRP { ENOTTY } else { EIO });
        }
        let me = p.current();
        let privileged = me.is_some_and(|m| m.privileged());
        match cmd {
            TIOCGPTN => put(mem, arg, &pair.index.to_le_bytes()),
            TIOCSPTLCK => {
                let v = get_i32(mem, arg)?;
                pair.st.lock().locked = v != 0;
                Ok(0)
            }
            TIOCGPTLCK => {
                // The lock's guard ends before the user copy (no user memory under a spinlock).
                let v = pair.st.lock().locked as i32;
                put(mem, arg, &v.to_le_bytes())
            }
            TIOCPKT => {
                if get_i32(mem, arg)? == 0 {
                    return Ok(0);
                }
                klog::event!(Info, "console", "pty.packet_mode", index = pair.index; "packet mode is not built: TIOCPKT answers ENOTTY");
                Err(ENOTTY)
            }
            TIOCGPKT => put(mem, arg, &0i32.to_le_bytes()),
            TIOCSIG => {
                // `pty_signal`: the slave's foreground group gets the signal.
                if !(1..=SIG_MAX).contains(&arg) {
                    return Err(EINVAL);
                }
                ttyioctl::signal_fg(&pair.tty, p, arg as u32);
                Ok(0)
            }
            // TIOCGPTPEER needs a new descriptor: `sys_ioctl` answers it before the file is asked (INT-A2b,
            // `peer_path`); a caller reaching this file directly gets ENOTTY.
            TIOCGPTPEER => Err(ENOTTY),
            TIOCSTI => {
                if !privileged {
                    return Err(EIO);
                }
                let mut c = [0u8; 1];
                if !mem.read(arg, &mut c) {
                    return Err(EFAULT);
                }
                // Into the master's own input (its read side); dropped when that is full, as
                // `tty_insert_flip_char` drops a byte past the buffer limit.
                if pair.out.reserve(1) == 1 {
                    pair.out.commit(1, &c);
                }
                Ok(0)
            }
            TIOCEXCL | TIOCNXCL => {
                pair.st.lock().excl = cmd == TIOCEXCL;
                Ok(0)
            }
            TIOCGEXCL => {
                let v = pair.st.lock().excl as i32;
                put(mem, arg, &v.to_le_bytes())
            }
            TIOCGETD => put(mem, arg, &0i32.to_le_bytes()),
            TIOCSETD => {
                if get_i32(mem, arg)? == 0 { Ok(0) } else { Err(EINVAL) }
            }
            TIOCVHANGUP => {
                if !privileged {
                    return Err(EPERM);
                }
                pair.st.lock().master_hung = true;
                pair.wake_all();
                Ok(0)
            }
            TIOCGDEV => put(mem, arg, &pair.tty.dev().to_le_bytes()),
            TIOCSBRK | TIOCCBRK | TCSBRK | TCSBRKP => Ok(0),
            // Nobody has the master as controlling terminal.
            TIOCNOTTY => Err(ENOTTY),
            // `tiocgpgrp`/`tiocgsid` with tty != real_tty: no controlling-terminal check.
            TIOCGPGRP => put(mem, arg, &(pair.tty.with_ctl(p, |c, _| c.pgrp).unwrap_or(0) as i32).to_le_bytes()),
            TIOCGSID => match pair.tty.with_ctl(p, |c, _| c.session) {
                Some(s) => put(mem, arg, &(s as i32).to_le_bytes()),
                None => Err(ENOTTY),
            },
            TIOCINQ => {
                let v = pair.out.buf.lock().len() as u32;
                put(mem, arg, &v.to_le_bytes())
            }
            TIOCOUTQ => put(mem, arg, &0i32.to_le_bytes()),
            TCXONC => {
                if arg <= TCION { Ok(0) } else { Err(EINVAL) }
            }
            TCFLSH => match arg {
                TCIFLUSH | TCIOFLUSH => {
                    pair.out.buf.lock().clear();
                    pair.out.room.wake_all();
                    Ok(0)
                }
                TCOFLUSH => Ok(0),
                _ => Err(EINVAL),
            },
            // On the slave (`real_tty`): window size (SIGWINCH to its foreground group), TIOCSCTTY, TIOCSPGRP and
            // the termios calls.
            TIOCGWINSZ | TIOCSWINSZ | TIOCSCTTY | TIOCSPGRP | TCGETS | TCGETS2 | TCGETA | TCSETS | TCSETSW | TCSETSF | TCSETS2
            | TCSETSW2 | TCSETSF2 | TCSETA | TCSETAW | TCSETAF | TIOCGSOFTCAR | TIOCSSOFTCAR => {
                let r = ttyioctl::ioctl(&pair.tty, cmd, arg, mem, p);
                if sets_termios(cmd) {
                    pair.feed();
                }
                r
            }
            _ => Err(ENOTTY),
        }
    }
}

impl Drop for MasterFile {
    fn drop(&mut self) {
        self.pair.master_closed();
    }
}

impl File for MasterFile {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_stream(buf, false)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.write_stream(buf, false)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.read(buf, nonblock, &ended)
    }
    fn write_stream(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        self.write(buf, nonblock, &ended)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o666
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(PTMX_DEV.0, PTMX_DEV.1)
    }
    fn events(&self) -> Events {
        self.poll_events()
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.pair.out.rq)
    }
    fn poll_wait(&self, add: &mut dyn FnMut(&PollQueue)) {
        add(&self.pair.out.rq);
        add(&self.pair.in_room);
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        Some(MasterFile::ioctl(self, cmd, arg, mem, jobs::procs()))
    }
    fn ioctl(&self, cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        let m = crate::RecordMem::new(arg, crate::TTY_RECORD_LIMIT);
        MasterFile::ioctl(self, cmd, arg, &m, jobs::procs())?;
        Ok(m.take())
    }
}

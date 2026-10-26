//! The pty slave (IN-7): the `/dev/pts/N` node and its open ends, IN-4's line discipline with job control, the
//! hang-up rules of `tty_vhangup` (linux-6.6/drivers/tty/tty_io.c:577). See `pty` for the whole picture.

use crate::jobs::{self, Procs};
use crate::pty::{Pair, PTS_MAJOR, SLAVE_PERM, SLICE_NS, hung_mask, ended, sets_termios, session_has_tty, live_pairs};
use crate::termios::ioc::*;
use crate::tty::Tty;
use crate::ttyioctl;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::poll::PollQueue;
use vfs::sys::UserMem;
use vfs::{Errno, Events, File};

const O_ACCMODE: u64 = 0o3;
const O_RDONLY: u64 = 0o0;
const O_RDWR: u64 = 0o2;
const O_NOCTTY: u64 = 0o400;
/// The `/dev/pts/N` node: opens a slave end while the master is open and unlocked.
pub struct PtsNode {
    pair: Weak<Pair>,
    index: u32,
}

impl PtsNode {
    pub(crate) fn new(pair: &Arc<Pair>) -> PtsNode {
        PtsNode { pair: Arc::downgrade(pair), index: pair.index }
    }

    pub(crate) fn open(&self, flags: u64) -> Result<Arc<SlaveFile>, Errno> {
        // `pts_unix98_lookup` (no pair: EIO) and `pty_open` (master closing or slave locked: EIO).
        let Some(pair) = self.pair.upgrade() else { return Err(EIO) };
        let epoch = {
            let mut s = pair.st.lock();
            if !s.master_open || s.locked {
                return Err(EIO);
            }
            s.slaves += 1;
            s.slave_closed = false;
            s.hangups
        };
        let f = Arc::new(SlaveFile { pair, epoch });
        // Only an open with read access (`FMODE_READ`, tty_jobctrl.c:153): O_RDONLY or O_RDWR, not O_WRONLY and
        // not the access mode 3 (OPEN_FMODE gives it neither read nor write).
        if flags & O_NOCTTY == 0 && matches!(flags & O_ACCMODE, O_RDONLY | O_RDWR) {
            open_sets_ctty(&f.pair.tty);
        }
        Ok(f)
    }
}

/// `tty_open_proc_set_tty`: a session leader without a controlling terminal opening the slave (no O_NOCTTY, with
/// read access) gets it, if no other session holds it.
fn open_sets_ctty(tty: &Tty) {
    let p = jobs::procs();
    let Some(me) = p.current() else { return };
    if !me.leader() || session_has_tty(me.sid, tty, p) {
        return;
    }
    let got = tty.with_ctl(p, |ctl, _| {
        if ctl.session.is_none() {
            ctl.assign(me.sid, me.pgid);
            true
        } else {
            false
        }
    });
    if got {
        klog::event!(Debug, "console", "pty.ctty", session = me.sid, pgrp = me.pgid; "the slave became the session's controlling terminal on open");
    }
}

impl File for PtsNode {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EIO)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EIO)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | SLAVE_PERM
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(PTS_MAJOR, self.index)
    }
    fn open_node(&self, flags: u64, _who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        Some(self.open(flags).map(|f| f as Arc<dyn File>))
    }
}

/// An open slave end.
pub struct SlaveFile {
    pair: Arc<Pair>,
    /// The pair's hang-up count at the open: a later hang-up hangs this file up.
    epoch: u64,
}

impl SlaveFile {
    pub fn pair(&self) -> &Arc<Pair> {
        &self.pair
    }

    fn hung(&self) -> bool {
        self.pair.st.lock().hangups != self.epoch
    }

    /// `read(2)` on the slave: the line discipline with job control; 0 once hung up (EIO for a read blocked when
    /// the hang-up came, as n_tty's `TTY_OTHER_CLOSED` check).
    pub fn read(&self, buf: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        if self.hung() {
            return Ok(0);
        }
        let stop = || abort() || self.hung();
        let r = self.pair.tty.read(buf, nonblock, &stop, true);
        self.pair.feed();
        match r {
            Err(_) if self.hung() => Err(EIO),
            r => r,
        }
    }

    /// `write(2)` on the slave: OPOST, then into the master's queue, waiting for the master to read when full.
    pub fn write(&self, data: &[u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        if self.hung() {
            return Err(EIO);
        }
        let stop = || abort() || self.hung();
        let out = &self.pair.out;
        let mut done = 0;
        while done < data.len() {
            // OPOST can make one byte up to `grow` bytes; the piece is sized so its output fits the reservation.
            let grow = self.pair.tty.output_growth();
            let left = data.len() - done;
            let room = out.reserve(left.min(crate::WRITE_PIECE).saturating_mul(grow));
            if room == 0 {
                if done > 0 {
                    break;
                }
                if nonblock {
                    return Err(EAGAIN);
                }
                if stop() {
                    return Err(if self.hung() { EIO } else { EINTR });
                }
                out.room.wait_until_timeout(|| out.room_left() > 0 || self.hung(), SLICE_NS);
                continue;
            }
            let n = (room / grow).clamp(1, left.min(crate::WRITE_PIECE));
            let bytes = match self.pair.tty.write(&data[done..done + n], nonblock, &stop, done == 0) {
                Ok(b) => b,
                Err(e) => {
                    out.commit(room, &[]);
                    if self.hung() {
                        return if done > 0 { Ok(done) } else { Err(EIO) };
                    }
                    if done == 0 {
                        return Err(e);
                    }
                    break;
                }
            };
            out.commit(room, &bytes);
            done += n;
        }
        Ok(done)
    }

    pub fn poll_events(&self) -> Events {
        if self.hung() {
            return Events(hung_mask());
        }
        let mut m = 0;
        if self.pair.tty.has_input() {
            m |= Events::IN | Events::RDNORM;
        }
        if !self.pair.tty.output_stopped() && self.pair.out.room_left() > 0 {
            m |= Events::OUT | Events::WRNORM;
        }
        Events(m)
    }

    /// The slave's ioctls: IN-4's terminal ioctls on its line discipline (`ttyioctl`), with the pty's own
    /// TIOCVHANGUP and the flip buffer's part of TCFLSH.
    pub fn ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem, p: &dyn Procs) -> Result<i64, Errno> {
        if self.hung() {
            return Err(if cmd == TIOCSPGRP { ENOTTY } else { EIO });
        }
        let pair = &self.pair;
        match cmd {
            TIOCVHANGUP => {
                if !p.current().is_some_and(|m| m.privileged()) {
                    return Err(EPERM);
                }
                pair.hangup_slave("TIOCVHANGUP");
                Ok(0)
            }
            // Only a master has a peer to open (linux-6.6/drivers/tty/pty.c:609: `tty->driver != ptm_driver`).
            TIOCGPTPEER => Err(EIO),
            _ => {
                if cmd == TCFLSH && (arg == TCIFLUSH || arg == TCIOFLUSH) {
                    // `tty_buffer_flush` before n_tty's part (tty_io.c TCFLSH).
                    pair.flush_pending();
                }
                let r = ttyioctl::ioctl(&pair.tty, cmd, arg, mem, p);
                if sets_termios(cmd) {
                    pair.feed();
                }
                r
            }
        }
    }
}

impl Drop for SlaveFile {
    fn drop(&mut self) {
        self.pair.slave_closed();
    }
}

impl File for SlaveFile {
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
        uapi::S_IFCHR | SLAVE_PERM
    }
    fn rdev(&self) -> u64 {
        self.pair.rdev()
    }
    fn events(&self) -> Events {
        self.poll_events()
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(self.pair.tty.poll_queue())
    }
    fn poll_wait(&self, add: &mut dyn FnMut(&PollQueue)) {
        add(self.pair.tty.poll_queue());
        add(&self.pair.out.room);
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        Some(SlaveFile::ioctl(self, cmd, arg, mem, jobs::procs()))
    }
    fn ioctl(&self, cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        let m = crate::RecordMem::new(arg, crate::TTY_RECORD_LIMIT);
        SlaveFile::ioctl(self, cmd, arg, &m, jobs::procs())?;
        Ok(m.take())
    }
}

/// `/dev/tty` for a caller whose session holds a pty (`tty_open_current_tty`): a new slave end of that pty
/// (O_NOCTTY implied). None: the caller's controlling terminal is not a pty (FS's `/dev/tty` answers as before).
/// For FS's `/dev/tty` open (change request IN-7 CR1).
pub fn open_ctty(flags: u64) -> Option<Result<Arc<dyn File>, Errno>> {
    let me = jobs::procs().current()?;
    let pair = live_pairs().into_iter().find(|p| p.tty.ctl().session == Some(me.sid))?;
    let node = PtsNode { pair: Arc::downgrade(&pair), index: pair.index };
    Some(node.open(flags | O_NOCTTY).map(|f| f as Arc<dyn File>))
}

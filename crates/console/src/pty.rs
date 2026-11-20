//! Pseudo-terminals (IN-7): `/dev/ptmx` and the slave nodes `/dev/pts/N`, after linux-7.3.1/drivers/tty/pty.c
//! (`ptmx_open` :767, `pty_open` :219, `pty_close` :47, `pty_unix98_ioctl` :625, `pty_set_termios` :241) and
//! fs/devpts/inode.c (`devpts_new_index` :466, `devpts_pty_new` :498, `devpts_pty_kill` :553).
//!
//! Each open of `/dev/ptmx` makes a `Pair`: the slave is IN-4's line discipline (`Tty` with a `Port`), the master
//! reads what the slave wrote (after OPOST) plus the slave's echo, and writes the slave's input. As in Linux the
//! master's writes wait in a bounded buffer (the flip buffer, 8 KiB, pty.c:384) until the line discipline has
//! room, so nothing typed is dropped; the slave's output waits for the master to read once the master's queue
//! (4 KiB read buffer + 8 KiB flip buffer) is full.
//!
//! Closing the master hangs the slave up (`tty_vhangup`, tty_io.c:577): the node goes away, open slave files read
//! 0 (a read blocked at that moment ends with EIO), writes and ioctls answer EIO, poll says IN|OUT|ERR|HUP; the
//! terminal loses its session and the session leader gets SIGHUP and SIGCONT (`tty_signal_session_leader`).
//! Closing the last slave file makes the master's reads end with EIO once drained and its poll say HUP, until a
//! slave is opened again.
//!
//! Job control: a session leader without a controlling terminal that opens a slave without O_NOCTTY gets it as its
//! controlling terminal (`tty_open_proc_set_tty`); a session holds at most one terminal (`session_has_tty`).
//! Packet mode (TIOCPKT) is not built: switching it on answers ENOTTY (V2-LANES IN-7: packet mode off).
//!
//! devpts instances (IN-PTY, fs/devpts/inode.c `devpts_acquire` :181, `devpts_pty_new` :544): a ptmx opened by
//! name makes its pty in the devpts instance it belongs to (`vfs::mount_fs::devpts_for_ptmx`: its own on devpts,
//! else the one mounted on `pts` next to it). Each instance has its own numbers from 0, its slave nodes appear in
//! its own root only (owner and mode from its `uid=`, `gid=`, `mode=`; `max=` bounds its numbers), and a slave
//! is opened only through its own instance's node. The boot instance (`/dev/pts`, IN-7 CR3's devpts-named tmpfs)
//! is the one it always was: nodes by path in the boot view, owner the opener, group tty, 0600.
//!
//! Locks: `console.ptys` (the index map) and `console.pts_instances` and each pair's `pty.state`, `pty.pending`, `pty.to_master` are leaves;
//! the line discipline's lock (`tty.state`) is never held while one of them is taken except `pty.to_master` (the
//! echo, `Port::echo`). Process hooks (signals) run with none of them held.

use crate::jobs::{self, SIGCONT, SIGHUP};
use crate::termios::{Termios, ioc::*};
use crate::tty::{Port, Tty};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ksync::SpinLock;
use uapi::errno::*;
use vfs::poll::PollQueue;
use vfs::sys::UserMem;
use vfs::{Errno, Events, File};

/// UNIX98_PTY_SLAVE_MAJOR: `/dev/pts/N` is 136:N (`MKDEV(136, 0) + index`, minors up to 2^20).
pub const PTS_MAJOR: u32 = 136;
/// TTYAUX_MAJOR 5, minor 2: `/dev/ptmx`.
pub const PTMX_DEV: (u32, u32) = (5, 2);
/// Group `tty`: the slave nodes' group, as CachyOS mounts devpts (`gid=5,mode=600,ptmxmode=000`).
pub const TTY_GID: u32 = 5;
/// The slave nodes' permissions (CachyOS's devpts `mode=600`; the oracle of the ABI diff).
pub const SLAVE_PERM: u32 = 0o600;
/// Master to slave: what the line discipline has no room for yet (`tty_buffer_set_limit(port, 8192)`, pty.c:384).
pub const TO_SLAVE_MAX: usize = 8192;
/// Slave to master: the master's N_TTY read buffer plus its flip buffer.
pub const TO_MASTER_MAX: usize = crate::ldisc::BUF_SIZE + 8192;
/// Echoes may use this much more than `TO_MASTER_MAX` (n_tty's echo buffer, which keeps echoes the master has no
/// room for; beyond it n_tty discards them, linux-7.3.1/drivers/tty/n_tty.c:711). Slave output never uses it.
pub const ECHO_ROOM: usize = crate::ldisc::BUF_SIZE;
/// The slave-to-master queue's length from the start: its bound, the echo's room, and room for the characters
/// by which concurrent `commit`s exceed their reservations. Both of a pty's queues are made whole with the pty
/// and never grow under their spin locks, where the heap can neither reclaim nor fall back to single frames
/// (a large block there brought the kernel down on the Mini-PC, 11.11.2026; see net's `sock::unix::fill`).
const TO_MASTER_CAP: usize = TO_MASTER_MAX + ECHO_ROOM + 4096;
/// How long a blocked read or write sleeps before it asks whether its program is being ended (B4).
pub(crate) const SLICE_NS: u64 = 50_000_000;
/// Bytes handed to the line discipline per step (its lock is held for one step).
const FEED_PIECE: usize = 512;
pub(crate) const SIG_MAX: u64 = 64;

/// `new_encode_dev` (include/linux/kdev_t.h): TIOCGDEV's 32-bit device number.
pub fn new_encode_dev(major: u32, minor: u32) -> u32 {
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

/// The live pairs by (instance, index); an entry exists from the master's open until the pair's last end is gone
/// (Linux frees the index in `pty_unix98_remove`, when both ends are released).
static PTYS: SpinLock<BTreeMap<(u64, u32), Weak<Pair>>> = SpinLock::named("console.ptys", BTreeMap::new());

/// `devpts_new_index`: the instance's lowest free index (`ida_alloc_max(.., max - 1)`; without `max=` no limit
/// apart from memory, R1). ENOSPC at `max`. Reserved with an empty entry.
fn reserve_index(inst: &Instance) -> Result<u32, Errno> {
    let mut m = PTYS.lock();
    let mut i = 0u32;
    for (&(_, k), _) in m.range((inst.id, 0)..=(inst.id, u32::MAX)) {
        if k != i {
            break;
        }
        i += 1;
    }
    if inst.max.is_some_and(|max| u64::from(i) >= max) {
        return Err(ENOSPC);
    }
    m.insert((inst.id, i), Weak::new());
    Ok(i)
}

/// A devpts instance as the pty layer sees it (Linux's `pts_fs_info`): its numbers' key in `PTYS`, where its
/// slave nodes are made and how they look. Id 0 is the boot instance (`BOOT`), whose nodes go by path.
pub struct Instance {
    id: u64,
    sb: Weak<vfs::SuperBlock>,
    root: Weak<vfs::Dentry>,
    uid: Option<u32>,
    gid: Option<u32>,
    mode: u32,
    max: Option<u64>,
}

/// The boot instance: `/dev/pts` of the boot view, nodes owned by the opener, group tty, `SLAVE_PERM`.
static BOOT: Instance = Instance { id: 0, sb: Weak::new(), root: Weak::new(), uid: None, gid: Some(TTY_GID), mode: SLAVE_PERM, max: None };
/// The mounted instances by super block. An entry goes when its super block is gone (pruned at the next mount
/// or lookup); pairs keep their instance as long as they live.
static INSTANCES: SpinLock<Vec<Arc<Instance>>> = SpinLock::named("console.pts_instances", Vec::new());
static NEXT_INSTANCE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// The instance of devpts super block `sb`: a mounted one's, or the boot one when `sb` is the boot view's
/// `/dev/pts`. ENODEV for any other (a devpts the pty layer did not make, e.g. without a `ptmx`).
fn instance_of(sb: &Arc<vfs::SuperBlock>) -> Result<Option<Arc<Instance>>, Errno> {
    let found = {
        let mut l = INSTANCES.lock();
        l.retain(|i| i.sb.strong_count() > 0);
        l.iter().find(|i| core::ptr::eq(i.sb.as_ptr(), Arc::as_ptr(sb))).cloned()
    };
    if found.is_some() {
        return Ok(found);
    }
    match vfs::root_ns().lookup_path(b"/dev/pts", true) {
        Ok(p) if Arc::ptr_eq(&p.mnt.sb, sb) => Ok(None),
        _ => Err(ENODEV),
    }
}

/// The `ptmx` node's object of a new devpts instance (`mknod_ptmx`): it is registered by its super block, and
/// a ptmx opened at a place that belongs to it makes its ptys there.
pub fn instance_ptmx(s: vfs::mount_fs::PtsSuper) -> Arc<dyn File> {
    let id = NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
    let inst = Arc::new(Instance { id, sb: s.sb, root: s.root, uid: s.uid, gid: s.gid, mode: s.mode & 0o777, max: s.max });
    {
        let mut l = INSTANCES.lock();
        l.retain(|i| i.sb.strong_count() > 0);
        l.push(inst);
    }
    klog::event!(Debug, "console", "pty.instance", id = id, mode = s.mode, ptmxmode = s.ptmxmode, max = s.max.unwrap_or(0));
    Arc::new(PtmxDev { perm: s.ptmxmode & 0o777 })
}

/// Every live pair (taken out of the map's lock, so dropping them never runs under it).
pub(crate) fn live_pairs() -> Vec<Arc<Pair>> {
    let m = PTYS.lock();
    m.values().filter_map(Weak::upgrade).collect()
}

/// How many pairs exist (tests, the monitor).
pub fn count() -> usize {
    live_pairs().len()
}

/// The numbers instance `id` holds in `PTYS` (reserved or live): the host tests check that none stays behind.
#[cfg(test)]
pub(crate) fn numbers_in(id: u64) -> usize {
    PTYS.lock().range((id, 0)..=(id, u32::MAX)).count()
}

/// Session `sid` holds a terminal other than `except` as its controlling terminal (Linux's
/// `current->signal->tty != NULL`; BitsOS keeps the terminal's session, so it asks every terminal).
pub fn session_has_tty(sid: u32, except: &Tty, p: &dyn jobs::Procs) -> bool {
    let holds = |t: &Tty| !core::ptr::eq(t, except) && t.with_ctl(p, |c, _| c.session) == Some(sid);
    holds(crate::tty()) || live_pairs().iter().any(|pair| holds(&pair.tty))
}

/// What the slave wrote and echoed, for the master's reads.
pub(crate) struct ToMaster {
    pub(crate) buf: SpinLock<VecDeque<u8>>,
    /// Master readers and poll (IN, HUP).
    pub(crate) rq: PollQueue,
    /// Slave writers waiting for room; the slave's poll OUT.
    pub(crate) room: PollQueue,
    /// Room slave writers reserved and have not filled yet; changed only under `buf`'s lock.
    reserved: AtomicUsize,
    dev: u32,
}

impl ToMaster {
    /// Room for slave output (reservations count as used).
    pub(crate) fn room_left(&self) -> usize {
        let b = self.buf.lock();
        TO_MASTER_MAX.saturating_sub(b.len() + self.reserved.load(Ordering::Relaxed))
    }

    /// Reserves up to `want` bytes of room for slave output; 0 when full. Taken under the queue's lock, so writers
    /// on several threads never together pass `TO_MASTER_MAX` (by more than the last character's expansion).
    pub(crate) fn reserve(&self, want: usize) -> usize {
        let b = self.buf.lock();
        let r = self.reserved.load(Ordering::Relaxed);
        let n = TO_MASTER_MAX.saturating_sub(b.len() + r).min(want);
        self.reserved.store(r + n, Ordering::Relaxed);
        n
    }

    /// Fills a reservation of `reserved` bytes with `bytes` (the caller sized its piece so that `bytes` exceeds the
    /// reservation by less than one character's expansion) and gives back what was not used.
    pub(crate) fn commit(&self, reserved: usize, bytes: &[u8]) {
        {
            let mut b = self.buf.lock();
            b.extend(bytes.iter().copied());
            let r = self.reserved.load(Ordering::Relaxed);
            self.reserved.store(r.saturating_sub(reserved), Ordering::Relaxed);
        }
        if !bytes.is_empty() {
            self.rq.wake_all();
        }
        if bytes.len() < reserved {
            self.room.wake_all();
        }
    }
}

impl Port for ToMaster {
    /// The slave's echo goes to the master like its output, with `ECHO_ROOM` more room; when the master does not
    /// read even that, the echo is dropped and master writes go on, as in Linux (n_tty.c:736 discards echoes; a
    /// master writer never waits for its own unread echo). Measured on CachyOS: 256000 bytes written into a cooked
    /// slave whose reader drains it, master never read: all 256000 reach the slave, about 19.5 KB of echo remain.
    fn echo(&self, bytes: &[u8]) {
        let kept = {
            let mut b = self.buf.lock();
            let room = (TO_MASTER_MAX + ECHO_ROOM).saturating_sub(b.len() + self.reserved.load(Ordering::Relaxed));
            let n = bytes.len().min(room);
            b.extend(bytes[..n].iter().copied());
            n
        };
        if kept < bytes.len() {
            klog::event!(Debug, "console", "pty.echo_dropped", bytes = bytes.len() - kept; "the master does not read: echo dropped");
        }
        if kept > 0 {
            self.rq.wake_all();
        }
    }
    fn dev(&self) -> u32 {
        self.dev
    }
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) master_open: bool,
    /// The master was hung up by TIOCVHANGUP: its file reads 0, the rest EIO.
    pub(crate) master_hung: bool,
    /// Slave files open now (hung ones included until closed).
    pub(crate) slaves: usize,
    /// `TTY_OTHER_CLOSED` on the master: the last slave file was closed.
    pub(crate) slave_closed: bool,
    /// `TTY_PTY_LOCK`: set at the master's open, TIOCSPTLCK clears it (unlockpt).
    pub(crate) locked: bool,
    /// Hang-ups of the slave so far: a slave file opened before the latest one is hung up.
    pub(crate) hangups: u64,
    pub(crate) excl: bool,
}

/// One pseudo-terminal: the slave's line discipline and both directions' buffers.
pub struct Pair {
    pub(crate) index: u32,
    pub(crate) tty: Tty,
    pub(crate) out: Arc<ToMaster>,
    /// Master writes the line discipline did not take yet, and a flush count (a feeder whose bytes were flushed
    /// meanwhile does not take them again).
    pending: SpinLock<(VecDeque<u8>, u64)>,
    /// Master writers waiting for room in `pending`; the master's poll OUT.
    pub(crate) in_room: PollQueue,
    pub(crate) st: SpinLock<State>,
    /// One feeder at a time hands `pending` to the line discipline; `again` asks it for another pass.
    feeding: AtomicBool,
    again: AtomicBool,
    /// The `/dev/pts/N` node's object (removed when the master closes).
    node: SpinLock<Option<Arc<dyn File>>>,
    /// The devpts instance the pty belongs to (None: the boot instance).
    inst: Option<Arc<Instance>>,
    /// The master file's address (`peer_path` recognises it).
    pub(crate) master_addr: AtomicUsize,
}

impl Pair {
    fn new(inst: Option<Arc<Instance>>) -> Result<Arc<Pair>, Errno> {
        let index = reserve_index(inst.as_deref().unwrap_or(&BOOT))?;
        let out = Arc::new(ToMaster { buf: SpinLock::named("pty.to_master", VecDeque::with_capacity(TO_MASTER_CAP)), rq: PollQueue::new(), room: PollQueue::new(), reserved: AtomicUsize::new(0), dev: new_encode_dev(PTS_MAJOR, index) });
        let pair = Arc::new(Pair {
            index,
            tty: Tty::with_port(Termios::pty(), out.clone()),
            out,
            pending: SpinLock::named("pty.pending", (VecDeque::with_capacity(TO_SLAVE_MAX), 0)),
            in_room: PollQueue::new(),
            st: SpinLock::named("pty.state", State { master_open: true, locked: true, ..State::default() }),
            feeding: AtomicBool::new(false),
            again: AtomicBool::new(false),
            node: SpinLock::named("pty.node", None),
            inst,
            master_addr: AtomicUsize::new(0),
        });
        PTYS.lock().insert((pair.inst().id, index), Arc::downgrade(&pair));
        Ok(pair)
    }

    fn inst(&self) -> &Instance {
        self.inst.as_deref().unwrap_or(&BOOT)
    }

    /// The slave node's permissions in its instance (`mode=`).
    pub(crate) fn slave_perm(&self) -> u32 {
        self.inst().mode
    }

    /// Whether `f` is this pair's master file.
    pub fn is_master(&self, f: &dyn File) -> bool {
        self.master_addr() == f as *const dyn File as *const u8 as usize
    }

    /// The devpts instance's id (0: the boot instance); tests and events.
    pub fn instance_id(&self) -> u64 {
        self.inst().id
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// The slave's line discipline.
    pub fn tty(&self) -> &Tty {
        &self.tty
    }

    pub fn path(&self) -> String {
        format!("/dev/pts/{}", self.index)
    }

    pub(crate) fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(PTS_MAJOR, self.index)
    }

    fn master_addr(&self) -> usize {
        self.master_addr.load(Ordering::Acquire)
    }

    /// Hands what the master wrote to the line discipline while it has room; the echo goes to the master.
    pub fn feed(&self) {
        self.again.store(true, Ordering::SeqCst);
        loop {
            if self.feeding.swap(true, Ordering::SeqCst) {
                return; // the feeder at work sees `again`
            }
            while self.again.swap(false, Ordering::SeqCst) {
                loop {
                    let (chunk, flushes): (Vec<u8>, u64) = {
                        let p = self.pending.lock();
                        (p.0.iter().take(FEED_PIECE).copied().collect(), p.1)
                    };
                    if chunk.is_empty() {
                        break;
                    }
                    let n = self.tty.input_flow(&chunk, false, &mut |e| self.out.echo(e));
                    if n > 0 {
                        {
                            let mut p = self.pending.lock();
                            if p.1 == flushes {
                                let n = n.min(p.0.len());
                                p.0.drain(..n);
                            }
                        }
                        self.in_room.wake_all();
                    }
                    if n < chunk.len() {
                        break;
                    }
                }
            }
            self.feeding.store(false, Ordering::SeqCst);
            if !self.again.load(Ordering::SeqCst) {
                return;
            }
        }
    }

    pub(crate) fn flush_pending(&self) {
        {
            let mut p = self.pending.lock();
            p.0.clear();
            p.1 += 1;
        }
        self.in_room.wake_all();
    }

    /// Takes as many master bytes for the slave as `pending` has room for (room check and append under one lock,
    /// so writers on several threads never pass `TO_SLAVE_MAX`); returns how many it took.
    pub(crate) fn push_pending(&self, bytes: &[u8]) -> usize {
        let mut p = self.pending.lock();
        let n = TO_SLAVE_MAX.saturating_sub(p.0.len()).min(bytes.len());
        p.0.extend(bytes[..n].iter().copied());
        n
    }

    pub(crate) fn pending_room(&self) -> usize {
        TO_SLAVE_MAX.saturating_sub(self.pending.lock().0.len())
    }

    /// `__tty_hangup` of the slave: open slave files are hung up, input is flushed, the terminal loses its session
    /// and the session leader gets SIGHUP and SIGCONT (`tty_signal_session_leader`, tty_jobctrl.c:196).
    pub(crate) fn hangup_slave(&self, why: &'static str) {
        self.st.lock().hangups += 1;
        self.flush_pending();
        self.tty.flush();
        let p = jobs::procs();
        let (sid, pgrp) = self.tty.with_ctl(p, |ctl, _| {
            let was = (ctl.session, ctl.pgrp);
            *ctl = jobs::Ctl::default();
            was
        });
        if let Some(sid) = sid {
            let hup = p.signal_proc(sid, SIGHUP);
            p.signal_proc(sid, SIGCONT);
            klog::event!(Info, "console", "pty.hangup", index = self.index, why = why, session = sid, pgrp = pgrp.unwrap_or(0), leader_signalled = hup;
                "the terminal hung up: its session leader got SIGHUP and SIGCONT");
        } else {
            klog::event!(Debug, "console", "pty.hangup", index = self.index, why = why);
        }
        self.wake_all();
    }

    pub(crate) fn wake_all(&self) {
        self.tty.wake_all();
        self.out.rq.wake_all();
        self.out.room.wake_all();
        self.in_room.wake_all();
    }

    /// The master's last close (`pty_close` of the master): the slave hangs up, `/dev/pts/N` goes away
    /// (`devpts_pty_kill`).
    pub(crate) fn master_closed(&self) {
        self.st.lock().master_open = false;
        // The node goes first, then the hang-up (`pty_close`: `devpts_pty_kill` before `tty_vhangup`): a woken
        // session leader finds no node any more.
        let node = self.node.lock().take();
        if let Some(node) = node {
            match &self.inst {
                None => remove_node(&self.path(), &node),
                Some(inst) => remove_node_in(inst, self.index, &node),
            }
        }
        self.hangup_slave("master closed");
        klog::event!(Debug, "console", "pty.master_closed", index = self.index);
    }

    /// The slave's last close (`pty_close` of the slave): the master's reads end with EIO once drained.
    pub(crate) fn slave_closed(&self) {
        let last = {
            let mut s = self.st.lock();
            s.slaves = s.slaves.saturating_sub(1);
            if s.slaves == 0 {
                s.slave_closed = true;
            }
            s.slaves == 0
        };
        if last {
            klog::event!(Debug, "console", "pty.slave_closed", index = self.index);
            self.out.rq.wake_all();
            self.in_room.wake_all();
        }
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        PTYS.lock().remove(&(self.inst().id, self.index));
    }
}

/// Removes `path` if it is still the pair's node (root may have replaced it meanwhile).
fn remove_node(path: &str, node: &Arc<dyn File>) {
    let ns = vfs::root_ns();
    let ours = match ns.lookup(path) {
        Ok(d) => matches!(&d.inode.data, vfs::inode::Data::Device(f) if Arc::ptr_eq(f, node)),
        Err(_) => false,
    };
    if !ours {
        klog::event!(Debug, "console", "pty.node_gone", path = path; "the slave node was removed or replaced already");
        return;
    }
    if let Err(e) = ns.remove(path) {
        klog::event!(Warn, "console", "pty.node_remove_failed", path = path, errno = e);
    }
}

/// `devpts_pty_new` in a mounted instance: the node `N` in its root, owner and mode from its options (else the
/// opener's fs ids), made under the instance's name lock. ENODEV once the instance is unmounted and gone.
fn make_node_in(inst: &Instance, index: u32, node: Arc<dyn File>, who: vfs::inode::Cred) -> Result<(), Errno> {
    let root = inst.root.upgrade().ok_or(ENODEV)?;
    let name = format!("{index}");
    let cred = vfs::inode::Cred { uid: inst.uid.unwrap_or(who.uid), gid: inst.gid.unwrap_or(who.gid) };
    let _g = root.sb.lock_names();
    if root.child(name.as_bytes()).is_some() {
        return Err(EEXIST);
    }
    root.create_child(name.as_bytes(), uapi::S_IFCHR | inst.mode, cred, vfs::inode::Data::Device(node)).map(|_| ())
}

/// `devpts_pty_kill` in a mounted instance: the node goes if it is still the pair's (an unmounted instance's tree
/// may be gone already: nothing to do then).
fn remove_node_in(inst: &Instance, index: u32, node: &Arc<dyn File>) {
    let Some(root) = inst.root.upgrade() else {
        klog::event!(Debug, "console", "pty.node_gone", instance = inst.id, index = index; "the devpts instance is unmounted: no node to remove");
        return;
    };
    let name = format!("{index}");
    let _g = root.sb.lock_names();
    let ours = root.child(name.as_bytes()).is_some_and(|d| matches!(&d.inode.data, vfs::inode::Data::Device(f) if Arc::ptr_eq(f, node)));
    if !ours {
        klog::event!(Debug, "console", "pty.node_gone", instance = inst.id, index = index; "the slave node was removed or replaced already");
        return;
    }
    if let Some(v) = root.detach(name.as_bytes()) {
        v.inode.drop_link();
        v.inode.meta.touch_ctime();
        root.inode.meta.touch_mtime();
    }
}

pub(crate) fn put(mem: &dyn UserMem, arg: u64, data: &[u8]) -> Result<i64, Errno> {
    if mem.write(arg, data) { Ok(0) } else { Err(EFAULT) }
}
pub(crate) fn get_i32(mem: &dyn UserMem, arg: u64) -> Result<i32, Errno> {
    let mut b = [0u8; 4];
    if mem.read(arg, &mut b) { Ok(i32::from_le_bytes(b)) } else { Err(EFAULT) }
}

/// A blocked pty read or write asks this between its slices.
pub(crate) fn ended() -> bool {
    vfs::dentry::interrupted() || crate::program_ended()
}

/// Calls that change the slave's termios (after them the line discipline may have room again).
pub(crate) fn sets_termios(cmd: u64) -> bool {
    matches!(cmd, TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 | TCSETA | TCSETAW | TCSETAF | TCFLSH)
}

// ---- /dev/ptmx

/// The `/dev/ptmx` node (and `/dev/pts/ptmx`, mode 0 as CachyOS's `ptmxmode=000`): every open makes a pair.
pub struct PtmxDev {
    perm: u32,
}

impl PtmxDev {
    /// `ptmx_open` in the boot instance: a new pair, its slave node `/dev/pts/N` owned by the opener and group
    /// tty, locked.
    pub fn open_master(who: vfs::inode::Cred) -> Result<Arc<crate::ptm::MasterFile>, Errno> {
        Self::open_in(None, who)
    }

    /// `ptmx_open` in devpts instance `inst` (None: the boot instance).
    pub fn open_in(inst: Option<Arc<Instance>>, who: vfs::inode::Cred) -> Result<Arc<crate::ptm::MasterFile>, Errno> {
        let pair = Pair::new(inst).inspect_err(|&e| {
            klog::event!(Info, "console", "pty.no_index", errno = e; "the devpts instance has no free number (max=): the open of ptmx fails");
        })?;
        let node: Arc<dyn File> = Arc::new(crate::pts::PtsNode::new(&pair));
        let path = pair.path();
        let made = match &pair.inst {
            None => vfs::root_ns().create_device(&path, node.clone()).map(|d| d.inode.meta.set_owner(who.uid, TTY_GID)),
            Some(inst) => make_node_in(inst, pair.index, node.clone(), who),
        };
        if let Err(e) = made {
            klog::event!(Warn, "console", "pty.node_failed", instance = pair.instance_id(), path = path.as_str(), errno = e; "no slave node: the open of ptmx fails");
            return Err(e);
        }
        *pair.node.lock() = Some(node);
        klog::event!(Debug, "console", "pty.open", instance = pair.instance_id(), index = pair.index, uid = who.uid);
        let m = Arc::new(crate::ptm::MasterFile::new(pair));
        m.pair().master_addr.store(Arc::as_ptr(&m) as *const u8 as usize, Ordering::Release);
        Ok(m)
    }
}

impl File for PtmxDev {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EIO)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EIO)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | self.perm
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(PTMX_DEV.0, PTMX_DEV.1)
    }
    /// Opened without a place (kernel users): the boot instance.
    fn open_node(&self, _flags: u64, who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        Some(PtmxDev::open_master(who).map(|m| m as Arc<dyn File>))
    }
    /// `ptmx_open` → `devpts_acquire`: the pty is made in the devpts instance the opened place belongs to.
    fn open_node_at(&self, at: &vfs::Path, _flags: u64, who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        let inst = match vfs::mount_fs::devpts_for_ptmx(at).and_then(|sb| instance_of(&sb)) {
            Ok(i) => i,
            Err(e) => {
                klog::event!(Info, "console", "pty.no_instance", errno = e; "no devpts instance belongs to this ptmx (none mounted on pts next to it): the open fails as on Linux");
                return Some(Err(e));
            }
        };
        Some(PtmxDev::open_in(inst, who).map(|m| m as Arc<dyn File>))
    }
}

pub fn make_ptmx() -> Arc<dyn File> {
    Arc::new(PtmxDev { perm: 0o666 })
}
pub fn make_pts_ptmx() -> Arc<dyn File> {
    Arc::new(PtmxDev { perm: 0 })
}

/// `hung_up_tty_poll`.
pub(crate) fn hung_mask() -> u32 {
    Events::IN | Events::OUT | Events::ERR | Events::HUP | Events::RDNORM | Events::WRNORM
}

/// TIOCGPTPEER's path for the syscall layer (change request IN-7 CR2): the slave node's name if `f` is an open
/// pty master (`ptm_open_peer` opens it with the caller's flags); None for any other file.
pub fn peer_path(f: &dyn File) -> Option<String> {
    master_pair(f).map(|p| p.path())
}

fn master_pair(f: &dyn File) -> Option<Arc<Pair>> {
    live_pairs().into_iter().find(|p| p.is_master(f))
}

/// `devpts_mntget` (linux-7.3.1/fs/devpts/inode.c:150): the mount of instance `inst` a master opened at `at`
/// reaches: up out of binds of a single file, then `at`'s own mount when it is the instance's, else the devpts
/// on `pts` next to it. ENODEV when that is not `inst`.
fn devpts_mntget(at: &vfs::Path, inst: &Instance) -> Result<Arc<vfs::Mount>, Errno> {
    let mut p = at.clone();
    while Arc::ptr_eq(&p.dentry, &p.mnt.root) {
        match p.mnt.mountpoint() {
            Some((mnt, dentry)) => p = vfs::Path { mnt, dentry },
            None => break,
        }
    }
    let ours = |m: &vfs::Mount| core::ptr::eq(inst.sb.as_ptr(), Arc::as_ptr(&m.sb));
    if p.mnt.sb.fstype == "devpts" && ours(&p.mnt) {
        return Ok(p.mnt);
    }
    match vfs::mount_fs::devpts_ptmx_path(&p) {
        Ok(pts) if ours(&pts.mnt) => Ok(pts.mnt),
        Ok(_) => Err(ENODEV),
        Err(e) => Err(e),
    }
}

/// TIOCGPTPEER (`ptm_open_peer`, linux-7.3.1/drivers/tty/pty.c:617): the slave of pty master `master`, opened with
/// `flags` in the master's own devpts instance (a boot pty by its path in the boot view; a mounted instance's
/// through the master's devpts mount, `devpts_mntget`). None: `master` is not a pty master. ENODEV: the master
/// was not opened on its instance's devpts (`devpts_mntget` finds none); EIO: the slave is locked or gone.
pub fn open_peer(master: &vfs::OpenFile, flags: u64) -> Option<Result<Arc<vfs::OpenFile>, Errno>> {
    let pair = master_pair(&*master.file)?;
    let Some(inst) = &pair.inst else {
        return Some(vfs::root_ns().open_flags(pair.path().as_bytes(), flags, 0));
    };
    Some((|| {
        let mnt = devpts_mntget(master.path.as_ref().ok_or(ENODEV)?, inst)?;
        let root = inst.root.upgrade().ok_or(EIO)?;
        let d = root.child(format!("{}", pair.index).as_bytes()).ok_or(EIO)?;
        let ours = { pair.node.lock().clone() };
        if !matches!((&d.inode.data, &ours), (vfs::inode::Data::Device(f), Some(n)) if Arc::ptr_eq(f, n)) {
            return Err(EIO);
        }
        let at = vfs::Path { mnt, dentry: d };
        let file = vfs::node::open_file_at(&at, vfs::inode::Cred::ROOT, flags, flags & 3 != 0)?;
        Ok(vfs::OpenFile::with_path(file, flags, at))
    })())
}

pub use crate::pts::open_ctty;

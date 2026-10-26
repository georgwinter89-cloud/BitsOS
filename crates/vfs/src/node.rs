//! The `File` trait (what an fd points at) and the files the VFS itself opens: regular tmpfs files,
//! directories, proc files, O_PATH handles. Devices bring their own `File` through `devnode!`.

use crate::dentry::{Dentry, SuperBlock};
use crate::inode::{Data, Inode};
use crate::poll::{Events, PollQueue, Pollable};
use crate::Errno;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::{SpinLock, WaitQueue};
use uapi::errno::*;

pub use crate::mount_ns::MountNs;

/// A file-like object behind an fd: regular data, a device, a pipe, a socket (later).
pub trait File: Send + Sync {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno>;
    fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno>;
    fn size(&self) -> u64 {
        0
    }
    /// Mode bits incl. type (S_IFREG, S_IFCHR, ...).
    fn mode(&self) -> u32;
    fn ioctl(&self, _cmd: u64, _arg: u64) -> Result<Vec<u8>, Errno> {
        Err(ENOTTY)
    }
    /// Whether lseek makes sense (regular files) or reads ignore the position (devices, pipes).
    fn seekable(&self) -> bool {
        false
    }
    fn events(&self) -> Events {
        Events(Events::IN | Events::OUT)
    }
    /// fsync/fdatasync: EINVAL for objects without one, as Linux.
    fn fsync(&self) -> Result<(), Errno> {
        Err(EINVAL)
    }
    /// read(2) on an object without a position (devices, pipes): `nonblock` is the fd's O_NONBLOCK. Objects
    /// that may wait (a terminal) wait here, in bounded slices that look at `dentry::interrupted()`.
    fn read_stream(&self, buf: &mut [u8], _nonblock: bool) -> Result<usize, Errno> {
        self.read_at(0, buf)
    }
    /// write(2) on an object without a position.
    fn write_stream(&self, buf: &[u8], _nonblock: bool) -> Result<usize, Errno> {
        self.write_at(0, buf)
    }
    /// `st_rdev` of a device node (`abi::encode_dev(major, minor)`).
    fn rdev(&self) -> u64 {
        0
    }
    /// A device's own lseek (Linux's `null_lseek`, `noop_llseek`): `Some(result)` replaces the generic one.
    fn lseek(&self, _off: i64, _whence: u32) -> Option<Result<u64, Errno>> {
        None
    }
    /// `f_op->show_fdinfo`: the object's own lines after fdinfo's pos/flags/mnt_id/ino (AB-7a CR1). Called
    /// while the reader holds its own file's position lock: read other files' positions with `pos_racy`.
    fn show_fdinfo(&self, _out: &mut alloc::string::String) {}
    /// An object without a name opened again with open(2)'s `flags` (through /proc/<pid>/fd/<n>): Linux opens
    /// its pseudo inode, which only pipes allow (`fifo_open`); everything else answers None (ENXIO there).
    /// `privileged`: the opener may exceed pipe-max-size.
    fn reopen(&self, _flags: u64, _privileged: bool) -> Option<Result<Arc<dyn File>, Errno>> {
        None
    }
    /// IN-4: an ioctl that reads its argument from the caller's memory or answers a value of its own (Linux's
    /// `unlocked_ioctl`: termios sets, EVIOCGNAME returning the bytes copied). None = not handled here, `ioctl`
    /// answers.
    fn ioctl_user(&self, _cmd: u64, _arg: u64, _mem: &dyn crate::sys::UserMem) -> Option<Result<i64, Errno>> {
        None
    }
    /// FS-3: the node's own open (Linux `f_op->open`), asked when a program opens the node by name. A FIFO
    /// makes a pipe end per open and may wait for the other side; a socket name or a device number without a
    /// driver answers ENXIO. `flags` are open(2)'s, `who` the opener (a new FIFO pipe's size). None: the node's
    /// object itself is the open file.
    fn open_node(&self, _flags: u64, _who: crate::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        None
    }
    /// FS-3: read(2)/readv(2) on the whole request at once (Linux `read_iter`): pipes need the full count
    /// (one read takes from every buffer), eventfd its total length. `status` is the open file's F_GETFL.
    /// None: the object reads through `read_stream`/`read_at` in chunks.
    fn read_iter(&self, _to: &mut dyn IoSink, _status: u64) -> Option<Result<usize, Errno>> {
        None
    }
    /// FS-3: write(2)/writev(2) on the whole request (pipes: PIPE_BUF atomicity and page merging see the
    /// full length; O_DIRECT in `status` is the packet mode). None: chunks through `write_stream`/`write_at`.
    fn write_iter(&self, _from: &mut dyn IoSource, _status: u64) -> Option<Result<usize, Errno>> {
        None
    }
    /// FS-3: type-specific fcntl commands (F_SETPIPE_SZ/F_GETPIPE_SZ on pipes; FS-10's seals). None: the
    /// object has none of them (fcntl answers as Linux does for such a file).
    fn fcntl(&self, _cmd: u64, _arg: u64, _cred: crate::inode::Cred) -> Option<Result<i64, Errno>> {
        None
    }
    /// fstat of an object without a name (pipes, eventfd): its own inode as Linux's pseudo file systems
    /// keep it. None: the generic answer (`OpenFile::stat`).
    fn attr(&self) -> Option<crate::abi::Attr> {
        None
    }
    /// FS-1b: the metadata fchmod/fchown/futimens change on an object without a name (a pipe's pipefs inode).
    /// None: the object keeps no metadata of its own (anonymous inodes); those calls succeed without effect.
    fn meta(&self) -> Option<&crate::inode::Meta> {
        None
    }
    /// FS-4 (`C: Pollable` for files): the object's main `PollQueue`, woken whenever `events()` may have
    /// changed (a pipe end: the queue of its direction). None: the object has none (see `poll_wait`).
    fn poll_head(&self) -> Option<&PollQueue> {
        None
    }
    /// FS-4: names every queue poll/select/epoll must hang on for this open object (Linux: the `poll_wait`
    /// calls of `f_op->poll`; an O_RDWR FIFO end names both of its pipe's queues). The queues must stay the
    /// same for the object's life. Default: `poll_head`.
    fn poll_wait(&self, add: &mut dyn FnMut(&PollQueue)) {
        if let Some(q) = self.poll_head() {
            add(q);
        }
    }
    /// FS-4: whether the object has a poll operation (Linux `file_can_poll`): epoll_ctl refuses the others
    /// with EPERM, and poll/select see them always ready (DEFAULT_POLLMASK, never waited for). Default: FIFOs,
    /// sockets, character devices and every object that names a `PollQueue` (anonymous objects such as a
    /// timerfd or signalfd are pollable once they have one) yes; regular files, directories, links, block
    /// devices and nameless objects without a queue say no.
    fn can_poll(&self) -> bool {
        matches!(self.mode() & uapi::S_IFMT, uapi::S_IFIFO | crate::abi::S_IFSOCK | uapi::S_IFCHR) || self.poll_head().is_some()
    }
    /// FS-4: the epoll behind an epoll file (Linux `is_file_epoll`).
    /// FS-14: the inotify instance behind an inotify fd (inotify_add_watch/rm_watch: EINVAL for any other file).
    fn as_inotify(&self) -> Option<&crate::inotify::Group> {
        None
    }
    fn as_epoll(&self) -> Option<&crate::epoll::Epoll> {
        None
    }
    /// FS-4: `events()` is already Linux's whole mask (eventfd, epoll: no RDNORM/WRNORM where Linux has none).
    /// False: the VFS adds RDNORM to IN and WRNORM to OUT (`poll::file_mask`), as most Linux files report.
    fn poll_exact(&self) -> bool {
        false
    }
    /// What `/proc/<pid>/fd/<n>` shows for an object without a path (`pipe:[ino]`, `anon_inode:[eventfd]`).
    fn link_name(&self) -> Option<String> {
        None
    }
}

/// The program's buffers a read copies into (Linux's `iov_iter` for `copy_to_iter`): one or several
/// segments, checked with `access_ok` before. A copy may stop early at a fault.
pub trait IoSink {
    /// Bytes still to fill (`iov_iter_count`).
    fn left(&self) -> usize;
    /// Copies `src` into the next bytes and advances; returns how many arrived (fewer than `src.len()`
    /// only at a fault).
    fn copy(&mut self, src: &[u8]) -> usize;
}

/// The program's buffers a write takes from (`copy_from_iter`).
pub trait IoSource {
    /// Bytes still to take (`iov_iter_count`).
    fn left(&self) -> usize;
    /// Bytes left in the current segment (`iter_iov_len`): files with a plain `write` (eventfd) see one
    /// segment per call, as Linux's `do_loop_readv_writev`.
    fn seg_left(&self) -> usize;
    /// Fills `dst` from the next bytes and advances; returns how many came (fewer only at a fault).
    fn copy(&mut self, dst: &mut [u8]) -> usize;
}

/// Kernel buffers as an `IoSink` (kernel users and host tests).
pub struct BufSink<'a> {
    pub buf: &'a mut [u8],
    pub done: usize,
}

impl<'a> BufSink<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        BufSink { buf, done: 0 }
    }
}

impl IoSink for BufSink<'_> {
    fn left(&self) -> usize {
        self.buf.len() - self.done
    }
    fn copy(&mut self, src: &[u8]) -> usize {
        let n = src.len().min(self.left());
        self.buf[self.done..self.done + n].copy_from_slice(&src[..n]);
        self.done += n;
        n
    }
}

/// Kernel bytes as an `IoSource`, one segment per slice.
pub struct BufSource<'a> {
    segs: Vec<&'a [u8]>,
    seg: usize,
    off: usize,
}

impl<'a> BufSource<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BufSource { segs: alloc::vec![data], seg: 0, off: 0 }
    }
    pub fn segments(segs: Vec<&'a [u8]>) -> Self {
        BufSource { segs, seg: 0, off: 0 }
    }
}

impl IoSource for BufSource<'_> {
    fn left(&self) -> usize {
        self.segs.iter().skip(self.seg).map(|s| s.len()).sum::<usize>() - self.off
    }
    fn seg_left(&self) -> usize {
        self.segs.get(self.seg).map_or(0, |s| s.len() - self.off)
    }
    fn copy(&mut self, dst: &mut [u8]) -> usize {
        let mut n = 0;
        while n < dst.len() && self.seg < self.segs.len() {
            let s = self.segs[self.seg];
            let k = (s.len() - self.off).min(dst.len() - n);
            dst[n..n + k].copy_from_slice(&s[self.off..self.off + k]);
            n += k;
            self.off += k;
            if self.off == s.len() {
                self.seg += 1;
                self.off = 0;
            }
        }
        n
    }
}

/// A regular file of the VFS (tmpfs pages or an external inode).
pub struct RegFile {
    pub inode: Arc<Inode>,
    pub sb: Arc<SuperBlock>,
}

impl File for RegFile {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.inode.read_at(pos, buf)
    }
    fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        crate::inode::check_limit(pos, buf.len())?;
        let n = buf.len().min((crate::abi::MAX_FILE_SIZE - pos) as usize);
        let _g = self.sb.write_begin()?;
        self.inode.write_at(pos, &buf[..n])
    }
    fn size(&self) -> u64 {
        self.inode.size()
    }
    fn mode(&self) -> u32 {
        self.inode.meta.mode()
    }
    fn seekable(&self) -> bool {
        true
    }
    /// The page cache's dirty pages go back through the file system's write-back hooks (tmpfs: none).
    fn fsync(&self) -> Result<(), Errno> {
        self.inode.sync(crate::pagecache::SyncKind::Fsync)
    }
}

/// A directory opened for reading entries (getdents64) or as a base for *at calls.
pub struct DirFile {
    pub inode: Arc<Inode>,
    /// Its file system's hooks (fsync of a directory of a disk file system makes its changes durable).
    pub ops: Option<Arc<dyn crate::fsops::FsOps>>,
}

impl File for DirFile {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EISDIR)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EISDIR)
    }
    fn mode(&self) -> u32 {
        self.inode.meta.mode()
    }
    fn seekable(&self) -> bool {
        true
    }
    fn fsync(&self) -> Result<(), Errno> {
        match &self.ops {
            Some(o) => o.sync(),
            None => Ok(()),
        }
    }
}

/// A proc file: rendered once per open, so sequential reads see one consistent text.
pub struct ProcFile {
    render: fn() -> String,
    mode: u32,
    text: SpinLock<Option<Arc<Vec<u8>>>>,
}

impl ProcFile {
    pub fn new(render: fn() -> String, mode: u32) -> Self {
        ProcFile { render, mode, text: SpinLock::named("vfs.proc.text", None) }
    }
    fn text(&self) -> Arc<Vec<u8>> {
        if let Some(t) = self.text.lock().clone() {
            return t;
        }
        // Rendered outside the lock (a renderer may take other locks or allocate a lot).
        let t = Arc::new((self.render)().into_bytes());
        self.text.lock().get_or_insert(t).clone()
    }
}

impl File for ProcFile {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let text = self.text();
        let p = pos.min(text.len() as u64) as usize;
        let n = buf.len().min(text.len() - p);
        buf[..n].copy_from_slice(&text[p..p + n]);
        Ok(n)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EACCES)
    }
    fn size(&self) -> u64 {
        0
    }
    fn mode(&self) -> u32 {
        self.mode
    }
    fn seekable(&self) -> bool {
        true
    }
}

/// An O_PATH handle (or a symlink opened with O_PATH|O_NOFOLLOW): no reads or writes.
pub struct PathFile {
    pub mode: u32,
}

impl File for PathFile {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EBADF)
    }
    fn mode(&self) -> u32 {
        self.mode
    }
}

/// The `File` of a dentry's inode as an open would make it (no permission checks: kernel use).
pub fn file_of(d: &Arc<Dentry>) -> Arc<dyn File> {
    let mode = d.inode.meta.mode();
    match &d.inode.data {
        Data::Device(f) => f.clone(),
        Data::Proc(r) => Arc::new(ProcFile::new(*r, mode)),
        Data::Synth(src) => Arc::new(crate::synth::SynthFile::new(src.clone(), crate::inode::Cred::ROOT, mode)),
        Data::Dir | Data::SynthDir(_) => Arc::new(DirFile { inode: d.inode.clone(), ops: d.sb.ops.clone() }),
        Data::Symlink(_) => Arc::new(PathFile { mode }),
        Data::Reg(_) | Data::External(_) => Arc::new(RegFile { inode: d.inode.clone(), sb: d.sb.clone() }),
    }
}

/// The `File` an open by a program gets: like `file_of`, but a synthetic file is made for `who` (what it shows
/// and whether it opens at all may depend on the caller, e.g. another process's memory map).
pub fn open_file(d: &Arc<Dentry>, who: crate::inode::Cred, write: bool) -> Result<Arc<dyn File>, Errno> {
    match &d.inode.data {
        Data::Synth(src) => {
            src.may_open(who, write)?;
            Ok(Arc::new(crate::synth::SynthFile::new(src.clone(), who, d.inode.meta.mode())))
        }
        Data::SynthDir(sd) => {
            for src in sd.sources() {
                src.may_open()?;
            }
            Ok(file_of(d))
        }
        _ => Ok(file_of(d)),
    }
}

/// `open_file` with open(2)'s flags: a node with its own open (FS-3: FIFOs, sockets, device numbers) gets
/// it here; a FIFO open may wait for the other side.
pub fn open_file_flags(d: &Arc<Dentry>, who: crate::inode::Cred, flags: u64, write: bool) -> Result<Arc<dyn File>, Errno> {
    if let Data::Device(f) = &d.inode.data
        && let Some(r) = f.open_node(flags, who)
    {
        return r;
    }
    open_file(d, who, write)
}

/// A dentry opened as a pollable `File` (contract `C: Pollable` test and kernel users).
pub struct NodeFile {
    pub node: Arc<Dentry>,
    file: Arc<dyn File>,
    waiters: WaitQueue,
}

impl NodeFile {
    pub fn new(node: Arc<Dentry>) -> Self {
        let file = file_of(&node);
        NodeFile { node, file, waiters: WaitQueue::new() }
    }
}

impl File for NodeFile {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.file.read_at(pos, buf)
    }
    fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.file.write_at(pos, buf)
    }
    fn size(&self) -> u64 {
        self.file.size()
    }
    fn mode(&self) -> u32 {
        self.file.mode()
    }
    fn ioctl(&self, cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        self.file.ioctl(cmd, arg)
    }
    fn seekable(&self) -> bool {
        self.file.seekable()
    }
    fn events(&self) -> Events {
        self.file.events()
    }
    fn fsync(&self) -> Result<(), Errno> {
        self.file.fsync()
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.file.read_stream(buf, nonblock)
    }
    fn write_stream(&self, buf: &[u8], nonblock: bool) -> Result<usize, Errno> {
        self.file.write_stream(buf, nonblock)
    }
    fn rdev(&self) -> u64 {
        self.file.rdev()
    }
    fn lseek(&self, off: i64, whence: u32) -> Option<Result<u64, Errno>> {
        self.file.lseek(off, whence)
    }
    fn read_iter(&self, to: &mut dyn IoSink, status: u64) -> Option<Result<usize, Errno>> {
        self.file.read_iter(to, status)
    }
    fn write_iter(&self, from: &mut dyn IoSource, status: u64) -> Option<Result<usize, Errno>> {
        self.file.write_iter(from, status)
    }
    fn fcntl(&self, cmd: u64, arg: u64, cred: crate::inode::Cred) -> Option<Result<i64, Errno>> {
        self.file.fcntl(cmd, arg, cred)
    }
    fn attr(&self) -> Option<crate::abi::Attr> {
        self.file.attr()
    }
    fn meta(&self) -> Option<&crate::inode::Meta> {
        self.file.meta()
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        self.file.poll_head()
    }
    fn poll_wait(&self, add: &mut dyn FnMut(&PollQueue)) {
        self.file.poll_wait(add)
    }
    fn can_poll(&self) -> bool {
        self.file.can_poll()
    }
    fn poll_exact(&self) -> bool {
        self.file.poll_exact()
    }
    fn link_name(&self) -> Option<String> {
        self.file.link_name()
    }
}

impl Pollable for NodeFile {
    fn events(&self) -> Events {
        File::events(self)
    }
    /// The object's own queue when it has one (pipes, eventfd), else this node's (never woken: "always ready").
    fn wait_queue(&self) -> &WaitQueue {
        match self.file.poll_head() {
            Some(q) => q,
            None => &self.waiters,
        }
    }
}

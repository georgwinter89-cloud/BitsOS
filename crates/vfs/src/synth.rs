//! Synthetic files and directories (FS-2): the framework under `/proc` and `/sys`. Content providers belong to
//! their owners; they plug in through two small traits:
//!
//! - `FileSource`: one file whose bytes are made when a program reads it (a snapshot per open, like Linux's
//!   `seq_file`), or read live at an offset (binary attributes such as a PCI function's `config`);
//! - `DirSource`: one directory whose names are computed (the process directories of `/proc`, a flat view such
//!   as `pci::sysfs`). A directory of a synthetic file system can carry several sources plus static names
//!   (`procfile!`); lookups ask the static names first, then each source in order. Two sources offering a
//!   directory of the same name are merged into one directory (so `/sys/devices` can hold PCI's tree and, later,
//!   the CPU files of another owner).
//!
//! Names found through a source get a dentry of their own for the walk (parent link, name) but are not cached:
//! every lookup asks the source again, so a process that ended is gone at once (Linux revalidates its `/proc`
//! dentries for the same reason). Inode numbers are stable: the source's own, or a hash of the parent's number
//! and the name.

use crate::dentry::Dentry;
use crate::inode::{Cred, Data, Inode};
use crate::node::File;
use crate::Errno;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;
use uapi::errno::*;

/// One synthetic file's content.
pub trait FileSource: Send + Sync {
    /// Asked at open (Linux checks access to another process's memory map there): EACCES and friends.
    fn may_open(&self, _who: Cred, _write: bool) -> Result<(), Errno> {
        Ok(())
    }
    /// The whole content for `who`, made at each read of an open file at offset 0.
    fn render(&self, who: Cred) -> Result<Vec<u8>, Errno>;
    /// Files read live at an offset instead of a snapshot (binary attributes): their size.
    fn live_size(&self) -> Option<u64> {
        None
    }
    fn read_live(&self, _who: Cred, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Ok(0)
    }
    /// A write (whole buffer, the position is ignored as for sysfs/proc attributes). Default: EIO, what Linux
    /// answers for a proc file without a write method that was opened for writing by root.
    fn write(&self, _who: Cred, _data: &[u8]) -> Result<usize, Errno> {
        Err(EIO)
    }
    /// A write at a position, for files where it counts (/proc/<pid>/mem). Default: `write`. AB-7a CR9.
    fn write_at(&self, who: Cred, _pos: u64, data: &[u8]) -> Result<usize, Errno> {
        self.write(who, data)
    }
    /// `st_size` (proc files 0, sysfs attributes one page).
    fn size(&self) -> u64 {
        self.live_size().unwrap_or(0)
    }
}

/// A computed directory.
pub trait DirSource: Send + Sync {
    /// The names in listing order. `cookie` is the listing position of each entry and must grow along the
    /// list; a source whose cookies do not grow is listed by index (fine for lists that rarely change).
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno>;
    /// One name (default: a search of `entries`).
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        Ok(self.entries()?.into_iter().find(|e| e.name == name))
    }
    /// Asked when a program opens the directory (Linux's `permission` op at open, e.g. /proc/<pid>/fdinfo's
    /// ptrace check): EACCES and friends. AB-7a CR4.
    fn may_open(&self) -> Result<(), Errno> {
        Ok(())
    }
    /// Links stat adds at getattr time (Linux's `proc_task_getattr`: 2 + threads for /proc/<pid>/task).
    fn extra_nlink(&self) -> u32 {
        0
    }
}

/// FS-10: a proc magic link (`/proc/<pid>/fd/<n>`, Linux's `proc_get_link`): readlink shows its text, a walk
/// that follows it continues at the object's own place (`nd_jump_link`), so an unlinked or nameless file can
/// be opened again through it.
pub trait MagicLink: Send + Sync {
    /// Where following it leads (ENOENT once the fd is closed; ENXIO for an object without a place).
    fn target(&self) -> Result<crate::mount_ns::Path, Errno>;
    /// Asked by readlink (Linux `proc_pid_readlink`'s access check: EACCES; ENOENT once the object is gone).
    /// lstat and lookups never ask, so a refused reader still sees the link. AB-7a CR4.
    fn may_read(&self) -> Result<(), Errno> {
        Ok(())
    }
    /// An open(2) that ends on the link (following it): the object opened again by the link itself, or None
    /// to follow `target()` as a walk does (AB-7a CR8: pipes, which have no place to walk to).
    fn open(&self, _flags: u64) -> Option<Result<Arc<crate::fd::OpenFile>, Errno>> {
        None
    }
}

pub enum SynthNode {
    Dir(Arc<dyn DirSource>),
    File(Arc<dyn FileSource>),
    Link(Vec<u8>),
    /// FS-10: a magic link: its text, permission bits (Linux derives them from the open file's mode) and target.
    Magic(Vec<u8>, Arc<dyn MagicLink>),
}

pub struct SynthEntry {
    pub name: Vec<u8>,
    pub node: SynthNode,
    /// Permission bits (the type comes from `node`).
    pub perm: u32,
    pub uid: u32,
    pub gid: u32,
    /// The inode number (None: a stable hash of the parent's number and the name).
    pub ino: Option<u64>,
    pub cookie: u64,
    /// `S_IMMUTABLE` on the inode (proc's process directories: writing into them is EPERM even for the owner).
    pub immutable: bool,
}

impl SynthEntry {
    pub fn dir(name: &str, src: Arc<dyn DirSource>) -> Self {
        SynthEntry { name: name.as_bytes().to_vec(), node: SynthNode::Dir(src), perm: 0o555, uid: 0, gid: 0, ino: None, cookie: 0, immutable: false }
    }
    pub fn file(name: &str, perm: u32, src: Arc<dyn FileSource>) -> Self {
        SynthEntry { name: name.as_bytes().to_vec(), node: SynthNode::File(src), perm, uid: 0, gid: 0, ino: None, cookie: 0, immutable: false }
    }
    pub fn link(name: &str, target: &str) -> Self {
        SynthEntry { name: name.as_bytes().to_vec(), node: SynthNode::Link(target.as_bytes().to_vec()), perm: 0o777, uid: 0, gid: 0, ino: None, cookie: 0, immutable: false }
    }
    /// FS-10: a magic link with permission bits `perm` (fd links: 0700 for O_RDWR, 0500 read, 0300 write).
    pub fn magic(name: &str, perm: u32, text: Vec<u8>, target: Arc<dyn MagicLink>) -> Self {
        SynthEntry { name: name.as_bytes().to_vec(), node: SynthNode::Magic(text, target), perm, uid: 0, gid: 0, ino: None, cookie: 0, immutable: false }
    }
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.uid = uid;
        self.gid = gid;
        self
    }
    pub fn cookie(mut self, c: u64) -> Self {
        self.cookie = c;
        self
    }
    pub fn immutable(mut self) -> Self {
        self.immutable = true;
        self
    }
    fn mode(&self) -> u32 {
        match self.node {
            SynthNode::Dir(_) => uapi::S_IFDIR | (self.perm & 0o7777),
            SynthNode::File(_) => uapi::S_IFREG | (self.perm & 0o7777),
            SynthNode::Link(_) => uapi::S_IFLNK | 0o777,
            SynthNode::Magic(..) => uapi::S_IFLNK | (self.perm & 0o777),
        }
    }
    fn dtype(&self) -> u8 {
        crate::abi::dtype(self.mode())
    }
}

/// The sources of one directory of a synthetic file system (`Data::SynthDir`).
pub struct SynthDir {
    sources: SpinLock<Vec<Arc<dyn DirSource>>>,
}

impl Default for SynthDir {
    fn default() -> Self {
        Self::new()
    }
}

impl SynthDir {
    pub fn new() -> Self {
        SynthDir { sources: SpinLock::named("vfs.synth.sources", Vec::new()) }
    }
    fn with(sources: Vec<Arc<dyn DirSource>>) -> Self {
        SynthDir { sources: SpinLock::named("vfs.synth.sources", sources) }
    }
    pub fn add(&self, src: Arc<dyn DirSource>) {
        self.sources.lock().push(src);
    }
    pub fn sources(&self) -> Vec<Arc<dyn DirSource>> {
        self.sources.lock().clone()
    }
    pub fn extra_nlink(&self) -> u32 {
        self.sources().iter().map(|s| s.extra_nlink()).sum()
    }
}

/// A stable inode number for a computed name: FNV-1a of the parent's number and the name, in the upper half of
/// the 63-bit range so it never meets the counter-made numbers of the static nodes.
pub fn synth_ino(parent: u64, name: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in parent.to_le_bytes().iter().chain(name) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h & ((1 << 62) - 1)) | (1 << 62)
}

fn inode_for(parent_ino: u64, e: SynthEntry) -> Arc<Inode> {
    let ino = e.ino.unwrap_or_else(|| synth_ino(parent_ino, &e.name));
    let mode = e.mode();
    let cred = Cred { uid: e.uid, gid: e.gid };
    let immutable = e.immutable;
    let data = match e.node {
        SynthNode::Dir(s) => Data::SynthDir(Arc::new(SynthDir::with(alloc::vec![s]))),
        SynthNode::File(f) => Data::Synth(f),
        SynthNode::Link(t) => Data::Symlink(t),
        SynthNode::Magic(t, m) => return Inode::new_magic(ino, mode, cred, t, m),
    };
    let i = Inode::new(ino, mode, cred, data);
    i.immutable.store(immutable, core::sync::atomic::Ordering::Relaxed);
    i
}

/// A name of `dir` found through its sources (the static names were asked first by the caller).
pub fn lookup(dir: &Arc<Dentry>, sd: &SynthDir, name: &[u8]) -> Result<Option<Arc<Dentry>>, Errno> {
    let mut first: Option<SynthEntry> = None;
    let mut dirs: Vec<Arc<dyn DirSource>> = Vec::new();
    for s in sd.sources() {
        let Some(e) = s.lookup(name)? else { continue };
        match &first {
            None => {
                if let SynthNode::Dir(d) = &e.node {
                    dirs.push(d.clone());
                }
                first = Some(e);
            }
            Some(f) => {
                if let (SynthNode::Dir(_), SynthNode::Dir(d)) = (&f.node, &e.node) {
                    dirs.push(d.clone());
                }
            }
        }
    }
    let Some(e) = first else { return Ok(None) };
    let parent_ino = dir.inode.meta.ino;
    let inode = if dirs.len() > 1 {
        let ino = e.ino.unwrap_or_else(|| synth_ino(parent_ino, &e.name));
        Inode::new(ino, uapi::S_IFDIR | (e.perm & 0o7777), Cred { uid: e.uid, gid: e.gid }, Data::SynthDir(Arc::new(SynthDir::with(dirs))))
    } else {
        inode_for(parent_ino, e)
    };
    Ok(Some(Dentry::ephemeral(dir, name, inode)))
}

/// A number name of `/proc` (`/proc/<pid>`, later `task/<tid>` and `fd/<n>`) as Linux 6.6
/// fs/proc/util.c:4 `name_to_int` reads it: ASCII digits only (no sign, no space), no leading zero unless the
/// name is "0", and refused once the value before a further digit reaches `(u32::MAX - 9) / 10` (Linux's
/// overflow guard, so 429496728x and above are no names).
pub fn name_to_int(name: &[u8]) -> Option<u32> {
    if name.is_empty() || (name.len() > 1 && name[0] == b'0') {
        return None;
    }
    let mut n: u32 = 0;
    for &b in name {
        let c = b.wrapping_sub(b'0');
        if c > 9 || n >= (u32::MAX - 9) / 10 {
            return None;
        }
        n = n * 10 + c as u32;
    }
    Some(n)
}

/// Positions of computed names in a directory listing: above every static listing number.
pub const DYN_BASE: u64 = 1 << 48;
const COOKIE_BITS: u32 = 40;

/// One record of a computed listing: (position after it, ino, type, name).
pub struct Listed {
    pub next: u64,
    pub ino: u64,
    pub dtype: u8,
    pub name: Vec<u8>,
}

/// The computed names of `dir` from position `pos` on (positions >= `DYN_BASE`), skipping names a static
/// child or an earlier source already has. At most `max` records.
pub fn list_from(dir: &Arc<Dentry>, sd: &SynthDir, pos: u64, max: usize) -> Result<Vec<Listed>, Errno> {
    let mut out = Vec::new();
    // A set, not a list: `/proc` with many processes would otherwise cost n² per getdents call.
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    let parent_ino = dir.inode.meta.ino;
    for (si, s) in sd.sources().iter().enumerate() {
        let entries = s.entries()?;
        let increasing = entries.windows(2).all(|w| w[0].cookie < w[1].cookie);
        for (i, e) in entries.into_iter().enumerate() {
            let c = if increasing { e.cookie } else { i as u64 };
            let c = c & ((1 << COOKIE_BITS) - 1);
            let at = DYN_BASE + ((si as u64) << COOKIE_BITS) + c;
            if dir.child(&e.name).is_some() || !seen.insert(e.name.clone()) {
                continue;
            }
            if at < pos {
                continue;
            }
            if out.len() >= max {
                return Ok(out);
            }
            let ino = e.ino.unwrap_or_else(|| synth_ino(parent_ino, &e.name));
            out.push(Listed { next: at + 1, ino, dtype: e.dtype(), name: e.name });
        }
    }
    Ok(out)
}

/// An open synthetic file: rendered at every read at offset 0 and kept for the reads after it (sequential
/// reads of one open file see one consistent text), or read live for binary attributes. Writes go to the source.
pub struct SynthFile {
    src: Arc<dyn FileSource>,
    who: Cred,
    mode: u32,
    text: SpinLock<Option<Arc<Vec<u8>>>>,
}

impl SynthFile {
    pub fn new(src: Arc<dyn FileSource>, who: Cred, mode: u32) -> Self {
        SynthFile { src, who, mode, text: SpinLock::named("vfs.proc.text", None) }
    }
    fn text(&self) -> Result<Arc<Vec<u8>>, Errno> {
        if let Some(t) = self.text.lock().clone() {
            return Ok(t);
        }
        // Made outside the lock: a source may take other locks or allocate a lot.
        let t = Arc::new(self.src.render(self.who)?);
        Ok(self.text.lock().get_or_insert(t).clone())
    }
    fn fresh(&self) -> Result<Arc<Vec<u8>>, Errno> {
        let t = Arc::new(self.src.render(self.who)?);
        *self.text.lock() = Some(t.clone());
        Ok(t)
    }
}

impl File for SynthFile {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        if let Some(size) = self.src.live_size() {
            if pos >= size {
                return Ok(0);
            }
            let n = buf.len().min((size - pos) as usize);
            return self.src.read_live(self.who, pos, &mut buf[..n]);
        }
        // A read at offset 0 makes the text again, as seq_file does (linux-6.6/fs/seq_file.c:180-187, the
        // `ki_pos == 0` reset): top, free and vmstat keep /proc files open and pread them at 0 (AB-7a CR10).
        let text = if pos == 0 { self.fresh()? } else { self.text()? };
        let p = pos.min(text.len() as u64) as usize;
        let n = buf.len().min(text.len() - p);
        buf[..n].copy_from_slice(&text[p..p + n]);
        Ok(n)
    }
    fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.src.write_at(self.who, pos, buf)
    }
    fn size(&self) -> u64 {
        self.src.size()
    }
    fn mode(&self) -> u32 {
        self.mode
    }
    fn seekable(&self) -> bool {
        true
    }
}

/// A fixed text as a `FileSource`.
pub struct Text(pub String);

impl FileSource for Text {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(self.0.as_bytes().to_vec())
    }
}

/// A sysfs attribute text: like `Text`, but `st_size` is one page as sysfs reports it.
pub struct Attr(pub String);

impl FileSource for Attr {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(self.0.as_bytes().to_vec())
    }
    fn size(&self) -> u64 {
        4096
    }
}

/// What a flat view lists: a path relative to where the view is attached (`bus/pci/devices/0000:00:00.0`).
pub enum FlatKind {
    Dir,
    /// A sysfs attribute with this text (0444).
    Text(String),
    /// A symbolic link with this target.
    Link(String),
    /// Any other file (binary attributes, writable files).
    File { perm: u32, src: Arc<dyn FileSource> },
}

pub struct FlatEntry {
    pub path: String,
    pub kind: FlatKind,
}

/// A `DirSource` over a flat, path-sorted list made fresh on each question (PCI's `pci::sysfs::entries()`): one
/// instance per directory level, `prefix` = that directory's path in the list ("" at the top).
pub struct FlatView {
    list: fn() -> Vec<FlatEntry>,
    prefix: String,
}

impl FlatView {
    pub fn new(list: fn() -> Vec<FlatEntry>) -> Self {
        FlatView { list, prefix: String::new() }
    }
    fn child_name<'a>(&self, path: &'a str) -> Option<&'a str> {
        let rest = if self.prefix.is_empty() { path } else { path.strip_prefix(self.prefix.as_str())?.strip_prefix('/')? };
        (!rest.is_empty() && !rest.contains('/')).then_some(rest)
    }
    fn entry(&self, name: &str, path: String, kind: FlatKind) -> SynthEntry {
        match kind {
            FlatKind::Dir => SynthEntry::dir(name, Arc::new(FlatView { list: self.list, prefix: path })).with_perm(0o755),
            FlatKind::Text(t) => SynthEntry::file(name, 0o444, Arc::new(Attr(t))),
            FlatKind::Link(t) => SynthEntry::link(name, &t),
            FlatKind::File { perm, src } => SynthEntry::file(name, perm, src),
        }
    }
}

impl SynthEntry {
    pub fn with_perm(mut self, perm: u32) -> Self {
        self.perm = perm;
        self
    }
}

impl DirSource for FlatView {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let mut out = Vec::new();
        for e in (self.list)() {
            if let Some(n) = self.child_name(&e.path) {
                let n = String::from(n);
                out.push(self.entry(&n, e.path, e.kind));
            }
        }
        Ok(out)
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        let Ok(name) = core::str::from_utf8(name) else { return Ok(None) };
        let want = if self.prefix.is_empty() { String::from(name) } else { alloc::format!("{}/{}", self.prefix, name) };
        Ok((self.list)().into_iter().find(|e| e.path == want).map(|e| self.entry(name, e.path, e.kind)))
    }
}

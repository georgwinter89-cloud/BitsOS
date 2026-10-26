//! One mounted FAT32 volume: geometry, the FAT, and every node the VFS has seen (by inode number), behind one
//! volume lock (`fat.vol`, a Mutex: every change of FAT, directories and FSInfo is serialised; data reads of
//! cached chains need it only briefly). Lock order: `vfs.sb.ns` > `vfs.sb.load` > `vfs.cache.sync` > `fat.vol`;
//! the volume never calls back into the VFS while it holds `fat.vol`.
//!
//! Write order (crash safety, V1 `fs/fatwrite.asm`, `spec/usb.md` §6.1): FSInfo's free count is set to unknown
//! before the first change, then data into free clusters, then the FATs (new chain entries before the link,
//! FAT 1 before FAT 2), then the directory entry (one sector), then freeing an old chain, and FSInfo with the
//! real count last. A cut therefore leaves at most clusters no entry reaches (or a chain one entry's size does
//! not cover yet), which fsck.fat reclaims; nothing that was committed before is damaged.

use crate::bpb::{self, Geometry};
use crate::dev::Dev;
use crate::dirent::{Entry, ATTR_DIR, ATTR_RO};
use crate::fat::Fat;
use crate::protect::{self, Class, Rules};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{Mutex, MutexGuard, SpinLock};
use uapi::errno::*;

pub const ROOT: u64 = 1;

/// Wall time for stamps: UTC seconds now, and the local offset (minutes east) at a UTC second.
#[derive(Clone, Copy)]
pub struct Clock {
    pub now: fn() -> i64,
    pub offset: fn(i64) -> i32,
}

fn zero_now() -> i64 {
    0
}
fn zero_offset(_: i64) -> i32 {
    0
}

impl Clock {
    pub const UTC_EPOCH: Clock = Clock { now: zero_now, offset: zero_offset };
    pub fn stamp_now(&self) -> crate::dirent::Stamp {
        let t = (self.now)();
        crate::dirent::stamp(t + (self.offset)(t) as i64 * 60)
    }
    /// UTC seconds of a FAT local date/time.
    pub fn utc(&self, date: u16, time: u16) -> i64 {
        let local = crate::dirent::local_secs(date, time);
        local - (self.offset)(local) as i64 * 60
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Writes may happen at all (the identified boot stick, or a test volume).
    pub writable: bool,
    /// The stick rules of `protect` apply.
    pub stick: bool,
}

/// A node the VFS knows.
pub struct Node {
    pub parent: u64,
    /// The shown name (long name, else the short one).
    pub name: Vec<u8>,
    /// Its short entry as on disk after our last write (root: a made-up one with the root cluster).
    pub e: Entry,
    /// Index of the short entry in the parent directory and its LFN entries before it.
    pub slot: u32,
    pub lfn: u32,
    /// It has a directory entry (false: the root, or removed while still open).
    pub linked: bool,
    pub chain: Option<Vec<u32>>,
    /// A directory's entries (all its clusters), once read.
    pub raw: Option<Vec<u8>>,
    /// The entry differs from the disk (size, first cluster or time): written by `commit`.
    pub entry_dirty: bool,
    pub class: Class,
    /// Changes of its data or size (`read_data` and the read-ahead compare it).
    pub dgen: Arc<AtomicU64>,
    /// Clusters promised to its cached data that the write-back has not allocated yet (`reserve`).
    pub reserved: u64,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.e.is_dir()
    }
}

pub struct State {
    pub fat: Fat,
    pub nodes: BTreeMap<u64, Node>,
    pub next_ino: u64,
    /// FSInfo sector content, when the volume has a valid one.
    pub fsinfo: Option<(u64, Vec<u8>)>,
    /// We wrote "free count unknown" and the real count is still to come.
    pub fsinfo_open: bool,
    /// Clusters promised to cached file data (the sum of the nodes' `reserved`).
    pub reserved: u64,
}

/// What the VFS gets for a node.
#[derive(Clone, Debug)]
pub struct Child {
    pub ino: u64,
    pub name: Vec<u8>,
    pub dir: bool,
    pub size: u64,
    pub read_only: bool,
    /// Below `\BITSOS2\home\` (owner 1000:1000, 0700/0600).
    pub home: bool,
    /// UTC seconds: modification, access (date only on FAT), creation.
    pub mtime: i64,
    pub atime: i64,
    pub ctime: i64,
}

klog::counter!(WRITES, "fat.writes");
klog::counter!(REFUSED, "fat.refused");

pub struct Volume {
    pub dev: Arc<dyn Dev>,
    pub geo: Geometry,
    pub st: Mutex<State>,
    rw: AtomicBool,
    /// Writing was turned off for good (`set_read_only`).
    never: AtomicBool,
    pub stick: bool,
    gone: AtomicBool,
    /// Inodes whose last reference went (`file::FatFile` drop): their chains are freed under the lock later.
    orphans: SpinLock<Vec<u64>>,
    /// The thread holding the SYS grant (`sysapi`), and the lock that makes it one at a time.
    sys_owner: SpinLock<Option<u64>>,
    sys_lock: Mutex<()>,
    pub clock: Clock,
    /// Volume label: the root's label entry, else the boot sector's.
    pub label: [u8; 11],
    pub changes: AtomicU64,
    /// `changes` at the last durable flush by the write-back.
    flushed_at: AtomicU64,
}

impl Volume {
    /// Reads the boot sector, FSInfo and the root directory.
    pub fn mount(dev: Arc<dyn Dev>, opts: Options, clock: Clock) -> Result<Arc<Volume>, &'static str> {
        let ss = dev.sector_size();
        let mut b0 = vec![0u8; ss];
        dev.read(0, &mut b0).map_err(|_| "boot sector unreadable")?;
        let geo = bpb::parse(&b0, ss, dev.sectors())?;
        let fsinfo = match geo.fsinfo {
            Some(s) => {
                let mut b = vec![0u8; ss];
                dev.read(s, &mut b).map_err(|_| "FSInfo unreadable")?;
                bpb::parse_fsinfo(&b).map(|_| (s, b))
            }
            None => None,
        };
        let mut fat = Fat::new(&geo);
        if let Some((_, b)) = &fsinfo
            && let Some(f) = bpb::parse_fsinfo(b)
            && geo.valid_cluster(f.next)
        {
            fat.hint = f.next;
        }
        let root_chain = fat.chain(&*dev, geo.root).map_err(|_| "root directory chain broken")?;
        let cb = geo.cluster_bytes();
        if root_chain.len() * cb > MAX_DIR_BYTES {
            return Err("root directory longer than FAT allows");
        }
        let mut raw = vec![0u8; root_chain.len() * cb];
        for (i, &c) in root_chain.iter().enumerate() {
            dev.read(geo.cluster_sector(c), &mut raw[i * cb..(i + 1) * cb]).map_err(|_| "root directory unreadable")?;
        }
        let label = crate::listing::parse(&raw).label.unwrap_or(geo.label);
        let root_e = Entry { name: *b"           ", attr: ATTR_DIR, ntres: 0, ctime_tenth: 0, ctime: 0, cdate: 0x21, adate: 0x21, first: geo.root, wtime: 0, wdate: 0x21, size: 0 };
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT, Node { parent: 0, name: Vec::new(), e: root_e, slot: 0, lfn: 0, linked: false, chain: Some(root_chain), raw: Some(raw), entry_dirty: false, class: if opts.stick { Class::Root } else { Class::Free }, dgen: Arc::default(), reserved: 0 });
        let rw = opts.writable && geo.writable_layout();
        Ok(Arc::new(Volume {
            dev,
            geo,
            st: Mutex::named("fat.vol", State { fat, nodes, next_ino: ROOT + 1, fsinfo, fsinfo_open: false, reserved: 0 }),
            rw: AtomicBool::new(rw),
            never: AtomicBool::new(false),
            stick: opts.stick,
            gone: AtomicBool::new(false),
            orphans: SpinLock::named("fat.orphans", Vec::new()),
            sys_owner: SpinLock::named("fat.sys", None),
            sys_lock: Mutex::named("fat.sys.grant", ()),
            clock,
            label,
            changes: AtomicU64::new(0),
            flushed_at: AtomicU64::new(0),
        }))
    }

    pub fn writable(&self) -> bool {
        self.rw.load(Ordering::Acquire) && !self.is_gone()
    }
    /// Turns writing off for good (a second stick appeared, the gate disarmed).
    pub fn set_read_only(&self) {
        self.never.store(true, Ordering::Release);
        self.rw.store(false, Ordering::Release);
    }
    /// Turns writing on (the stick's identity was confirmed after mounting read-only). False when writing was
    /// turned off for good meanwhile, the disk went, or the layout does not allow it.
    pub fn allow_writes(&self) -> bool {
        if self.never.load(Ordering::Acquire) || self.is_gone() || !self.geo.writable_layout() {
            return false;
        }
        self.rw.store(true, Ordering::Release);
        if self.never.load(Ordering::Acquire) {
            self.rw.store(false, Ordering::Release);
            return false;
        }
        true
    }
    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }
    /// The disk went away: every later call is EIO.
    pub fn set_gone(&self) {
        self.gone.store(true, Ordering::Release);
        self.rw.store(false, Ordering::Release);
    }
    pub fn label_text(&self) -> Vec<u8> {
        let l = self.label.iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
        self.label[..l].to_vec()
    }

    pub(crate) fn lock(&self) -> Result<VolGuard<'_>, i64> {
        if self.is_gone() {
            return Err(EIO);
        }
        let lift = self.dev.lift();
        let g = match self.st.try_lock() {
            Some(g) => g,
            None => {
                let _w = lift.and_then(|l| l.wait());
                self.st.lock()
            }
        };
        Ok(VolGuard { _held: lift.map(|l| l.hold()), g })
    }

    pub fn rules(&self) -> Rules {
        let sys = matches!(*self.sys_owner.lock(), Some(t) if t == ksync::thread_id());
        Rules { rw: self.writable(), stick: self.stick, sys }
    }

    /// Takes the SYS grant for the calling thread until the guard drops (one holder at a time; others sleep).
    pub fn grant_sys(&self) -> SysGrant<'_> {
        let g = self.sys_lock.lock();
        *self.sys_owner.lock() = Some(ksync::thread_id());
        SysGrant { vol: self, _g: g }
    }

    pub fn refused(&self, what: &'static str, ino: u64, e: i64) -> i64 {
        REFUSED.inc();
        klog::event!(Info, "fat", "write.refused", what = what, ino = ino, errno = e);
        e
    }

    // ---- nodes ----

    pub fn node(st: &State, ino: u64) -> Result<&Node, i64> {
        st.nodes.get(&ino).ok_or(ENOENT)
    }
    pub fn node_mut(st: &mut State, ino: u64) -> Result<&mut Node, i64> {
        st.nodes.get_mut(&ino).ok_or(ENOENT)
    }

    pub fn child_of(&self, n: &Node, ino: u64) -> Child {
        let e = &n.e;
        Child {
            ino,
            name: n.name.clone(),
            dir: e.is_dir(),
            size: if e.is_dir() { 0 } else { e.size as u64 },
            read_only: e.attr & ATTR_RO != 0,
            home: n.class == Class::Home,
            mtime: self.clock.utc(e.wdate, e.wtime),
            atime: self.clock.utc(e.adate, 0),
            ctime: self.clock.utc(e.cdate, e.ctime),
        }
    }

    /// The class of a new child `name` (alias key `alias`) of directory `dir`.
    pub fn class_of_new(st: &State, dir: u64, name: &[u8], alias: &[u8]) -> Result<Class, i64> {
        Ok(protect::child_class(Self::node(st, dir)?.class, name, alias))
    }

    /// A node's cluster chain (read once, then kept).
    pub fn chain<'a>(&self, st: &'a mut State, ino: u64) -> Result<&'a Vec<u32>, i64> {
        let first = Self::node(st, ino)?.e.first;
        if Self::node(st, ino)?.chain.is_none() {
            let c = st.fat.chain(&*self.dev, first)?;
            Self::node_mut(st, ino)?.chain = Some(c);
        }
        Self::node(st, ino)?.chain.as_ref().ok_or(EIO)
    }

    /// A directory's raw entries (read once; at most 2 MiB = 65536 entries, the FAT limit).
    pub fn dir_raw<'a>(&self, st: &'a mut State, ino: u64) -> Result<&'a mut Vec<u8>, i64> {
        if Self::node(st, ino)?.raw.is_none() {
            let chain = self.chain(st, ino)?.clone();
            let cb = self.geo.cluster_bytes();
            if chain.len() * cb > MAX_DIR_BYTES {
                klog::event!(Warn, "fat", "dir.too_long", ino = ino, clusters = chain.len(); "directory longer than FAT allows: refused");
                return Err(EIO);
            }
            let mut raw = Vec::new();
            raw.try_reserve_exact(chain.len() * cb).map_err(|_| ENOMEM)?;
            raw.resize(chain.len() * cb, 0);
            for (i, &c) in chain.iter().enumerate() {
                self.dev.read(self.geo.cluster_sector(c), &mut raw[i * cb..(i + 1) * cb])?;
            }
            Self::node_mut(st, ino)?.raw = Some(raw);
        }
        Self::node_mut(st, ino)?.raw.as_mut().ok_or(EIO)
    }

    /// The sector holding entry `slot` of directory `ino` (its chain is loaded).
    pub fn slot_sector(&self, st: &State, ino: u64, slot: u32) -> Result<u64, i64> {
        let byte = slot as usize * crate::dirent::ENTRY;
        let cb = self.geo.cluster_bytes();
        let chain = Self::node(st, ino)?.chain.as_ref().ok_or(EIO)?;
        let c = *chain.get(byte / cb).ok_or(EIO)?;
        Ok(self.geo.cluster_sector(c) + ((byte % cb) / self.geo.bps) as u64)
    }

    /// The children of directory `ino` (each gets a node; called once per directory by the VFS).
    pub fn list(&self, ino: u64) -> Result<Vec<Child>, i64> {
        let mut st = self.lock()?;
        let raw = self.dir_raw(&mut st, ino)?.clone();
        let parent_class = Self::node(&st, ino)?.class;
        let l = crate::listing::parse(&raw);
        let mut out = Vec::new();
        // A set: a directory may hold 65536 entries (a stick made elsewhere), so no quadratic scan under the lock.
        let mut seen = alloc::collections::BTreeSet::new();
        for it in l.items {
            let key = crate::name::fold(&it.name);
            if !seen.insert(key) {
                // Two entries a case-insensitive lookup cannot tell apart (made elsewhere): the first wins.
                klog::event!(Info, "fat", "dir.duplicate", dir = ino, name = alloc::string::String::from_utf8_lossy(&it.name).into_owned());
                continue;
            }
            let class = protect::child_class(parent_class, &it.name, &crate::dirent::alias_key(&it.e));
            let id = st.next_ino;
            st.next_ino += 1;
            let n = Node { parent: ino, name: it.name, e: it.e, slot: it.slot, lfn: it.lfn, linked: true, chain: None, raw: None, entry_dirty: false, class, dgen: Arc::default(), reserved: 0 };
            out.push(self.child_of(&n, id));
            st.nodes.insert(id, n);
        }
        Ok(out)
    }

    /// The shown name of the child of `dir` whose 8.3 alias is `key` (upper case "BASE.EXT").
    pub fn alias(&self, dir: u64, key: &[u8]) -> Option<Vec<u8>> {
        let st = self.lock().ok()?;
        st.nodes.values().find(|n| n.parent == dir && n.linked && crate::dirent::alias_key(&n.e) == key).map(|n| n.name.clone())
    }

    /// The short entry at `path` (components from the root, compared like lookups: case-insensitive or by
    /// alias) without making nodes: the identity checks at mount read STICK.ID this way.
    pub fn peek(&self, path: &[&[u8]]) -> Result<Option<Entry>, i64> {
        let mut st = self.lock()?;
        let mut raw = self.dir_raw(&mut st, ROOT)?.clone();
        for (i, comp) in path.iter().enumerate() {
            let key = crate::name::fold(comp);
            let l = crate::listing::parse(&raw);
            let Some(it) = l.items.into_iter().find(|it| crate::name::fold(&it.name) == key || crate::dirent::alias_key(&it.e) == key) else {
                return Ok(None);
            };
            if i + 1 == path.len() {
                return Ok(Some(it.e));
            }
            if !it.e.is_dir() {
                return Ok(None);
            }
            let chain = st.fat.chain(&*self.dev, it.e.first)?;
            let cb = self.geo.cluster_bytes();
            if chain.len() * cb > MAX_DIR_BYTES {
                return Err(EIO);
            }
            raw = vec![0u8; chain.len() * cb];
            for (k, &c) in chain.iter().enumerate() {
                self.dev.read(self.geo.cluster_sector(c), &mut raw[k * cb..(k + 1) * cb])?;
            }
        }
        Ok(None)
    }

    /// The first `max` bytes (at most) of the file of entry `e`.
    pub fn peek_read(&self, e: &Entry, max: usize) -> Result<Vec<u8>, i64> {
        let mut st = self.lock()?;
        let chain = st.fat.chain(&*self.dev, e.first)?;
        let len = (e.size as usize).min(max);
        let cb = self.geo.cluster_bytes();
        let mut out = vec![0u8; len.div_ceil(cb) * cb];
        for (k, &c) in chain.iter().take(out.len() / cb).enumerate() {
            self.dev.read(self.geo.cluster_sector(c), &mut out[k * cb..(k + 1) * cb])?;
        }
        out.truncate(len);
        Ok(out)
    }

    /// The size the directory entry holds now (in memory: what the last write-back made it).
    pub fn disk_size(&self, ino: u64) -> Result<u64, i64> {
        let st = self.lock()?;
        Ok(Self::node(&st, ino)?.e.size as u64)
    }

    /// The class a new child `name` of directory `dir` would get (before its alias is known).
    pub fn class_for_name(&self, dir: u64, name: &[u8]) -> Result<Class, i64> {
        let st = self.lock()?;
        Self::class_of_new(&st, dir, name, &crate::name::fold(name))
    }

    /// The node's class (for the VFS's open-for-write question).
    pub fn class(&self, ino: u64) -> Result<Class, i64> {
        let st = self.lock()?;
        Ok(Self::node(&st, ino)?.class)
    }

    /// May the content of `ino` change (open for writing, write, truncate)?
    pub fn check_write(&self, ino: u64) -> Result<(), i64> {
        let c = self.class(ino)?;
        self.rules().write(c).map_err(|e| self.refused("write", ino, e))
    }

    /// A node's last reference went (`file::FatFile` drop): its node goes, and its clusters too when it was
    /// removed while open. Queued without I/O (drops happen anywhere); `reap` frees under the lock.
    pub fn forget(&self, ino: u64) {
        self.orphans.lock().push(ino);
    }

    /// Frees what `forget` queued (called at the start of every change and by `sync`).
    pub fn reap(&self, st: &mut State) -> Result<(), i64> {
        let q: Vec<u64> = core::mem::take(&mut *self.orphans.lock());
        let mut freed = false;
        for ino in q {
            let Some(n) = st.nodes.remove(&ino) else { continue };
            st.reserved -= n.reserved.min(st.reserved);
            if n.linked || n.e.first == 0 || !self.writable() {
                continue;
            }
            let chain = match n.chain {
                Some(c) => c,
                None => st.fat.chain(&*self.dev, n.e.first)?,
            };
            self.begin(st)?;
            st.fat.release(&*self.dev, &chain)?;
            freed = true;
        }
        if freed {
            st.fat.write_out(&*self.dev)?;
        }
        Ok(())
    }

    /// Promises the clusters file `ino` needs to be `end` bytes long when its cached data is written back
    /// (FAT allocates at write-back; `write` must still answer ENOSPC at once, as Linux's vfat does when it
    /// allocates in write_begin). Every other allocation leaves the promised clusters free.
    pub fn reserve(&self, ino: u64, end: u64) -> Result<(), i64> {
        let mut st = self.lock()?;
        // Files whose last reference went give their clusters back first.
        self.reap(&mut st)?;
        let cb = self.geo.cluster_bytes() as u64;
        let need = end.div_ceil(cb);
        let have = self.chain(&mut st, ino)?.len() as u64 + Self::node(&st, ino)?.reserved;
        if need <= have {
            return Ok(());
        }
        let more = need - have;
        let free = st.fat.count_free(&*self.dev)? as u64;
        if free < st.reserved + more {
            klog::event!(Info, "fat", "nospace", ino = ino, want = more, free = free, promised = st.reserved);
            return Err(ENOSPC);
        }
        Self::node_mut(&mut st, ino)?.reserved += more;
        st.reserved += more;
        Ok(())
    }

    /// `n` free clusters for node `owner` (None: a directory's growth or a new directory): the owner's promise
    /// pays first, and the rest must leave every other promise free (ENOSPC). The caller links them at once.
    pub(crate) fn allocate(&self, st: &mut State, owner: Option<u64>, n: usize) -> Result<Vec<u32>, i64> {
        let own = owner.and_then(|o| st.nodes.get(&o)).map_or(0, |x| x.reserved);
        let used = own.min(n as u64);
        let free = st.fat.count_free(&*self.dev)? as u64;
        if free + used < n as u64 + st.reserved {
            klog::event!(Info, "fat", "nospace", ino = owner.unwrap_or(0), want = n, free = free, promised = st.reserved);
            return Err(ENOSPC);
        }
        let got = st.fat.find_free(&*self.dev, n)?;
        if let Some(o) = owner
            && let Some(x) = st.nodes.get_mut(&o)
        {
            x.reserved -= used;
        }
        st.reserved -= used;
        Ok(got)
    }

    /// After a shrink of `ino` to `size`: promises beyond what the new size needs go back.
    pub(crate) fn unreserve_past(&self, st: &mut State, ino: u64, size: u64) -> Result<(), i64> {
        let cb = self.geo.cluster_bytes() as u64;
        let chain = self.chain(st, ino)?.len() as u64;
        let keep = size.div_ceil(cb).saturating_sub(chain);
        let n = Self::node_mut(st, ino)?;
        let back = n.reserved.saturating_sub(keep);
        n.reserved -= back;
        st.reserved -= back.min(st.reserved);
        Ok(())
    }

    /// Before the first change after the last FSInfo update: FSInfo's free count becomes unknown (a cut from now
    /// on makes every reader count again instead of trusting a stale number).
    pub fn begin(&self, st: &mut State) -> Result<(), i64> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        if st.fsinfo_open {
            return Ok(());
        }
        if st.fat.free.is_none() {
            st.fat.count_free(&*self.dev)?;
        }
        if let Some((s, b)) = &st.fsinfo {
            let f = bpb::parse_fsinfo(b).ok_or(EIO)?;
            let nb = bpb::fsinfo_with(b, bpb::FsInfo { free: bpb::UNKNOWN, next: f.next });
            self.write(*s, &nb)?;
            st.fsinfo = Some((*s, nb));
        }
        st.fsinfo_open = true;
        Ok(())
    }

    /// FSInfo with the real count (the last write of a change). While some node's entry still has to be
    /// written (a write-back between its runs and its commit) or a removed file still holds its clusters (open
    /// after unlink), the disk's FAT holds clusters no entry reaches: the count stays "unknown" until then.
    pub fn finish(&self, st: &mut State) -> Result<(), i64> {
        if !st.fsinfo_open {
            return Ok(());
        }
        if st.nodes.values().any(|n| n.entry_dirty || (!n.linked && !n.is_dir() && n.e.first != 0 && n.parent != 0)) {
            st.fat.write_out(&*self.dev)?;
            return Ok(());
        }
        st.fat.write_out(&*self.dev)?;
        if let Some((s, b)) = &st.fsinfo {
            let free = st.fat.count_free(&*self.dev)?;
            let nb = bpb::fsinfo_with(b, bpb::FsInfo { free, next: st.fat.hint });
            self.write(*s, &nb)?;
            st.fsinfo = Some((*s, nb));
        }
        st.fsinfo_open = false;
        Ok(())
    }

    pub fn write(&self, sector: u64, data: &[u8]) -> Result<(), i64> {
        WRITES.inc();
        self.dev.write(sector, data)
    }

    /// Everything to stable storage: queued frees, FSInfo, then the device's cache (SYNCHRONIZE CACHE).
    pub fn sync(&self, durable: bool) -> Result<(), i64> {
        {
            let mut st = self.lock()?;
            self.reap(&mut st)?;
            self.finish(&mut st)?;
        }
        if durable && self.writable() { self.dev.flush() } else { Ok(()) }
    }

    /// statfs: (cluster bytes, clusters, free clusters).
    pub fn statfs(&self) -> Result<(u64, u64, u64), i64> {
        let mut st = self.lock()?;
        let free = st.fat.count_free(&*self.dev)?;
        Ok((self.geo.cluster_bytes() as u64, self.geo.clusters as u64, free as u64))
    }
}

/// The background write-back reaches the volume after its files (every 5 s): when anything changed since the
/// last device flush, FSInfo gets its real count and the stick its SYNCHRONIZE CACHE, so a stick pulled a few
/// seconds after the last write holds everything (no reboot or `sync` needed).
impl vfs::writeback::Flush for Volume {
    fn flush(&self, why: vfs::pagecache::SyncKind) -> Result<(), i64> {
        if why.durable() {
            let changed = self.changes.load(Ordering::Relaxed);
            self.sync(true)?;
            self.flushed_at.store(changed, Ordering::Relaxed);
            return Ok(());
        }
        self.flush_if_changed()
    }
}

impl Volume {
    /// `sync(true)` when anything changed since the last one by the write-back.
    pub fn flush_if_changed(&self) -> Result<(), i64> {
        let changed = self.changes.load(Ordering::Relaxed);
        if changed == self.flushed_at.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.sync(true)?;
        self.flushed_at.store(changed, Ordering::Relaxed);
        Ok(())
    }
}

pub const MAX_DIR_BYTES: usize = 65536 * crate::dirent::ENTRY;

/// The volume lock (`Volume::lock`); while held, the holder's disk requests are lifted (`lift`).
pub(crate) struct VolGuard<'a> {
    // Declared first: dropped before the lock is released, so it never clears the next holder's mark.
    _held: Option<crate::lift::Held<'a>>,
    g: MutexGuard<'a, State>,
}

impl core::ops::Deref for VolGuard<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.g
    }
}

impl core::ops::DerefMut for VolGuard<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.g
    }
}

/// The SYS grant of one thread (`Volume::grant_sys`).
pub struct SysGrant<'a> {
    vol: &'a Volume,
    _g: MutexGuard<'a, ()>,
}

impl Drop for SysGrant<'_> {
    fn drop(&mut self) {
        *self.vol.sys_owner.lock() = None;
    }
}

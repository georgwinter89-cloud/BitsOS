//! SX-2: `/proc/<pid>/ns/` (linux-6.6/fs/proc/namespaces.c) and the user namespace files `uid_map`,
//! `gid_map`, `projid_map`, `setgroups` (fs/proc/base.c :3037-3200, kernel/user_namespace.c).
//!
//! The ns links: readlink shows `<kind>:[<inum>]`, an open gives a namespace file (setns takes it), a walk
//! through them lands on nsfs's place (`proc::ns::file`); readlink and follow need ptrace read access
//! (`proc_ns_get_link`: EACCES).
//!
//! The map files remember what Linux keeps in the open file: at the open, the target's user namespace and
//! the opener's credentials (`proc_id_map_open`, `f_cred`); a read shows the map relative to the opener, a
//! write is checked against the opener and the writer (`proc::ns::user::write_map`).

use super::{Target, may_read_mm};
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;
use proc::ns::Kind;
use proc::ns::file::{link_text, ns_of, open_file, place};
use proc::ns::user::{self, MapKind, Opened};
use uapi::errno::*;
use vfs::synth::{DirSource, FileSource, MagicLink, SynthEntry};
use vfs::{Cred, Errno, OpenFile};

/// `/proc/<pid>/ns`.
pub(super) struct NsDir(pub(super) Target);

fn entry(t: &Target, k: Kind, i: usize) -> Result<SynthEntry, Errno> {
    let (_, p) = t.live()?;
    let ns = ns_of(&p, k);
    let text = link_text(k, ns.inum()).into_bytes();
    Ok(super::owned(SynthEntry::magic(k.name(), 0o777, text, Arc::new(NsLink { t: t.clone(), k })), &p, false).cookie(i as u64 + 1))
}

impl DirSource for NsDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Kind::ALL.iter().enumerate().map(|(i, k)| entry(&self.0, *k, i)).collect()
    }
    fn lookup(&self, name: &[u8]) -> Result<Option<SynthEntry>, Errno> {
        match Kind::ALL.iter().enumerate().find(|(_, k)| k.name().as_bytes() == name) {
            Some((i, k)) => entry(&self.0, *k, i).map(Some),
            None => Ok(None),
        }
    }
}

struct NsLink {
    t: Target,
    k: Kind,
}

impl NsLink {
    fn check(&self) -> Result<Arc<proc::Process>, Errno> {
        let (_, p) = self.t.live().map_err(|_| ENOENT)?;
        if !may_read_mm(&p) {
            return Err(EACCES);
        }
        Ok(p)
    }
}

impl MagicLink for NsLink {
    fn target(&self) -> Result<vfs::Path, Errno> {
        let p = self.check()?;
        place(&ns_of(&p, self.k))
    }
    fn may_read(&self) -> Result<(), Errno> {
        self.check().map(|_| ())
    }
    /// open(2) through the link gives the namespace file itself (nsfs's dentry in Linux).
    fn open(&self, flags: u64) -> Option<Result<Arc<OpenFile>, Errno>> {
        Some(self.check().map(|p| open_file(ns_of(&p, self.k), flags)))
    }
}

/// Which user namespace file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MapWhich {
    Map(MapKind),
    Setgroups,
}

pub(super) struct MapFile {
    pub(super) t: Target,
    pub(super) which: MapWhich,
    opened: SpinLock<Option<Opened>>,
}

impl MapFile {
    pub(super) fn new(t: Target, which: MapWhich) -> MapFile {
        MapFile { t, which, opened: SpinLock::named("syscall.procfs.idmap", None) }
    }

    /// What the open recorded, or (a kernel reader, or no open seen) the target's namespace and the
    /// caller's credentials now.
    fn opened(&self) -> Result<Opened, Errno> {
        if let Some(o) = self.opened.lock().clone() {
            return Ok(o);
        }
        let (_, p) = self.t.live()?;
        let opener = proc::current().map_or_else(|| Arc::new(proc::cred::Cred::new(0, 0)), |me| me.cred());
        Ok(Opened { ns: p.cred().user_ns.clone(), opener })
    }
}

impl FileSource for MapFile {
    /// `proc_id_map_open` / `proc_setgroups_open`: the target's user namespace and the opener's
    /// credentials; setgroups opened for writing needs CAP_SYS_ADMIN in that namespace (EPERM).
    fn may_open(&self, _who: Cred, write: bool) -> Result<(), Errno> {
        let (_, p) = self.t.live()?;
        let opener = proc::current().map_or_else(|| Arc::new(proc::cred::Cred::new(0, 0)), |me| me.cred());
        let o = Opened { ns: p.cred().user_ns.clone(), opener };
        if write && self.which == MapWhich::Setgroups {
            user::may_open_setgroups(&o.ns, &o.opener)?;
        }
        *self.opened.lock() = Some(o);
        Ok(())
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        let o = self.opened()?;
        Ok(match self.which {
            MapWhich::Map(k) => user::render_map(&o.ns, k, &o.opener.user_ns).into_bytes(),
            MapWhich::Setgroups => (if o.ns.setgroups_allowed() { "allow\n" } else { "deny\n" }).as_bytes().to_vec(),
        })
    }
    fn write(&self, who: Cred, data: &[u8]) -> Result<usize, Errno> {
        self.write_at(who, 0, data)
    }
    fn write_at(&self, _who: Cred, pos: u64, data: &[u8]) -> Result<usize, Errno> {
        let o = self.opened()?;
        let writer = proc::current().map_or_else(|| o.opener.clone(), |me| me.cred());
        match self.which {
            MapWhich::Map(k) => user::write_map(&o, k, pos, data, &writer),
            MapWhich::Setgroups => user::write_setgroups(&o, pos, data),
        }
    }
    /// FS-16: `proc_id_map_open` is `seq_open`, `proc_setgroups_open` `single_open`: pwrite is ESPIPE.
    fn positional_write(&self, _opened_for_write: bool) -> bool {
        false
    }
}

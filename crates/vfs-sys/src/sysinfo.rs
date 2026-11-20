//! PCI's view under `/sys` (PC-4's `pci::sysfs`, FS-2). `/proc/cpuinfo`, `/proc/meminfo` and `/proc/uptime`
//! moved to AB's `syscall::procfs` (AB-7b, content providers belong to their owners).

use alloc::sync::Arc;
use alloc::vec::Vec;
use vfs::synth::{DirSource, FileSource, FlatEntry, FlatKind, FlatView};
use vfs::{Cred, Errno};

/// A PCI function's configuration space, read live (`pci_read_config`): without root only the first 64 bytes
/// (Linux: CAP_SYS_ADMIN for the rest).
struct Config {
    loc: pci::Loc,
    size: u16,
}

impl FileSource for Config {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(Vec::new())
    }
    fn live_size(&self) -> Option<u64> {
        Some(self.size as u64)
    }
    fn read_live(&self, who: Cred, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let limit = if who.uid == 0 { self.size as u64 } else { 64.min(self.size as u64) };
        if pos >= limit {
            return Ok(0);
        }
        let n = buf.len().min((limit - pos) as usize);
        Ok(pci::sysfs::read_config(self.loc, pos as usize, &mut buf[..n]))
    }
}

/// PCI's view in the VFS's form, kept while PCI hands out the same view object.
type Kept = (Arc<Vec<pci::sysfs::SysEntry>>, Arc<Vec<FlatEntry>>);
static PCI_KEPT: ksync::SpinLock<Option<Kept>> = ksync::SpinLock::named("vfs.sys.pci_view", None);

fn pci_entries() -> Arc<Vec<FlatEntry>> {
    let from = pci::sysfs::entries_shared();
    if let Some((_, flat)) = PCI_KEPT.lock().as_ref().filter(|(of, _)| Arc::ptr_eq(of, &from)) {
        return flat.clone();
    }
    let flat: Vec<FlatEntry> = from
        .iter()
        .map(|e| FlatEntry {
            path: e.path.clone(),
            kind: match &e.kind {
                pci::sysfs::SysKind::Dir => FlatKind::Dir,
                pci::sysfs::SysKind::Text(t) => FlatKind::Text(t.clone()),
                pci::sysfs::SysKind::Link(t) => FlatKind::Link(t.clone()),
                pci::sysfs::SysKind::Config { loc, size } => FlatKind::File { perm: 0o444, src: Arc::new(Config { loc: *loc, size: *size }) },
            },
        })
        .collect();
    let flat = Arc::new(flat);
    let old = core::mem::replace(&mut *PCI_KEPT.lock(), Some((from, flat.clone())));
    // Freed with the lock released.
    drop(old);
    flat
}

fn pci_view() -> Arc<dyn DirSource> {
    Arc::new(FlatView::new(pci_entries))
}
vfs::procdir!("/sys", pci_view);

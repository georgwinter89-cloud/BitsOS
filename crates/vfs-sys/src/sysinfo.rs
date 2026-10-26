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

fn pci_entries() -> Vec<FlatEntry> {
    pci::sysfs::entries()
        .into_iter()
        .map(|e| FlatEntry {
            path: e.path,
            kind: match e.kind {
                pci::sysfs::SysKind::Dir => FlatKind::Dir,
                pci::sysfs::SysKind::Text(t) => FlatKind::Text(t),
                pci::sysfs::SysKind::Link(t) => FlatKind::Link(t),
                pci::sysfs::SysKind::Config { loc, size } => FlatKind::File { perm: 0o444, src: Arc::new(Config { loc, size }) },
            },
        })
        .collect()
}

fn pci_view() -> Arc<dyn DirSource> {
    Arc::new(FlatView::new(pci_entries))
}
vfs::procdir!("/sys", pci_view);

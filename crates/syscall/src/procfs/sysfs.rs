//! AB-7b: AB's part of `/sys`, one source on `/sys` beside PCI's view (directories of the same name merge):
//! `devices/system/cpu` (`sys_cpu`), `devices/virtual/net` and `class/net` (`sys_net`).

use super::sys_cpu::cpu_root;
use super::sys_net::{net_class, net_devices};
use alloc::sync::Arc;
use alloc::vec::Vec;
use vfs::Errno;
use vfs::inode::Cred;
use vfs::synth::{DirSource, FileSource, SynthEntry};

/// A fixed directory of named subdirectories (sysfs mode 0755).
struct Fixed(&'static [(&'static str, fn() -> Arc<dyn DirSource>)]);

impl DirSource for Fixed {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(self.0.iter().enumerate().map(|(i, (n, f))| SynthEntry::dir(n, f()).with_perm(0o755).cookie(i as u64 + 1)).collect())
    }
}

fn system() -> Arc<dyn DirSource> {
    Arc::new(Fixed(&[("cpu", cpu_root)]))
}

fn virtual_() -> Arc<dyn DirSource> {
    Arc::new(Fixed(&[("net", net_devices)]))
}

fn devices() -> Arc<dyn DirSource> {
    Arc::new(Fixed(&[("system", system), ("virtual", virtual_)]))
}

fn class() -> Arc<dyn DirSource> {
    Arc::new(Fixed(&[("net", net_class)]))
}

/// MM-13: `/sys/kernel/mm/transparent_hugepage/enabled` (Linux `enabled_show`/`enabled_store`, mode 0644; a word
/// other than always/madvise/never is EINVAL).
struct ThpEnabled;

impl FileSource for ThpEnabled {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(mm::thp::enabled_text().as_bytes().to_vec())
    }
    fn write(&self, _who: Cred, data: &[u8]) -> Result<usize, Errno> {
        mm::thp::store(data).map(|_| data.len()).map_err(|_| uapi::errno::EINVAL)
    }
    fn size(&self) -> u64 {
        4096
    }
}

/// W35 MM-3: `/sys/kernel/mm/transparent_hugepage/defrag` (Linux `defrag_show`/`defrag_store`, mode 0644: every
/// mode with the active one in brackets; a word other than always/defer/defer+madvise/madvise/never is EINVAL).
struct ThpDefrag;

impl FileSource for ThpDefrag {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(mm::thp::defrag_text().into_bytes())
    }
    fn write(&self, _who: Cred, data: &[u8]) -> Result<usize, Errno> {
        mm::thp::defrag_store(data).map(|_| data.len()).map_err(|_| uapi::errno::EINVAL)
    }
    fn size(&self) -> u64 {
        4096
    }
}

/// MM-13: `/sys/kernel/mm/transparent_hugepage/hpage_pmd_size` (mode 0444).
struct ThpPmdSize;

impl FileSource for ThpPmdSize {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(mm::thp::hpage_pmd_size_text().as_bytes().to_vec())
    }
    fn size(&self) -> u64 {
        4096
    }
}

struct ThpDir;

impl DirSource for ThpDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(alloc::vec![
            SynthEntry::file("defrag", 0o644, Arc::new(ThpDefrag)).cookie(3),
            SynthEntry::file("enabled", 0o644, Arc::new(ThpEnabled)).cookie(1),
            SynthEntry::file("hpage_pmd_size", 0o444, Arc::new(ThpPmdSize)).cookie(2),
        ])
    }
}

fn transparent_hugepage() -> Arc<dyn DirSource> {
    Arc::new(ThpDir)
}

fn kernel_mm() -> Arc<dyn DirSource> {
    Arc::new(Fixed(&[("transparent_hugepage", transparent_hugepage)]))
}

fn kernel() -> Arc<dyn DirSource> {
    Arc::new(Fixed(&[("mm", kernel_mm)]))
}

fn sys_root() -> Arc<dyn DirSource> {
    Arc::new(Fixed(&[("class", class), ("devices", devices), ("kernel", kernel)]))
}
vfs::procdir!("/sys", sys_root);

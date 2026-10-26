//! AB-7b: AB's part of `/sys`, one source on `/sys` beside PCI's view (directories of the same name merge):
//! `devices/system/cpu` (`sys_cpu`), `devices/virtual/net` and `class/net` (`sys_net`).

use super::sys_cpu::cpu_root;
use super::sys_net::{net_class, net_devices};
use alloc::sync::Arc;
use alloc::vec::Vec;
use vfs::Errno;
use vfs::synth::{DirSource, SynthEntry};

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

fn sys_root() -> Arc<dyn DirSource> {
    Arc::new(Fixed(&[("class", class), ("devices", devices)]))
}
vfs::procdir!("/sys", sys_root);

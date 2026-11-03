//! The /sys entries libdrm's `drmGetDevices2` walks (gx-scout-uapi.md §2; linux-6.6/drivers/gpu/drm/drm_sysfs.c,
//! drivers/pci/pci-driver.c:1571 `pci_uevent`): `dev/char/226:<m>` and `class/drm/renderD<m>` as relative links
//! to `<function>/drm/renderD<m>` (with `dev`, `uevent`, `device` back to the function, `subsystem` to class/drm),
//! `class/drm/version`, and for a device without a PCI function behind it (the fake) the function directory itself:
//! vendor, device, subsystem_*, revision, class, modalias, uevent (PCI_SLOT_NAME), config, the `subsystem` link to
//! bus/pci. A real function's own files are PC's (change request in plan/notes/dr-DR-2.md). One `procdir!` on /sys:
//! the synthetic file system merges it with PCI's and AB's sources. Nothing is listed while no device exists.

use crate::device::{self, Device, Place, DRM_MAJOR};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use vfs::inode::Cred;
use vfs::synth::{DirSource, FileSource, FlatEntry, FlatKind, FlatView};
use vfs::Errno;

/// `CLASS_ATTR_STRING(version, ..)` (drm_sysfs.c:129).
const CLASS_VERSION: &str = "drm 1.1.0 20060810\n";

/// "../" once per component of `dir` (a link's target relative to the directory holding it).
fn up(dir: &str) -> String {
    "../".repeat(dir.split('/').filter(|c| !c.is_empty()).count())
}

fn dir(out: &mut Vec<FlatEntry>, path: &str) {
    if !out.iter().any(|e| e.path == path) {
        out.push(FlatEntry { path: String::from(path), kind: FlatKind::Dir });
    }
}

fn text(out: &mut Vec<FlatEntry>, path: String, t: String) {
    out.push(FlatEntry { path, kind: FlatKind::Text(t) });
}

fn link(out: &mut Vec<FlatEntry>, path: String, target: String) {
    out.push(FlatEntry { path, kind: FlatKind::Link(target) });
}

/// Every directory on the way to `path` (the flat view needs each level).
fn dirs_to(out: &mut Vec<FlatEntry>, path: &str) {
    let mut at = String::new();
    for c in path.split('/').filter(|c| !c.is_empty()) {
        if !at.is_empty() {
            at.push('/');
        }
        at.push_str(c);
        dir(out, &at);
    }
}

/// The first 64 bytes of a type-0 configuration header (all a non-root reader of Linux's `config` sees), then
/// zeros up to 256.
pub fn config_bytes(d: &Device) -> Vec<u8> {
    let p = d.driver().pci();
    let mut c = alloc::vec![0u8; 256];
    c[0..2].copy_from_slice(&p.vendor.to_le_bytes());
    c[2..4].copy_from_slice(&p.device.to_le_bytes());
    // Memory space and bus master on, capability list off (no capabilities are faked).
    c[4..6].copy_from_slice(&0x0006u16.to_le_bytes());
    c[8] = p.revision;
    c[9] = p.class as u8;
    c[10] = (p.class >> 8) as u8;
    c[11] = (p.class >> 16) as u8;
    c[0x2c..0x2e].copy_from_slice(&p.subsystem_vendor.to_le_bytes());
    c[0x2e..0x30].copy_from_slice(&p.subsystem_device.to_le_bytes());
    c
}

struct Config(Vec<u8>);

impl FileSource for Config {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(self.0.clone())
    }
    fn live_size(&self) -> Option<u64> {
        Some(self.0.len() as u64)
    }
    fn read_live(&self, _who: Cred, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let Some(rest) = usize::try_from(pos).ok().and_then(|p| self.0.get(p..)) else { return Ok(0) };
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }
}

/// The PCI function's own files, for a device that has no function in PCI's view.
fn function_files(out: &mut Vec<FlatEntry>, d: &Device, fdir: &str) {
    let p = d.driver().pci();
    let at = |name: &str| format!("{fdir}/{name}");
    text(out, at("vendor"), format!("0x{:04x}\n", p.vendor));
    text(out, at("device"), format!("0x{:04x}\n", p.device));
    text(out, at("subsystem_vendor"), format!("0x{:04x}\n", p.subsystem_vendor));
    text(out, at("subsystem_device"), format!("0x{:04x}\n", p.subsystem_device));
    text(out, at("revision"), format!("0x{:02x}\n", p.revision));
    text(out, at("class"), format!("0x{:06x}\n", p.class));
    let modalias = format!(
        "pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{:02X}sc{:02X}i{:02X}",
        p.vendor, p.device, p.subsystem_vendor, p.subsystem_device, (p.class >> 16) as u8, (p.class >> 8) as u8, p.class as u8
    );
    text(out, at("modalias"), format!("{modalias}\n"));
    // dev_uevent's DRIVER= first, then pci_uevent's keys (pci-driver.c:1571-1588).
    text(
        out,
        at("uevent"),
        format!(
            "DRIVER={}\nPCI_CLASS={:04X}\nPCI_ID={:04X}:{:04X}\nPCI_SUBSYS_ID={:04X}:{:04X}\nPCI_SLOT_NAME={}\nMODALIAS={modalias}\n",
            p.driver,
            p.class,
            p.vendor,
            p.device,
            p.subsystem_vendor,
            p.subsystem_device,
            p.slot_name()
        ),
    );
    out.push(FlatEntry { path: at("config"), kind: FlatKind::File { perm: 0o644, src: Arc::new(Config(config_bytes(d))) } });
    link(out, at("subsystem"), format!("{}bus/pci", up(fdir)));
    link(out, at("driver"), format!("{}bus/pci/drivers/{}", up(fdir), p.driver));
}

/// The whole DRM view below /sys, sorted by path.
pub fn entries() -> Vec<FlatEntry> {
    entries_of(&device::devices())
}

pub fn entries_of(devs: &[Arc<Device>]) -> Vec<FlatEntry> {
    let mut out = Vec::new();
    if devs.is_empty() {
        return out;
    }
    dirs_to(&mut out, "class/drm");
    text(&mut out, String::from("class/drm/version"), String::from(CLASS_VERSION));
    dirs_to(&mut out, "dev/char");
    for d in devs {
        let p = d.driver().pci();
        let fdir = p.sysfs_dir();
        let m = d.render_minor();
        let name = format!("renderD{m}");
        let mdir = format!("{fdir}/drm/{name}");
        dirs_to(&mut out, &mdir);
        if matches!(p.place, Place::Own) {
            function_files(&mut out, d, &fdir);
        }
        text(&mut out, format!("{mdir}/dev"), format!("{DRM_MAJOR}:{m}\n"));
        text(&mut out, format!("{mdir}/uevent"), format!("MAJOR={DRM_MAJOR}\nMINOR={m}\nDEVNAME=dri/{name}\nDEVTYPE=drm_minor\n"));
        link(&mut out, format!("{mdir}/device"), format!("../../../{}", p.slot_name()));
        link(&mut out, format!("{mdir}/subsystem"), format!("{}class/drm", up(&mdir)));
        link(&mut out, format!("class/drm/{name}"), format!("../../{mdir}"));
        link(&mut out, format!("dev/char/{DRM_MAJOR}:{m}"), format!("../../{mdir}"));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn view() -> Arc<dyn DirSource> {
    Arc::new(FlatView::new(entries))
}
vfs::procdir!("/sys", view);

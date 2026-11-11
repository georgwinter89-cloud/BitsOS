//! The Windows answers the installer reads, parsed from their raw bytes so that the host's tests check them: the
//! storage descriptor (bus type, removable, names), the drive geometry, a volume's disk extents, the multi-strings
//! of mount points and page files, and from all that the `Disk` the list shows. Layouts are those of the 64-bit
//! Windows SDK (winioctl.h).

use super::{Disk, disk_id};
use std::path::PathBuf;

/// `STORAGE_BUS_TYPE::BusTypeUsb`.
pub const BUS_USB: i32 = 7;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Descriptor {
    pub bus: i32,
    pub removable: bool,
    pub vendor: String,
    pub product: String,
    pub serial: String,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A NUL-terminated ASCII string at `off` inside the descriptor; 0 (and anything outside it) means none.
fn c_str(b: &[u8], off: u32) -> String {
    let off = off as usize;
    if off == 0 || off >= b.len() {
        return String::new();
    }
    let s = &b[off..];
    let s = &s[..s.iter().position(|c| *c == 0).unwrap_or(s.len())];
    String::from_utf8_lossy(s).chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

/// `STORAGE_DEVICE_DESCRIPTOR` as IOCTL_STORAGE_QUERY_PROPERTY returned it; its own Size field bounds it.
pub fn descriptor(raw: &[u8]) -> Option<Descriptor> {
    let size = (u32_at(raw, 4)? as usize).min(raw.len());
    let b = &raw[..size];
    if b.len() < 36 {
        return None;
    }
    Some(Descriptor {
        removable: b[10] != 0,
        vendor: c_str(b, u32_at(b, 12)?),
        product: c_str(b, u32_at(b, 16)?),
        serial: c_str(b, u32_at(b, 24)?),
        bus: u32_at(b, 28)? as i32,
    })
}

/// `DISK_GEOMETRY_EX`: (bytes per sector, disk size in bytes).
pub fn geometry(b: &[u8]) -> Option<(u64, u64)> {
    let sector = u32_at(b, 20)? as u64;
    let size = u64_at(b, 24)?;
    (sector > 0).then_some((sector, size))
}

/// `VOLUME_DISK_EXTENTS`: the disk numbers a volume lies on.
pub fn extents(b: &[u8]) -> Option<Vec<u32>> {
    let n = u32_at(b, 0)? as usize;
    (0..n).map(|i| u32_at(b, 8 + i * 24)).collect()
}

/// A list of NUL-terminated UTF-16 strings ending in an empty one (mount points, REG_MULTI_SZ).
pub fn multi_sz(w: &[u16]) -> Vec<String> {
    w.split(|c| *c == 0).take_while(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}

/// The drive of one `ExistingPageFiles` entry (`\??\C:\pagefile.sys` -> `C:\`).
pub fn page_file_drive(s: &str) -> Option<String> {
    let s = s.strip_prefix("\\??\\").unwrap_or(s);
    let b = s.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':').then(|| format!("{}:\\", b[0].to_ascii_uppercase() as char))
}

/// One volume as the list sees it.
#[derive(Clone, Debug, Default)]
pub struct Volume {
    /// `\\?\Volume{...}\`.
    pub name: String,
    pub disks: Vec<u32>,
    /// Drive letters and folders it is mounted at.
    pub paths: Vec<String>,
    /// Why it belongs to the running Windows (its own volume, the boot partition, a page file).
    pub role: Option<String>,
}

/// The disk `n` from what Windows said about it; `geom` is `None` when the drive has no medium.
pub fn disk(n: u32, d: &Descriptor, geom: Option<(u64, u64)>, vols: &[Volume]) -> Disk {
    let size = geom.map_or(0, |g| g.1);
    let mine = vols.iter().filter(|v| v.disks.contains(&n));
    let mut mounted: Vec<String> = mine.clone().flat_map(|v| v.paths.iter().cloned()).collect();
    mounted.sort();
    mounted.dedup();
    let system = mine.clone().find_map(|v| v.role.clone());
    Disk {
        id: disk_id(&format!("disk{n}"), size, &d.vendor, &d.product, &d.serial),
        path: PathBuf::from(format!("\\\\.\\PhysicalDrive{n}")),
        vendor: d.vendor.clone(),
        model: d.product.clone(),
        serial: d.serial.clone(),
        size,
        removable: d.removable,
        usb: d.bus == BUS_USB,
        mounted,
        system,
    }
}

/// The volumes of disk `n`, for locking and dismounting.
pub fn volumes_of(n: u32, vols: &[Volume]) -> Vec<Volume> {
    vols.iter().filter(|v| v.disks.contains(&n)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A descriptor as a USB stick's driver fills it in.
    fn raw_descriptor(bus: i32, removable: bool, vendor: &str, product: &str, serial: Option<&str>) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        b[0..4].copy_from_slice(&40u32.to_le_bytes()); // Version: the structure's size
        b[8] = 0; // DIRECT_ACCESS_DEVICE
        b[10] = removable as u8;
        let put = |s: &str, field: usize, b: &mut Vec<u8>| {
            let off = b.len() as u32;
            b.extend_from_slice(s.as_bytes());
            b.push(0);
            b[field..field + 4].copy_from_slice(&off.to_le_bytes());
        };
        put(vendor, 12, &mut b);
        put(product, 16, &mut b);
        put("1.00", 20, &mut b);
        if let Some(s) = serial {
            put(s, 24, &mut b);
        }
        b[28..32].copy_from_slice(&bus.to_le_bytes());
        let size = b.len() as u32;
        b[4..8].copy_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&[0xee; 16]); // bytes past Size are not the descriptor's
        b
    }

    #[test]
    fn descriptors_give_bus_removable_and_names() {
        let d = descriptor(&raw_descriptor(BUS_USB, true, "SanDisk ", "Cruzer Blade    ", Some("4C530001230726111403"))).unwrap();
        assert_eq!(d, Descriptor { bus: BUS_USB, removable: true, vendor: "SanDisk".into(), product: "Cruzer Blade".into(), serial: "4C530001230726111403".into() });
        let nvme = descriptor(&raw_descriptor(17, false, "", "Samsung SSD 990 PRO 2TB", None)).unwrap();
        assert_eq!((nvme.bus, nvme.removable, nvme.serial.as_str(), nvme.vendor.as_str()), (17, false, "", ""));
        assert!(descriptor(&[0u8; 20]).is_none());
        // An offset that points outside the returned bytes is no string, not a crash.
        let mut b = raw_descriptor(BUS_USB, true, "V", "P", None);
        b[24..28].copy_from_slice(&5000u32.to_le_bytes());
        assert_eq!(descriptor(&b).unwrap().serial, "");
    }

    #[test]
    fn geometry_and_extents() {
        let mut g = vec![0u8; 40];
        g[20..24].copy_from_slice(&512u32.to_le_bytes());
        g[24..32].copy_from_slice(&31_004_295_168u64.to_le_bytes());
        assert_eq!(geometry(&g), Some((512, 31_004_295_168)));
        assert_eq!(geometry(&[0u8; 40]), None);
        let mut e = vec![0u8; 8 + 2 * 24];
        e[0..4].copy_from_slice(&2u32.to_le_bytes());
        e[8..12].copy_from_slice(&3u32.to_le_bytes());
        e[32..36].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(extents(&e), Some(vec![3, 5]));
        assert_eq!(extents(&e[..20]), None);
    }

    #[test]
    fn multi_strings_and_page_files() {
        let w: Vec<u16> = "E:\\\0D:\\Mnt\\stick\\\0\0garbage".encode_utf16().collect();
        assert_eq!(multi_sz(&w), ["E:\\", "D:\\Mnt\\stick\\"]);
        assert!(multi_sz(&[0, 0]).is_empty());
        assert_eq!(page_file_drive("\\??\\c:\\pagefile.sys").as_deref(), Some("C:\\"));
        assert_eq!(page_file_drive("D:\\pagefile.sys").as_deref(), Some("D:\\"));
        assert_eq!(page_file_drive("?:\\pagefile.sys"), None);
    }

    #[test]
    fn only_removable_usb_without_the_system_is_offered() {
        let stick = Descriptor { bus: BUS_USB, removable: true, vendor: "SanDisk".into(), product: "Cruzer Blade".into(), serial: "4C53".into() };
        let usb_ssd_fixed = Descriptor { bus: BUS_USB, removable: false, ..stick.clone() };
        let nvme = Descriptor { bus: 17, removable: false, ..Descriptor::default() };
        let sd_internal = Descriptor { bus: 12, removable: true, ..Descriptor::default() }; // BusTypeSd
        let vols = [
            Volume { name: "\\\\?\\Volume{c}\\".into(), disks: vec![0], paths: vec!["C:\\".into()], role: Some("it holds the running Windows (C:\\)".into()) },
            Volume { name: "\\\\?\\Volume{e}\\".into(), disks: vec![2], paths: vec!["E:\\".into()], role: None },
            Volume { name: "\\\\?\\Volume{f}\\".into(), disks: vec![2], paths: vec![], role: None },
            Volume { name: "\\\\?\\Volume{w}\\".into(), disks: vec![3], paths: vec!["W:\\".into()], role: Some("it holds the running Windows (W:\\)".into()) },
        ];
        let g = Some((512, 31_004_295_168));
        let d0 = disk(0, &nvme, Some((512, 2_000_398_934_016)), &vols);
        let d1 = disk(1, &sd_internal, g, &vols);
        let d2 = disk(2, &stick, g, &vols);
        let d3 = disk(3, &stick, g, &vols); // Windows To Go: a USB stick that runs Windows
        let d4 = disk(4, &stick, None, &vols); // a card reader slot without a card
        let d5 = disk(5, &usb_ssd_fixed, g, &vols);
        assert!(!d0.offered() && !d1.offered() && !d3.offered() && !d4.offered() && !d5.offered());
        assert!(d2.offered());
        assert_eq!(d2.id, disk_id("disk2", 31_004_295_168, "SanDisk", "Cruzer Blade", "4C53"));
        assert!(d5.why_not().unwrap().contains("fixed disk"));
        assert_eq!(d2.path, PathBuf::from("\\\\.\\PhysicalDrive2"));
        assert_eq!(d2.mounted, ["E:\\"]);
        assert!(d3.system.as_deref().unwrap().contains("running Windows"));
        assert_eq!(d4.size, 0);
        assert_eq!(volumes_of(2, &vols).len(), 2);
        assert!(d2.describe().starts_with(&format!("{}  31.0 GB  SanDisk Cruzer Blade, serial 4C53, mounted at E:\\", d2.id)));
    }
}

//! The disks the installer may offer, behind one boundary per operating system: `linux` reads sysfs, `windows` asks
//! the storage drivers (`winparse` reads their answers, `aligned` gives Windows' raw disks whole-sector I/O; both
//! are plain Rust and tested on any host). Whatever the system, a disk is offered only when it is a removable USB
//! mass-storage disk with a medium in it and no partition of it carries the running system.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(windows)]
#[allow(unsafe_code)]
pub mod windows;
#[cfg(any(windows, test))]
pub mod aligned;
#[cfg(any(windows, test))]
pub mod winparse;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    /// What the human types to confirm: the system's name for the disk, its size and a fingerprint of its vendor,
    /// model and serial (`sdb-31.0G-3fa2`).
    pub id: String,
    /// Where it is opened (`/dev/sdb`).
    pub path: PathBuf,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub size: u64,
    pub removable: bool,
    pub usb: bool,
    /// Mount points of its partitions that are not the system's (a stick the desktop mounted).
    pub mounted: Vec<String>,
    /// Why it is never offered: it carries the running system (a mount point, swap or a device-mapper user), or it
    /// cannot carry a stick BitsOS reads (sectors other than 512 bytes).
    pub system: Option<String>,
}

impl Disk {
    /// Offered at all (the list shows only these).
    pub fn offered(&self) -> bool {
        self.why_not().is_none()
    }

    /// Why the list shows the disk but does not offer it.
    pub fn why_not(&self) -> Option<String> {
        if let Some(s) = &self.system {
            return Some(s.clone());
        }
        if !self.usb {
            return Some("it is not a USB disk".into());
        }
        if self.size == 0 {
            return Some("no medium in it".into());
        }
        // Many USB SSDs and some fast sticks say "fixed": the same answer an internal disk gives.
        (!self.removable).then(|| "it reports itself as a fixed disk, not a removable stick".into())
    }

    pub fn describe(&self) -> String {
        let name = [self.vendor.as_str(), self.model.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
        let serial = if self.serial.is_empty() { String::new() } else { format!(", serial {}", self.serial) };
        let mounted = if self.mounted.is_empty() { String::new() } else { format!(", mounted at {}", self.mounted.join(" ")) };
        format!("{}  {}  {}{serial}{mounted}", self.id, size_text(self.size), if name.is_empty() { "(no name)" } else { &name })
    }

    /// Two looks at a disk name the same physical disk (nothing was swapped in between).
    pub fn same_as(&self, o: &Disk) -> bool {
        self.id == o.id && self.path == o.path && self.size == o.size && self.serial == o.serial && self.model == o.model && self.vendor == o.vendor
    }
}

/// `31.0 GB` style (powers of 1000, as the stick's box says).
pub fn size_text(b: u64) -> String {
    if b >= 1_000_000_000 { format!("{:.1} GB", b as f64 / 1e9) } else { format!("{:.0} MB", b as f64 / 1e6) }
}

/// An image file's id: its name and its size in GB with one decimal.
pub fn make_id(name: &str, size: u64) -> String {
    format!("{name}-{:.1}G", size as f64 / 1e9)
}

/// A disk's id: its name and size, and four hex digits of its vendor, model and serial. The system's name for a disk
/// is reused (Linux's `sdb`, Windows' drive numbers); with the fingerprint an id read in an earlier `--list` no longer
/// matches another stick of the same size that took the name since.
pub fn disk_id(name: &str, size: u64, vendor: &str, model: &str, serial: &str) -> String {
    let mut h = crate::sha::Sha256::default();
    h.update(format!("{vendor}\0{model}\0{serial}").as_bytes());
    format!("{}-{}", make_id(name, size), &h.hex()[..4])
}

/// A disk (or image file) opened for writing.
pub trait BlockDev: Read + Write + Seek {
    fn size(&self) -> u64;
    /// Everything written so far reaches the device.
    fn sync(&mut self) -> io::Result<()>;
}

/// One operating system's way to find and open disks.
pub trait Devices {
    /// Every disk the system knows that is USB and removable (offered or not, with `system` saying why not).
    fn list(&self) -> Result<Vec<Disk>, String>;
    /// Opens `d` exclusively for writing; fails while one of its partitions is mounted.
    fn open(&self, d: &Disk) -> Result<Box<dyn BlockDev>, String>;
}

/// The devices of the system this installer runs on.
pub fn system() -> Result<Box<dyn Devices>, String> {
    #[cfg(target_os = "linux")]
    return Ok(Box::new(linux::Linux::system()));
    #[cfg(windows)]
    return Ok(Box::new(windows::Windows));
    #[cfg(not(any(target_os = "linux", windows)))]
    Err("this installer knows the disks of Linux and Windows only".into())
}

/// An image file standing in for a stick (tests, `--image`).
pub struct ImageFile {
    f: std::fs::File,
    size: u64,
}

impl Read for ImageFile {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.f.read(b)
    }
}
impl Write for ImageFile {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.f.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.f.flush()
    }
}
impl Seek for ImageFile {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.f.seek(p)
    }
}
impl BlockDev for ImageFile {
    fn size(&self) -> u64 {
        self.size
    }
    fn sync(&mut self) -> io::Result<()> {
        self.f.sync_all()
    }
}

/// Places an image file may never be: device and kernel file systems.
fn device_like(p: &Path) -> bool {
    let s = p.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    // Windows' canonical form of an ordinary path is verbatim (`\\?\C:\...`, `\\?\UNC\...`); other verbatim names
    // (`\\?\Volume{...}`, `\\?\GLOBALROOT\...`) are devices.
    let s = match s.strip_prefix("//?/") {
        Some(r) if r.len() >= 3 && r.as_bytes()[0].is_ascii_alphabetic() && &r[1..3] == ":/" => r.to_string(),
        Some(r) if r.starts_with("unc/") => format!("//{}", &r[4..]),
        _ => s,
    };
    ["/dev/", "/sys/", "/proc/", "/run/udev/"].iter().any(|d| s.starts_with(d)) || s.starts_with("//./") || s.starts_with("//?/") || s.contains("physicaldrive")
}

/// Opens (or creates, `size` bytes, sparse, readable by its owner only) a regular image file. A device path, a link
/// to one, or anything that is not a regular file is refused: `--image` is for tests and never reaches a disk. An
/// existing file is never truncated: opening happens before the confirmation, so it keeps its bytes until the write.
pub fn open_image(p: &Path, size: Option<u64>) -> Result<ImageFile, String> {
    let refuse = || Err(format!("refused: --image {} is not a plain file (devices are chosen from the list, never by path)", p.display()));
    if device_like(p) {
        return refuse();
    }
    if let Ok(m) = std::fs::symlink_metadata(p)
        && !m.file_type().is_file()
    {
        return refuse();
    }
    let parent = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let canon = parent.canonicalize().map_err(|e| format!("--image {}: {e}", p.display()))?;
    if device_like(&canon.join("x")) {
        return refuse();
    }
    let existed = std::fs::symlink_metadata(p).is_ok();
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true).create(size.is_some()).truncate(false);
    // An image can carry the remote-control token in its CONFIG.TXT: a new one is its owner's alone from the start.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let f = o.open(p).map_err(|e| format!("--image {}: {e}", p.display()))?;
    let meta = f.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return refuse();
    }
    match size {
        Some(s) if existed && meta.len() != s => {
            return Err(format!("refused: --image {} exists with {} bytes, not --image-size {s}; remove it first or leave --image-size out", p.display(), meta.len()));
        }
        Some(s) if !existed => {
            #[cfg(windows)]
            windows::vm::set_sparse(&f).map_err(|e| format!("--image {}: cannot make it sparse: {e}", p.display()))?;
            f.set_len(s).map_err(|e| format!("--image {}: {e}", p.display()))?
        }
        _ => {}
    }
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    if size < 64 << 20 {
        return Err(format!("--image {}: {} bytes is too small for a stick (give --image-size)", p.display(), size));
    }
    Ok(ImageFile { f, size })
}

/// The pseudo-disk an image file is listed as.
pub fn image_disk(p: &Path, size: u64) -> Disk {
    Disk {
        id: make_id("image", size),
        path: p.to_path_buf(),
        vendor: String::new(),
        model: "image file".into(),
        serial: String::new(),
        size,
        removable: true,
        usb: true,
        mounted: Vec::new(),
        system: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_paths_never_reach_devices() {
        for p in ["/dev/sdb", "/dev/disk/by-id/usb-x", "/sys/block/sdb", "\\\\.\\PhysicalDrive1", "//./physicaldrive2"] {
            assert!(open_image(Path::new(p), Some(1 << 30)).is_err(), "{p}");
        }
        let d = std::env::temp_dir().join(format!("bi-img-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/dev/null", d.join("link")).unwrap();
            assert!(open_image(&d.join("link"), Some(1 << 30)).is_err());
        }
        let img = open_image(&d.join("a.img"), Some(128 << 20)).unwrap();
        assert_eq!(img.size(), 128 << 20);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.join("a.img")).unwrap().permissions().mode() & 0o777, 0o600, "the owner's alone from the start");
        }
        assert!(open_image(&d.join("b.img"), Some(1 << 20)).is_err(), "too small");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_existing_image_keeps_its_bytes_until_the_write() {
        let d = std::env::temp_dir().join(format!("bi-img-keep-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("old.img");
        let mut old = vec![0u8; 128 << 20];
        old[..5].copy_from_slice(b"stick");
        std::fs::write(&p, &old).unwrap();
        assert!(open_image(&p, Some(256 << 20)).err().unwrap().contains("exists"), "another size is refused");
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 128 << 20);
        let mut img = open_image(&p, Some(128 << 20)).unwrap();
        let mut b = [0u8; 5];
        img.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"stick", "opening the same size truncates nothing");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn windows_paths_of_files_are_not_devices() {
        for p in ["\\\\?\\C:\\Users\\me\\stick.img", "\\\\?\\UNC\\server\\share\\stick.img", "C:\\Users\\me\\stick.img", "Z:/tmp/x.img"] {
            assert!(!device_like(Path::new(p)), "{p}");
        }
        for p in ["\\\\?\\Volume{0a1b2c3d-0000-0000-0000-000000000000}\\", "\\\\?\\GLOBALROOT\\Device\\Harddisk1\\Partition0", "\\\\.\\E:", "\\\\?\\C:\\x\\PhysicalDrive1"] {
            assert!(device_like(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn ids_carry_the_size_and_who_the_disk_is() {
        assert_eq!(make_id("image", 31_004_295_168), "image-31.0G");
        assert_eq!(size_text(31_004_295_168), "31.0 GB");
        let a = disk_id("sdb", 31_004_295_168, "SanDisk", "Cruzer Blade", "4C530001230726111403");
        assert!(a.starts_with("sdb-31.0G-") && a.len() == "sdb-31.0G-".len() + 4, "{a}");
        assert_eq!(a, disk_id("sdb", 31_004_295_168, "SanDisk", "Cruzer Blade", "4C530001230726111403"));
        // Another stick of the same size under the same name: another id.
        assert_ne!(a, disk_id("sdb", 31_004_295_168, "SanDisk", "Cruzer Blade", "4C530001230726111404"));
        assert_ne!(a, disk_id("sdb", 31_004_295_168, "Kingston", "DataTraveler", "4C530001230726111403"));
    }
}

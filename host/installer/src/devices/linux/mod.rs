//! Disks on Linux, from sysfs, `/proc/self/mountinfo` and `/proc/swaps` (all under one root, so tests read a made-up
//! tree). A disk counts as USB when an ancestor of its sysfs node is a USB device (it has `idVendor`); removable is
//! the kernel's own `removable` flag. A disk carries the system when one of its partitions, or anything stacked on
//! one (device mapper, md), is mounted at a system path or used as swap. The one unsafe call is the ioctl that
//! drops a written stick's page cache, so the read-back reads the stick.

use super::{BlockDev, Devices, Disk, disk_id};
use std::collections::BTreeSet;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Mount points that belong to the running system. Anything else (a desktop's /run/media/...) is only "mounted".
const SYSTEM_MOUNTS: &[&str] = &["/", "/boot", "/boot/efi", "/efi", "/usr", "/var", "/home", "/opt", "/srv", "/nix", "/gnu", "/etc", "/root", "/tmp"];

pub struct Linux {
    /// `/` normally; a test's fake tree otherwise.
    pub root: PathBuf,
}

impl Linux {
    pub fn system() -> Linux {
        Linux { root: PathBuf::from("/") }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel.trim_start_matches('/'))
    }
}

fn read_trim(p: &Path) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// A mountinfo field with its octal escapes (`\040` for a space) undone.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 4]).unwrap_or("0"), 8).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Mount {
    dev: String,
    point: String,
    source: String,
}

fn mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            let sep = f.iter().position(|x| *x == "-")?;
            Some(Mount { dev: f.get(2)?.to_string(), point: unescape(f.get(4)?), source: unescape(f.get(sep + 2)?) })
        })
        .collect()
}

/// Names of the block devices that are this disk, its partitions, and everything stacked on them.
fn family(sys_block: &Path, disk_node: &Path, name: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut names = BTreeSet::from([name.to_string()]);
    let mut devs = BTreeSet::new();
    let mut todo: Vec<PathBuf> = vec![disk_node.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(disk_node) {
        for e in rd.flatten() {
            if e.path().join("partition").is_file() {
                names.insert(e.file_name().to_string_lossy().into_owned());
                todo.push(e.path());
            }
        }
    }
    let mut seen = BTreeSet::new();
    while let Some(n) = todo.pop() {
        if !seen.insert(n.clone()) || seen.len() > 256 {
            continue;
        }
        let dev = read_trim(&n.join("dev"));
        if !dev.is_empty() {
            devs.insert(dev);
        }
        if let Ok(rd) = std::fs::read_dir(n.join("holders")) {
            for h in rd.flatten() {
                let hn = h.file_name().to_string_lossy().into_owned();
                names.insert(hn.clone());
                todo.push(sys_block.join(&hn));
            }
        }
    }
    (names, devs)
}

impl Linux {
    fn disk(&self, name: &str, mounts: &[Mount], swaps: &[String]) -> Option<Disk> {
        let sys_block = self.p("sys/block");
        let link = sys_block.join(name);
        let node = std::fs::canonicalize(&link).ok()?;
        node.join("device").exists().then_some(())?;
        let size = read_trim(&node.join("size")).parse::<u64>().ok()? * 512;
        let removable = read_trim(&node.join("removable")) == "1";
        let sector = read_trim(&node.join("queue/logical_block_size"));
        let devices = self.p("sys/devices");
        let usb_dev = node.ancestors().take_while(|a| a.starts_with(&devices)).find(|a| a.join("idVendor").is_file()).map(Path::to_path_buf);
        let usb = usb_dev.is_some();
        let dev_dir = node.join("device");
        let mut vendor = read_trim(&dev_dir.join("vendor"));
        let mut model = read_trim(&dev_dir.join("model"));
        let mut serial = String::new();
        if let Some(u) = &usb_dev {
            serial = read_trim(&u.join("serial"));
            if vendor.is_empty() {
                vendor = read_trim(&u.join("manufacturer"));
            }
            if model.is_empty() {
                model = read_trim(&u.join("product"));
            }
        }
        let (names, devs) = family(&sys_block, &node, name);
        let ours = |m: &Mount| devs.contains(&m.dev) || m.source.strip_prefix("/dev/").is_some_and(|s| names.contains(s) || names.contains(s.trim_start_matches("mapper/")));
        let mut system = None;
        let mut mounted = Vec::new();
        for m in mounts.iter().filter(|m| ours(m)) {
            if SYSTEM_MOUNTS.contains(&m.point.as_str()) {
                system.get_or_insert(format!("it holds the running system's {} ({})", m.point, m.source));
            } else {
                mounted.push(m.point.clone());
            }
        }
        for s in swaps {
            if s.strip_prefix("/dev/").is_some_and(|n| names.contains(n)) {
                system.get_or_insert(format!("it holds the running system's swap ({s})"));
            }
        }
        // Something stacked on it (LVM, LUKS, RAID) is in use even when nothing of it is mounted here.
        if system.is_none() && names.iter().any(|n| n.starts_with("dm-") || n.starts_with("md")) {
            system = Some("a device mapper or RAID device sits on it".into());
        }
        // The partition table counts 512-byte sectors, as the firmware and BitsOS read a stick.
        if system.is_none() && sector != "512" {
            system = Some(format!("it has {}-byte sectors; a BitsOS stick needs 512", if sector.is_empty() { "unknown" } else { &sector }));
        }
        mounted.sort();
        mounted.dedup();
        Some(Disk { id: disk_id(name, size, &vendor, &model, &serial), path: self.p("dev").join(name), vendor, model, serial, size, removable, usb, mounted, system })
    }
}

impl Devices for Linux {
    fn list(&self) -> Result<Vec<Disk>, String> {
        let sys_block = self.p("sys/block");
        let rd = std::fs::read_dir(&sys_block).map_err(|e| format!("{}: {e}", sys_block.display()))?;
        let ms = mounts(&std::fs::read_to_string(self.p("proc/self/mountinfo")).map_err(|e| format!("mountinfo: {e}"))?);
        let swaps: Vec<String> = std::fs::read_to_string(self.p("proc/swaps")).unwrap_or_default().lines().skip(1).filter_map(|l| l.split_whitespace().next().map(unescape)).collect();
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        // USB disks that say "fixed" are listed too, as not offered, so a stick that does is not simply missing.
        Ok(names.iter().filter_map(|n| self.disk(n, &ms, &swaps)).filter(|d| d.usb).collect())
    }

    fn open(&self, d: &Disk) -> Result<Box<dyn BlockDev>, String> {
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
        if let Some(why) = d.why_not() {
            return Err(format!("refused: {} is not offered ({why})", d.id));
        }
        if !d.mounted.is_empty() {
            return Err(format!(
                "refused: {} is mounted at {}; unmount it first (in the file manager, without unplugging it, or: sudo umount {}). Nothing was written.",
                d.id,
                d.mounted.join(" "),
                d.mounted.iter().map(|m| shell_word(m)).collect::<Vec<_>>().join(" ")
            ));
        }
        // O_EXCL on a block device: the kernel refuses while anything holds it (a mount, another opener with O_EXCL).
        const O_EXCL: i32 = 0o200;
        let f = std::fs::OpenOptions::new().read(true).write(true).custom_flags(O_EXCL).open(&d.path).map_err(|e| match e.kind() {
            io::ErrorKind::PermissionDenied => format!("{}: {e}: writing a whole disk needs root (sudo)", d.path.display()),
            _ => format!("{}: {e} (still mounted or in use?)", d.path.display()),
        })?;
        let ft = f.metadata().map_err(|e| e.to_string())?.file_type();
        if !ft.is_block_device() {
            return Err(format!("refused: {} is not a block device", d.path.display()));
        }
        let mut f = f;
        let size = f.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        if size != d.size {
            return Err(format!("refused: {} now has {size} bytes, the list said {}. Nothing was written.", d.path.display(), d.size));
        }
        f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        Ok(Box::new(DevFile { f, size }))
    }
}

struct DevFile {
    f: std::fs::File,
    size: u64,
}

impl Read for DevFile {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.f.read(b)
    }
}
impl Write for DevFile {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.f.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.f.flush()
    }
}
impl Seek for DevFile {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.f.seek(p)
    }
}
impl BlockDev for DevFile {
    fn size(&self) -> u64 {
        self.size
    }
    fn sync(&mut self) -> io::Result<()> {
        self.f.sync_all()?;
        drop_cache(&self.f)
    }
}

/// Once the writes are on the stick, the device's cached pages go, so the read-back that follows reads the stick
/// itself: a stick that is smaller than it says, or flash that fails, shows there instead of in RAM.
#[allow(unsafe_code)]
fn drop_cache(f: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // _IO(0x12, 97): write back and invalidate the block device's buffer cache (needs CAP_SYS_ADMIN, as writing does).
    const BLKFLSBUF: std::ffi::c_ulong = 0x1261;
    unsafe extern "C" {
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
    }
    // SAFETY: `f` is an open block device for the whole call; BLKFLSBUF reads and writes no memory of ours.
    if unsafe { ioctl(f.as_raw_fd(), BLKFLSBUF, 0usize) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A mount point as a shell word (desktops mount sticks under their label, which may hold spaces).
fn shell_word(s: &str) -> String {
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b)) { s.to_string() } else { format!("'{}'", s.replace('\'', "'\\''")) }
}

#[cfg(test)]
mod tests;

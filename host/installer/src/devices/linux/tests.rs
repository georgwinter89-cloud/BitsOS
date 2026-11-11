//! The listing against made-up sysfs trees: a machine with internal disks, a stick, a live system on USB, a card
//! reader and fixed or 4Kn USB disks.

use super::*;

struct Fake {
    root: PathBuf,
}

impl Fake {
    fn new() -> Fake {
        let root = std::env::temp_dir().join(format!("bi-sysfs-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["sys/block", "proc/self", "dev"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Fake { root }
    }
    fn w(&self, rel: &str, text: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{text}\n")).unwrap();
    }
    /// A disk at `devpath` (below sys/devices) with partitions (name, dev).
    fn disk(&self, name: &str, devpath: &str, dev: &str, sectors: u64, removable: bool, parts: &[(&str, &str)]) {
        let node = format!("sys/devices/{devpath}/block/{name}");
        self.w(&format!("{node}/size"), &sectors.to_string());
        self.w(&format!("{node}/queue/logical_block_size"), "512");
        self.w(&format!("{node}/removable"), if removable { "1" } else { "0" });
        self.w(&format!("{node}/dev"), dev);
        std::fs::create_dir_all(self.root.join(&node).join("holders")).unwrap();
        std::os::unix::fs::symlink(self.root.join(format!("sys/devices/{devpath}")), self.root.join(&node).join("device")).unwrap();
        for (p, pd) in parts {
            self.w(&format!("{node}/{p}/partition"), "1");
            self.w(&format!("{node}/{p}/dev"), pd);
            std::fs::create_dir_all(self.root.join(&node).join(p).join("holders")).unwrap();
        }
        std::os::unix::fs::symlink(self.root.join(&node), self.root.join("sys/block").join(name)).unwrap();
    }
    fn usb(&self, devpath_usb: &str, vendor: &str, model: &str, serial: &str) {
        self.w(&format!("sys/devices/{devpath_usb}/idVendor"), "0781");
        self.w(&format!("sys/devices/{devpath_usb}/serial"), serial);
        self.w(&format!("sys/devices/{devpath_usb}/manufacturer"), vendor);
        self.w(&format!("sys/devices/{devpath_usb}/product"), model);
    }
    fn list(&self) -> Vec<Disk> {
        Linux { root: self.root.clone() }.list().unwrap()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const USB_STICK: &str = "pci0000:00/0000:00:14.0/usb2/2-1";
const USB_DISK: &str = "pci0000:00/0000:00:14.0/usb2/2-2";
const USB_SD: &str = "pci0000:00/0000:00:14.0/usb1/1-4";

fn machine() -> Fake {
    let f = Fake::new();
    // Internal NVMe and SATA: never USB, never removable.
    f.disk("nvme0n1", "pci0000:00/0000:00:02.4/0000:02:00.0/nvme/nvme0", "259:0", 2_000_409_264, false, &[("nvme0n1p1", "259:1"), ("nvme0n1p2", "259:2")]);
    f.disk("sda", "pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0", "8:0", 976_773_168, false, &[("sda1", "8:1")]);
    // The USB stick to write.
    f.usb(USB_STICK, "SanDisk", "Cruzer Blade", "4C530001230726111403");
    f.disk("sdb", &format!("{USB_STICK}/2-1:1.0/host6/target6:0:0/6:0:0:0"), "8:16", 60_555_264, true, &[("sdb1", "8:17")]);
    f.w(&format!("sys/devices/{USB_STICK}/2-1:1.0/host6/target6:0:0/6:0:0:0/vendor"), "SanDisk ");
    f.w(&format!("sys/devices/{USB_STICK}/2-1:1.0/host6/target6:0:0/6:0:0:0/model"), "Cruzer Blade    ");
    // A USB disk that holds the running system's root (a live system started from it).
    f.usb(USB_DISK, "Samsung", "Portable SSD T7", "S6WYNS0T123456");
    f.disk("sdc", &format!("{USB_DISK}/2-2:1.0/host7/target7:0:0/7:0:0:0"), "8:32", 1_953_525_168, true, &[("sdc1", "8:33"), ("sdc2", "8:34")]);
    // A USB card reader: one slot with a card, one empty slot.
    f.usb(USB_SD, "Generic", "SD Reader", "000000000819");
    f.disk("sdd", &format!("{USB_SD}/1-4:1.0/host8/target8:0:0/8:0:0:0"), "8:48", 0, true, &[]);
    // The laptop's own SD slot on the MMC bus: removable, but not USB.
    f.disk("mmcblk0", "pci0000:00/0000:00:1c.0/0000:01:00.0/mmc_host/mmc0/mmc0:aaaa", "179:0", 62_333_952, true, &[("mmcblk0p1", "179:1")]);
    f.w(
        "proc/self/mountinfo",
        "22 1 0:34 / / rw,relatime shared:1 - btrfs /dev/sdc2 rw,ssd\n\
         23 22 259:1 / /boot rw,relatime shared:2 - vfat /dev/nvme0n1p1 rw\n\
         40 22 8:17 / /run/media/me/STICK rw,nosuid shared:9 - vfat /dev/sdb1 rw\n\
         41 22 179:1 / /run/media/me/SD rw shared:10 - vfat /dev/mmcblk0p1 rw",
    );
    f.w("proc/swaps", "Filename\tType\tSize\tUsed\tPriority\n/dev/nvme0n1p2\tpartition\t8388604\t0\t-2");
    f
}

#[test]
fn only_removable_usb_sticks_are_offered() {
    let f = machine();
    let all = f.list();
    let names: Vec<&str> = all.iter().map(|d| d.id.rsplit_once('-').unwrap().0).collect();
    // NVMe, SATA and the MMC slot are not even listed; the system disk and the empty slot are listed, not offered.
    assert_eq!(names, ["sdb-31.0G", "sdc-1000.2G", "sdd-0.0G"]);
    assert_eq!(all[0].id, disk_id("sdb", 60_555_264 * 512, "SanDisk", "Cruzer Blade", "4C530001230726111403"));
    let offered: Vec<&Disk> = all.iter().filter(|d| d.offered()).collect();
    assert_eq!(offered.len(), 1);
    let s = offered[0];
    assert_eq!((s.vendor.as_str(), s.model.as_str(), s.serial.as_str()), ("SanDisk", "Cruzer Blade", "4C530001230726111403"));
    assert_eq!(s.size, 60_555_264 * 512);
    assert_eq!(s.mounted, ["/run/media/me/STICK"]);
    assert_eq!(s.path, f.root.join("dev/sdb"));
    let sys = all.iter().find(|d| d.id.starts_with("sdc")).unwrap();
    assert!(sys.system.as_deref().unwrap().contains("running system's /"), "{:?}", sys.system);
}

#[test]
fn a_disk_under_device_mapper_or_swap_is_the_systems() {
    let f = machine();
    // The stick's partition carries a LUKS mapping that is mounted somewhere harmless: still in use.
    std::fs::create_dir_all(f.root.join("sys/devices/virtual/block/dm-0")).unwrap();
    f.w("sys/devices/virtual/block/dm-0/dev", "254:0");
    std::os::unix::fs::symlink(f.root.join("sys/devices/virtual/block/dm-0"), f.root.join("sys/block/dm-0")).unwrap();
    let holder = f.root.join(format!("sys/devices/{USB_STICK}/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb/sdb1/holders/dm-0"));
    std::fs::create_dir_all(holder).unwrap();
    let sdb = f.list().into_iter().find(|d| d.id.starts_with("sdb")).unwrap();
    assert!(!sdb.offered() && sdb.system.is_some());
    // Swap on the stick.
    let f = machine();
    f.w("proc/swaps", "Filename\tType\tSize\tUsed\tPriority\n/dev/sdb1\tpartition\t100\t0\t-2");
    let sdb = f.list().into_iter().find(|d| d.id.starts_with("sdb")).unwrap();
    assert!(sdb.system.as_deref().unwrap().contains("swap"));
}

#[test]
fn fixed_and_4k_usb_disks_are_listed_but_not_offered() {
    let f = machine();
    f.usb("pci0000:00/0000:00:14.0/usb2/2-3", "Samsung", "Portable SSD T5", "S49WNP0N1234");
    f.disk("sde", "pci0000:00/0000:00:14.0/usb2/2-3/2-3:1.0/host9/target9:0:0/9:0:0:0", "8:64", 1_000_215_216, false, &[("sde1", "8:65")]);
    f.usb("pci0000:00/0000:00:14.0/usb2/2-4", "Generic", "4Kn Stick", "4K0001");
    f.disk("sdf", "pci0000:00/0000:00:14.0/usb2/2-4/2-4:1.0/host10/target10:0:0/10:0:0:0", "8:80", 60_555_264, true, &[]);
    f.w("sys/devices/pci0000:00/0000:00:14.0/usb2/2-4/2-4:1.0/host10/target10:0:0/10:0:0:0/block/sdf/queue/logical_block_size", "4096");
    let all = f.list();
    let sde = all.iter().find(|d| d.id.starts_with("sde-")).expect("a fixed USB disk is listed");
    assert!(sde.why_not().unwrap().contains("fixed disk"), "{:?}", sde.why_not());
    let sdf = all.iter().find(|d| d.id.starts_with("sdf-")).unwrap();
    assert!(sdf.why_not().unwrap().contains("4096-byte sectors"), "{:?}", sdf.why_not());
    assert!(Linux { root: f.root.clone() }.open(sdf).err().unwrap().contains("4096"));
}

#[test]
fn mounted_sticks_are_not_opened() {
    let f = machine();
    let sdb = f.list().into_iter().find(|d| d.id.starts_with("sdb")).unwrap();
    let e = Linux { root: f.root.clone() }.open(&sdb).err().unwrap();
    assert!(e.contains("unmount") && e.contains("sudo umount /run/media/me/STICK"), "{e}");
    assert_eq!(shell_word("/run/media/me/MY STICK"), "'/run/media/me/MY STICK'");
}

#[test]
fn mountinfo_escapes() {
    let m = mounts("40 22 8:17 / /run/media/me/MY\\040STICK rw - vfat /dev/sdb1 rw");
    assert_eq!(m[0].point, "/run/media/me/MY STICK");
    assert_eq!(m[0].source, "/dev/sdb1");
}

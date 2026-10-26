//! The whole tool on directories that stand in for a mounted stick, with the mount table and udev facts faked:
//! the refusal matrix (V1 `stick-test.mjs` make-stick cases, ported), retire-v1, generations and pruning. The
//! same flows run on real FAT32 loop mounts in `host/tests/bt/50-stick.sh`.
use super::bundle::tests::{fake_bitsboot, fake_initrd, fake_kernel};
use super::gate::{Blk, Probe};
use super::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

const ID: &str = "0123456789abcdef0123456789abcdef";
const OTHER_ID: &str = "fedcba9876543210fedcba9876543210";

struct Fake {
    mounts: RefCell<String>,
    blk: RefCell<HashMap<String, Blk>>,
    free: u64,
}

impl Probe for Fake {
    fn mounts(&self) -> Result<String, String> {
        Ok(self.mounts.borrow().clone())
    }
    fn blk(&self, dev: &str) -> Result<Blk, String> {
        self.blk.borrow().get(dev).cloned().ok_or_else(|| format!("no {dev}"))
    }
    fn free_bytes(&self, _: &Path) -> Result<u64, String> {
        Ok(self.free)
    }
}

fn stick_blk() -> Blk {
    Blk {
        label: "BITSOS".into(),
        fstype: "vfat".into(),
        fsver: "FAT32".into(),
        uuid: "1234-ABCD".into(),
        size: "7.6G".into(),
        disk: "/dev/sdz".into(),
        disks: vec!["/dev/sdz".into()],
        disk_kind: "disk".into(),
        tran: "usb".into(),
        model: "Flash Disk".into(),
        backing: String::new(),
    }
}

struct Env {
    dir: PathBuf,
    stick: PathBuf,
    fake: Fake,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn env(name: &str) -> Env {
    let dir = std::env::temp_dir().join(format!("xt-stick-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("stick")).unwrap();
    let dir = dir.canonicalize().unwrap();
    let stick = dir.join("stick");
    let mounts = format!("/dev/nvme0n1p2 / ext4 rw 0 0\n/dev/sdz1 {} vfat rw,nosuid 0 0\n", stick.display());
    let sys = Blk { disk: "/dev/nvme0n1".into(), disks: vec!["/dev/nvme0n1".into()], tran: "nvme".into(), fstype: "ext4".into(), ..Blk::default() };
    let blk = HashMap::from([("/dev/sdz1".to_string(), stick_blk()), ("/dev/nvme0n1p2".to_string(), sys)]);
    Env { dir, stick, fake: Fake { mounts: RefCell::new(mounts), blk: RefCell::new(blk), free: 1 << 34 } }
}

impl Env {
    fn put(&self, rel: &str, bytes: &[u8]) {
        let p = self.stick.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    fn has(&self, rel: &str) -> bool {
        self.stick.join(rel).exists()
    }
    fn get(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.stick.join(rel)).unwrap()
    }
    fn set_blk(&self, f: impl FnOnce(&mut Blk)) {
        f(self.fake.blk.borrow_mut().get_mut("/dev/sdz1").unwrap());
    }
    fn v1(&self) {
        self.put("BITSOS/STICK.ID", format!("{ID}\r\n").as_bytes());
        self.put("BITSOS/KERNEL.BIN", b"V1 kernel");
        self.put("BITSOS/CONFIG.TXT", b"screen=native\r\n");
        self.put("BITSOS/GAMES/DOOM.BXE", &[7u8; 3000]);
        std::fs::create_dir_all(self.stick.join("BITSOS/SAVES")).unwrap();
        self.put("EFI/BOOT/BOOTX64.EFI", b"MZ....BitsOS loader on ....");
        self.put("System Volume Information/IndexerVolumeGuid", b"{guid}");
    }
    fn v2(&self) {
        self.put("BITSOS2/STICK.ID", ID.as_bytes());
        self.put("EFI/BOOT/BOOTX64.EFI", &fake_bitsboot(b"old"));
    }
    /// A bundle folder `name` whose files carry `tag`; the loader carries `boot`.
    fn bundle(&self, name: &str, tag: &str, boot: &str) -> String {
        let d = self.dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("KERNEL"), fake_kernel(tag.as_bytes())).unwrap();
        std::fs::write(d.join("INITRD"), fake_initrd(tag.as_bytes())).unwrap();
        std::fs::write(d.join("BOOTX64.EFI"), fake_bitsboot(boot.as_bytes())).unwrap();
        d.display().to_string()
    }
    fn run(&self, args: &[&str], answer: &str) -> Result<(), String> {
        self.run_with(args, &mut |_| answer.to_string())
    }
    fn run_with(&self, args: &[&str], ask: &mut dyn FnMut(&str) -> String) -> Result<(), String> {
        let mut v: Vec<String> = vec!["--target".into(), self.stick.display().to_string()];
        if !args.iter().any(|a| a.starts_with("--id")) {
            v.extend(["--id".to_string(), ID.to_string()]);
        }
        v.extend(args.iter().map(|s| s.to_string()));
        let mut o = Opts::parse(&self.dir, &mut Args::new(v))?;
        o.lock_dir = self.dir.clone();
        run(&self.fake, &o, ask)
    }
    fn snap(&self) -> BTreeMap<String, Option<Vec<u8>>> {
        snapshot(&self.stick)
    }
    fn markers(&self, n: u32) -> String {
        let mut v: Vec<String> = std::fs::read_dir(self.stick.join(format!("BITSOS2/SYS/{n}"))).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v.join(",")
    }
    fn gens(&self) -> Vec<u32> {
        let mut v: Vec<u32> = std::fs::read_dir(self.stick.join("BITSOS2/SYS"))
            .map(|r| r.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let t = retire::walk(root).unwrap();
    let mut m: BTreeMap<String, Option<Vec<u8>>> = t.dirs.into_iter().map(|d| (d, None)).collect();
    for (f, _) in t.files {
        let b = std::fs::read(root.join(&f)).unwrap();
        m.insert(f, Some(b));
    }
    m
}

/// `r` is a refusal naming `why`, and the stick is as before.
fn refused(e: &Env, r: Result<(), String>, before: &BTreeMap<String, Option<Vec<u8>>>, why: &str) {
    let msg = r.expect_err("must be refused");
    assert!(msg.contains(why), "'{msg}' lacks '{why}'");
    assert_eq!(&e.snap(), before, "the stick changed although '{msg}'");
}

#[test]
fn refusal_matrix() {
    let e = env("refuse");
    e.v2();
    let b = e.bundle("b", "k1", "new");
    let w = ["--bundle", b.as_str(), "--write"];
    let s0 = e.snap();
    // The paths themselves: no such path, not a mount point, a system mount point.
    let o = |t: &str| Opts::parse(&e.dir, &mut Args::new(vec!["--target".into(), t.into(), "--id".into(), ID.into()])).unwrap();
    let missing = o(&e.dir.join("nope").display().to_string());
    assert!(run(&e.fake, &missing, &mut |_| "J".into()).unwrap_err().contains("refused"));
    std::fs::create_dir_all(e.stick.join("BITSOS2/SYS")).unwrap();
    let sub = o(&e.stick.join("BITSOS2").display().to_string());
    assert!(run(&e.fake, &sub, &mut |_| "J".into()).unwrap_err().contains("not a mount point"));
    std::fs::remove_dir_all(e.stick.join("BITSOS2/SYS")).unwrap();
    let root = o("/");
    assert!(run(&e.fake, &root, &mut |_| "J".into()).unwrap_err().contains("running system"));

    // Label: wrong, missing; --force never helps.
    for (label, force) in [("ZORINOS", false), ("ZORINOS", true), ("", true), ("bitsos", true), ("BITSOS2", false)] {
        e.set_blk(|b| b.label = label.into());
        let mut a = w.to_vec();
        if force {
            a.push("--force");
        }
        refused(&e, e.run(&a, "J"), &s0, "must be labelled BITSOS");
    }
    e.set_blk(|b| *b = stick_blk());
    // File system: NTFS, FAT16.
    { let m = e.fake.mounts.borrow().replace(" vfat ", " ntfs3 "); e.fake.mounts.replace(m); }
    refused(&e, e.run(&w, "J"), &s0, "not FAT32");
    { let m = e.fake.mounts.borrow().replace(" ntfs3 ", " vfat "); e.fake.mounts.replace(m); }
    e.set_blk(|b| b.fsver = "FAT16".into());
    refused(&e, e.run(&w, "J"), &s0, "not FAT32");
    e.set_blk(|b| *b = stick_blk());
    // Bus: built-in disks never, other buses only with --force.
    for tran in ["nvme", "sata", "ata"] {
        e.set_blk(|b| b.tran = tran.into());
        refused(&e, e.run(&[&w[..], &["--force"]].concat(), "J"), &s0, "built-in disk");
    }
    e.set_blk(|b| b.tran = "mmc".into());
    refused(&e, e.run(&w, "J"), &s0, "--force accepts it");
    e.run(&["--bundle", &b, "--force"], "").unwrap(); // dry run passes with --force
    e.set_blk(|b| *b = stick_blk());
    // The disk Windows/CachyOS starts from (a partition of the disk that holds /).
    e.set_blk(|b| (b.disk, b.disks) = ("/dev/nvme0n1".into(), vec!["/dev/nvme0n1".into()]));
    refused(&e, e.run(&w, "J"), &s0, "also holds / of the running system");
    // A device over two disks (RAID, LVM), and one whose disks udev cannot name.
    e.set_blk(|b| b.disks.push("/dev/sdy".into()));
    refused(&e, e.run(&[&w[..], &["--force"]].concat(), "J"), &s0, "exactly one disk");
    e.set_blk(|b| b.disks.clear());
    refused(&e, e.run(&[&w[..], &["--force"]].concat(), "J"), &s0, "exactly one disk");
    e.set_blk(|b| *b = stick_blk());
    // The system disk check fails closed: a system mount whose disk cannot be told refuses every stick.
    let sys = e.fake.blk.borrow_mut().remove("/dev/nvme0n1p2").unwrap();
    refused(&e, e.run(&[&w[..], &["--force"]].concat(), "J"), &s0, "cannot tell which disk holds /");
    let mut no_disks = sys.clone();
    no_disks.disks.clear();
    e.fake.blk.borrow_mut().insert("/dev/nvme0n1p2".into(), no_disks);
    refused(&e, e.run(&[&w[..], &["--force"]].concat(), "J"), &s0, "cannot tell which disk holds /");
    // Root on dm-crypt over an nvme partition: the disk under the mapper device counts.
    let mounts = e.fake.mounts.borrow().replace("/dev/nvme0n1p2 /", "/dev/mapper/root /");
    e.fake.mounts.replace(mounts);
    let mut crypt = sys.clone();
    crypt.disks = vec!["/dev/sdz".into()];
    e.fake.blk.borrow_mut().insert("/dev/mapper/root".into(), crypt);
    refused(&e, e.run(&w, "J"), &s0, "also holds / of the running system");
    let mounts = e.fake.mounts.borrow().replace("/dev/mapper/root /", "/dev/nvme0n1p2 /");
    e.fake.mounts.replace(mounts);
    e.fake.blk.borrow_mut().insert("/dev/nvme0n1p2".into(), sys);
    // A Windows folder.
    std::fs::create_dir(e.stick.join("Windows")).unwrap();
    let sw = e.snap();
    refused(&e, e.run(&w, "J"), &sw, "Windows folder");
    std::fs::remove_dir(e.stick.join("Windows")).unwrap();
    // Identity: wrong recorded ID, no record, garbage, none, V1 and V2 differ.
    refused(&e, e.run(&[&w[..], &["--id", OTHER_ID]].concat(), "J"), &s0, "is not the recorded ID");
    refused(&e, e.run(&[&w[..], &["--id-file", "/nonexistent/stick.id"]].concat(), "J"), &s0, &format!("no recorded stick ID to compare with (this stick says {ID})"));
    e.put("BITSOS2/STICK.ID", b"not an id");
    let sg = e.snap();
    refused(&e, e.run(&w, "J"), &sg, "holds no stick ID");
    std::fs::remove_file(e.stick.join("BITSOS2/STICK.ID")).unwrap();
    let sn = e.snap();
    refused(&e, e.run(&w, "J"), &sn, "not Georg's BitsOS stick");
    e.put("BITSOS2/STICK.ID", ID.as_bytes());
    e.put("BITSOS/STICK.ID", OTHER_ID.as_bytes());
    let sd = e.snap();
    refused(&e, e.run(&w, "J"), &sd, "differ");
    std::fs::remove_dir_all(e.stick.join("BITSOS")).unwrap();
    // Another system's loader.
    e.put("EFI/BOOT/BOOTX64.EFI", b"MZ grub");
    let sf = e.snap();
    refused(&e, e.run(&[&w[..], &["--force"]].concat(), "J"), &sf, "belongs to another system");
    e.put("EFI/BOOT/BOOTX64.EFI", &fake_bitsboot(b"old"));
    assert_eq!(e.snap(), s0);
    // Cancelled, read-only, no room, bad bundle files, a CONFIG.TXT that turns on NVMe writes.
    refused(&e, e.run(&w, "n"), &s0, "cancelled");
    refused(&e, e.run(&w, ""), &s0, "cancelled");
    { let m = e.fake.mounts.borrow().replace("rw,nosuid", "ro,nosuid"); e.fake.mounts.replace(m); }
    e.run(&["--bundle", &b], "").unwrap();
    refused(&e, e.run(&w, "J"), &s0, "read-only");
    { let m = e.fake.mounts.borrow().replace("ro,nosuid", "rw,nosuid"); e.fake.mounts.replace(m); }
    std::fs::create_dir_all(e.dir.join("t")).unwrap();
    let tight = Env { dir: e.dir.join("t"), stick: e.stick.clone(), fake: Fake { mounts: RefCell::new(e.fake.mounts.borrow().clone()), blk: RefCell::new(e.fake.blk.borrow().clone()), free: 1000 } };
    refused(&e, tight.run(&w, "J"), &s0, "bytes free");
    let bad = e.bundle("bad", "x", "new");
    std::fs::write(format!("{bad}/KERNEL"), b"MZ not an ELF").unwrap();
    refused(&e, e.run(&["--bundle", &bad, "--write"], "J"), &s0, "no BitsOS kernel");
    let bad = e.bundle("bad2", "x", "new");
    std::fs::write(format!("{bad}/BOOTX64.EFI"), b"MZ grub").unwrap();
    refused(&e, e.run(&["--bundle", &bad, "--write"], "J"), &s0, "no V2 bitsboot");
    std::fs::write(e.dir.join("cfg"), b"loglevel=info\nnvme_write=on\n").unwrap();
    refused(&e, e.run(&[&w[..], &["--config", &e.dir.join("cfg").display().to_string()]].concat(), "J"), &s0, "only Georg turns that on");
}

#[test]
fn swapped_while_the_question_waited() {
    let e = env("swap");
    e.v2();
    let b = e.bundle("b", "k1", "new");
    let s0 = e.snap();
    // Another stick with the same mount point and label: the UUID differs.
    let r = e.run_with(&["--bundle", &b, "--write"], &mut |_| {
        e.set_blk(|b| b.uuid = "5678-EF01".into());
        "J".into()
    });
    refused(&e, r, &s0, "no longer the one shown above");
    e.set_blk(|b| *b = stick_blk());
    // The same device, but its identity changed.
    let r = e.run_with(&["--bundle", &b, "--write"], &mut |_| {
        std::fs::write(e.stick.join("BITSOS2/STICK.ID"), OTHER_ID).unwrap();
        "J".into()
    });
    assert!(r.unwrap_err().contains("changed while the question waited"));
    assert!(!e.has("BITSOS2/SYS"));
    e.put("BITSOS2/STICK.ID", ID.as_bytes());
    // Unmounted while the question waited.
    let r = e.run_with(&["--bundle", &b, "--write"], &mut |_| {
        e.fake.mounts.replace(String::new());
        "J".into()
    });
    assert!(r.unwrap_err().contains("not a mount point"));
    assert_eq!(e.snap(), s0);
}

#[test]
fn dry_run_is_the_default() {
    let e = env("dry");
    e.v2();
    let b = e.bundle("b", "k1", "new");
    let s0 = e.snap();
    e.run(&["--bundle", &b], "J").unwrap();
    e.run(&["--bundle", &b, "--dry-run", "--prune", "--unpin"], "J").unwrap();
    e.run(&[], "J").unwrap();
    assert_eq!(e.snap(), s0);
    assert!(Opts::parse(&e.dir, &mut Args::new(vec!["--target".into(), "/x".into(), "--write".into(), "--dry-run".into()])).is_err());
}

#[test]
fn generations_bundle_confirm_replace_prune() {
    let e = env("gens");
    e.v2();
    let b1 = e.bundle("b1", "k1", "boot1");
    e.run(&["--bundle", &b1, "--write"], "J").unwrap();
    assert_eq!(e.gens(), vec![1]);
    assert_eq!(e.markers(1), "INITRD,KERNEL,NEW");
    assert_eq!(e.get("EFI/BOOT/BOOTX64.EFI"), fake_bitsboot(b"boot1"), "bitsboot changed, so it is replaced");
    assert!(!e.has("EFI/BOOT/BOOTX64.NEW") && !e.has("BITSOS2/SYS/STAGING"));
    assert_eq!(e.get("BITSOS2/SYS/1/KERNEL"), fake_kernel(b"k1"));

    // An unconfirmed newest generation is replaced, not added to; the same bundle again needs --force.
    let b2 = e.bundle("b2", "k2", "boot1");
    e.run(&["--bundle", &b2, "--write"], "J").unwrap();
    assert_eq!((e.gens(), e.markers(1)), (vec![1], "INITRD,KERNEL,NEW".into()));
    assert_eq!(e.get("BITSOS2/SYS/1/KERNEL"), fake_kernel(b"k2"));
    let s = e.snap();
    refused(&e, e.run(&["--bundle", &b2, "--write"], "J"), &s, "same as generation 1");

    // Confirm: only the newest, only after a start (TRY).
    refused(&e, e.run(&["--confirm", "1", "--write"], "J"), &s, "never started");
    e.put("BITSOS2/SYS/1/TRY", b"");
    let s = e.snap();
    refused(&e, e.run(&["--confirm", "2", "--write"], "J"), &s, "the newest generation is 1");
    e.run(&["--confirm", "1", "--write"], "J").unwrap();
    assert_eq!(e.markers(1), "INITRD,KERNEL,OK");

    // A bundle above a confirmed generation adds one; the unchanged bitsboot stays as it is.
    let boot_before = e.get("EFI/BOOT/BOOTX64.EFI");
    let b3 = e.bundle("b3", "k3", "boot1");
    e.run(&["--bundle", &b3, "--write"], "J").unwrap();
    assert_eq!((e.gens(), e.markers(2)), (vec![1, 2], "INITRD,KERNEL,NEW".into()));
    assert_eq!(e.get("EFI/BOOT/BOOTX64.EFI"), boot_before);

    // Tried and not confirmed, then a Reload-proven bundle: it takes 2's place as OK and ends the pin.
    e.put("BITSOS2/SYS/2/TRY", b"");
    e.put("BITSOS2/SYS/PIN", b"1\n");
    let b4 = e.bundle("b4", "k4", "boot1");
    e.run(&["--bundle", &b4, "--proven", "--write"], "J").unwrap();
    assert_eq!((e.gens(), e.markers(2)), (vec![1, 2], "INITRD,KERNEL,OK".into()));
    assert_eq!(e.get("BITSOS2/SYS/2/KERNEL"), fake_kernel(b"k4"));
    assert!(!e.has("BITSOS2/SYS/PIN"));

    // More proven bundles with sys_keep 2 from CONFIG.TXT: the newest two OK ones and the pinned one stay.
    e.put("BITSOS2/CONFIG.TXT", b"loglevel=info\nsys_keep=9\n# sys_keep=1\nsys_keep = 2\n");
    e.put("BITSOS2/SYS/PIN", b"1");
    for (i, k) in ["k5", "k6", "k7"].iter().enumerate() {
        let b = e.bundle(&format!("b{}", i + 5), k, "boot1");
        e.run(&["--bundle", &b, "--proven", "--write"], "J").unwrap();
    }
    // The proven installs ended the pin, so generation 1 went with the others.
    assert_eq!(e.gens(), vec![4, 5]);
    // A pin on a kept generation survives a plain bundle and the pruning.
    e.put("BITSOS2/SYS/PIN", b"4");
    let b8 = e.bundle("b8", "k8", "boot1");
    e.run(&["--bundle", &b8, "--write", "--sys-keep", "1"], "J").unwrap();
    assert_eq!(e.gens(), vec![4, 5, 6]);
    assert_eq!(e.markers(6), "INITRD,KERNEL,NEW");
    assert_eq!(e.get("BITSOS2/SYS/PIN"), b"4");
    e.run(&["--unpin", "--write", "--sys-keep", "1"], "J").unwrap();
    assert_eq!(e.gens(), vec![5, 6], "unpinned: 4 goes, 5 (newest OK) and 6 (under trial) stay");
}

#[test]
fn leftovers_stale_markers_junk_and_config() {
    let e = env("tidy");
    e.v2();
    for n in 1..=3 {
        e.put(&format!("BITSOS2/SYS/{n}/KERNEL"), &fake_kernel(format!("k{n}").as_bytes()));
        e.put(&format!("BITSOS2/SYS/{n}/INITRD"), &fake_initrd(b""));
        e.put(&format!("BITSOS2/SYS/{n}/OK"), b"");
    }
    e.put("BITSOS2/SYS/2/TRY", b""); // a confirmation cut between OK and deleting TRY
    e.put("BITSOS2/SYS/STAGING/KERNEL", b"half");
    e.put("BITSOS2/SYS/notes.txt", b"mine");
    e.run(&["--prune", "--write"], "J").unwrap();
    assert_eq!(e.markers(2), "INITRD,KERNEL,OK");
    assert!(!e.has("BITSOS2/SYS/STAGING"), "a write clears what an interrupted one left");
    e.put("BITSOS2/SYS/STAGING/KERNEL", b"half");
    let b = e.bundle("b", "kb", "old");
    let cfg = e.dir.join("cfg");
    std::fs::write(&cfg, b"loglevel=info\npanic=reboot\n").unwrap();
    e.run(&["--bundle", &b, "--config", &cfg.display().to_string(), "--write"], "J").unwrap();
    assert!(!e.has("BITSOS2/SYS/STAGING"));
    assert_eq!(e.get("BITSOS2/SYS/notes.txt"), b"mine");
    assert_eq!(e.get("BITSOS2/CONFIG.TXT"), b"loglevel=info\npanic=reboot\n");
    assert_eq!(e.gens(), vec![1, 2, 3, 4]);
    // The stick's own CONFIG.TXT is never replaced.
    e.put("BITSOS2/CONFIG.TXT", b"screen=firmware\n");
    e.put("BITSOS2/SYS/4/TRY", b"");
    e.run(&["--confirm", "4", "--config", &cfg.display().to_string(), "--write"], "J").unwrap();
    assert_eq!(e.get("BITSOS2/CONFIG.TXT"), b"screen=firmware\n");
    // Two folders for one number are refused.
    std::fs::create_dir(e.stick.join("BITSOS2/SYS/04")).unwrap();
    let s = e.snap();
    refused(&e, e.run(&["--prune", "--write"], "J"), &s, "two folders");
}

#[test]
fn a_stick_without_loader_needs_bitsboot() {
    let e = env("noloader");
    e.put("BITSOS2/STICK.ID", ID.as_bytes());
    let d = e.dir.join("kb");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("KERNEL"), fake_kernel(b"k")).unwrap();
    std::fs::write(d.join("INITRD"), fake_initrd(b"k")).unwrap();
    let s = e.snap();
    refused(&e, e.run(&["--bundle", &d.display().to_string(), "--write"], "J"), &s, "would not start");
    let (k, i) = (d.join("KERNEL").display().to_string(), d.join("INITRD").display().to_string());
    let bb = e.dir.join("bb.efi");
    std::fs::write(&bb, fake_bitsboot(b"v")).unwrap();
    e.run(&["--kernel", &k, "--initrd", &i, "--bitsboot", &bb.display().to_string(), "--write"], "J").unwrap();
    assert_eq!(e.get("EFI/BOOT/BOOTX64.EFI"), fake_bitsboot(b"v"));
    assert_eq!(e.gens(), vec![1]);
}

#[test]
fn retire_v1_backs_up_compares_and_removes_only_v1() {
    let e = env("retire");
    e.v1();
    e.put("README.TXT", b"Georg's note");
    let s0 = e.snap();
    let backup = e.dir.join("backup");
    let bk = backup.display().to_string();
    // V1 on the stick: any V2 operation is refused until V1 is retired.
    let b = e.bundle("b", "k1", "boot1");
    refused(&e, e.run(&["--bundle", &b, "--write"], "J"), &s0, "retire V1 first");
    // Dry run: nothing copied, nothing removed.
    e.run(&["--retire-v1", "--backup", &bk], "J").unwrap();
    assert!(!backup.exists());
    refused(&e, e.run(&["--retire-v1", "--backup", &bk, "--write"], "nein"), &s0, "cancelled");
    // A backup inside the stick, or a used backup folder, is refused.
    refused(&e, e.run(&["--retire-v1", "--backup", &e.stick.join("bk").display().to_string(), "--write"], "J"), &s0, "on the stick itself");
    std::fs::create_dir_all(e.dir.join("used")).unwrap();
    std::fs::write(e.dir.join("used/x"), b"x").unwrap();
    refused(&e, e.run(&["--retire-v1", "--backup", &e.dir.join("used").display().to_string(), "--write"], "J"), &s0, "not empty");
    assert!(Opts::parse(&e.dir, &mut Args::new(vec!["--target".into(), "/x".into(), "--retire-v1".into(), "--prune".into()])).is_err());

    e.run(&["--retire-v1", "--backup", &bk, "--write"], "J").unwrap();
    assert_eq!(snapshot(&backup.join("files")), s0, "the backup equals the stick as it was");
    assert_eq!(std::fs::read(backup.join("SHA256-stick.txt")).unwrap(), std::fs::read(backup.join("SHA256-copy.txt")).unwrap());
    assert!(std::fs::read_to_string(backup.join("RESULT.txt")).unwrap().contains("compare green"));
    assert!(std::fs::read_to_string(backup.join("VOLUME.txt")).unwrap().contains("UUID=1234-ABCD"));
    let mut want = s0.clone();
    want.retain(|k, _| !k.starts_with("BITSOS/") && k != "BITSOS" && k != "EFI/BOOT/BOOTX64.EFI");
    want.insert("BITSOS2".into(), None);
    want.insert("BITSOS2/STICK.ID".into(), Some(format!("{ID}\r\n").into_bytes()));
    assert_eq!(e.snap(), want, "only V1's paths are gone, STICK.ID moved with the same bytes");

    // Once only; then the first bundle (with bitsboot) goes on.
    let s1 = e.snap();
    refused(&e, e.run(&["--retire-v1", "--backup", &e.dir.join("b2").display().to_string(), "--write"], "J"), &s1, "V1 is not on this stick");
    e.run(&["--bundle", &b, "--write"], "J").unwrap();
    assert_eq!(e.markers(1), "INITRD,KERNEL,NEW");
    assert_eq!(e.get("EFI/BOOT/BOOTX64.EFI"), fake_bitsboot(b"boot1"));
}

#[test]
fn retire_keeps_an_existing_v2_identity_and_refuses_a_swap() {
    let e = env("retire2");
    e.v1();
    e.put("BITSOS2/STICK.ID", ID.as_bytes());
    let bk = e.dir.join("bk").display().to_string();
    e.run(&["--retire-v1", "--backup", &bk, "--write"], "J").unwrap();
    assert_eq!(e.get("BITSOS2/STICK.ID"), ID.as_bytes(), "the V2 identity stays as it was");
    assert!(!e.has("BITSOS") && !e.has("EFI/BOOT/BOOTX64.EFI") && e.has("EFI/BOOT"));

    let e = env("retire3");
    e.v1();
    let s0 = e.snap();
    let bk = e.dir.join("bk");
    let r = e.run_with(&["--retire-v1", "--backup", &bk.display().to_string(), "--write"], &mut |_| {
        e.set_blk(|b| b.uuid = "0000-0001".into());
        "J".into()
    });
    refused(&e, r, &s0, "no longer the one shown above");
    assert!(!bk.exists(), "nothing copied either");
}

#[test]
fn build_runs_the_build_step_and_writes_its_bundle() {
    let e = env("build");
    e.v2();
    let built = e.bundle("built", "kb", "bootb");
    let mut seen = Vec::new();
    let args = |extra: &[&str]| {
        let mut v: Vec<String> = ["--target", &e.stick.display().to_string(), "--id", ID].iter().map(|s| s.to_string()).collect();
        v.extend(extra.iter().map(|s| s.to_string()));
        Args::new(v)
    };
    let mut build = |root: &Path, release: bool| {
        seen.push((root.to_path_buf(), release));
        Ok(bundle::Sources::from_dir(Path::new(&built)))
    };
    let mut o = prepare(&e.dir, &mut args(&["--build", "--release", "--write"]), &mut build).unwrap().unwrap();
    o.lock_dir = e.dir.clone();
    run(&e.fake, &o, &mut |_| "J".into()).unwrap();
    assert_eq!(e.markers(1), "INITRD,KERNEL,NEW");
    assert_eq!(e.get("BITSOS2/SYS/1/KERNEL"), fake_kernel(b"kb"));
    assert_eq!(e.get("EFI/BOOT/BOOTX64.EFI"), fake_bitsboot(b"bootb"));
    // --build without --release builds a debug bundle; --proven goes with --build too.
    prepare(&e.dir, &mut args(&["--build", "--proven"]), &mut build).unwrap().unwrap();
    // A failed build stops before the stick is looked at; --build excludes the other bundle sources.
    let s = e.snap();
    let mut broken = |_: &Path, _: bool| -> Result<bundle::Sources, String> { Err("cargo build: error".into()) };
    assert!(prepare(&e.dir, &mut args(&["--build", "--write"]), &mut broken).err().unwrap_or_default().contains("cargo build"));
    assert!(prepare(&e.dir, &mut args(&["--build", "--bundle", "/x"]), &mut broken).is_err());
    assert!(prepare(&e.dir, &mut args(&["--build", "--kernel", "/k", "--initrd", "/i"]), &mut broken).is_err());
    assert!(prepare(&e.dir, &mut args(&["--help"]), &mut broken).unwrap().is_none());
    assert_eq!(e.snap(), s);
    assert_eq!(seen, vec![(e.dir.clone(), true), (e.dir.clone(), false)]);
}

#[test]
fn a_second_run_on_the_same_stick_is_refused() {
    let e = env("lock");
    e.v2();
    let b = e.bundle("b", "k1", "boot1");
    let s = e.snap();
    let held = std::fs::File::create(e.dir.join("bitsos-stick-1234-ABCD.lock")).unwrap();
    held.lock().unwrap();
    refused(&e, e.run(&["--bundle", &b, "--write"], "J"), &s, "another `xtask stick` run is working on this stick");
    drop(held);
    e.run(&["--bundle", &b, "--write"], "J").unwrap();
    assert_eq!(e.gens(), vec![1]);
}

#[test]
fn sys_keep_is_never_guessed() {
    let e = env("keep");
    e.v2();
    for n in 1..=3 {
        e.put(&format!("BITSOS2/SYS/{n}/KERNEL"), &fake_kernel(format!("k{n}").as_bytes()));
        e.put(&format!("BITSOS2/SYS/{n}/INITRD"), &fake_initrd(b""));
        e.put(&format!("BITSOS2/SYS/{n}/OK"), b"");
    }
    // Larger than any CONFIG.TXT, a sys_keep that is no number: refused, nothing pruned.
    let mut big = b"sys_keep=9\n".to_vec();
    big.resize(2 << 20, b'#');
    e.put("BITSOS2/CONFIG.TXT", &big);
    let s = e.snap();
    refused(&e, e.run(&["--prune", "--write"], "J"), &s, "nothing is pruned on a guess");
    e.put("BITSOS2/CONFIG.TXT", b"sys_keep=ten\n");
    let s = e.snap();
    refused(&e, e.run(&["--prune", "--write"], "J"), &s, "sys_keep=ten is no number");
    // --sys-keep overrides a CONFIG.TXT the tool cannot use.
    e.run(&["--prune", "--write", "--sys-keep", "3"], "J").unwrap();
    assert_eq!(e.gens(), vec![1, 2, 3]);
    // Not UTF-8 around a good line: the line still counts.
    e.put("BITSOS2/CONFIG.TXT", b"\xff\xfe title=\xc3\n sys_keep = 2 \n\x00\x01\n");
    e.run(&["--prune", "--write"], "J").unwrap();
    assert_eq!(e.gens(), vec![2, 3]);
}

#[test]
fn a_leftover_loader_update_goes() {
    let e = env("bootnew");
    e.v2();
    e.put("BITSOS2/SYS/1/KERNEL", &fake_kernel(b"k1"));
    e.put("BITSOS2/SYS/1/INITRD", &fake_initrd(b""));
    e.put("BITSOS2/SYS/1/OK", b"");
    e.put("EFI/BOOT/BOOTX64.NEW", &fake_bitsboot(b"half"));
    let s = e.snap();
    e.run(&["--prune"], "J").unwrap();
    assert_eq!(e.snap(), s, "the dry run names it and leaves it");
    e.run(&["--prune", "--write"], "J").unwrap();
    assert!(!e.has("EFI/BOOT/BOOTX64.NEW"));
    assert_eq!(e.get("EFI/BOOT/BOOTX64.EFI"), fake_bitsboot(b"old"));
}

#[test]
fn hostile_stick_contents_are_refused_or_ignored_never_a_panic() {
    let e = env("hostile");
    e.v2();
    let b = e.bundle("b", "k1", "boot1");
    let w = ["--bundle", b.as_str(), "--write"];
    // STICK.ID: megabytes, trailing garbage, binary; CRLF and spaces are fine.
    e.put("BITSOS2/STICK.ID", &vec![b'a'; 3 << 20]);
    let s = e.snap();
    refused(&e, e.run(&w, "J"), &s, "more than the 4096 allowed");
    e.put("BITSOS2/STICK.ID", format!("{ID}\nfedcba").as_bytes());
    let s = e.snap();
    refused(&e, e.run(&w, "J"), &s, "holds no stick ID");
    e.put("BITSOS2/STICK.ID", &[0xff, 0xfe, 0, 1, b'\n']);
    let s = e.snap();
    refused(&e, e.run(&w, "J"), &s, "holds no stick ID");
    e.put("BITSOS2/STICK.ID", format!("  {ID}\r\n").as_bytes());
    e.run(&w, "J").unwrap();
    assert_eq!(e.gens(), vec![1]);
    // Below SYS: a number too large for bitsboot, non-ASCII digits, a non-UTF-8 name, a file named like a
    // generation, a huge PIN that names nothing: all stay, none counts as a generation.
    for d in ["99999999999", "\u{663}", "-1"] {
        std::fs::create_dir_all(e.stick.join("BITSOS2/SYS").join(d)).unwrap();
    }
    e.put("BITSOS2/SYS/7", b"a file");
    e.put("BITSOS2/SYS/PIN", &vec![b'9'; 1 << 20]);
    use std::os::unix::ffi::OsStrExt;
    let odd = e.stick.join("BITSOS2/SYS").join(std::ffi::OsStr::from_bytes(b"\xff\xfe"));
    std::fs::create_dir(&odd).unwrap();
    e.put("BITSOS2/SYS/1/TRY", b"");
    e.run(&["--confirm", "1", "--prune", "--write", "--sys-keep", "1"], "J").unwrap();
    assert_eq!(e.markers(1), "INITRD,KERNEL,OK");
    for d in ["99999999999", "\u{663}", "-1", "7", "PIN"] {
        assert!(e.has(&format!("BITSOS2/SYS/{d}")), "{d} stays");
    }
    assert!(odd.is_dir());
    let b2 = e.bundle("b2", "k2", "boot1");
    e.run(&["--bundle", &b2, "--write"], "J").unwrap();
    assert!(e.has("BITSOS2/SYS/2/NEW"), "the next generation is 2, not 99999999999 + 1");
}

/// Sets how many file operations succeed before the injected cut; None = no cut.
fn cut_after(n: Option<usize>) {
    sys::FAIL_AFTER.with(|c| c.set(n));
}

#[test]
fn a_bundle_write_cut_anywhere_leaves_a_stick_that_starts_and_a_rerun_finishes() {
    let setup = |e: &Env| {
        e.v2();
        for (n, m) in [(1, "OK"), (2, "NEW"), (2, "TRY")] {
            e.put(&format!("BITSOS2/SYS/{n}/KERNEL"), &fake_kernel(format!("k{n}").as_bytes()));
            e.put(&format!("BITSOS2/SYS/{n}/INITRD"), &fake_initrd(b""));
            e.put(&format!("BITSOS2/SYS/{n}/{m}"), b"");
        }
    };
    let mut n = 0;
    loop {
        let e = env(&format!("cut{n}"));
        setup(&e);
        let b = e.bundle("b", "k3", "boot3");
        cut_after(Some(n));
        let r = e.run(&["--bundle", &b, "--write"], "J");
        cut_after(None);
        let finished = r.is_ok();
        if let Err(m) = &r {
            assert!(m.contains("injected fault"), "cut {n}: {m}");
        }
        // Whatever the cut: a whole loader, the confirmed generation 1 untouched, every numbered
        // generation complete (the old 2 under trial, or the new 2).
        let boot = e.get("EFI/BOOT/BOOTX64.EFI");
        assert!(boot == fake_bitsboot(b"old") || boot == fake_bitsboot(b"boot3"), "cut {n}: loader torn");
        assert_eq!((e.markers(1), e.get("BITSOS2/SYS/1/KERNEL")), ("INITRD,KERNEL,OK".into(), fake_kernel(b"k1")), "cut {n}");
        if e.has("BITSOS2/SYS/2") {
            let (k, m) = (e.get("BITSOS2/SYS/2/KERNEL"), e.markers(2));
            assert!((k == fake_kernel(b"k2") && m == "INITRD,KERNEL,NEW,TRY") || (k == fake_kernel(b"k3") && m == "INITRD,KERNEL,NEW"), "cut {n}: {m}");
        }
        // The same command again ends in the planned state (or says there is nothing left to write).
        match e.run(&["--bundle", &b, "--write"], "J") {
            Ok(()) => {}
            Err(m) => assert!(m.contains("same as generation 2"), "rerun after cut {n}: {m}"),
        }
        assert_eq!((e.gens(), e.markers(2)), (vec![1, 2], "INITRD,KERNEL,NEW".into()), "after cut {n}");
        assert_eq!(e.get("BITSOS2/SYS/2/KERNEL"), fake_kernel(b"k3"));
        assert_eq!(e.get("EFI/BOOT/BOOTX64.EFI"), fake_bitsboot(b"boot3"));
        assert!(!e.has("BITSOS2/SYS/STAGING") && !e.has("EFI/BOOT/BOOTX64.NEW"), "after cut {n}: leftovers");
        if finished {
            break;
        }
        n += 1;
        assert!(n < 100);
    }
    assert!(n >= 10, "the plan has its steps");
}

#[test]
fn a_retirement_cut_anywhere_never_loses_v1_without_a_green_backup() {
    let mut n = 0;
    loop {
        let e = env(&format!("rcut{n}"));
        e.v1();
        e.put("README.TXT", b"Georg's note");
        let s0 = e.snap();
        let bk = e.dir.join("bk");
        cut_after(Some(n));
        let r = e.run(&["--retire-v1", "--backup", &bk.display().to_string(), "--write"], "J");
        cut_after(None);
        let finished = r.is_ok();
        if let Err(m) = &r {
            assert!(m.contains("injected fault"), "cut {n}: {m}");
        }
        // The stick still has an identity, Georg's other files are untouched, and if anything of V1 is gone
        // the backup is complete and green.
        let v1_left = e.has("BITSOS/KERNEL.BIN") && e.has("BITSOS/GAMES/DOOM.BXE") && e.has("BITSOS/CONFIG.TXT") && e.has("EFI/BOOT/BOOTX64.EFI");
        if !v1_left {
            assert!(std::fs::read_to_string(bk.join("RESULT.txt")).unwrap_or_default().contains("compare green"), "cut {n}: V1 touched without a green backup");
            assert_eq!(snapshot(&bk.join("files")), s0, "cut {n}");
        }
        let id_v1 = e.has("BITSOS/STICK.ID");
        let id_v2 = e.has("BITSOS2/STICK.ID") && e.get("BITSOS2/STICK.ID") == format!("{ID}\r\n").into_bytes();
        assert!(id_v1 || id_v2, "cut {n}: the stick lost its identity");
        assert_eq!(e.get("README.TXT"), b"Georg's note");
        // A rerun (with a fresh backup folder; a used one is never overwritten) finishes the job.
        let r2 = e.run(&["--retire-v1", "--backup", &e.dir.join("bk2").display().to_string(), "--write"], "J");
        if e.has("BITSOS") || e.has("EFI/BOOT/BOOTX64.EFI") {
            panic!("cut {n}: rerun left V1: {r2:?}");
        }
        if let Err(m) = r2 {
            assert!(m.contains("V1 is not on this stick"), "rerun after cut {n}: {m}");
        }
        let mut want = s0.clone();
        want.retain(|k, _| !k.starts_with("BITSOS/") && k != "BITSOS" && k != "EFI/BOOT/BOOTX64.EFI");
        want.insert("BITSOS2".into(), None);
        want.insert("BITSOS2/STICK.ID".into(), Some(format!("{ID}\r\n").into_bytes()));
        assert_eq!(e.snap(), want, "after cut {n}");
        if finished {
            break;
        }
        n += 1;
        assert!(n < 100);
    }
    assert!(n >= 10, "copy, flushes and removal all had their cut");
}

#[test]
fn retire_reports_how_it_compared() {
    let e = env("retire4");
    e.v1();
    let bk = e.dir.join("bk");
    e.run(&["--retire-v1", "--backup", &bk.display().to_string(), "--write"], "J").unwrap();
    let r = std::fs::read_to_string(bk.join("RESULT.txt")).unwrap();
    assert!(r.contains("O_DIRECT"), "{r}");
}

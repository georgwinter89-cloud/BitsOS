//! FS-15 (1): a FAT file rewritten to another length shows the same size through every path that reads it:
//! stat by path, the dentry's attr (what MCP `stat` takes), fstat of a new and of an older open file, a read to
//! the end, SEEK_END, the parent's listing, and after a remount (the dirent on the disk) and mtools.

use super::util::*;
use std::string::String;
use std::format;
use vfs::abi::{O_RDONLY, O_TRUNC, O_WRONLY, SEEK_END};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

/// Every way the kernel answers "how long is `path`" (`path` below /boot), all equal to `want`.
fn sizes_agree(ns: &vfs::MountNs, path: &str, want: u64) {
    let c = Ctx::kernel(ns);
    let full = format!("/boot{path}");
    assert_eq!(c.stat(None, full.as_bytes(), true).unwrap().size, want, "stat {path}");
    assert_eq!(c.lookup(None, full.as_bytes(), true).unwrap().attr().size, want, "dentry attr {path}");
    let f = c.open(None, full.as_bytes(), O_RDONLY, 0).unwrap();
    assert_eq!(f.stat().size, want, "fstat {path}");
    assert_eq!(f.seek(0, SEEK_END).unwrap(), want, "SEEK_END {path}");
    let mut buf = std::vec![0u8; want as usize + 4096];
    let (mut at, mut n) = (0usize, 1usize);
    while n > 0 {
        n = f.pread(at as u64, &mut buf[at..]).unwrap();
        at += n;
    }
    assert_eq!(at as u64, want, "read to the end {path}");
    assert_eq!(ns.read_file(&full, 1 << 20).unwrap().len() as u64, want, "read_file {path}");
    let (dir, name) = full.rsplit_once('/').unwrap();
    let listed = ns.list_dir(dir).unwrap().into_iter().find(|(n, _)| n == name.as_bytes()).unwrap().1;
    assert_eq!(listed.size, want, "listing of {dir}");
}

/// After an fsync of the file and the volume: a fresh mount of the image and mtools read `want` bytes.
/// `orphan`: a replaced file is still open, so its clusters are still taken (fsck would reclaim them).
fn disk_agrees(dir: &std::path::Path, dev: &crate::MemDev, path: &str, want: u64, stick: bool, orphan: bool) {
    let img = save(dir, dev);
    assert_eq!(get(&img, path).unwrap().len() as u64, want, "mtools {path}");
    let dev2 = load(&img);
    let vol2 = if stick { mount_stick(&dev2) } else { mount_plain(&dev2) };
    sizes_agree(&view(&vol2), path, want);
    if !orphan {
        assert_clean(dir, &dev.image());
    }
}

fn sync(ns: &vfs::MountNs, vol: &crate::Volume, path: &str) {
    ns.lookup(&format!("/boot{path}")).unwrap().inode.sync(SyncKind::Fsync).unwrap();
    vol.sync(true).unwrap();
}

#[test]
fn a_rewritten_file_has_one_size_everywhere() {
    let dir = scratch("sizes");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    mkdir(&img, "/BITSOS2");
    put_bytes(&img, &dir, "/BITSOS2/a.txt", &pattern(179, 1));
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    const A: &str = "/BITSOS2/a.txt";
    let a = format!("/boot{A}");
    sizes_agree(&ns, A, 179);
    // An open file from before the rewrites: it sees the same inode, so its fstat follows every change.
    let early = c.open(None, a.as_bytes(), O_RDONLY, 0).unwrap();

    // Shorter by O_TRUNC and a write.
    let f = c.open(None, a.as_bytes(), O_WRONLY | O_TRUNC, 0).unwrap();
    f.write(&pattern(143, 2)).unwrap();
    drop(f);
    sizes_agree(&ns, A, 143);
    assert_eq!(early.stat().size, 143);
    // Longer by a write past the end, shorter by truncate.
    c.open(None, a.as_bytes(), O_WRONLY, 0).unwrap().pwrite(299, b"x").unwrap();
    sizes_agree(&ns, A, 300);
    c.truncate(None, a.as_bytes(), 50).unwrap();
    sizes_agree(&ns, A, 50);
    assert_eq!(early.stat().size, 50);
    sync(&ns, &vol, A);
    disk_agrees(&dir, &dev, A, 50, false, false);

    // settingsd's way (write_atomic): a.txt.new, then rename over a.txt. The path shows the new file; the
    // older open file keeps the replaced inode (linux-6.6/fs/namei.c vfs_rename: the victim lives on unlinked).
    ns.create_file_bytes(format!("{a}.new").as_bytes(), 0o644, &pattern(120, 3)).unwrap();
    c.rename(None, format!("{a}.new").as_bytes(), None, a.as_bytes(), 0).unwrap();
    sizes_agree(&ns, A, 120);
    assert_eq!(early.stat().size, 50, "the replaced inode keeps its own size");
    // Syncing the replaced (unlinked) file must not reach the slot the new file now holds.
    early.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    sync(&ns, &vol, A);
    disk_agrees(&dir, &dev, A, 120, false, true);
    drop(early);
    sync(&ns, &vol, A);
    disk_agrees(&dir, &dev, A, 120, false, false);

    // Longer again by rename over, then shorter by rename over.
    for (len, seed) in [(4097usize, 4u32), (7, 5)] {
        ns.create_file_bytes(format!("{a}.new").as_bytes(), 0o644, &pattern(len, seed)).unwrap();
        c.rename(None, format!("{a}.new").as_bytes(), None, a.as_bytes(), 0).unwrap();
        sizes_agree(&ns, A, len as u64);
    }
    sync(&ns, &vol, A);
    disk_agrees(&dir, &dev, A, 7, false, false);
}

/// A replaced file still open for writing grows after the rename: its size goes to its own (gone) entry,
/// never to the slot the new file took over.
#[test]
fn a_replaced_file_written_later_leaves_the_new_size_alone() {
    let dir = scratch("sizes-victim");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    mkdir(&img, "/BITSOS2");
    put_bytes(&img, &dir, "/BITSOS2/b.txt", &pattern(179, 1));
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    const B: &str = "/BITSOS2/b.txt";
    let b = format!("/boot{B}");
    let victim = c.open(None, b.as_bytes(), O_WRONLY, 0).unwrap();
    ns.create_file_bytes(format!("{b}.new").as_bytes(), 0o644, &pattern(143, 2)).unwrap();
    c.rename(None, format!("{b}.new").as_bytes(), None, b.as_bytes(), 0).unwrap();
    victim.pwrite(10_000, b"late").unwrap();
    assert_eq!(victim.stat().size, 10_004);
    victim.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    sizes_agree(&ns, B, 143);
    sync(&ns, &vol, B);
    disk_agrees(&dir, &dev, B, 143, false, true);
    drop(victim);
    sync(&ns, &vol, B);
    disk_agrees(&dir, &dev, B, 143, false, false);
}

/// CONFIG.TXT on a stick, rewritten the ways settingsd, MCP write_file and an editor do: the kernel's size is
/// the file's length everywhere. The 143 of MCP `stat` on the Mini-PC is the length of the redacted view
/// (`protect::redact_config`, WL-4 CR 4): a 32-character token and a 10-character wlan_psk read as `***`.
#[test]
fn config_rewrites_keep_one_size_and_the_view_is_shorter() {
    let token = "0123456789abcdef0123456789abcdef";
    let psk = "geheim1234";
    let mut cfg = format!("nvme_write=off\nscreen=2560x1440\ntoken={token}\nwlan=on\nwlan_ssid=FRITZ!Box 7590 XY\nwlan_psk={psk}\nsnd_output=hdmi\n");
    while cfg.len() < 179 {
        cfg.push_str("#\n");
    }
    cfg.truncate(179);
    cfg.pop();
    cfg.push('\n');
    assert_eq!(cfg.len(), 179);
    let view_len = crate::protect::redact_config(cfg.as_bytes()).len();
    assert_eq!(view_len, 143, "what MCP stat and read_file show on the Mini-PC");

    let dir = scratch("sizes-cfg");
    let mut img = super::rules::stick_image(&dir, false);
    let p = dir.join("cfg.img");
    std::fs::write(&p, &img).unwrap();
    put_bytes(&p, &dir, "/BITSOS2/CONFIG.TXT", cfg.as_bytes());
    img = std::fs::read(&p).unwrap();
    let dev = std::sync::Arc::new(crate::MemDev::new(img, 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    const CF: &str = "/BITSOS2/CONFIG.TXT";
    let cf = format!("/boot{CF}");
    sizes_agree(&ns, CF, 179);

    // settingsd: CONFIG.TXT.new without the snd_output line, renamed over (guarded line byte-identical).
    let shorter: String = cfg.replace("snd_output=hdmi\n", "");
    ns.create_file_bytes(format!("{cf}.new").as_bytes(), 0o644, shorter.as_bytes()).unwrap();
    c.rename(None, format!("{cf}.new").as_bytes(), None, cf.as_bytes(), 0).unwrap();
    sizes_agree(&ns, CF, shorter.len() as u64);
    // write_file style: in place from the first line on, then cut to the new length (longer).
    let longer = format!("{shorter}snd_output=analog\nconsole_log=info\n");
    let f = c.open(None, cf.as_bytes(), O_WRONLY, 0).unwrap();
    f.pwrite(0, longer.as_bytes()).unwrap();
    drop(f);
    c.truncate(None, cf.as_bytes(), longer.len() as u64).unwrap();
    sizes_agree(&ns, CF, longer.len() as u64);
    // Back to the 179 bytes by rename over (an O_TRUNC rewrite would re-add nvme_write: EPERM, R8a).
    ns.create_file_bytes(format!("{cf}.new").as_bytes(), 0o644, cfg.as_bytes()).unwrap();
    c.rename(None, format!("{cf}.new").as_bytes(), None, cf.as_bytes(), 0).unwrap();
    sizes_agree(&ns, CF, 179);
    let shown = crate::protect::redact_config(&ns.read_file(&cf, 1 << 16).unwrap());
    assert_eq!(shown.len(), 143);
    sync(&ns, &vol, CF);
    disk_agrees(&dir, &dev, CF, 179, true, false);
}

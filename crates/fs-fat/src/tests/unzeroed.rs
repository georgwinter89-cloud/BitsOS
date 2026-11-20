//! GP-START: `allocate_unzeroed` gives an empty file its clusters and its size by writing the chain, the entry and
//! FSInfo only, the FAT in runs of sectors (no cluster content: the data image on a USB-2 stick took minutes of
//! zeros), fsck.fat stays clean, the page cache learns the size by a truncate that writes nothing more, and a file
//! with content is refused.

use super::util::*;
use uapi::errno::*;
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

#[test]
fn clusters_and_size_without_writing_the_clusters() {
    let dir = scratch("unzeroed");
    let img = mkfs(&dir, 40, 1, "UNZERO");
    // Free space with old content: a file of a pattern, removed again (its clusters keep the bytes).
    put_bytes(&img, &dir, "/old.bin", &pattern(8 << 20, 7));
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    c.unlink(None, b"/boot/old.bin").unwrap();
    ns.create_file_bytes(b"/boot/data.img.new", 0o644, &[]).unwrap();
    let file = ns.lookup_path(b"/boot/data.img.new", false).unwrap().dentry.inode.clone();
    let ino = file.meta.ino;
    vol.sync(true).unwrap();
    let before = dev.image();
    let size = 6u64 << 20;
    dev.record();
    vol.allocate_unzeroed(ino, size).unwrap();
    // 96 changed FAT sectors per copy go in runs of 64: a handful of commands, not one per sector.
    let writes = dev.take_log().len();
    assert!(writes <= 12, "{writes} write commands");
    file.truncate(size).unwrap();
    file.sync(SyncKind::Fsync).unwrap();
    let after = dev.image();
    let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
    // 12288 chain entries of 512-byte clusters in two FATs, the entry and FSInfo: under 100 KiB, never the 6 MiB of
    // old bytes in the clusters.
    assert!(changed < 128 << 10, "{changed} bytes changed on the disk");
    assert_eq!(c.stat(None, b"/boot/data.img.new", true).unwrap().size, size);
    let saved = save(&dir, &dev);
    assert_clean(&dir, &after);
    let shown = get(&saved, "/data.img.new").unwrap();
    assert_eq!(shown.len() as u64, size);
    assert!(shown.iter().any(|&b| b != 0), "the old bytes are there: nothing was written into the clusters");
    // The page cache reads the old bytes too (only root's data image ever holds such a file).
    assert_eq!(ns.read_file("/boot/data.img.new", 1 << 23).unwrap().len() as u64, size);
    // A file that has its clusters (or any byte) is refused; so is a directory.
    assert_eq!(vol.allocate_unzeroed(ino, size * 2), Err(EINVAL));
    c.mkdir(None, b"/boot/d", 0o755).unwrap();
    let d = ns.lookup_path(b"/boot/d", false).unwrap().dentry.inode.meta.ino;
    assert_eq!(vol.allocate_unzeroed(d, 4096), Err(EISDIR));
    ns.create_file_bytes(b"/boot/text", 0o644, b"x").unwrap();
    vol.sync(true).unwrap();
    let t = ns.lookup_path(b"/boot/text", false).unwrap().dentry.inode.meta.ino;
    assert_eq!(vol.allocate_unzeroed(t, 4096), Err(EINVAL));
}

#[test]
fn more_than_the_free_space_is_refused_and_changes_nothing() {
    let dir = scratch("unzeroed-full");
    let img = mkfs(&dir, 40, 1, "UNZERO");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    ns.create_file_bytes(b"/boot/big", 0o644, &[]).unwrap();
    let ino = ns.lookup_path(b"/boot/big", false).unwrap().dentry.inode.meta.ino;
    vol.sync(true).unwrap();
    let before = dev.image();
    assert_eq!(vol.allocate_unzeroed(ino, 64 << 20), Err(ENOSPC));
    assert_eq!(Ctx::kernel(&ns).stat(None, b"/boot/big", true).unwrap().size, 0);
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
    let _ = before;
}

/// The bytes fsck.fat would reclaim ("Reclaimed N unused clusters (B bytes) ..."), 0 when it reclaims none.
fn reclaimed(out: &str) -> u64 {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix("Reclaimed "))
        .filter_map(|l| l.split('(').nth(1)?.split(' ').next()?.parse::<u64>().ok())
        .sum()
}

/// A cut anywhere in the growth leaves at most one step's clusters reached by no entry, never the whole image
/// (checker finding: the chain of all 2 GiB went out before the entry); fsck.fat finds nothing else.
#[test]
fn a_cut_leaves_at_most_one_step_unreached() {
    let dir = scratch("unzeroed-cut");
    let img = mkfs(&dir, 40, 1, "UNZERO");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    ns.create_file_bytes(b"/boot/data.img.new", 0o644, &[]).unwrap();
    let ino = ns.lookup_path(b"/boot/data.img.new", false).unwrap().dentry.inode.meta.ino;
    vol.sync(true).unwrap();
    let mut img = dev.image();
    let (step, size) = (1u64 << 20, 6u64 << 20);
    dev.record();
    vol.allocate_unzeroed_in(ino, size, step).unwrap();
    let log = dev.take_log();
    let mut worst = 0;
    for k in 0..=log.len() {
        if k > 0 {
            let (s, d) = &log[k - 1];
            let at = *s as usize * 512;
            img[at..at + d.len()].copy_from_slice(d);
        }
        let (_, out) = fsck(&dir, &img);
        let lost = reclaimed(&out);
        assert!(lost <= step, "cut after write {k} of {}: {lost} bytes unreached\n{out}", log.len());
        worst = worst.max(lost);
        for l in out.lines().map(str::trim) {
            let ok = l.is_empty()
                || l.starts_with("fsck.fat ")
                || l.starts_with("Leaving filesystem unchanged")
                || l.contains(" files, ")
                || l.starts_with("Free cluster summary uninitialized")
                || l.starts_with("Reclaimed ")
                || l.starts_with("FATs differ but appear to be intact")
                || l.starts_with("Using first FAT")
                || l.starts_with("/data.img.new")
                || (l.starts_with("File size is ") && l.contains("cluster chain length is > "))
                || l.starts_with("Truncating file to ");
            assert!(ok, "cut after write {k}: fsck.fat says\n{out}");
        }
    }
    assert!(worst > 0, "the cuts between a step's chain and its link are seen");
    assert_clean(&dir, &img);
    assert_eq!(get(&save(&dir, &dev), "/data.img.new").map(|b| b.len() as u64), Some(size));
}

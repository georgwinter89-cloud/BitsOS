//! FSInfo's free count after a completed sync (nv/43-boot-nvme saw fsck.fat's "Free cluster summary
//! uninitialized"): a file grown by appends with an fsync and a volume sync after each, as the measurement run
//! writes its protocol, leaves the count right at every step; a removed file still held open keeps it "unknown"
//! until the last holder lets go (its clusters are still in use), and the first sync after that writes the count.

use super::util::*;
use std::format;
use std::vec::Vec;
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

#[test]
fn appends_with_a_sync_after_each_leave_fsinfo_right_at_every_step() {
    let dir = scratch("fsinfo-append");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    mkdir(&img, "/BITSOS2");
    let dev = load(&img);
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let head = b"## head\n".to_vec();
    let d = ns.create_file_bytes(b"/boot/BITSOS2/REPORT/GPUM0001.TXT", 0o644, &head).unwrap();
    d.inode.sync(SyncKind::Fsync).unwrap();
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
    let mut at = head.len() as u64;
    let mut all = head.clone();
    for i in 0..40u32 {
        // Stages of very different sizes: some cross a cluster, some fill many.
        let chunk: Vec<u8> = format!("SPUR {i} {}\n", "x".repeat((i as usize * 997) % 9000)).into_bytes();
        {
            let _w = d.sb.write_begin().unwrap();
            assert_eq!(d.inode.write_at(at, &chunk).unwrap(), chunk.len());
        }
        d.inode.sync(SyncKind::Fsync).unwrap();
        vol.sync(true).unwrap();
        at += chunk.len() as u64;
        all.extend_from_slice(&chunk);
        assert_clean(&dir, &dev.image());
    }
    assert_eq!(ns.read_file("/boot/BITSOS2/REPORT/GPUM0001.TXT", 1 << 22).unwrap(), all);
}

#[test]
fn a_removed_file_held_open_keeps_the_count_unknown_until_it_is_let_go() {
    let dir = scratch("fsinfo-held");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    mkdir(&img, "/BITSOS2");
    let dev = load(&img);
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/boot/BITSOS2/held.bin", O_CREAT | O_RDWR, 0o644).unwrap();
    f.write(&pattern(200_000, 7)).unwrap();
    f.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
    c.unlink(None, b"/boot/BITSOS2/held.bin").unwrap();
    vol.sync(true).unwrap();
    // Still open: its clusters are in use and no entry reaches them, so the count says "unknown" (by design).
    let (_, out) = fsck(&dir, &dev.image());
    assert!(out.contains("Free cluster summary uninitialized"), "{out}");
    drop(f);
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
}

#[test]
fn a_file_renewed_by_create_leaves_the_count_right_after_the_next_sync() {
    let dir = scratch("fsinfo-renew");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    mkdir(&img, "/BITSOS2");
    let dev = load(&img);
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    // gp-report renews its file by creating it again (kernel/src/gpreport.rs write_file): the old inode goes.
    for i in 0..6u32 {
        let d = ns.create_file_bytes(b"/boot/BITSOS2/REPORT/GP000001.TXT", 0o644, &pattern(5000 + i as usize * 3000, i)).unwrap();
        d.inode.sync(SyncKind::Fsync).unwrap();
        vol.sync(true).unwrap();
        drop(d);
        vol.sync(true).unwrap();
        assert_clean(&dir, &dev.image());
    }
}

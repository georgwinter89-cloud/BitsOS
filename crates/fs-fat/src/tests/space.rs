//! A full volume: `write` answers ENOSPC at once (clusters are promised when the cache grows, allocated at
//! write-back), mkdir and a directory that must grow answer ENOSPC before anything changes, a rename into a
//! full directory never loses the file, nothing leaks, fsck.fat stays clean, and freeing space makes writes
//! work again.

use super::util::*;
use std::sync::Arc;
use std::vec::Vec;
use std::{format, vec};
use uapi::errno::*;
use vfs::abi::{O_CREAT, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

#[test]
fn a_full_volume_answers_enospc_and_recovers() {
    let dir = scratch("full");
    // The smallest FAT32 with 512-byte clusters (65525 clusters and a little more).
    let img = mkfs(&dir, 34, 1, "FULL");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let big = c.open(None, b"/boot/big", O_WRONLY | O_CREAT, 0o644).unwrap();
    let mut data: Vec<u8> = Vec::new();
    // Big pieces, then smaller ones, until not one more byte fits.
    for piece in [1 << 20, 64 << 10, 4096, 512, 1] {
        loop {
            let chunk = pattern(piece, data.len() as u32);
            match big.pwrite(data.len() as u64, &chunk) {
                Ok(n) => data.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    assert_eq!(e, ENOSPC, "piece {piece}");
                    break;
                }
            }
            if data.len() % 512 == 0 && piece == 1 {
                // A partly filled last cluster takes bytes without a new cluster; stop at the boundary.
                break;
            }
        }
    }
    big.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    let (_, _, free) = vol.statfs().unwrap();
    assert_eq!(free, 0, "the promised clusters were all allocated");
    // A new file's entry fits in the root's free slots; its first byte needs a cluster.
    let x = c.open(None, b"/boot/x", O_WRONLY | O_CREAT, 0o644).unwrap();
    assert_eq!(x.pwrite(0, b"1").err(), Some(ENOSPC));
    assert_eq!(c.mkdir(None, b"/boot/d", 0o755).err(), Some(ENOSPC));
    // Fill the root's slots: once it must grow, create answers ENOSPC.
    let mut made = 0;
    let full = loop {
        match c.open(None, format!("/boot/f{made}").as_bytes(), O_WRONLY | O_CREAT, 0o644) {
            Ok(_) => made += 1,
            Err(e) => break e,
        }
        assert!(made < 10_000);
    };
    assert_eq!(full, ENOSPC);
    // A rename into the full directory needs more slots: refused before the old entry is cleared.
    assert_eq!(c.rename(None, b"/boot/big", None, b"/boot/a much longer name for big.bin", 0).err(), Some(ENOSPC));
    assert_eq!(ns.read_file("/boot/big", 64 << 20).unwrap().len(), data.len());
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
    let after = save(&dir, &dev);
    assert!(get(&after, "/big").unwrap() == data, "the big file reads back whole");
    // Freeing space makes everything work again.
    drop(big);
    c.unlink(None, b"/boot/big").unwrap();
    x.pwrite(0, &vec![7u8; 100_000]).unwrap();
    x.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    c.mkdir(None, b"/boot/d", 0o755).unwrap();
    c.rename(None, b"/boot/f0", None, b"/boot/a much longer name for f0.bin", 0).unwrap();
    drop(x);
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
    let after = save(&dir, &dev);
    assert_eq!(get(&after, "/x").unwrap(), vec![7u8; 100_000]);
    assert!(get(&after, "/big").is_none());
    let _ = Arc::strong_count(&vol);
}

#[test]
fn promises_hold_between_files() {
    let dir = scratch("promise");
    let img = mkfs(&dir, 34, 1, "PROMISE");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let (_, _, free) = vol.statfs().unwrap();
    let half = (free / 2 * 512) as usize;
    let a = c.open(None, b"/boot/a", O_WRONLY | O_CREAT, 0o644).unwrap();
    let b = c.open(None, b"/boot/b", O_WRONLY | O_CREAT, 0o644).unwrap();
    // Both only cached: the second one's promise must already see the first.
    a.pwrite(0, &pattern(half, 1)).unwrap();
    b.pwrite(0, &pattern(half - 512 * 4, 2)).unwrap();
    assert_eq!(b.pwrite((half - 512 * 4) as u64, &vec![1u8; 512 * 8]).err(), Some(ENOSPC));
    // Shrinking a gives its promise back.
    c.truncate(None, b"/boot/a", 0).unwrap();
    b.pwrite((half - 512 * 4) as u64, &vec![1u8; 512 * 8]).unwrap();
    b.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    a.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    drop((a, b));
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
}

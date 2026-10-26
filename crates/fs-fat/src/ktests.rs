//! In-kernel tests of FS-7 on the boot stick (the xtask image in QEMU, Georg's stick on the Mini-PC).
//!
//! Safe (nothing is written; rerunnable; the Mini-PC runs them over MCP):
//! - `fs.fat.mounted`: the stick is at /boot, label BITSOS, identity confirmed (read-write), `\BITSOS2\STICK.ID`
//!   readable; the block layer's stick disk is opened by FAT's gate.
//! - `fs.fat.tree`: the whole tree is read: every directory listed, every file up to 1 MiB read whole, bigger
//!   ones at their start and end; nothing written.
//! - `fs.fat.refusals`: writes outside `\BITSOS2\`, to STICK.ID, into SYS and a guarded CONFIG.TXT line are
//!   refused (EROFS/EPERM) before anything reaches the stick (its request counter does not move).
//!
//! QemuOnly (they write the stick image of this run):
//! - `fs.fat.write`: files of several sizes and names below `\BITSOS2\ktest-fs7\`, read back, rename over,
//!   unlink of an open file, truncate, fsync (SYNCHRONIZE CACHE reaches the device); the folder is removed at
//!   the end; `\BITSOS2\FS7CHECK.TXT` stays for the scenario's fsck.fat and mtype check.
//! - `fs.fat.sysapi`: a generation 99999999 made, written, read back, marked, pinned and removed again.
//! - `fs.fat.gate`: raw writes to the stick's block 0 and to a data sector are refused by the gate while FAT's
//!   own writes pass.

use crate::stick;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use uapi::errno::*;
use vfs::abi::{O_APPEND, O_CREAT, O_RDWR, O_TRUNC, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn mounted() -> Result<alloc::sync::Arc<crate::Volume>, TestResult> {
    match stick::wait_mounted(40_000) {
        Some(v) => Ok(v),
        None => Err(match stick::last_reason() {
            Some(r) => Err(format!("the boot stick was not mounted: {r}")),
            None => ktest::skip("no boot stick (no USB disk recognised as the stick)"),
        }),
    }
}

fn stick_disk() -> Option<alloc::sync::Arc<block::Disk>> {
    block::disks().into_iter().find(|d| d.kind() == block::DiskKind::Usb && d.stick() == block::StickMatch::Yes)
}

fn fs_fat_mounted() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    kassert!(vol.label_text().eq_ignore_ascii_case(b"BITSOS"), "label {:?}", String::from_utf8_lossy(&vol.label_text()));
    kassert!(vol.writable(), "the stick is read-only: {}", stick::last_reason().unwrap_or("?"));
    let ns = vfs::root_ns();
    let id = ns.read_file("/boot/BITSOS2/STICK.ID", 128).map_err(e)?;
    kassert!(stick::parse_stick_id(&id).is_some(), "STICK.ID {:?}", String::from_utf8_lossy(&id));
    let d = stick_disk().ok_or("no USB disk matches the stick")?;
    kassert!(d.stick_opened(), "the block layer's stick disk is not opened by FAT's gate");
    let (cb, clusters, free) = vol.statfs().map_err(e)?;
    klog::event!(Info, "fat", "ktest.mounted", disk = String::from(d.name()), cluster_bytes = cb, clusters = clusters, free = free);
    Ok(())
}
ktest!("fs.fat.mounted", Safe, fs_fat_mounted);

fn fs_fat_tree() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    let ns = vfs::root_ns();
    let before = stick_disk().map(|d| d.stats());
    let changes = vol.changes.load(core::sync::atomic::Ordering::Relaxed);
    let (mut dirs, mut files, mut bytes) = (0u64, 0u64, 0u64);
    let mut work = vec![String::from("/boot")];
    while let Some(dir) = work.pop() {
        dirs += 1;
        for (name, a) in ns.list_dir(&dir).map_err(|x| format!("{dir}: {x}"))? {
            let p = format!("{dir}/{}", String::from_utf8_lossy(&name));
            if a.mode & uapi::S_IFMT == uapi::S_IFDIR {
                work.push(p);
                continue;
            }
            files += 1;
            let f = vfs::node::file_of(&ns.lookup(&p).map_err(|x| format!("{p}: {x}"))?);
            let size = a.size;
            let mut buf = vec![0u8; 64 << 10];
            let mut at = 0u64;
            while at < size {
                let n = f.read_at(at, &mut buf).map_err(|x| format!("{p} at {at}: {x}"))?;
                kassert!(n > 0, "{p}: short read at {at} of {size}");
                bytes += n as u64;
                at += n as u64;
                if size > (1 << 20) && at >= (64 << 10) && at < size - (64 << 10) {
                    at = size - (64 << 10);
                }
            }
        }
    }
    kassert!(vol.changes.load(core::sync::atomic::Ordering::Relaxed) == changes, "reading changed the volume");
    if let (Some(b), Some(d)) = (before, stick_disk()) {
        kassert!(d.stats().refused == b.refused);
    }
    klog::event!(Info, "fat", "ktest.tree", dirs = dirs, files = files, bytes = bytes);
    Ok(())
}
ktest!("fs.fat.tree", Safe, fs_fat_tree);

fn fs_fat_refusals() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    let ns = vfs::root_ns();
    let c = Ctx::kernel(&ns);
    // Load the directories first (reads), then count: no refusal may send a request.
    for p in ["/boot", "/boot/BITSOS2", "/boot/EFI", "/boot/EFI/BOOT", "/boot/BITSOS2/SYS"] {
        let _ = ns.list_dir(p);
    }
    // CONFIG.TXT's check reads its content: cached now, so the count below sees writes only.
    let _ = ns.read_file("/boot/BITSOS2/CONFIG.TXT", 1 << 20);
    let d = stick_disk().ok_or("no stick disk")?;
    let before = d.stats().requests;
    let changes = vol.changes.load(core::sync::atomic::Ordering::Relaxed);
    let open = |p: &str, f: u64| c.open(None, p.as_bytes(), f, 0o644).map(|_| ()).err();
    kassert!(open("/boot/fs7-refused.txt", O_WRONLY | O_CREAT) == Some(EROFS));
    kassert!(c.mkdir(None, b"/boot/EFI/fs7-refused", 0o755).err() == Some(EROFS));
    kassert!(open("/boot/EFI/BOOT/BOOTX64.EFI", O_WRONLY) == Some(EROFS));
    kassert!(c.unlink(None, b"/boot/EFI/BOOT/BOOTX64.EFI").err() == Some(EROFS));
    if vol.writable() {
        kassert!(open("/boot/BITSOS2/STICK.ID", O_WRONLY) == Some(EPERM));
        kassert!(open("/boot/bitsos2/stick.id", O_RDWR | O_TRUNC) == Some(EPERM));
        kassert!(c.unlink(None, b"/boot/BITSOS2/STICK.ID").err() == Some(EPERM));
        kassert!(c.rename(None, b"/boot/BITSOS2/STICK.ID", None, b"/boot/BITSOS2/fs7.id", 0).err() == Some(EPERM));
        kassert!(c.mkdir(None, b"/boot/BITSOS2/SYS/fs7-refused", 0o755).err() == Some(EPERM));
        if ns.lookup("/boot/BITSOS2/CONFIG.TXT").is_ok() {
            let cfg = c.open(None, b"/boot/BITSOS2/CONFIG.TXT", O_WRONLY | O_APPEND, 0).map_err(e)?;
            kassert!(cfg.write(b"\nusb_msd_write=on\n").err() == Some(EPERM));
            kassert!(cfg.write(b"\nnvme_write=on\n").err() == Some(EPERM));
        }
    }
    kassert!(d.stats().requests == before, "a refused write sent {} requests", d.stats().requests - before);
    kassert!(vol.changes.load(core::sync::atomic::Ordering::Relaxed) == changes, "a refusal began a change");
    Ok(())
}
ktest!("fs.fat.refusals", Safe, fs_fat_refusals);

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (s >> 16) as u8
        })
        .collect()
}

fn fs_fat_write() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    kassert!(vol.writable(), "read-only: {}", stick::last_reason().unwrap_or("?"));
    let ns = vfs::root_ns();
    let c = Ctx::kernel(&ns);
    let dir = "/boot/BITSOS2/ktest-fs7";
    let _ = ns.remove_tree(dir);
    c.mkdir(None, dir.as_bytes(), 0o755).map_err(e)?;
    let d = stick_disk().ok_or("no stick disk")?;
    let flushes_before = d.stats().dispatches;
    let mut want: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, len, seed) in [("empty", 0usize, 0u32), ("Größe.txt", 4097, 1), ("A long name with spaces and dots.v1.2.bin", 70_000, 2), ("big", 2 << 20, 3)] {
        let p = format!("{dir}/{name}");
        let data = pattern(len, seed);
        let node = ns.create_file_bytes(p.as_bytes(), 0o644, &data).map_err(|x| format!("{p}: {x}"))?;
        node.inode.sync(SyncKind::Fsync).map_err(e)?;
        want.push((p, data));
    }
    // Rename over an existing file while a reader holds the old one; unlink of an open file.
    let target = format!("{dir}/big");
    let old = c.open(None, target.as_bytes(), O_RDWR, 0).map_err(e)?;
    let small = format!("{dir}/Größe.txt");
    c.rename(None, small.as_bytes(), None, target.as_bytes(), 0).map_err(e)?;
    let mut head = [0u8; 16];
    old.pread(0, &mut head).map_err(e)?;
    kassert!(head[..] == pattern(2 << 20, 3)[..16], "the replaced file's reader lost its content");
    drop(old);
    want.retain(|(p, _)| *p != small);
    want.iter_mut().find(|(p, _)| *p == target).ok_or("big")?.1 = pattern(4097, 1);
    let open_gone = format!("{dir}/empty");
    let f = c.open(None, open_gone.as_bytes(), O_RDWR, 0).map_err(e)?;
    c.unlink(None, open_gone.as_bytes()).map_err(e)?;
    f.pwrite(0, b"written after unlink").map_err(e)?;
    let mut b = [0u8; 20];
    f.pread(0, &mut b).map_err(e)?;
    kassert!(&b == b"written after unlink");
    drop(f);
    want.retain(|(p, _)| *p != open_gone);
    let t = format!("{dir}/A long name with spaces and dots.v1.2.bin");
    c.truncate(None, t.as_bytes(), 1000).map_err(e)?;
    want.iter_mut().find(|(p, _)| *p == t).ok_or("t")?.1.truncate(1000);
    for (p, data) in &want {
        let got = ns.read_file(p, 4 << 20).map_err(|x| format!("{p}: {x}"))?;
        kassert!(got == *data, "{p}: {} bytes back, want {}", got.len(), data.len());
    }
    kassert!(d.stats().dispatches > flushes_before);
    ns.remove_tree(dir).map_err(e)?;
    // The row's QEMU check: one file stays for fsck.fat and mtype on the image after the run.
    let check = b"FS-7: written by BitsOS V2 on its boot stick\r\n";
    let node = ns.create_file_bytes(b"/boot/BITSOS2/FS7CHECK.TXT", 0o644, check).map_err(e)?;
    node.inode.sync(SyncKind::Fsync).map_err(e)?;
    vol.sync(true).map_err(e)?;
    klog::event!(Info, "fat", "ktest.written", files = want.len(), check = "BITSOS2/FS7CHECK.TXT");
    Ok(())
}
ktest!("fs.fat.write", QemuOnly, fs_fat_write);

fn fs_fat_sysapi() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    kassert!(vol.writable());
    let n = 99_999_999;
    stick::with_sys(|s| -> TestResult {
        let _ = s.remove_generation(n);
        s.create_generation(n).map_err(e)?;
        let k = pattern(300_000, 9);
        s.write_file(n, "KERNEL", &k).map_err(e)?;
        kassert!(s.write_file(n, "KERNEL", b"x") == Err(EEXIST));
        kassert!(s.read_file(n, "KERNEL", 1 << 20).map_err(e)? == k);
        s.set_marker(n, crate::sysapi::Marker::Ok, true).map_err(e)?;
        let g = s.generations().map_err(e)?;
        kassert!(g.iter().any(|g| g.n == n && g.ok), "{g:?}");
        let ns = vfs::root_ns();
        kassert!(Ctx::kernel(&ns).unlink(None, format!("/boot/BITSOS2/SYS/{n}/OK").as_bytes()).err() == Some(EPERM), "SYS outside the API");
        s.remove_generation(n).map_err(e)?;
        kassert!(!s.generations().map_err(e)?.iter().any(|g| g.n == n));
        Ok(())
    })
    .map_err(e)?
}
ktest!("fs.fat.sysapi", QemuOnly, fs_fat_sysapi);

fn fs_fat_gate() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    kassert!(vol.writable());
    let d = stick_disk().ok_or("no stick disk")?;
    let bs = d.block_size() as usize;
    let b0 = d.read_vec(0, bs, devmodel::block::IoPriority::Normal).map_err(e)?;
    let refused = Err(block::BlockError::ReadOnly(block::Refusal::Gate("fat.stick")));
    kassert!(d.write(0, &b0, false, devmodel::block::IoPriority::Normal) == refused, "block 0");
    let data = stick::start_lba() + vol.geo.data_start + 100;
    let s = d.read_vec(data, bs, devmodel::block::IoPriority::Normal).map_err(e)?;
    kassert!(d.write(data, &s, false, devmodel::block::IoPriority::Normal) == refused, "a data sector FAT did not announce");
    kassert!(d.discard(data, 1, devmodel::block::IoPriority::Normal) == refused, "a discard");
    Ok(())
}
ktest!("fs.fat.gate", QemuOnly, fs_fat_gate);

/// Scenario `host/tests/fs/30-fat.sh` (second boot): a clone of the boot stick on a second USB port. Both
/// match bitsboot's block 0: a doubt, so /boot is read-only and the gate opens neither disk.
fn fs_fat_doubt() -> TestResult {
    let yes = || block::disks().into_iter().filter(|d| d.kind() == block::DiskKind::Usb && d.stick() == block::StickMatch::Yes).count();
    // Wait until USB is quiet (no disk came or went for 2 s; at most 40 s), as block's USB tests do.
    let now = || ksync::now_ns().unwrap_or(u64::MAX);
    let end = now().saturating_add(40_000_000_000);
    let (mut last, mut quiet) = (block::generation(), now());
    while now() < end {
        sched::sleep_ns(50_000_000);
        let g = block::generation();
        if g != last {
            last = g;
            quiet = now();
        } else if now().saturating_sub(quiet) >= 2_000_000_000 && yes() >= 1 {
            break;
        }
    }
    if yes() < 2 {
        return ktest::skip("needs a second disk that looks like the boot stick (host/tests/fs/30-fat.sh)");
    }
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    let end = ksync::now_ns().unwrap_or(0) + 5_000_000_000;
    while vol.writable() && ksync::now_ns().unwrap_or(u64::MAX) < end {
        sched::sleep_ns(20_000_000);
    }
    kassert!(!vol.writable(), "two boot sticks and /boot still writable");
    for d in block::disks().into_iter().filter(|d| d.kind() == block::DiskKind::Usb) {
        kassert!(!d.stick_opened(), "{}: the gate is open", d.name());
    }
    let ns = vfs::root_ns();
    let c = Ctx::kernel(&ns);
    kassert!(c.open(None, b"/boot/BITSOS2/doubt.txt", O_WRONLY | O_CREAT, 0o644).err() == Some(EROFS));
    kassert!(stick::with_sys(|s| s.create_generation(99_999_998)).map_err(e)? == Err(EROFS));
    klog::event!(Info, "fat", "ktest.doubt", sticks = yes(), reason = stick::last_reason().unwrap_or("second stick after the mount"));
    Ok(())
}
ktest!("fs.fat.doubt", QemuOnly, fs_fat_doubt);

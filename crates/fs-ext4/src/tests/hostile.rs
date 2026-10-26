//! Hostile disks: bytes of metadata blocks flipped at random (with and without checksums to catch them),
//! then mount, read everything and change a little. Nothing may panic or hang; errors are fine.

use super::util::*;
use crate::inode::ROOT;
use crate::{Kind, MemDev, Volume};
use std::format;
use std::sync::Arc;
use std::string::String;
use std::vec;
use std::vec::Vec;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn poke(v: &Volume, dir: u32, depth: u32, budget: &mut u32) {
    if depth > 6 || *budget == 0 {
        return;
    }
    *budget -= 1;
    let Ok(l) = v.list(dir) else { return };
    for (name, ino, _) in l.into_iter().take(50) {
        let Ok(st) = v.stat(ino) else { continue };
        if st.is_dir() {
            poke(v, ino, depth + 1, budget);
        } else if st.is_symlink() {
            let _ = v.readlink(ino);
        } else {
            let mut b = std::vec![0u8; st.size().min(256 * 1024) as usize];
            let _ = v.read(ino, 0, &mut b);
            let n = b.len().min(4096);
            let _ = v.read(ino, st.size().saturating_sub(4096), &mut b[..n]);
        }
        let _ = v.xattrs(ino);
        if depth == 1 {
            let _ = v.rename(dir, &name, ino, ROOT, format!("r{ino}").as_bytes(), None);
        }
    }
    let _ = v.create(dir, b"new-file", 0o100644, 0, 0, Kind::Reg).map(|n| {
        let _ = v.write(n.ino, 0, &[1u8; 9000], 9000, 0..3);
        let _ = v.set_size(n.ino, 10);
        let _ = v.remove(dir, b"new-file", n.ino);
    });
}

fn metadata_blocks(s: &Scratch, img: &[u8], bs: usize) -> Vec<usize> {
    // Blocks that are not file data: everything debugfs's icheck does not call a data block of a file is
    // a candidate; simpler: the first 2 MiB (superblock, descriptors, bitmaps, inode tables, directories
    // and extent blocks of the small tree all sit there).
    let _ = s;
    (0..(img.len().min(2 << 20) / bs)).collect()
}

#[test]
fn random_damage_never_panics() {
    for (opts_, seed) in [(&["-b", "4096"][..], 1u64), (&["-b", "1024", "-O", "^metadata_csum"][..], 2), (&["-b", "4096", "-O", "^metadata_csum,^64bit"][..], 3)] {
        let s = Scratch::new();
        let t = tree(&s);
        let base = mkfs(&s, "16M", opts_, Some(&t));
        let bs = if opts_.contains(&"1024") { 1024 } else { 4096 };
        let blocks = metadata_blocks(&s, &base, bs);
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for round in 0..400 {
            let mut img = base.clone();
            for _ in 0..1 + rng.next() % 6 {
                let b = blocks[(rng.next() % blocks.len() as u64) as usize];
                let at = b * bs + (rng.next() % bs as u64) as usize;
                img[at] ^= 1 << (rng.next() % 8);
                if rng.next() % 4 == 0 {
                    img[at] = (rng.next() & 0xFF) as u8;
                }
            }
            let r = std::panic::catch_unwind(|| {
                let dev = Arc::new(MemDev::new(img));
                if let Ok(v) = Volume::mount(dev, opts()) {
                    let mut budget = 200;
                    poke(&v, ROOT, 0, &mut budget);
                    let _ = v.statfs();
                    let _ = v.sync_fs();
                }
            });
            assert!(r.is_ok(), "round {round} (seed {seed}) panicked");
        }
    }
}

/// Changes `img` with debugfs -w commands (sif/ssv/unlink/link: debugfs reseals the checksums).
fn damage(s: &Scratch, img: &[u8], cmds: &[String]) -> Vec<u8> {
    let p = s.path("dmg.img");
    std::fs::write(&p, img).unwrap();
    let f = s.path("dmg.cmds");
    std::fs::write(&f, cmds.join("\n") + "\n").unwrap();
    let o = std::process::Command::new("debugfs").args(["-w", "-f"]).arg(&f).arg(&p).output().unwrap();
    assert!(o.status.success(), "debugfs: {}", String::from_utf8_lossy(&o.stderr));
    std::fs::read(&p).unwrap()
}

/// Mount, poke and sync on its own thread; it must mount and end within a minute without a panic.
fn survives(img: Vec<u8>, what: &str) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(|| {
            let dev = Arc::new(MemDev::new(img));
            let v = Volume::mount(dev, opts()).ok()?;
            let mut budget = 200;
            poke(&v, ROOT, 0, &mut budget);
            let _ = v.statfs();
            let _ = v.sync_fs();
            Some(())
        });
        let _ = tx.send(r.map(|m| m.is_some()));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(60)) {
        Ok(Ok(mounted)) => assert!(mounted, "{what}: the mount was refused"),
        Ok(Err(_)) => panic!("{what}: panicked"),
        Err(_) => panic!("{what}: did not end within 60 s"),
    }
}

/// Orphan lists and trees a hostile disk can bring: loops, a bad member, a directory loop.
#[test]
fn hostile_orphans_and_loops() {
    for opts_ in [&["-b", "4096"][..], &["-b", "1024", "-O", "^metadata_csum"][..]] {
        let s = Scratch::new();
        let t = tree(&s);
        let base = mkfs(&s, "64M", opts_, Some(&t));
        let (_d, v) = super::util::mount(base.clone());
        let a = lookup(&v, "/hello.txt");
        let b = lookup(&v, "/sub/deeper/data.bin");
        let c = lookup(&v, "/sparse");
        let d = lookup(&v, "/sub");
        drop(v);
        let cases: Vec<(&str, Vec<String>)> = vec![
            ("orphan pointing at itself", vec![format!("ssv last_orphan {a}"), format!("sif <{a}> dtime {a}")]),
            ("two orphans in a ring", vec![format!("ssv last_orphan {a}"), format!("sif <{a}> dtime {b}"), format!("sif <{b}> dtime {a}")]),
            ("unlinked orphan in a ring", vec![
                "unlink /sparse".into(), format!("sif <{c}> links_count 0"),
                format!("ssv last_orphan {c}"), format!("sif <{c}> dtime {b}"), format!("sif <{b}> dtime {c}"),
            ]),
            ("orphan past the inode count", vec!["ssv last_orphan 4000000000".into()]),
            ("orphan with a broken extent tree", vec![
                "unlink /sub/deeper/data.bin".into(), format!("sif <{b}> links_count 0"),
                format!("sif <{b}> block[2] 0xdeadbeef"), format!("sif <{b}> block[4] 0xffffffff"), format!("ssv last_orphan {b}"),
            ]),
            ("unused inode on the orphan list", vec![format!("ssv last_orphan {}", c + 400)]),
            ("directory loop", vec![format!("link <{d}> /sub/deeper/up")]),
            ("directory that holds itself", vec![format!("link <{d}> /sub/self"), format!("sif <{d}> links_count 1")]),
        ];
        for (what, cmds) in cases {
            survives(damage(&s, &base, &cmds), what);
        }
    }
}

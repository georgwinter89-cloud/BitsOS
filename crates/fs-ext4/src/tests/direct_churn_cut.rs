//! FS-W2 (the checker's probe, taken over): three programs create, write (background write-back and fsync),
//! truncate inside a block, write again further on and unlink files through the VFS on a nearly full volume on the
//! disk path, so freed blocks are taken again at once; a cut at every device call, the unflushed writes lost at
//! random. A file shows only its own bytes, zeros in the truncated gap, never a deleted file's old bytes and never
//! its own bytes from before the truncate; e2fsck finds nothing.
use super::disk::*;
use super::flight::{pattern, read_all, step, MIB};
use super::flight_cut::{leave_old_bytes, OLD};
use super::lay::{cut_image, Rng};
use super::util::*;
use super::write::file;
use crate::inode::ROOT;
use crate::Volume;
use std::format;
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

const THREADS: usize = 3;
const GENS: usize = 6;
const CH: usize = 256 << 10;
const CUT: usize = (300 << 10) + 904;
const AGAIN: usize = 400 << 10;
const AGAIN_LEN: usize = 200 << 10;

fn full(t: usize) -> usize {
    (700 << 10) + t * (300 << 10) + 100
}
fn want(t: usize, g: usize) -> Vec<u8> {
    pattern(full(t), 1000 + (t * 100 + g) as u32)
}

fn run(ns: &Arc<vfs::MountNs>) {
    let mut threads = Vec::new();
    for t in 0..THREADS {
        let ns = ns.clone();
        threads.push(std::thread::spawn(move || {
            for g in 0..GENS {
                let data = want(t, g);
                let path = format!("/data/t{t}g{g}");
                let c = Ctx::kernel(&ns);
                let f = c.open(None, path.as_bytes(), O_RDWR | O_CREAT, 0o644).unwrap();
                for (i, at) in (0..data.len()).step_by(CH).enumerate() {
                    let end = (at + CH).min(data.len());
                    assert_eq!(f.pwrite(at as u64, &data[at..end]), Ok(end - at), "{path}");
                    let kind = match (i + g + t) % 3 {
                        0 => Some(SyncKind::Background),
                        1 => Some(SyncKind::Fsync),
                        _ => None,
                    };
                    if let Some(k) = kind {
                        ns.lookup(&path).unwrap().inode.sync(k).unwrap();
                    }
                }
                drop(f);
                if g > 0 {
                    let prev = format!("/data/t{t}g{}", g - 1);
                    if (t + g) % 2 == 0 {
                        c.unlink(None, prev.as_bytes()).unwrap();
                    } else {
                        // Cut inside a block, then written again further on: the gap reads zeros.
                        let old = want(t, g - 1);
                        c.truncate(None, prev.as_bytes(), CUT as u64).unwrap();
                        let f = c.open(None, prev.as_bytes(), O_RDWR, 0).unwrap();
                        assert_eq!(f.pwrite(AGAIN as u64, &old[AGAIN..AGAIN + AGAIN_LEN]), Ok(AGAIN_LEN));
                        if g % 2 == 0 {
                            ns.lookup(&prev).unwrap().inode.sync(SyncKind::Fsync).unwrap();
                        } else {
                            ns.lookup(&prev).unwrap().inode.sync(SyncKind::Background).unwrap();
                        }
                    }
                }
                if g > 1 && (t + g) % 2 == 0 {
                    // The truncated file of two generations ago goes too.
                    let _ = c.unlink(None, format!("/data/t{t}g{}", g - 2).as_bytes());
                }
            }
        }));
    }
    threads.into_iter().for_each(|t| t.join().unwrap());
}

fn check(vol: &Volume, what: &str, bad: &mut Vec<String>) {
    for (name, ino, _) in vol.list(ROOT).unwrap() {
        let name = String::from_utf8_lossy(&name).into_owned();
        let Some(rest) = name.strip_prefix('t') else { continue };
        let Some((t, g)) = rest.split_once('g') else { continue };
        let (t, g): (usize, usize) = (t.parse().unwrap(), g.parse().unwrap());
        let data = want(t, g);
        let got = read_all(vol, ino);
        if got.len() > data.len() {
            bad.push(format!("{what}: {name} is {} bytes", got.len()));
            continue;
        }
        let again = got.len() == AGAIN + AGAIN_LEN;
        for (i, blk) in got.chunks(4096).enumerate() {
            let at = i * 4096;
            let own = &data[at..at + blk.len()];
            let zero = blk.iter().all(|&b| b == 0);
            let edge = at == CUT / 4096 * 4096 && blk.len() > CUT % 4096 && blk[..CUT % 4096] == own[..CUT % 4096] && blk[CUT % 4096..].iter().all(|&b| b == 0);
            let ok = if again && at >= CUT.div_ceil(4096) * 4096 && at < AGAIN {
                zero
            } else if again && at == CUT / 4096 * 4096 {
                edge
            } else {
                blk == own || zero && got.len() <= AGAIN + AGAIN_LEN || edge
            };
            if !ok {
                let why = if blk.contains(&OLD) && blk.iter().filter(|&&b| b == OLD).count() > 2048 { "old bytes of a deleted file" } else if blk == own { "its own bytes from before the truncate, inside the gap" } else { "other bytes" };
                bad.push(format!("{what}: block {i} of {name} (size {}): {why}", got.len()));
                break;
            }
        }
    }
}

#[test]
fn create_truncate_and_unlink_on_a_nearly_full_disk_under_a_cut() {
    for round in 0..2 {
        let s = Scratch::new();
        let (ld, _dev, ns, v, _m) = disk_ns(&s, "32M", &["-b", "4096", "-m", "0"]);
        let ns = Arc::new(ns);
        leave_old_bytes(&v);
        // Ballast: all but 9 MiB of the free space, so freed blocks are taken again soon.
        let ballast = file(&v, ROOT, "ballast");
        let free = v.st.lock().free_now() as usize * 4096;
        let n = (free.saturating_sub(9 * MIB)) / MIB;
        let b = std::vec![0x5Au8; MIB];
        for i in 0..n {
            step(&v, ballast, (i * MIB) as u64, &b, (n * MIB) as u64).unwrap();
        }
        v.sync_fs().unwrap();
        let before = ld.snapshot();
        ld.record();
        run(&ns);
        let log = ld.take_log();
        std::eprintln!("round {round}: {} device calls, {} flushes", log.len(), log.iter().filter(|e| e.0 == crate::dev::FLUSHED).count());
        let mut bad = Vec::new();
        let mut rng = Rng(0x0BAD_5EED_1234_5678 + round as u64);
        for k in (0..=log.len()).flat_map(|k| [k; 2]) {
            let mut kept = || rng.bit();
            let (img, flush) = cut_image(&before, &log, k, 4096, Some(&mut kept));
            let what = format!("round {round}: cut after {k} of {} device calls (last flush at {flush})", log.len());
            let dev = Arc::new(crate::MemDev::new(img));
            let vol = match Volume::mount(dev.clone(), opts()) {
                Ok(v) => v,
                Err(e) => {
                    bad.push(format!("{what}: mount errno {e}"));
                    continue;
                }
            };
            check(&vol, &what, &mut bad);
            vol.sync_fs().unwrap();
            drop(vol);
            let (code, out) = fsck(&s, &dev.image());
            if code != 0 {
                bad.push(format!("{what}: e2fsck {code}: {}", out.lines().take(4).collect::<Vec<_>>().join(" | ")));
            }
        }
        assert!(bad.is_empty(), "{} bad findings, first:\n{}", bad.len(), bad.iter().take(8).cloned().collect::<Vec<_>>().join("\n"));
    }
}

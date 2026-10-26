//! VS-2 host check (VS-4 grows the rescue tool from it): reads a bitsvol pool image on the host, read-only.
//!
//!   export <pool.img> list                  every volume and snapshot: id origin kind auto created_ns label
//!   export <pool.img> <id> <out.img>        volume or snapshot <id> as a sparse image
//!   export <pool.img> fsck [label-prefix]   every snapshot (whose label starts with the prefix) written to a
//!                                           scratch file next to the pool image, then `e2fsck -fn`; the ext4
//!                                           superblock must not ask for a journal replay (a frozen snapshot is
//!                                           clean; an orphan list is run first, as a mount does). One line per
//!                                           snapshot; exit 1 when any is not clean.

use bitsvol::{Config, Pool, PoolHandle};
use devmodel::DevError;
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::sync::Arc;

const BS: u32 = 4096;

struct HostFile(std::fs::File, u64);

impl BlockDevice for HostFile {
    fn name(&self) -> &str {
        "host"
    }
    fn info(&self) -> BlockInfo {
        BlockInfo { block_size: BS, blocks: self.1 / BS as u64, read_only: true, max_blocks_per_request: 256, supports_discard: false, has_volatile_cache: false }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], _p: IoPriority) -> Result<(), DevError> {
        self.0.read_exact_at(buf, lba * BS as u64).map_err(|_| DevError::Io)
    }
    fn write_blocks(&self, _: u64, _: &[u8], _: bool, _: IoPriority) -> Result<(), DevError> {
        Err(DevError::Refused)
    }
    fn flush(&self) -> Result<(), DevError> {
        Ok(())
    }
}

fn die(m: &str) -> ! {
    eprintln!("export: {m}");
    std::process::exit(2)
}

/// Writes volume `id` to `out` (zero pieces are skipped: a sparse file); returns the ext4 RECOVER flag.
fn export(h: &Arc<PoolHandle>, id: u64, out: &str) -> Result<bool, String> {
    let vd = h.open_volume(id).map_err(|e| format!("open {id}: {e:?}"))?;
    let blocks = vd.info().blocks;
    let mut f = std::fs::File::create(out).map_err(|e| format!("{out}: {e}"))?;
    f.set_len(blocks * BS as u64).map_err(|e| format!("{out}: {e}"))?;
    let step = 256u64;
    let mut buf = vec![0u8; (step * BS as u64) as usize];
    let mut recover = false;
    let mut lba = 0;
    while lba < blocks {
        let n = step.min(blocks - lba);
        let b = &mut buf[..(n * BS as u64) as usize];
        vd.read_blocks(lba, b, IoPriority::Normal).map_err(|e| format!("read {id} at {lba}: {e:?}"))?;
        if lba == 0 {
            // ext4 superblock at byte 1024: s_feature_incompat at 0x60, INCOMPAT_RECOVER = 0x4.
            recover = u32::from_le_bytes([b[1024 + 0x60], b[1024 + 0x61], b[1024 + 0x62], b[1024 + 0x63]]) & 4 != 0;
        }
        if b.iter().any(|&x| x != 0) {
            f.seek(SeekFrom::Start(lba * BS as u64)).and_then(|_| f.write_all(b)).map_err(|e| format!("{out}: {e}"))?;
        }
        lba += n;
    }
    Ok(recover)
}

fn e2fsck(opt: &str, img: &str) -> Result<std::process::Output, String> {
    std::process::Command::new("e2fsck").env("LC_ALL", "C").args([opt, img]).output().map_err(|e| format!("e2fsck: {e}"))
}

/// The ext4 superblock's s_last_orphan (offset 0xE8 of the superblock at 1024).
fn last_orphan(img: &str) -> u32 {
    let mut b = [0u8; 4];
    std::fs::File::open(img).and_then(|f| f.read_exact_at(&mut b, 1024 + 0xE8)).map_or(0, |()| u32::from_le_bytes(b))
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        die("usage: export <pool.img> list | <id> <out.img> | fsck [label-prefix]");
    }
    let f = std::fs::File::open(&a[1]).unwrap_or_else(|e| die(&format!("{}: {e}", a[1])));
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let pool = Pool::open(Arc::new(HostFile(f, len)), Config::default()).unwrap_or_else(|e| die(&format!("the pool does not open: {e:?}")));
    let vols = pool.list().unwrap_or_else(|e| die(&format!("list: {e:?}")));
    let h = PoolHandle::new(pool, "host");
    match a[2].as_str() {
        "list" => {
            for v in &vols {
                println!("{} {} {} {} {} {}", v.id, v.origin, if v.snapshot { "snapshot" } else { "volume" }, v.auto as u8, v.created_ns, v.label);
            }
        }
        "fsck" => {
            let prefix = a.get(3).map(String::as_str).unwrap_or("");
            let scratch = format!("{}.fsck-scratch", a[1]);
            let (mut good, mut bad) = (0, 0);
            for v in vols.iter().filter(|v| v.snapshot && v.label.starts_with(prefix)) {
                let r = export(&h, v.id, &scratch).and_then(|recover| {
                    let o = e2fsck("-fn", &scratch)?;
                    // A file unlinked while still referenced is on the orphan list in a frozen ext4, as in Linux;
                    // `e2fsck -n` does not run the list (it reports "deleted inode has zero dtime"), a mount
                    // does (`ext4_orphan_cleanup`). So on the throwaway copy: the list run by `e2fsck -fy`, which
                    // must fix nothing else, then `-fn` clean (INT-W12).
                    if o.status.code() != Some(0) && last_orphan(&scratch) != 0 {
                        let y = e2fsck("-fy", &scratch)?;
                        let out = String::from_utf8_lossy(&y.stdout);
                        let only_orphans = out.lines().all(|l| {
                            let l = l.trim();
                            l.is_empty() || l.starts_with("Pass ") || l.starts_with("Clearing orphaned inode") || l.contains(" files (") || l.starts_with("e2fsck ")
                                || l.contains("***** FILE SYSTEM WAS MODIFIED *****")
                        });
                        if only_orphans {
                            let n = e2fsck("-fn", &scratch)?;
                            if n.status.code() == Some(0) {
                                println!("snapshot {} {} orphan list run as a mount does", v.id, v.label);
                                return Ok((recover, n));
                            }
                        }
                    }
                    Ok((recover, o))
                });
                match r {
                    Ok((recover, o)) => {
                        let rc = o.status.code().unwrap_or(-1);
                        println!("snapshot {} {} e2fsck={rc} recover={}", v.id, v.label, recover as u8);
                        if rc == 0 && !recover {
                            good += 1;
                        } else {
                            bad += 1;
                            print!("{}", String::from_utf8_lossy(&o.stdout));
                        }
                    }
                    Err(m) => {
                        println!("snapshot {} {} error {m}", v.id, v.label);
                        bad += 1;
                    }
                }
            }
            let _ = std::fs::remove_file(&scratch);
            println!("fsck: {good} clean, {bad} not clean");
            std::process::exit(if bad == 0 && good > 0 { 0 } else { 1 });
        }
        id => {
            let id: u64 = id.parse().unwrap_or_else(|_| die("want a volume id"));
            let out = a.get(3).unwrap_or_else(|| die("want an output file"));
            match export(&h, id, out) {
                Ok(recover) => println!("exported volume {id} to {out} (ext4 needs recovery: {recover})"),
                Err(m) => die(&m),
            }
        }
    }
}

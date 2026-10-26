//! `--retire-v1`, once, for A0 (V2-PLAN §7, with Georg's OK for the layout change): every file of the stick is
//! copied to a backup folder on the Gaming-PC and compared byte for byte (and by SHA-256 lists, the format of
//! the desk's backup of 14.10.); only after a green compare does STICK.ID move to `\BITSOS2\` (same bytes) and
//! V1's `\BITSOS\` and its `\EFI\BOOT\BOOTX64.EFI` go. Nothing else on the stick changes.
use super::gate::{Ident, Loader, find_ci, id_hex, parse_id, resolve_ci};
use super::sys::{checkpoint, sync_fs};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Room the backup's disk keeps beyond the copy.
const BACKUP_MARGIN: u64 = 64 << 20;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tree {
    /// `/`-separated, sorted.
    pub dirs: Vec<String>,
    pub files: Vec<(String, u64)>,
}

impl Tree {
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|(_, n)| n).sum()
    }
}

/// Every directory and file below `root` (a FAT stick holds nothing else; anything else is refused).
pub fn walk(root: &Path) -> Result<Tree, String> {
    fn go(root: &Path, rel: &str, t: &mut Tree) -> Result<(), String> {
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
        let mut es: Vec<_> = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.collect::<Result<_, _>>().map_err(|e| format!("{}: {e}", dir.display()))?;
        es.sort_by_key(|e| e.file_name());
        for e in es {
            let name = e.file_name().to_str().ok_or_else(|| format!("{}: a name that is not UTF-8", dir.display()))?.to_string();
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let m = std::fs::symlink_metadata(e.path()).map_err(|x| format!("{}: {x}", e.path().display()))?;
            if m.is_dir() {
                t.dirs.push(r.clone());
                go(root, &r, t)?;
            } else if m.is_file() {
                t.files.push((r, m.len()));
            } else {
                return Err(format!("{} is neither a file nor a folder", e.path().display()));
            }
        }
        Ok(())
    }
    let mut t = Tree::default();
    go(root, "", &mut t)?;
    t.dirs.sort();
    t.files.sort();
    Ok(t)
}

fn first_is(rel: &str, name: &str) -> bool {
    rel.split('/').next().is_some_and(|c| c.eq_ignore_ascii_case(name))
}

pub struct Plan {
    pub tree: Tree,
    pub backup: PathBuf,
    /// What goes from the stick: everything below `\BITSOS\`, and V1's loader.
    pub v1_paths: Vec<String>,
    pub loader: Option<String>,
}

pub fn plan(id: &Ident, backup: &Path, backup_free: impl Fn(&Path) -> Result<u64, String>) -> Result<Plan, String> {
    if !id.v1_dir && id.loader != Loader::V1 {
        return Err("refused: --retire-v1: V1 is not on this stick (no \\BITSOS\\, no V1 loader)".into());
    }
    if backup.exists() && (!backup.is_dir() || std::fs::read_dir(backup).map(|mut r| r.next().is_some()).unwrap_or(true)) {
        return Err(format!("refused: the backup folder {} exists and is not empty; a backup is never overwritten (give another --backup)", backup.display()));
    }
    let anchor = backup.ancestors().find(|a| a.exists()).ok_or("refused: the backup folder has no existing parent")?;
    let anchor = anchor.canonicalize().map_err(|e| format!("{}: {e}", anchor.display()))?;
    if anchor.starts_with(&id.point) {
        return Err(format!("refused: the backup folder {} lies on the stick itself", backup.display()));
    }
    let tree = walk(&id.point)?;
    let free = backup_free(&anchor)?;
    if free < tree.bytes().saturating_add(BACKUP_MARGIN) {
        return Err(format!("refused: {} has {free} bytes free, the copy needs {} plus {BACKUP_MARGIN}", anchor.display(), tree.bytes()));
    }
    let mut v1_paths: Vec<String> = tree.dirs.iter().chain(tree.files.iter().map(|(f, _)| f)).filter(|r| first_is(r, "BITSOS")).cloned().collect();
    v1_paths.sort();
    let loader = if id.loader == Loader::V1 {
        resolve_ci(&id.point, "EFI/BOOT/BOOTX64.EFI").and_then(|p| p.strip_prefix(&id.point).ok().map(|r| r.to_string_lossy().to_string()))
    } else {
        None
    };
    Ok(Plan { tree, backup: backup.to_path_buf(), v1_paths, loader })
}

pub fn show(p: &Plan, id: &Ident) {
    println!("retire-v1 plan:");
    println!("  1. copy all {} files and {} folders ({} bytes) to {}/files", p.tree.files.len(), p.tree.dirs.len(), p.tree.bytes(), p.backup.display());
    println!("  2. compare every copied file byte for byte and by SHA-256 lists; stop here if anything differs");
    match id.id_from_v1 {
        true => println!("  3. write \\BITSOS2\\STICK.ID with the same bytes as \\BITSOS\\STICK.ID ({})", id_hex(&id.id)),
        false => println!("  3. \\BITSOS2\\STICK.ID already holds the identity ({}) and stays", id_hex(&id.id)),
    }
    match &p.loader {
        Some(l) => println!("  4. delete V1's loader \\{}", l.replace('/', "\\")),
        None => println!("  4. (no V1 loader on the stick, \\EFI\\BOOT\\ stays as it is)"),
    }
    let v1_files = p.v1_paths.iter().filter(|r| p.tree.files.iter().any(|(f, _)| f == *r)).count();
    println!("  5. delete \\BITSOS\\ with its {v1_files} files; everything else on the stick stays");
    println!("  afterwards the stick has no loader until the first bundle brings bitsboot");
}

/// Linux x86_64 `O_DIRECT`: reads go to the device, past the page cache (std has no name for it).
const O_DIRECT: i32 = 0o40000;
/// O_DIRECT wants buffer, offset and length aligned to the logical block size (512 or 4096).
const DIO_ALIGN: usize = 4096;
const CHUNK: usize = 1 << 20;

/// Opens `p` read-only for a compare that does not trust the page cache: with O_DIRECT where the file system
/// takes it (vfat, btrfs, ext4, tmpfs do), else plainly (the answer says which).
fn open_uncached(p: &Path) -> Result<(File, bool), String> {
    match File::options().read(true).custom_flags(O_DIRECT).open(p) {
        Ok(f) => Ok((f, true)),
        Err(e) if e.raw_os_error() == Some(22) => File::open(p).map(|f| (f, false)).map_err(|e| format!("{}: {e}", p.display())),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

/// A CHUNK-sized window of `v` that starts on a DIO_ALIGN boundary.
fn aligned(v: &mut [u8]) -> &mut [u8] {
    let off = v.as_ptr().align_offset(DIO_ALIGN).min(DIO_ALIGN);
    &mut v[off..off + CHUNK]
}

/// The next `want` bytes (at most CHUNK) of `f`; the request is rounded up to the block size as O_DIRECT
/// needs, the file's end gives the short count.
fn read_chunk<'b>(f: &mut File, buf: &'b mut [u8], want: usize) -> std::io::Result<&'b [u8]> {
    let ask = want.next_multiple_of(DIO_ALIGN).min(buf.len());
    let mut n = 0;
    while n < want {
        match f.read(&mut buf[n..ask])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(&buf[..n.min(want)])
}

/// Byte compare of `size` bytes, both sides read past the page cache where possible. Returns (equal, whether
/// both sides were read with O_DIRECT).
fn same_bytes(a: &Path, b: &Path, size: u64) -> Result<(bool, bool), String> {
    let ((mut fa, da), (mut fb, db)) = (open_uncached(a)?, open_uncached(b)?);
    let (mut va, mut vb) = (vec![0u8; CHUNK + DIO_ALIGN], vec![0u8; CHUNK + DIO_ALIGN]);
    let (ba, bb) = (aligned(&mut va), aligned(&mut vb));
    let mut left = size;
    while left > 0 {
        let want = left.min(CHUNK as u64) as usize;
        let ca = read_chunk(&mut fa, ba, want).map_err(|e| format!("{}: {e}", a.display()))?;
        let cb = read_chunk(&mut fb, bb, want).map_err(|e| format!("{}: {e}", b.display()))?;
        if ca.len() != want || ca != cb {
            return Ok((false, da && db));
        }
        left -= want as u64;
    }
    Ok((true, da && db))
}

/// Flushes a written file or folder to its disk; an error stops the retirement.
fn fsync(p: &Path) -> Result<(), String> {
    File::open(p).and_then(|f| f.sync_all()).map_err(|e| format!("flush {}: {e}", p.display()))
}

/// `sha256sum` over the tree's files, run from `root` (`<hash>  ./<path>` lines, sorted by path).
fn sha_list(root: &Path, t: &Tree) -> Result<String, String> {
    let mut out = String::new();
    for chunk in t.files.chunks(200) {
        let o = Command::new("sha256sum")
            .current_dir(root)
            .args(chunk.iter().map(|(f, _)| format!("./{f}")))
            .output()
            .map_err(|e| format!("sha256sum: {e}"))?;
        if !o.status.success() {
            return Err(format!("sha256sum in {}: {}", root.display(), String::from_utf8_lossy(&o.stderr).trim()));
        }
        out.push_str(&String::from_utf8_lossy(&o.stdout));
    }
    Ok(out)
}

fn now(fmt: &str) -> String {
    Command::new("date").arg(fmt).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

pub fn today() -> String {
    now("+%F")
}

/// Step 1 and 2: the copy and the compare. Writes only below `p.backup`. The copy is flushed to the backup's
/// disk (every file, every folder, then syncfs) before it is compared, and the compare reads both sides with
/// O_DIRECT, so "green" means the bytes on the Gaming-PC's disk equal a second read of the stick, not two
/// views of the same cached pages.
pub fn backup(id: &Ident, p: &Plan) -> Result<(), String> {
    let files = p.backup.join("files");
    checkpoint("make the backup folder")?;
    std::fs::create_dir_all(&files).map_err(|e| format!("{}: {e}", files.display()))?;
    for d in &p.tree.dirs {
        std::fs::create_dir_all(files.join(d)).map_err(|e| format!("{d}: {e}"))?;
    }
    for (f, _) in &p.tree.files {
        let (src, dst) = (id.point.join(f), files.join(f));
        checkpoint(&format!("copy {f}"))?;
        std::fs::copy(&src, &dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
        if let Ok(t) = std::fs::metadata(&src).and_then(|m| m.modified()) {
            let _ = File::options().write(true).open(&dst).and_then(|h| h.set_modified(t));
        }
        fsync(&dst)?;
    }
    for d in p.tree.dirs.iter().rev() {
        fsync(&files.join(d))?;
    }
    for d in [&files, &p.backup] {
        fsync(d)?;
    }
    checkpoint("flush the backup")?;
    sync_fs(&p.backup)?;
    println!("  copied {} files ({} bytes) to {}, flushed to its disk", p.tree.files.len(), p.tree.bytes(), files.display());
    let (now_stick, copy) = (walk(&id.point)?, walk(&files)?);
    if now_stick != p.tree {
        return Err("RED: the stick changed while it was copied; nothing was removed".into());
    }
    if copy != p.tree {
        return Err(format!("RED: the copy's file list differs from the stick's; nothing was removed (see {})", files.display()));
    }
    checkpoint("compare")?;
    let mut all_direct = true;
    for (f, n) in &p.tree.files {
        let (same, direct) = same_bytes(&id.point.join(f), &files.join(f), *n)?;
        if !same {
            return Err(format!("RED: {f} differs between the stick and the copy; nothing was removed"));
        }
        all_direct &= direct;
    }
    let how = if all_direct {
        "both sides read with O_DIRECT, past the page cache"
    } else {
        "read through the page cache: a file system here takes no O_DIRECT (the copy was flushed first)"
    };
    let (s1, s2) = (sha_list(&id.point, &p.tree)?, sha_list(&files, &p.tree)?);
    let w = |n: &str, t: &str| std::fs::write(p.backup.join(n), t).map_err(|e| format!("{n}: {e}"));
    w("SHA256-stick.txt", &s1)?;
    w("SHA256-copy.txt", &s2)?;
    if s1 != s2 {
        return Err("RED: the SHA-256 lists of the stick and the copy differ; nothing was removed".into());
    }
    w(
        "VOLUME.txt",
        &format!(
            "LABEL={}\nUUID={}\nTYPE={} {}\nDEVICE={}\nDISK={} ({})\nSIZE={}\nSTICK.ID={}\nDATE={}\n",
            id.blk.label,
            id.blk.uuid,
            id.blk.fstype,
            id.blk.fsver,
            id.dev,
            id.blk.disk,
            if id.blk.disk_kind == "loop" { format!("loop image {}", id.blk.backing) } else { format!("{} {}", id.blk.tran, id.blk.model).trim().to_string() },
            id.blk.size,
            id_hex(&id.id),
            now("-Iseconds")
        ),
    )?;
    let removed: String = p.v1_paths.iter().chain(&p.loader).map(|r| format!("{r}\n")).collect();
    w("REMOVED.txt", &removed)?;
    w("RESULT.txt", &format!("compare green: {} files, {} bytes, byte for byte ({how}) and SHA-256\n", p.tree.files.len(), p.tree.bytes()))?;
    for n in ["SHA256-stick.txt", "SHA256-copy.txt", "VOLUME.txt", "REMOVED.txt", "RESULT.txt"] {
        fsync(&p.backup.join(n))?;
    }
    sync_fs(&p.backup)?;
    println!("  compare green: {} files byte for byte ({how}), SHA-256 lists equal ({}/SHA256-*.txt)", p.tree.files.len(), p.backup.display());
    Ok(())
}

/// Steps 3 to 5 on the stick. The caller has checked the stick again just before.
pub fn remove_v1(id: &Ident, p: &Plan) -> Result<(), String> {
    let point = &id.point;
    match resolve_ci(point, "BITSOS2/STICK.ID") {
        Some(f) => {
            let b = std::fs::read(&f).map_err(|e| format!("{}: {e}", f.display()))?;
            if parse_id(&b) != Some(id.id) {
                return Err("RED: \\BITSOS2\\STICK.ID changed; V1 stays".into());
            }
        }
        None => {
            let d = match find_ci(point, "BITSOS2") {
                Some(d) => d,
                None => {
                    let d = point.join("BITSOS2");
                    checkpoint("make \\BITSOS2")?;
                    std::fs::create_dir(&d).map_err(|e| format!("\\BITSOS2: {e}"))?;
                    d
                }
            };
            let f = d.join("STICK.ID");
            checkpoint("write \\BITSOS2\\STICK.ID")?;
            let mut h = File::create(&f).map_err(|e| format!("\\BITSOS2\\STICK.ID: {e}"))?;
            h.write_all(&id.id_bytes).and_then(|_| h.sync_all()).map_err(|e| format!("\\BITSOS2\\STICK.ID: {e}"))?;
            if std::fs::read(&f).map_err(|e| e.to_string())? != id.id_bytes {
                return Err("RED: \\BITSOS2\\STICK.ID reads back differently; V1 stays".into());
            }
            // On the stick before anything of V1 goes: vfat keeps no order between directory and FAT
            // writes without a flush, and a cut must never leave a stick without any STICK.ID.
            checkpoint("flush the new STICK.ID")?;
            sync_fs(point)?;
            println!("  wrote \\BITSOS2\\STICK.ID ({} bytes, same as V1's), flushed", id.id_bytes.len());
        }
    }
    if let Some(l) = &p.loader {
        checkpoint("delete V1's loader")?;
        std::fs::remove_file(point.join(l)).map_err(|e| format!("\\{l}: {e}"))?;
        println!("  deleted V1's loader \\{}", l.replace('/', "\\"));
    }
    if let Some(d) = find_ci(point, "BITSOS").filter(|d| d.is_dir()) {
        checkpoint("delete \\BITSOS")?;
        std::fs::remove_dir_all(&d).map_err(|e| format!("\\BITSOS: {e}"))?;
        println!("  deleted \\BITSOS\\ ({} entries)", p.v1_paths.len());
    }
    checkpoint("flush the stick")?;
    sync_fs(point)
}

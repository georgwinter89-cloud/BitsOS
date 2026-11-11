//! Writing a stick: a fresh one (wiped ends, MBR, FAT32 labelled BITSOS, then the files), or a new release added
//! to a BitsOS stick as a new generation, as `cargo xtask stick` lays it out (`\BITSOS2\SYS\STAGING\` first, renamed
//! to its number once its files and marker are written; the loader written beside the old one and renamed over it).
//! `CONFIG.TXT`, `STICK.ID`, `data.img`, `home\`, `mcp\` and the older generations of an updated stick stay.

use crate::archive;
use crate::blockio::Window;
use crate::devices::BlockDev;
use crate::fat;
use crate::gens::{self, Sys};
use crate::mbr::{self, Part};
use crate::sha::Hashing;
use crate::tar::EntryKind;
use fatfs::{Dir, ReadWriteSeek};
use std::io::{Read, SeekFrom};
use std::path::PathBuf;

/// A vendor file already fetched and checked on the host, for `\BITSOS2\<tree>\<rel>`.
#[derive(Clone, Debug)]
pub struct VendorFile {
    pub rel: String,
    pub file: PathBuf,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct TreeInput {
    pub name: String,
    /// The tree's asset (`BITSOS2/<name>/...`), or its runtime asset when vendor files complete it.
    pub archive: PathBuf,
    pub asset: String,
    pub vendor: Vec<VendorFile>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub system: PathBuf,
    pub system_asset: String,
    pub trees: Vec<TreeInput>,
    /// Bytes and files of everything written (assets unpacked plus vendor files).
    pub bytes: u64,
    pub files: u64,
}

pub enum Mode {
    Fresh { stick_id: String, config: String },
    Update,
}

#[derive(Debug)]
pub struct Existing {
    pub part: Part,
    pub label: String,
    pub stick_id: Option<String>,
    pub sys: Sys,
    pub has_config: bool,
    pub free: u64,
    pub cluster: u64,
}

impl Existing {
    /// A BitsOS stick: labelled BITSOS, with a valid STICK.ID and a generation folder.
    pub fn is_bitsos(&self) -> bool {
        self.label == "BITSOS" && self.stick_id.is_some()
    }
}

#[derive(Debug)]
pub struct Done {
    pub generation: u32,
    pub sys: String,
    pub files: u64,
    pub bytes: u64,
    pub notes: Vec<String>,
}

const STAGING: &str = "BITSOS2/SYS/STAGING";
const LOADER: &str = "EFI/BOOT/BOOTX64.EFI";
const LOADER_NEW: &str = "EFI/BOOT/BOOTX64.NEW";

fn io<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Reads the FAT volume of an existing stick, if it has one.
pub fn inspect(dev: &mut dyn BlockDev) -> Result<Option<Existing>, String> {
    let mut s0 = [0u8; 4096];
    dev.seek(SeekFrom::Start(0)).map_err(io("read the stick"))?;
    dev.read_exact(&mut s0).map_err(io("read the stick"))?;
    let sectors = dev.size() / mbr::SECTOR;
    let Some(part) = mbr::find_fat(&s0, sectors) else { return Ok(None) };
    if (part.start * mbr::SECTOR) % crate::blockio::BLOCK != 0 {
        return Ok(None);
    }
    let len = part.sectors * mbr::SECTOR / crate::blockio::BLOCK * crate::blockio::BLOCK;
    let mut w = Window::new(dev, part.start * mbr::SECTOR, len);
    let Ok(fs) = fat::mount(&mut w) else { return Ok(None) };
    let label = fat::label(&fs);
    let root = fs.root_dir();
    let stick_id = fat::read_small(&root, "BITSOS2/STICK.ID", 64).ok().map(|b| String::from_utf8_lossy(&b).trim().to_ascii_lowercase()).filter(|s| crate::secrets::is_stick_id(s));
    let sys = gens::scan(&root)?;
    let has_config = fat::exists(&root, "BITSOS2/CONFIG.TXT");
    let stats = fs.stats().map_err(io("read the stick"))?;
    let cluster = stats.cluster_size() as u64;
    let free = stats.free_clusters() as u64 * cluster;
    drop(root);
    drop(fs);
    Ok(Some(Existing { part, label, stick_id, sys, has_config, free, cluster }))
}

/// The bytes `bytes` in `files` files take on a volume with `cluster`-byte clusters (each file rounded up, folders on
/// top).
fn needed(bytes: u64, files: u64, cluster: u64) -> u64 {
    bytes + files * cluster + (64 << 20)
}

/// The cluster size `fat::format` picks for a partition of `space` bytes.
fn cluster_for(space: u64) -> u64 {
    if space <= 8 << 30 { 4096 } else { 16384 }
}

/// About the bytes a fresh stick (or sparse image) of `size` bytes gets written with: the files, their clusters'
/// slack, the folders and the FAT.
pub fn fresh_need(size: u64, bytes: u64, files: u64) -> Result<u64, String> {
    let space = mbr::layout(size / mbr::SECTOR)?.sectors * mbr::SECTOR;
    Ok(needed(bytes, files, cluster_for(space)))
}

/// Whether a fresh stick of `size` bytes holds `bytes` in `files` files; checked again before the wipe, and by the
/// run before any download, so a stick that is too small is found before the waiting.
pub fn check_fresh_space(size: u64, bytes: u64, files: u64) -> Result<(), String> {
    let space = mbr::layout(size / mbr::SECTOR)?.sectors * mbr::SECTOR;
    let need = fresh_need(size, bytes, files)?;
    if need > space {
        return Err(format!("refused: the release needs about {} MB, the stick has {} MB. Nothing was written.", need >> 20, space >> 20));
    }
    Ok(())
}

/// Said after an error once the stick was changed: what is on it now, and the way out.
const ERASED_HALF: &str = "The stick was erased and is incomplete; it will not start. Run the installer again: it starts over.";
const UPDATED_HALF: &str = "The update stopped halfway: the older generations are still on the stick, but trees or firmware may be \
missing. Run the installer again with the same release: it does the update over.";

/// `f`'s error, with `then` added once `touched` says the stick was changed.
fn after_touch<T>(f: impl FnOnce(&std::cell::Cell<bool>) -> Result<T, String>, then: &str) -> Result<T, String> {
    let touched = std::cell::Cell::new(false);
    f(&touched).map_err(|e| if touched.get() { format!("{e}\n{then}") } else { e })
}

struct Writer<'r, T: ReadWriteSeek> {
    root: &'r Dir<'r, T>,
    written: Vec<(String, u64, String)>,
    log: &'r mut dyn FnMut(&str),
}

impl<T: ReadWriteSeek> Writer<'_, T> {
    fn put(&mut self, dest: &str, size: u64, data: &mut dyn Read) -> Result<(), String> {
        let mut h = Hashing::new(data);
        fat::write_file(self.root, dest, size, &mut h).map_err(io(dest))?;
        self.written.push((dest.to_string(), size, h.sha.hex()));
        Ok(())
    }

    /// Streams one asset onto the stick; `map` says where each member goes (None: refused).
    fn asset(&mut self, path: &std::path::Path, map: &dyn Fn(&str) -> Option<String>) -> Result<(u64, u64), String> {
        let mut s = archive::open(path)?;
        let (mut files, mut bytes) = (0u64, 0u64);
        while let Some(e) = s.next_entry().map_err(io(&path.display().to_string()))? {
            if e.kind == EntryKind::Dir || e.path == "VENDOR.txt" {
                continue;
            }
            let dest = map(&e.path).ok_or_else(|| format!("refused: {} holds {}, which has no place on the stick. Stopped.", path.display(), e.path))?;
            self.put(&dest, e.size, &mut s)?;
            files += 1;
            bytes += e.size;
        }
        Ok((files, bytes))
    }
}

/// Where a member of the system asset goes (None: nowhere, the archive is refused).
pub fn system_dest(p: &str, asset: &str) -> Option<String> {
    let up = p.to_ascii_uppercase();
    let stem = asset.trim_end_matches(".tar.zst");
    if let Some(f) = up.strip_prefix("SYS/") {
        return ["KERNEL", "INITRD"].contains(&f).then(|| format!("{STAGING}/{f}"));
    }
    if up == LOADER {
        return Some(LOADER_NEW.into());
    }
    for keep in ["BITSOS2/FIRMWARE/", "BITSOS2/BIN/", "BITSOS2/LICENSES/"] {
        if up.starts_with(keep) {
            return Some(p.to_string());
        }
    }
    (!p.contains('/') || up.starts_with("LICENSES/")).then(|| format!("BITSOS2/LICENSES/{stem}/{}", p.trim_start_matches("LICENSES/")))
}

/// Where a member of a tree (or runtime) asset goes: only into its own tree.
pub fn tree_dest(p: &str, tree: &str, asset: &str) -> Option<String> {
    let prefix = format!("BITSOS2/{tree}/");
    if p.len() > prefix.len() && p[..prefix.len()].eq_ignore_ascii_case(&prefix) {
        return Some(format!("{prefix}{}", &p[prefix.len()..]));
    }
    (!p.contains('/')).then(|| format!("BITSOS2/LICENSES/{}/{p}", asset.trim_end_matches(".tar.zst")))
}

fn write_trees<T: ReadWriteSeek>(w: &mut Writer<'_, T>, plan: &Plan, replace: bool) -> Result<(), String> {
    for t in &plan.trees {
        let dir = format!("BITSOS2/{}", t.name);
        if replace {
            fat::remove_tree(w.root, &dir).map_err(io(&dir))?;
        }
        (w.log)(&format!("writing \\BITSOS2\\{}\\", t.name));
        let (n, b) = w.asset(&t.archive, &|p| tree_dest(p, &t.name, &t.asset))?;
        for v in &t.vendor {
            let mut f = std::fs::File::open(&v.file).map_err(io(&v.file.display().to_string()))?;
            w.put(&format!("{dir}/{}", v.rel), v.size, &mut f)?;
            if w.written.last().is_some_and(|(_, _, sha)| *sha != v.sha256) {
                return Err(format!("{}: changed on the host since it was checked. Stopped.", v.file.display()));
            }
        }
        (w.log)(&format!("  {} files, {} MB", n + t.vendor.len() as u64, (b + t.vendor.iter().map(|v| v.size).sum::<u64>()) >> 20));
    }
    Ok(())
}

fn write_system<T: ReadWriteSeek>(w: &mut Writer<'_, T>, plan: &Plan) -> Result<(), String> {
    (w.log)("writing the system: bitsboot, kernel, initramfs, firmware, \\BITSOS2\\bin\\");
    let asset = plan.system_asset.clone();
    w.asset(&plan.system, &|p| system_dest(p, &asset))?;
    for need in [format!("{STAGING}/KERNEL"), format!("{STAGING}/INITRD"), LOADER_NEW.to_string()] {
        if !w.written.iter().any(|(p, _, _)| *p == need) {
            return Err(format!("refused: the system asset has no {need}. Stopped."));
        }
    }
    Ok(())
}

/// The loader written beside the old one is renamed over it.
fn swap_loader<T: ReadWriteSeek>(root: &Dir<T>) -> Result<(), String> {
    let boot = root.open_dir("EFI/BOOT").map_err(io("\\EFI\\BOOT"))?;
    if let Some(old) = fat::find(&boot, "BOOTX64.EFI") {
        boot.remove(&old.file_name()).map_err(io("remove the old BOOTX64.EFI"))?;
    }
    boot.rename("BOOTX64.NEW", &boot, "BOOTX64.EFI").map_err(io("rename BOOTX64.NEW"))
}

fn finish_generation<T: ReadWriteSeek>(root: &Dir<T>, number: u32, marker: &str) -> Result<(), String> {
    fat::write_bytes(root, &format!("{STAGING}/{marker}"), b"").map_err(io("generation marker"))?;
    let sys = root.open_dir("BITSOS2/SYS").map_err(io("\\BITSOS2\\SYS"))?;
    sys.rename("STAGING", &sys, &number.to_string()).map_err(io("rename STAGING"))
}

fn verify<T: ReadWriteSeek>(root: &Dir<T>, written: &[(String, u64, String)]) -> Result<(), String> {
    for (p, size, sha) in written {
        let path = p.replace(LOADER_NEW, LOADER);
        let f = root.open_file(&path).map_err(|e| format!("RED: {path} is not on the stick after writing ({e})"))?;
        let (got, n) = crate::sha::of_reader(f).map_err(io(&path))?;
        if n != *size || got != *sha {
            return Err(format!("RED: {path} reads back different from what was written"));
        }
    }
    Ok(())
}

/// Wipes the stick and writes the release onto it.
pub fn fresh(dev: &mut dyn BlockDev, plan: &Plan, stick_id: &str, config: &str, log: &mut dyn FnMut(&str)) -> Result<Done, String> {
    after_touch(|touched| fresh_inner(dev, plan, stick_id, config, log, touched), ERASED_HALF)
}

fn fresh_inner(dev: &mut dyn BlockDev, plan: &Plan, stick_id: &str, config: &str, log: &mut dyn FnMut(&str), touched: &std::cell::Cell<bool>) -> Result<Done, String> {
    let sectors = dev.size() / mbr::SECTOR;
    let part = mbr::layout(sectors)?;
    check_fresh_space(dev.size(), plan.bytes, plan.files)?;
    touched.set(true);
    let wipe = vec![0u8; 1 << 20];
    for at in [0, (sectors * mbr::SECTOR).saturating_sub(1 << 20) / 4096 * 4096] {
        dev.seek(SeekFrom::Start(at)).map_err(io("wipe"))?;
        let n = (dev.size() - at).min(1 << 20) as usize;
        dev.write_all(&wipe[..n]).map_err(io("wipe the stick's ends"))?;
    }
    let sig = u32::from_str_radix(&crate::secrets::random_hex(4)?, 16).unwrap_or(0x4249_5453);
    let mut sector0 = [0u8; 4096];
    sector0[..512].copy_from_slice(&mbr::build(part, sig));
    dev.seek(SeekFrom::Start(0)).map_err(io("partition table"))?;
    dev.write_all(&sector0).map_err(io("partition table"))?;
    let mut w = Window::new(dev, part.start * mbr::SECTOR, part.sectors * mbr::SECTOR);
    fat::format(&mut w, sig.rotate_left(16)).map_err(io("format FAT32"))?;
    let written = {
        let fs = fat::mount(&mut w).map_err(io("mount the new FAT32"))?;
        let root = fs.root_dir();
        let mut wr = Writer { root: &root, written: Vec::new(), log };
        write_trees(&mut wr, plan, false)?;
        write_system(&mut wr, plan)?;
        fat::write_bytes(&root, "BITSOS2/CONFIG.TXT", config.as_bytes()).map_err(io("CONFIG.TXT"))?;
        fat::write_bytes(&root, "BITSOS2/STICK.ID", stick_id.as_bytes()).map_err(io("STICK.ID"))?;
        swap_loader(&root)?;
        // A release is tested before it is published: generation 1 starts as confirmed.
        finish_generation(&root, 1, "OK")?;
        let written = wr.written;
        drop(root);
        fs.unmount().map_err(io("unmount"))?;
        written
    };
    crate::dots::fix(&mut w).map_err(io("folder entries"))?;
    w.sync().map_err(io("flush the stick"))?;
    let fs = fat::mount(&mut w).map_err(io("mount for the check"))?;
    let written: Vec<(String, u64, String)> = written.into_iter().map(|(p, s, h)| (p.replace(&format!("{STAGING}/"), "BITSOS2/SYS/1/"), s, h)).collect();
    verify(&fs.root_dir(), &written)?;
    let id = fat::read_small(&fs.root_dir(), "BITSOS2/STICK.ID", 64).map_err(io("STICK.ID"))?;
    if id != stick_id.as_bytes() || fat::label(&fs) != "BITSOS" {
        return Err("RED: STICK.ID or the label reads back wrong".into());
    }
    let sys = gens::scan(&fs.root_dir())?.summary();
    Ok(Done { generation: 1, sys, files: written.len() as u64, bytes: written.iter().map(|(_, s, _)| s).sum(), notes: Vec::new() })
}

/// Adds the release to a BitsOS stick as a new generation, keeping its settings and data.
pub fn update(dev: &mut dyn BlockDev, plan: &Plan, log: &mut dyn FnMut(&str)) -> Result<Done, String> {
    after_touch(|touched| update_inner(dev, plan, log, touched), UPDATED_HALF)
}

fn update_inner(dev: &mut dyn BlockDev, plan: &Plan, log: &mut dyn FnMut(&str), touched: &std::cell::Cell<bool>) -> Result<Done, String> {
    let ex = inspect(dev)?.ok_or("refused: the stick has no FAT32 volume BitsOS could use. Nothing was written.")?;
    if !ex.is_bitsos() {
        return Err("refused: the stick is no BitsOS stick (label BITSOS and \\BITSOS2\\STICK.ID); erase it instead. Nothing was written.".into());
    }
    let len = ex.part.sectors * mbr::SECTOR / crate::blockio::BLOCK * crate::blockio::BLOCK;
    let mut w = Window::new(dev, ex.part.start * mbr::SECTOR, len);
    let mut notes = Vec::new();
    let (written, number) = {
        let fs = fat::mount(&mut w).map_err(io("mount the stick"))?;
        let root = fs.root_dir();
        let st = gens::scan(&root)?;
        let (number, replaces) = st.next()?;
        // What goes anyway counts as free: the trees and firmware replaced, an unconfirmed newest generation.
        let mut freed = 0u64;
        let mut dirs: Vec<String> = plan.trees.iter().map(|t| format!("BITSOS2/{}", t.name)).collect();
        dirs.push("BITSOS2/firmware".into());
        if let Some(g) = &replaces {
            dirs.push(format!("BITSOS2/SYS/{}", g.name));
        }
        for d in &dirs {
            let mut v = Vec::new();
            if fat::is_dir(&root, d) {
                fat::walk(&root, d, &mut v).map_err(io(d))?;
            }
            freed += v.iter().map(|(_, s)| s.div_ceil(ex.cluster) * ex.cluster).sum::<u64>();
        }
        let need = needed(plan.bytes, plan.files, ex.cluster);
        if need > ex.free + freed {
            return Err(format!("refused: the release needs about {} MB, the stick has {} MB free (with what it replaces). Nothing was written.", need >> 20, (ex.free + freed) >> 20));
        }
        touched.set(true);
        if st.staging {
            notes.push("\\BITSOS2\\SYS\\STAGING was left over from an interrupted write and went".into());
            fat::remove_tree(&root, STAGING).map_err(io(STAGING))?;
        }
        fat::remove_tree(&root, LOADER_NEW).map_err(io(LOADER_NEW))?;
        fat::remove_tree(&root, "BITSOS2/firmware").map_err(io("firmware"))?;
        let mut wr = Writer { root: &root, written: Vec::new(), log };
        write_trees(&mut wr, plan, true)?;
        write_system(&mut wr, plan)?;
        swap_loader(&root)?;
        if let Some(g) = &replaces {
            notes.push(format!("generation {} ({}) was never confirmed: the new one took its place", g.number, g.markers()));
            fat::remove_tree(&root, &format!("BITSOS2/SYS/{}", g.name)).map_err(io("old generation"))?;
        }
        finish_generation(&root, number, "NEW")?;
        let after = gens::scan(&root)?;
        let config = fat::read_small(&root, "BITSOS2/CONFIG.TXT", 1 << 20).ok();
        match gens::sys_keep(config.as_deref()) {
            Some(keep) => {
                for n in gens::prune(&after.gens, keep, after.pin) {
                    let g = after.gens.iter().find(|g| g.number == n).expect("listed");
                    fat::remove_tree(&root, &format!("BITSOS2/SYS/{}", g.name)).map_err(io("prune"))?;
                    notes.push(format!("generation {n} ({}) was pruned (sys_keep {keep})", g.markers()));
                }
            }
            None => notes.push("CONFIG.TXT's sys_keep is no number: no generation was pruned".into()),
        }
        if let Some(p) = after.pin {
            notes.push(format!("\\BITSOS2\\SYS\\PIN names generation {p}: bitsboot keeps starting it until you remove the pin"));
        }
        let written = wr.written;
        drop(root);
        fs.unmount().map_err(io("unmount"))?;
        (written, number)
    };
    crate::dots::fix(&mut w).map_err(io("folder entries"))?;
    w.sync().map_err(io("flush the stick"))?;
    let fs = fat::mount(&mut w).map_err(io("mount for the check"))?;
    let root = fs.root_dir();
    let written: Vec<(String, u64, String)> = written.into_iter().map(|(p, s, h)| (p.replace(&format!("{STAGING}/"), &format!("BITSOS2/SYS/{number}/")), s, h)).collect();
    verify(&root, &written)?;
    let sys = gens::scan(&root)?.summary();
    Ok(Done { generation: number, sys, files: written.len() as u64, bytes: written.iter().map(|(_, s, _)| s).sum(), notes })
}

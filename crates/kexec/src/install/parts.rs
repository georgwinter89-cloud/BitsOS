//! UPD-4: a version's parts on the boot medium (`parts_rules` has the names and the formats). The update node
//! writes a part file by file into `\BITSOS2\SYS\STORE\<name>.<16 hex>.NEW\` under the SYS grant, checks every
//! path, the announced count and bytes and, at `seal`, that the list of what arrived has the hash the part is
//! named for; only then is the folder renamed and its record written. A part that is there already is never
//! written again (an unchanged tree is not downloaded twice). The cleanup (`gc`) removes every part no slot's
//! `FILES` names, never one bound in this boot or held by an update on its way.

use super::parts_rules::{self as rules, List};
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use fs_fat::sysapi::Sys;
use ksync::SpinLock;
use mcp::sha256::{self, Sha256};
use vfs::OpenFile;

/// The largest `FILES` and record read (a slot names a few dozen parts; a record is five lines).
pub const SMALL_MAX: usize = 1 << 20;

/// Store folders an update holds while it runs (session id, folder): `gc` leaves them alone.
static HELD: SpinLock<Vec<(u64, String)>> = SpinLock::named("kexec.parts.held", Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn session_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn hold(id: u64, folder: &str) {
    let mut h = HELD.lock();
    if !h.iter().any(|(i, f)| *i == id && f == folder) && h.try_reserve(1).is_ok() {
        h.push((id, folder.to_string()));
    }
}

pub fn release(id: u64) {
    HELD.lock().retain(|(i, _)| *i != id);
}

fn errno(what: &str, e: i64) -> String {
    format!("{what}: {}", super::errno_name(e))
}

/// One sealed part in STORE, by its record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub name: String,
    pub hash: String,
    pub files: u64,
    pub bytes: u64,
}

impl Stored {
    pub fn folder(&self) -> String {
        rules::folder(&self.name, &self.hash)
    }
}

/// The record of part (`name`, `hash`) if it is sealed: the record holds this very hash and the folder is there.
pub fn present(s: &Sys<'_>, name: &str, hash: &str) -> Option<Stored> {
    let t = s.store_read(&rules::record_name(name, hash), SMALL_MAX).ok()?;
    let (n, h, files, bytes) = rules::record_parse(&t).ok()?;
    let folder = s.store_path(&rules::folder(name, hash)).ok()?;
    let dir = s.ns.lookup_path(folder.as_bytes(), false).ok().is_some_and(|p| p.dentry.is_dir());
    (n == name && h == hash && dir).then_some(Stored { name: n, hash: h, files, bytes })
}

/// Every sealed part.
pub fn stored(s: &Sys<'_>) -> Result<Vec<Stored>, String> {
    let mut out = Vec::new();
    for (e, dir) in s.store_list().map_err(|e| errno("listing \\BITSOS2\\SYS\\STORE", e))? {
        let Some(f) = e.strip_suffix(".PART").filter(|_| !dir) else { continue };
        let Some((name, short)) = f.split_once('.') else { continue };
        let Ok(t) = s.store_read(&e, SMALL_MAX) else { continue };
        if let Ok((n, h, files, bytes)) = rules::record_parse(&t)
            && n == name
            && h.starts_with(short)
            && present(s, &n, &h).is_some()
        {
            out.push(Stored { name: n, hash: h, files, bytes });
        }
    }
    Ok(out)
}

/// The parts slot `n` names (none: a slot without `FILES`, i.e. installed before UPD-4 or by `install_kernel`).
pub fn slot_parts(s: &Sys<'_>, n: u32) -> Result<Vec<(String, String)>, String> {
    let path = format!("{}/BITSOS2/SYS/{n}/{}", s.mount, rules::FILES);
    match s.ns.read_file(&path, SMALL_MAX) {
        Ok(t) => rules::files_parse(&t).map_err(|e| format!("generation {n}: {e}")),
        Err(uapi::errno::ENOENT) => Ok(Vec::new()),
        Err(e) => Err(errno(&format!("reading generation {n}'s FILES"), e)),
    }
}

/// The folders bound in this boot (`fs_fat::binds`): `\BITSOS2\SYS\STORE\<folder>`.
pub fn bound_folders() -> Vec<String> {
    fs_fat::binds::bound().into_iter().filter_map(|b| b.from.last().map(|f| String::from_utf8_lossy(f).into_owned())).collect()
}

/// Removes what no slot uses: parts no `FILES` names (beside `keep`, this boot's binds and what updates hold),
/// unfinished `.NEW` folders nobody holds, records without their folder. Returns the bytes of the parts removed.
/// Anything it cannot read stops it before it removes a thing.
pub fn gc(s: &Sys<'_>, keep: &[String]) -> Result<u64, String> {
    let entries = s.store_list().map_err(|e| errno("listing \\BITSOS2\\SYS\\STORE", e))?;
    if entries.is_empty() {
        return Ok(0);
    }
    let mut used: BTreeSet<String> = keep.iter().cloned().collect();
    used.extend(bound_folders());
    used.extend(HELD.lock().iter().map(|(_, f)| f.clone()));
    for g in s.generations().map_err(|e| errno("listing \\BITSOS2\\SYS", e))? {
        for (n, h) in slot_parts(s, g.n)? {
            used.insert(rules::folder(&n, &h));
        }
    }
    let dirs: BTreeSet<String> = entries.iter().filter(|e| e.1).map(|e| e.0.clone()).collect();
    let stored = stored(s)?;
    let mut freed = 0;
    for (e, dir) in &entries {
        let gone = if *dir {
            // A sealed or unfinished part folder; any other name was made elsewhere and stays.
            let base = e.strip_suffix(".NEW").unwrap_or(e);
            base.contains('.') && !used.contains(e) && !used.contains(base)
        } else if let Some(f) = e.strip_suffix(".PART") {
            !used.contains(f) || !dirs.contains(f)
        } else {
            false
        };
        if !gone {
            continue;
        }
        // The record first: a part half removed is never taken for a sealed one.
        if *dir && let Some(p) = stored.iter().find(|p| p.folder() == *e) {
            s.store_remove(&rules::record_name(&p.name, &p.hash)).map_err(|x| errno("removing a part's record", x))?;
            freed += p.bytes;
        }
        s.store_remove(e).map_err(|x| errno(&format!("removing \\BITSOS2\\SYS\\STORE\\{e}"), x))?;
        klog::event!(Info, "kexec", "parts.removed", entry = e.as_str(); "no kept slot uses it");
    }
    Ok(freed)
}

/// The file being written.
struct Cur {
    f: Arc<OpenFile>,
    path: String,
    size: u64,
    got: u64,
    sha: Sha256,
}

/// One part on its way into STORE.
pub struct Writer {
    pub name: String,
    pub hash: String,
    want_files: u64,
    want_dirs: u64,
    want_bytes: u64,
    list: List,
    sum: Sha256,
    cur: Option<Cur>,
    made: String,
}

/// What `begin` found.
pub enum Begun {
    Present(Stored),
    Writing(Writer),
}

fn free_bytes(s: &Sys<'_>) -> Result<(u64, u64), String> {
    let (cb, _, free) = s.vol.statfs().map_err(|e| errno("reading the medium's free space", e))?;
    Ok((cb, cb.saturating_mul(free)))
}

impl Writer {
    /// `part <name> <hash> <files> <dirs> <bytes>`: present, or a fresh `.NEW` folder (an old one removed) once
    /// the part fits (parts no slot uses are removed first when it would not).
    pub fn begin(s: &Sys<'_>, id: u64, keep: &[String], name: &str, hash: &str, files: u64, dirs: u64, bytes: u64) -> Result<Begun, String> {
        rules::name_ok(name)?;
        if !rules::hex_ok(hash) {
            return Err(format!("part {name}: the hash is 64 lower-case hex digits"));
        }
        // Held before anything else: a part that is there already, but that no slot names yet (left by an update
        // that stopped before its install), must survive the cleanup another part's `begin` may run.
        let (new, folder) = (rules::new_folder(name, hash), rules::folder(name, hash));
        hold(id, &new);
        hold(id, &folder);
        if let Some(p) = present(s, name, hash) {
            return Ok(Begun::Present(p));
        }
        if bound_folders().contains(&folder) {
            return Err(format!("part {name}: the running version uses it but its record is damaged; restart and try again"));
        }
        // A record without its part (a power cut while it was written or removed) would block the seal for good.
        s.store_remove(&rules::record_name(name, hash)).map_err(|e| errno("removing a damaged part record", e))?;
        let (cluster, mut free) = free_bytes(s)?;
        let need = rules::on_medium(files, dirs, bytes, cluster);
        if free < need {
            // Under the install lock: an install or a confirmation may be pruning (and cleaning) right now.
            if let Some(_l) = super::LOCK.lock_timeout(60_000_000_000) {
                gc(s, keep)?;
            }
            free = free_bytes(s)?.1;
        }
        if free < need {
            return Err(format!("no space: the medium has {} MiB free, part {name} needs up to {} MiB", free >> 20, need.div_ceil(1 << 20)));
        }
        s.store_remove(&new).map_err(|e| errno("removing an unfinished part", e))?;
        s.store_mkdir(&new).map_err(|e| errno(&format!("making the folder of part {name}"), e))?;
        klog::event!(Info, "kexec", "parts.begin", part = name, hash = hash, files = files, bytes = bytes);
        Ok(Begun::Writing(Writer {
            name: name.to_string(),
            hash: hash.to_string(),
            want_files: files,
            want_dirs: dirs,
            want_bytes: bytes,
            list: List::default(),
            sum: Sha256::default(),
            cur: None,
            made: String::new(),
        }))
    }

    fn rel(&self, path: &str) -> String {
        format!("{}/{path}", rules::new_folder(&self.name, &self.hash))
    }

    /// `file <size> <path>`: true when its bytes follow (false: an empty file, done already).
    pub fn file(&mut self, s: &Sys<'_>, size: u64, path: &str) -> Result<bool, String> {
        if self.cur.is_some() {
            return Err(String::from("a file before the last one's bytes arrived"));
        }
        self.list.add(path, self.name == rules::BIN, size)?;
        // The folders too: the space check before the first byte counted the announced ones.
        if self.list.count > self.want_files || self.list.bytes > self.want_bytes || self.list.dirs() > self.want_dirs {
            return Err(format!(
                "part {}: more files, folders or bytes than announced ({} files, {} folders, {} bytes)",
                self.name, self.want_files, self.want_dirs, self.want_bytes
            ));
        }
        if let Some((parent, _)) = path.rsplit_once('/')
            && parent != self.made
        {
            s.store_mkdir(&self.rel(parent)).map_err(|e| errno(&format!("making the folder of {path}"), e))?;
            self.made = parent.to_string();
        }
        let f = s.store_create(&self.rel(path)).map_err(|e| errno(&format!("creating {path}"), e))?;
        self.cur = Some(Cur { f, path: path.to_string(), size, got: 0, sha: Sha256::default() });
        if size == 0 {
            self.finish_file(s)?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Bytes of the current file; how many were taken, and whether the file is complete.
    pub fn take(&mut self, s: &Sys<'_>, b: &[u8]) -> Result<(usize, bool), String> {
        let Some(c) = self.cur.as_mut() else { return Ok((0, false)) };
        let k = ((c.size - c.got) as usize).min(b.len());
        s.store_write(&c.f, c.got, &b[..k]).map_err(|e| errno(&format!("writing {}", c.path), e))?;
        c.sha.update(&b[..k]);
        c.got += k as u64;
        if c.got < c.size {
            return Ok((k, false));
        }
        self.finish_file(s)?;
        Ok((k, true))
    }

    fn finish_file(&mut self, s: &Sys<'_>) -> Result<(), String> {
        let Some(c) = self.cur.take() else { return Ok(()) };
        s.store_fsync(&c.f).map_err(|e| errno(&format!("writing {} to the medium", c.path), e))?;
        self.sum.update(rules::line(&sha256::hex(&c.sha.finish()), c.size, &c.path).as_bytes());
        Ok(())
    }

    /// `seal`: every announced file in, the list's hash the one the part is named for; the folder renamed, the
    /// record written (both durable). The writer is used up either way.
    pub fn seal(self, s: &Sys<'_>) -> Result<Stored, String> {
        let new = rules::new_folder(&self.name, &self.hash);
        let r = self.seal_inner(s);
        if r.is_err() {
            let _ = s.store_remove(&new);
        }
        r
    }

    /// The bytes the current file still expects (0: none is open).
    pub fn expecting(&self) -> u64 {
        self.cur.as_ref().map_or(0, |c| c.size - c.got)
    }

    fn seal_inner(mut self, s: &Sys<'_>) -> Result<Stored, String> {
        if self.cur.is_some() {
            return Err(String::from("seal before the last file's bytes arrived"));
        }
        if self.list.count != self.want_files || self.list.bytes != self.want_bytes || self.list.dirs() != self.want_dirs {
            return Err(format!(
                "part {}: {} files in {} folders with {} bytes arrived, {} in {} with {} announced",
                self.name,
                self.list.count,
                self.list.dirs(),
                self.list.bytes,
                self.want_files,
                self.want_dirs,
                self.want_bytes
            ));
        }
        let got = sha256::hex(&core::mem::take(&mut self.sum).finish());
        if got != self.hash {
            return Err(format!("part {}: its files have the hash {got}, not the one it is named for", self.name));
        }
        let (new, folder) = (rules::new_folder(&self.name, &self.hash), rules::folder(&self.name, &self.hash));
        let record = rules::record_name(&self.name, &self.hash);
        // Never the folder the running version shows at \BITSOS2\<name>; the record goes before the folder it
        // stands for, so a cut in between never leaves a record over a half part.
        if bound_folders().contains(&folder) {
            return Err(format!("part {}: the running version uses its folder; restart and try again", self.name));
        }
        s.store_remove(&record).map_err(|e| errno("removing the part's old record", e))?;
        s.store_remove(&folder).map_err(|e| errno("clearing the part's folder", e))?;
        s.store_rename(&new, &folder).map_err(|e| errno("sealing the part", e))?;
        let rec = rules::record_render(&self.name, &self.hash, self.list.count, self.list.bytes);
        s.store_put(&record, rec.as_bytes()).map_err(|e| errno("writing the part's record", e))?;
        klog::event!(Info, "kexec", "parts.sealed", part = self.name.as_str(), hash = self.hash.as_str(), files = self.list.count, bytes = self.list.bytes);
        Ok(Stored { name: self.name.clone(), hash: self.hash.clone(), files: self.list.count, bytes: self.list.bytes })
    }

    /// Gives up: the `.NEW` folder goes.
    pub fn abort(self, s: &Sys<'_>) {
        drop(self.cur);
        let new = rules::new_folder(&self.name, &self.hash);
        klog::event!(Warn, "kexec", "parts.aborted", part = self.name.as_str(), hash = self.hash.as_str(), files = self.list.count; "the part was given up before its seal");
        if let Err(e) = s.store_remove(&new) {
            klog::event!(Warn, "kexec", "parts.abort_left", folder = new.as_str(), errno = e; "the next update or cleanup removes it");
        }
    }
}

/// Makes `\BITSOS2\<name>` for each part (the place its bind needs at the next start).
pub fn mount_points(s: &Sys<'_>, parts: &[(String, String)]) -> Result<(), String> {
    for (n, _) in parts {
        s.mount_point(n).map_err(|e| errno(&format!("\\BITSOS2\\{n} cannot be a folder"), e))?;
    }
    Ok(())
}

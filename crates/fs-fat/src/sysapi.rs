//! The narrow API for the system generations in `\BITSOS2\SYS\` (V2-PLAN §7; RB-6's `install_kernel` and the
//! kernel's confirmation of its own boot). Only here may anything below `SYS` change, and only these names:
//! generation folders `<n>` (1 ..= 99999999), their `KERNEL` and `INITRD` (written once, never changed: a file
//! that exists is EEXIST), their markers `NEW`, `TRY`, `OK` (empty files) and `SYS\PIN` (the number of a
//! generation). Every change is durable when the call returns (the files' data, the entries, FSInfo, then the
//! stick's cache flush). Pruning and the marker rule itself are RB-6's and bitsboot's; this is the mechanism.
//! It works only on the identified boot stick mounted read-write (else EROFS, as every other write).

use crate::vol::Volume;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::abi::{O_CREAT, O_EXCL, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;
use vfs::MountNs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    New,
    Try,
    Ok,
}

impl Marker {
    fn name(self) -> &'static str {
        match self {
            Marker::New => "NEW",
            Marker::Try => "TRY",
            Marker::Ok => "OK",
        }
    }
}

/// The files a generation holds.
pub const FILES: &[&str] = &["KERNEL", "INITRD"];

/// One generation as it is on the stick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Generation {
    pub n: u32,
    pub new: bool,
    pub try_: bool,
    pub ok: bool,
    /// KERNEL/INITRD with their sizes; other names found there (made elsewhere) too.
    pub files: Vec<(String, u64)>,
}

/// The SYS folder of a mounted volume: `mount` is where the volume's root is in `ns` ("/boot").
pub struct Sys<'a> {
    pub vol: &'a Arc<Volume>,
    pub ns: &'a MountNs,
    pub mount: &'a str,
}

fn check_n(n: u32) -> Result<(), i64> {
    if (1..=99_999_999).contains(&n) { Ok(()) } else { Err(EINVAL) }
}

impl Sys<'_> {
    fn sys(&self) -> String {
        format!("{}/BITSOS2/SYS", self.mount)
    }
    fn gen_dir(&self, n: u32) -> String {
        format!("{}/{n}", self.sys())
    }

    /// Every generation folder, sorted by number (names that are not numbers are left out).
    pub fn generations(&self) -> Result<Vec<Generation>, i64> {
        let mut out = Vec::new();
        let list = match self.ns.list_dir(&self.sys()) {
            Ok(l) => l,
            Err(ENOENT) => return Ok(out),
            Err(e) => return Err(e),
        };
        for (name, a) in list {
            let Some(n) = core::str::from_utf8(&name).ok().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if a.mode & uapi::S_IFMT != uapi::S_IFDIR || check_n(n).is_err() {
                continue;
            }
            let mut g = Generation { n, ..Default::default() };
            for (f, fa) in self.ns.list_dir(&self.gen_dir(n))? {
                let f = String::from_utf8_lossy(&f).to_ascii_uppercase();
                match f.as_str() {
                    "NEW" => g.new = true,
                    "TRY" => g.try_ = true,
                    "OK" => g.ok = true,
                    _ => g.files.push((f, fa.size)),
                }
            }
            out.push(g);
        }
        out.sort_by_key(|g| g.n);
        Ok(out)
    }

    /// `SYS\PIN`: the pinned generation, if the file holds a number.
    pub fn pin(&self) -> Result<Option<u32>, i64> {
        match self.ns.read_file(&format!("{}/PIN", self.sys()), 64) {
            Ok(t) => Ok(core::str::from_utf8(&t).ok().and_then(|s| s.trim().parse().ok())),
            Err(ENOENT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn durable(&self) -> Result<(), i64> {
        self.vol.sync(true)
    }

    /// Makes `SYS\<n>\` (and `SYS` when missing). EEXIST when the generation exists.
    pub fn create_generation(&self, n: u32) -> Result<(), i64> {
        check_n(n)?;
        let _g = self.vol.grant_sys();
        let c = Ctx::kernel(self.ns);
        match c.mkdir(None, self.sys().as_bytes(), 0o755) {
            Ok(_) | Err(EEXIST) => {}
            Err(e) => return Err(e),
        }
        c.mkdir(None, self.gen_dir(n).as_bytes(), 0o755)?;
        self.durable()?;
        klog::event!(Info, "fat", "sys.generation_made", n = n);
        Ok(())
    }

    /// Writes `KERNEL` or `INITRD` of generation `n` once (EEXIST if it is there), durable.
    pub fn write_file(&self, n: u32, name: &str, data: &[u8]) -> Result<(), i64> {
        check_n(n)?;
        if !FILES.contains(&name) {
            return Err(EINVAL);
        }
        let _g = self.vol.grant_sys();
        let c = Ctx::kernel(self.ns);
        let path = format!("{}/{name}", self.gen_dir(n));
        let f = c.open(None, path.as_bytes(), O_WRONLY | O_CREAT | O_EXCL, 0o644)?;
        let mut done = 0;
        while done < data.len() {
            let w = f.pwrite(done as u64, &data[done..])?;
            if w == 0 {
                return Err(EIO);
            }
            done += w;
        }
        f.inode().ok_or(EIO)?.sync(SyncKind::Fsync)?;
        drop(f);
        self.durable()?;
        klog::event!(Info, "fat", "sys.file_written", n = n, file = name, bytes = data.len());
        Ok(())
    }

    /// Reads `KERNEL` or `INITRD` of generation `n` as the stick holds it (at most `max` bytes; EFBIG beyond).
    pub fn read_file(&self, n: u32, name: &str, max: usize) -> Result<Vec<u8>, i64> {
        check_n(n)?;
        if !FILES.contains(&name) {
            return Err(EINVAL);
        }
        // From the disk, not the page cache: a read-back checks what the stick holds.
        let d = self.ns.lookup(&format!("{}/{name}", self.gen_dir(n)))?;
        self.vol.read_disk(d.inode.meta.ino, max)
    }

    /// Up to `buf.len()` bytes of `KERNEL` or `INITRD` of generation `n` at `off` (below the end: a multiple of
    /// the sector size) as the stick holds it; the count read, 0 at the end.
    pub fn read_at(&self, n: u32, name: &str, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        check_n(n)?;
        if !FILES.contains(&name) {
            return Err(EINVAL);
        }
        let d = self.ns.lookup(&format!("{}/{name}", self.gen_dir(n)))?;
        self.vol.read_disk_at(d.inode.meta.ino, off, buf)
    }

    /// Sets (an empty file) or clears a marker of generation `n`, durable. Setting one that is there and
    /// clearing one that is not are no-ops.
    pub fn set_marker(&self, n: u32, m: Marker, on: bool) -> Result<(), i64> {
        check_n(n)?;
        let _g = self.vol.grant_sys();
        let c = Ctx::kernel(self.ns);
        let path = format!("{}/{}", self.gen_dir(n), m.name());
        if self.ns.lookup_path(self.gen_dir(n).as_bytes(), true).is_err() {
            return Err(ENOENT);
        }
        let r = if on {
            match c.open(None, path.as_bytes(), O_WRONLY | O_CREAT | O_EXCL, 0o644) {
                Ok(_) | Err(EEXIST) => Ok(()),
                Err(e) => Err(e),
            }
        } else {
            match c.unlink(None, path.as_bytes()) {
                Ok(()) | Err(ENOENT) => Ok(()),
                Err(e) => Err(e),
            }
        };
        r?;
        self.durable()?;
        klog::event!(Info, "fat", "sys.marker", n = n, marker = m.name(), on = on);
        Ok(())
    }

    /// Writes `SYS\PIN` with `n`, or removes it (None), durable.
    pub fn set_pin(&self, n: Option<u32>) -> Result<(), i64> {
        if let Some(n) = n {
            check_n(n)?;
        }
        let _g = self.vol.grant_sys();
        let c = Ctx::kernel(self.ns);
        let path = format!("{}/PIN", self.sys());
        match c.unlink(None, path.as_bytes()) {
            Ok(()) | Err(ENOENT) => {}
            Err(e) => return Err(e),
        }
        if let Some(n) = n {
            let f = c.open(None, path.as_bytes(), O_WRONLY | O_CREAT | O_EXCL, 0o644)?;
            let text = format!("{n}\r\n");
            f.pwrite(0, text.as_bytes())?;
            f.inode().ok_or(EIO)?.sync(SyncKind::Fsync)?;
        }
        self.durable()?;
        klog::event!(Info, "fat", "sys.pin", n = n.unwrap_or(0));
        Ok(())
    }

    /// Removes generation `n` with everything in it, durable.
    pub fn remove_generation(&self, n: u32) -> Result<(), i64> {
        check_n(n)?;
        let _g = self.vol.grant_sys();
        let c = Ctx::kernel(self.ns);
        let dir = self.gen_dir(n);
        for (name, a) in self.ns.list_dir(&dir)? {
            if a.mode & uapi::S_IFMT == uapi::S_IFDIR {
                // A folder in a generation was made elsewhere: left alone, so the generation stays.
                return Err(ENOTEMPTY);
            }
            let p = format!("{dir}/{}", String::from_utf8_lossy(&name));
            c.unlink(None, p.as_bytes())?;
        }
        c.rmdir(None, dir.as_bytes())?;
        self.durable()?;
        klog::event!(Info, "fat", "sys.generation_removed", n = n);
        Ok(())
    }
}

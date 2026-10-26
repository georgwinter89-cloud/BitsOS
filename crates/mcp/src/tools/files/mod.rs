//! MC-4a: the file tools on the VFS (`read_file`, `write_file`, `list_dir`, `stat`, `delete`) and `fetch`
//! (`V2-PLAN.md` §6, `spec/net.md` §5.2: V1's `read_file`/`write_file`/`list_files`/`delete_file` with base64,
//! offset and append, plus the kernel pulling big files from the host's HTTP server).
//!
//! The kernel acts as itself on the root view (`vfs::root_ns()`), so the rules that keep Claude from breaking
//! things live here, in one place (`place`):
//! - writes, deletes and fetches land only on tmpfs and, since MC-push, on the boot stick below `\BITSOS2\`
//!   (FS-7's write rule; never `\BITSOS2\SYS\`, `CONFIG.TXT` or `STICK.ID`, which belong to `install_kernel`
//!   and `fs_fat::sysapi`; fs-fat enforces the same again by long name and 8.3 alias), never on another file
//!   system and never in the trees the kernel builds itself (`/dev`, `/proc`, `/sys`);
//! - only regular files are written or read (a device node could wait for ever or change device state);
//! - a file named `CONFIG.TXT` never gains or changes an `nvme_write=`, `stick_write=` or `usb_msd_write=` line (R8a:
//!   only Georg's own edit turns them on), whatever the write does to the rest of it (`wlan=` is free, R8d);
//! - every access asks `policy::check` (B3) besides the per-call `McpAction` question of rpc.
//!
//! A whole-file write (no offset, no append) and `fetch` write into a hidden sibling first and rename it over
//! the target at the end, so a running program keeps its old file and a failed transfer leaves nothing half.

mod fetch;
#[cfg(target_os = "none")]
mod ktests;
#[cfg(target_os = "none")]
mod push_ktests;
mod read;
/// MC-push: manifest and move_tree.
mod tree;
mod write;

pub use fetch::{check_url, fetch_into};
pub use read::read_whole;
pub use tree::{overlaps, rel_ok, within};
pub use write::{Change, apply, config_guard};

use super::{bad, opt_str};
use crate::{ToolContext, ToolError};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use uapi::errno::*;
use vfs::inode::Data;
use vfs::walk::LastKind;
use vfs::{Ctx, MountNs, Path};

/// Bytes one `read_file` call returns at most (a bound on one answer, not a count: 2 MiB is 2.7 MB as base64,
/// inside the 4 MB answer limit). Larger files are read in pieces with `offset`.
pub const READ_MAX: u64 = 2 * 1024 * 1024;
/// Linux PATH_MAX (a bound against hostile input).
pub const PATH_MAX: usize = 4096;
/// Top-level trees whose nodes the kernel makes (device nodes, proc and sys files).
pub const KERNEL_TREES: &[&str] = &["dev", "proc", "sys"];

/// A path argument: absolute, at most PATH_MAX bytes, no NUL.
pub(crate) fn path_arg<'a>(args: &'a crate::Json, key: &str) -> Result<&'a str, ToolError> {
    let p = opt_str(args, key)?.ok_or_else(|| bad(key))?;
    if !p.starts_with('/') {
        return Err(ToolError::BadArgument(format!("{key} (an absolute path, starting with /)")));
    }
    if p.len() > PATH_MAX || p.bytes().any(|b| b == 0) {
        return Err(ToolError::BadArgument(format!("{key} (at most {PATH_MAX} bytes, no NUL)")));
    }
    Ok(p)
}

/// File permission bits given as octal text ("0755", "644").
pub(crate) fn perm_arg(args: &crate::Json, key: &str) -> Result<Option<u32>, ToolError> {
    let Some(t) = opt_str(args, key)? else { return Ok(None) };
    let t = t.trim();
    let v = (!t.is_empty() && t.len() <= 6).then(|| u32::from_str_radix(t, 8).ok()).flatten();
    match v {
        Some(v) if v <= 0o7777 => Ok(Some(v)),
        _ => Err(ToolError::BadArgument(format!("{key} (octal text like \"0644\" or \"0755\", at most 07777)"))),
    }
}

pub fn errno_name(e: i64) -> &'static str {
    match e {
        ENOENT => "no such file or directory (ENOENT)",
        ENOTDIR => "a part of the path is not a directory (ENOTDIR)",
        EISDIR => "is a directory (EISDIR)",
        EEXIST => "exists already (EEXIST)",
        ENOTEMPTY => "directory not empty (ENOTEMPTY)",
        ENOSPC => "no space left on the file system (ENOSPC)",
        ENOMEM => "out of memory (ENOMEM)",
        EFBIG => "file too large (EFBIG)",
        EACCES => "permission denied (EACCES)",
        EPERM => "operation not permitted (EPERM)",
        EBUSY => "busy: a mount or the root (EBUSY)",
        ENAMETOOLONG => "name too long (ENAMETOOLONG)",
        ELOOP => "too many symbolic links (ELOOP)",
        EROFS => "read-only file system (EROFS)",
        EXDEV => "crosses a mount (EXDEV)",
        EINVAL => "invalid argument (EINVAL)",
        EINTR => "interrupted (EINTR)",
        _ => "error",
    }
}

pub(crate) fn fail(path: &str, e: i64) -> ToolError {
    ToolError::Failed(format!("{path}: {} (errno {e})", errno_name(e)))
}

pub(crate) fn refuse(tool: &str, path: &str, why: String) -> ToolError {
    klog::event!(Info, "mcp", "file.refused", tool = tool, path = path, reason = why.as_str());
    ToolError::Refused(why)
}

/// B3: the caller's access to `path` (rpc already asked about the tool itself).
pub(crate) fn policy_path(ctx: &ToolContext, tool: &str, path: &str, write: bool, create: bool) -> Result<(), ToolError> {
    policy::check(&ctx.caller, &policy::Access::Path { path, write, create }).map_err(|d| refuse(tool, path, format!("policy: {}", d.reason)))
}

pub(crate) fn utf8(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The absolute path of `p` in the root view ("?" if it is not reachable, which only another view's mount is).
pub(crate) fn canonical(ns: &MountNs, p: &Path) -> String {
    ns.path_of(&ns.root_path(), p).map(|b| utf8(&b)).unwrap_or_else(|| String::from("?"))
}

/// The first component of an absolute canonical path ("" for "/").
pub fn top_level(canon: &str) -> &str {
    canon.trim_start_matches('/').split('/').next().unwrap_or("")
}

/// What the target node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// tmpfs file content (or an external file system's regular file).
    Regular,
    /// A `procfile!` (rendered on read).
    Proc,
    Dir,
    Symlink,
    /// A device node (or anything else the VFS hands to its own `File`).
    Device,
    /// FS-3 CR4 (INT-A2a): a FIFO (named pipe) or a socket name; reading them through the tools answers at once
    /// (never opens or waits), so they are named as Linux's `ls -l` does.
    Fifo,
    Socket,
}

pub(crate) fn kind_of(p: &Path) -> Kind {
    match &p.dentry.inode.data {
        Data::Reg(_) | Data::External(_) => Kind::Regular,
        // FS-2's synthetic files (/proc, /sys) render on read like a procfile!.
        Data::Proc(_) | Data::Synth(_) => Kind::Proc,
        Data::Dir | Data::SynthDir(_) => Kind::Dir,
        Data::Symlink(_) => Kind::Symlink,
        Data::Device(f) => match f.mode() & uapi::S_IFMT {
            uapi::S_IFIFO => Kind::Fifo,
            vfs::abi::S_IFSOCK => Kind::Socket,
            _ => Kind::Device,
        },
    }
}

pub(crate) fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Regular => "file",
        Kind::Proc => "proc file",
        Kind::Dir => "directory",
        Kind::Symlink => "symbolic link",
        Kind::Device => "device node",
        Kind::Fifo => "fifo",
        Kind::Socket => "socket",
    }
}

/// Where a write or delete lands: the directory, the last name, the canonical path, the node there now.
pub struct Place {
    pub dir: Path,
    pub name: Vec<u8>,
    pub canon: String,
    pub existing: Option<Path>,
}

/// Where the boot stick is mounted (CONTRACTS: "Boot stick at /boot (vfat)").
pub const STICK_MOUNT: &str = "boot";
/// The names directly below `\BITSOS2\` the file tools never create, change or remove (folded).
pub const STICK_KEEP_OFF: &[&str] = &["sys", "config.txt", "stick.id"];

/// A FAT name as FAT compares it: ASCII case folded, trailing dots and spaces dropped (vfat ignores them).
pub fn fat_fold(name: &[u8]) -> String {
    utf8(name).trim_end_matches(['.', ' ']).to_ascii_lowercase()
}

/// MC-push: FS-7's write rule for the boot stick, on the canonical directory `dir` that receives or loses the
/// entry `name` (pure, host-tested). Only below `/boot/BITSOS2/`; never in or of `SYS`, never `CONFIG.TXT` or
/// `STICK.ID` there. None = allowed (fs-fat still has the last word, also for 8.3 aliases).
pub fn stick_refusal(dir: &str, name: &[u8]) -> Option<String> {
    let comps: Vec<&str> = dir.split('/').filter(|c| !c.is_empty()).collect();
    let below = comps.len() >= 2 && comps[0] == STICK_MOUNT && fat_fold(comps[1].as_bytes()) == "bitsos2";
    if !below {
        return Some(format!("{dir} is on the boot stick outside \\BITSOS2\\: the file tools write on the stick only below /boot/BITSOS2/ (FS-7)"));
    }
    let first = comps.get(2).map(|c| fat_fold(c.as_bytes())).unwrap_or_else(|| fat_fold(name));
    if STICK_KEEP_OFF.contains(&first.as_str()) {
        return Some(format!(
            "/boot/BITSOS2/{first}: the system generations (SYS), CONFIG.TXT and STICK.ID are never changed by the file tools (install_kernel and its config argument do that)"
        ));
    }
    None
}

/// Whether canonical path `p` is the stick's `\BITSOS2\home\` or below it (pure, host-tested; FAT case and
/// trailing dots folded). fs-fat gives what is made or renamed there uid/gid 1000 and 0700/0600 (class Home,
/// INT-W7 `FatOps::reowner`), what is renamed out of it root's again; `move_tree` names the crossing.
pub fn in_stick_home(p: &str) -> bool {
    stick_home_depth(p).is_some()
}

/// Whether canonical path `p` is `\BITSOS2\home` itself: `move_tree` never moves it away (the user's home and
/// its class would go with it; a merge would end with removing it).
pub fn is_stick_home(p: &str) -> bool {
    stick_home_depth(p) == Some(0)
}

/// How many components below `\BITSOS2\home` the canonical path `p` is (None = not in the home).
fn stick_home_depth(p: &str) -> Option<usize> {
    let comps: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
    let home = comps.len() >= 3 && comps[0] == STICK_MOUNT && fat_fold(comps[1].as_bytes()) == "bitsos2" && fat_fold(comps[2].as_bytes()) == "home";
    home.then(|| comps.len() - 3)
}

/// Why a directory may not receive or lose the entry `name` (None = it may).
fn dir_refusal(ns: &MountNs, dir: &Path, name: &[u8]) -> Option<String> {
    let canon = canonical(ns, dir);
    // First: since FS-2, /dev, /proc and /sys are their own mounts (devtmpfs, proc, sysfs); the kernel-tree
    // reason says more than "not tmpfs".
    let top = top_level(&canon);
    let made_here = if canon == "/" { utf8(name) } else { String::new() };
    if KERNEL_TREES.contains(&top) || KERNEL_TREES.contains(&made_here.as_str()) {
        let top = if top.is_empty() { made_here.as_str() } else { top };
        return Some(format!("/{top} holds the nodes the kernel makes itself (devices, proc and sys files); the file tools never write or delete there"));
    }
    let fs = dir.dentry.sb.fstype;
    match fs {
        "tmpfs" => {}
        "vfat" => {
            if let Some(why) = stick_refusal(&canon, name) {
                return Some(why);
            }
        }
        _ => {
            return Some(format!(
                "{canon} is on {fs}: the file tools write only into the RAM file system (tmpfs) and the boot stick below /boot/BITSOS2/ (never a disk)"
            ));
        }
    }
    if dir.mnt.readonly {
        return Some(format!("{canon} is mounted read-only"));
    }
    None
}

/// The place `path` names for a write (`delete` = false: the node must be absent or a writable regular
/// file) or a delete (the node must exist and may be a file, link or directory). Every refusal of the module
/// rules happens here, before anything changes.
pub(crate) fn place(ns: &MountNs, tool: &str, path: &str, delete: bool) -> Result<Place, ToolError> {
    let c = Ctx::kernel(ns);
    let mut w = c.walker();
    let (dir, last) = w.parent(ns.root_path(), path.as_bytes()).map_err(|e| fail(path, e))?;
    if last.kind != LastKind::Normal {
        return Err(refuse(tool, path, format!("{path} does not name an entry ('/', '.' or '..' at its end)")));
    }
    if last.name.len() > 255 {
        return Err(fail(path, ENAMETOOLONG));
    }
    if !dir.dentry.is_dir() {
        return Err(fail(path, ENOTDIR));
    }
    if let Some(why) = dir_refusal(ns, &dir, &last.name) {
        return Err(refuse(tool, path, why));
    }
    let existing = w.child(&dir, &last.name).map_err(|e| fail(path, e))?;
    let mut canon = canonical(ns, &dir);
    if !canon.ends_with('/') {
        canon.push('/');
    }
    canon.push_str(&utf8(&last.name));
    let pl = Place { dir, name: last.name, canon, existing };
    match (&pl.existing, delete) {
        (None, true) => return Err(fail(path, ENOENT)),
        (None, false) => {
            if last.trailing {
                return Err(refuse(tool, path, format!("{path} ends in '/': a file name must not")));
            }
        }
        (Some(e), true) => {
            if e.is_mount_root() || ns.is_mountpoint(e) {
                return Err(refuse(tool, path, format!("{} is a mount point", pl.canon)));
            }
            if top_level(&pl.canon) == pl.canon.trim_start_matches('/') {
                return Err(refuse(tool, path, format!("{} is a top-level entry of /; those stay", pl.canon)));
            }
        }
        (Some(e), false) => {
            let k = kind_of(e);
            if k == Kind::Dir {
                return Err(fail(path, EISDIR));
            }
            if k != Kind::Regular {
                return Err(refuse(tool, path, format!("{} is a {}: the file tools write only regular files (give the target path of a link)", pl.canon, kind_name(k))));
            }
            if e.dentry.inode.meta.mode() & 0o222 == 0 {
                return Err(refuse(tool, path, format!("{} is read-only (mode {:04o})", pl.canon, e.dentry.inode.meta.mode() & 0o7777)));
            }
        }
    }
    Ok(pl)
}

/// Makes every missing directory above `path` (mode 0755), checking each directory it creates in with the
/// same rules as a write (never below /dev, /proc, /sys, off tmpfs or outside the stick's \BITSOS2\) and
/// asking the caller's policy for each one before it is made (so a write the policy refuses leaves no
/// directories behind, on the stick either).
pub(crate) fn make_parents(ctx: &ToolContext, ns: &MountNs, tool: &str, path: &str) -> Result<(), ToolError> {
    let c = Ctx::kernel(ns);
    let (comps, _) = vfs::walk::split(path.as_bytes());
    let Some((_, dirs)) = comps.split_last() else { return Ok(()) };
    let mut cur = ns.root_path();
    for comp in dirs {
        if *comp == b"." || *comp == b".." {
            return Err(ToolError::BadArgument(String::from("path (with parents:true it may not contain . or ..)")));
        }
        cur = match c.lookup(Some(&cur), comp, true) {
            Ok(p) if p.dentry.is_dir() => p,
            Ok(_) => return Err(fail(path, ENOTDIR)),
            Err(ENOENT) => {
                if let Some(why) = dir_refusal(ns, &cur, comp) {
                    return Err(refuse(tool, path, why));
                }
                let mut made = canonical(ns, &cur);
                if !made.ends_with('/') {
                    made.push('/');
                }
                made.push_str(&utf8(comp));
                policy_path(ctx, tool, &made, true, true)?;
                match c.mkdir(Some(&cur), comp, 0o755) {
                    Ok(p) => p,
                    Err(EEXIST) => c.lookup(Some(&cur), comp, true).map_err(|e| fail(path, e))?,
                    Err(e) => return Err(fail(path, e)),
                }
            }
            Err(e) => return Err(fail(path, e)),
        };
    }
    Ok(())
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// A hidden sibling name for a whole-file write (`.<name>.mcp-<n>~`), unique in this boot.
pub(crate) fn temp_name(name: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(name.len() + 24);
    t.push(b'.');
    // Keep the temp name inside NAME_MAX: a long name is shortened (the sequence number keeps it unique).
    t.extend_from_slice(&name[..name.len().min(200)]);
    t.extend_from_slice(format!(".mcp-{}~", TMP_SEQ.fetch_add(1, Ordering::Relaxed)).as_bytes());
    t
}

/// Writes all of `data` at `pos` (pwrite until done; a write that takes nothing is ENOSPC).
pub(crate) fn write_all_at(f: &vfs::OpenFile, mut pos: u64, mut data: &[u8]) -> Result<(), i64> {
    while !data.is_empty() {
        let n = f.pwrite(pos, data)?;
        if n == 0 {
            return Err(ENOSPC);
        }
        pos += n as u64;
        data = &data[n..];
    }
    Ok(())
}

/// FNV-1a 64 (the hash `reload_info` shows for the initramfs; host tools compare uploads with it).
#[derive(Clone, Copy)]
pub struct Fnv(pub u64);

impl Fnv {
    pub const fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    pub fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tree_tests;

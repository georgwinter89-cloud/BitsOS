//! SX-3a: the file systems mount(2) makes (`do_new_mount` → `vfs_get_tree`): their option strings as Linux
//! parses them (fs/fs_context.c `generic_parse_monolithic`, `vfs_parse_sb_flag`; mm/shmem.c
//! `shmem_parse_one`; fs/devpts/inode.c `devpts_parse_param`; fs/proc/root.c `proc_parse_param`) and the super
//! blocks with what `show_options` prints for them.
//!
//! overlay: `overlay.rs` (lower layers only).
//! tmpfs, devpts and mqueue are new instances per mount (Linux keeps one mqueue instance per IPC namespace).
//! proc (SX-3b): a new instance per mount in the mounter's pid namespace (`proc_instance`); hidepid/subset/gid are
//! checked as Linux and not applied yet (a Warn event names them). sysfs: a new mount shows the boot instance.

use crate::dentry::{Dentry, SuperBlock};
use crate::inode::{Cred, Data, Inode};
use crate::Errno;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use uapi::errno::*;

/// The types mount(2) knows, and whether Linux lets a user namespace's root mount them (`FS_USERNS_MOUNT`).
pub fn known(fstype: &[u8]) -> Option<&'static str> {
    Some(match fstype {
        b"tmpfs" => "tmpfs",
        b"proc" => "proc",
        b"sysfs" => "sysfs",
        b"devpts" => "devpts",
        b"mqueue" => "mqueue",
        b"overlay" => "overlay",
        _ => return None,
    })
}

/// Who mounts: kernel ids, and the mapping of ids written in options (`make_kuid(current_user_ns(), n)`).
pub struct Mounter<'a> {
    pub fsuid: u32,
    pub fsgid: u32,
    pub uid_in: &'a dyn Fn(u32) -> Option<u32>,
    pub gid_in: &'a dyn Fn(u32) -> Option<u32>,
    /// The caller's user namespace is the initial one.
    pub init_ns: bool,
    /// The kernel id of the caller namespace's root, if it has one (devpts' ptmx owner).
    pub ns_root: Option<(u32, u32)>,
}

/// A parsed option string: the super block flags it named and the rest, key and value.
pub struct Opts {
    pub sb_ro: Option<bool>,
    pub source: Option<Vec<u8>>,
    pub rest: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

/// `vfs_parse_sb_flag` and `forbidden_sb_flag`, then the file system's own keys for the rest.
pub fn split(data: &[u8]) -> Result<Opts, Errno> {
    const FORBIDDEN: [&[u8]; 20] = [
        b"bind", b"move", b"private", b"remount", b"shared", b"slave", b"unbindable", b"rec", b"noatime", b"relatime", b"norelatime", b"strictatime", b"nostrictatime", b"nodiratime", b"dev", b"nodev", b"exec", b"noexec", b"suid", b"nosuid",
    ];
    let mut o = Opts { sb_ro: None, source: None, rest: Vec::new() };
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    for tok in data[..end].split(|&b| b == b',').filter(|t| !t.is_empty()) {
        let (k, v) = match tok.iter().position(|&b| b == b'=') {
            Some(i) => (&tok[..i], Some(tok[i + 1..].to_vec())),
            None => (tok, None),
        };
        match k {
            b"ro" if v.is_none() => o.sb_ro = Some(true),
            b"rw" if v.is_none() => o.sb_ro = Some(false),
            b"sync" | b"async" | b"dirsync" | b"lazytime" | b"nolazytime" | b"mand" | b"nomand" | b"posixacl" | b"noposixacl" if v.is_none() => {}
            _ if FORBIDDEN.contains(&k) => return Err(EINVAL),
            b"source" => o.source = Some(v.ok_or(EINVAL)?),
            _ => o.rest.push((k.to_vec(), v)),
        }
    }
    Ok(o)
}

fn num(v: &Option<Vec<u8>>, radix: u32) -> Result<u64, Errno> {
    let s = core::str::from_utf8(v.as_deref().ok_or(EINVAL)?).map_err(|_| EINVAL)?;
    let s = if radix == 16 { s.trim_start_matches("0x") } else { s };
    u64::from_str_radix(s, radix).map_err(|_| EINVAL)
}

/// `memparse` with tmpfs' `%` of RAM: bytes.
fn size_bytes(v: &Option<Vec<u8>>, ram_bytes: u64) -> Result<u64, Errno> {
    let raw = v.as_deref().ok_or(EINVAL)?;
    let digits = raw.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return Err(EINVAL);
    }
    let n: u64 = core::str::from_utf8(&raw[..digits]).ok().and_then(|s| s.parse().ok()).ok_or(EINVAL)?;
    let shift = match &raw[digits..] {
        b"" => 0,
        b"k" | b"K" => 10,
        b"m" | b"M" => 20,
        b"g" | b"G" => 30,
        b"t" | b"T" => 40,
        b"p" | b"P" => 50,
        b"e" | b"E" => 60,
        // Linux wraps here; a hostile percentage must not, so a product past u64 is a bad value.
        b"%" => return (ram_bytes / 100).checked_mul(n).ok_or(EINVAL),
        _ => return Err(EINVAL),
    };
    n.checked_shl(shift).filter(|x| x >> shift == n).ok_or(EINVAL)
}

/// What a tmpfs mount asked for (`shmem_options`).
pub struct Tmpfs {
    pub size_pages: Option<u64>,
    pub nr_inodes: Option<u64>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub inode64: bool,
    pub huge: Option<&'static str>,
}

/// `shmem_parse_one` for `rest`; the root belongs to the mounter and is 1777 unless asked otherwise.
pub fn tmpfs_opts(o: &Opts, who: &Mounter<'_>, ram_bytes: u64) -> Result<Tmpfs, Errno> {
    let mut t = Tmpfs { size_pages: None, nr_inodes: None, mode: 0o1777, uid: who.fsuid, gid: who.fsgid, inode64: true, huge: None };
    for (k, v) in o.rest.iter() {
        match k.as_slice() {
            b"size" => t.size_pages = Some(size_bytes(v, ram_bytes)?.div_ceil(4096)),
            // shmem_parse_one: nr_blocks above LONG_MAX and nr_inodes above ULONG_MAX / BOGO_INODE_SIZE are bad.
            b"nr_blocks" => t.size_pages = Some(Some(size_bytes(v, ram_bytes)?).filter(|&b| b <= i64::MAX as u64).ok_or(EINVAL)?),
            b"nr_inodes" => t.nr_inodes = Some(Some(size_bytes(v, ram_bytes)?).filter(|&n| n <= u64::MAX / 1024).ok_or(EINVAL)?),
            b"mode" => t.mode = num(v, 8)? as u32 & 0o7777,
            b"uid" => t.uid = (who.uid_in)(num(v, 10)? as u32).ok_or(EINVAL)?,
            b"gid" => t.gid = (who.gid_in)(num(v, 10)? as u32).ok_or(EINVAL)?,
            b"inode32" if v.is_none() => t.inode64 = false,
            b"inode64" if v.is_none() => t.inode64 = true,
            b"huge" => {
                t.huge = Some(match v.as_deref() {
                    Some(b"never") => "never",
                    Some(b"always") => "always",
                    Some(b"within_size") => "within_size",
                    Some(b"advise") => "advise",
                    _ => return Err(EINVAL),
                })
            }
            b"mpol" => {
                mpol_ok(v.as_deref().ok_or(EINVAL)?)?;
                klog::event!(Info, "vfs", "tmpfs.mpol_ignored"; "one memory node: the policy has nothing to choose");
            }
            // Linux 6.6: "Turning off swap in unprivileged tmpfs mounts unsupported".
            b"noswap" if v.is_none() && who.init_ns => {}
            _ => return Err(EINVAL),
        }
    }
    Ok(t)
}

/// `mpol_parse_str` on a machine whose only memory node is 0: `mode[=static|relative][:nodelist]`.
fn mpol_ok(v: &[u8]) -> Result<(), Errno> {
    let (head, nodes) = match v.iter().position(|&c| c == b':') {
        Some(i) => (&v[..i], Some(&v[i + 1..])),
        None => (v, None),
    };
    let (mode, flags) = match head.iter().position(|&c| c == b'=') {
        Some(i) => (&head[..i], Some(&head[i + 1..])),
        None => (head, None),
    };
    // nodelist_parse plus nodes_subset(N_MEMORY): ranges and lists that name node 0 only; empty is allowed.
    let mut any = false;
    if let Some(n) = nodes {
        for part in n.split(|&c| c == b',').filter(|p| !p.is_empty()) {
            let ok = part.split(|&c| c == b'-').all(|x| x == b"0");
            if !ok || part.split(|&c| c == b'-').count() > 2 {
                return Err(EINVAL);
            }
            any = true;
        }
    }
    let ok = match mode {
        b"prefer" => nodes.is_none_or(|n| any && n.iter().all(u8::is_ascii_digit)),
        b"interleave" => nodes.is_none() || any,
        b"local" => nodes.is_none(),
        b"default" => return if nodes.is_none() && flags.is_none() { Ok(()) } else { Err(EINVAL) },
        b"bind" | b"prefer (many)" => any,
        _ => false,
    };
    let flags_ok = matches!(flags, None | Some(b"static") | Some(b"relative"));
    if ok && flags_ok { Ok(()) } else { Err(EINVAL) }
}

/// `shmem_show_options` for `t` (size and inodes only when given, mode unless 1777, owner unless root).
pub fn tmpfs_show(t: &Tmpfs) -> String {
    let mut s = String::new();
    if let Some(p) = t.size_pages {
        // K(): the page count shifted to KiB in an unsigned long, which wraps as in Linux instead of panicking.
        let _ = write!(s, ",size={}k", p.wrapping_mul(4));
    }
    if let Some(n) = t.nr_inodes {
        let _ = write!(s, ",nr_inodes={n}");
    }
    if t.mode != 0o1777 {
        let _ = write!(s, ",mode={:03o}", t.mode);
    }
    if t.uid != 0 {
        let _ = write!(s, ",uid={}", t.uid);
    }
    if t.gid != 0 {
        let _ = write!(s, ",gid={}", t.gid);
    }
    if let Some(h) = t.huge {
        let _ = write!(s, ",huge={h}");
    }
    s.push_str(if t.inode64 { ",inode64" } else { ",inode32" });
    s
}

/// A new tmpfs instance for mount(2).
pub fn tmpfs(t: &Tmpfs) -> (Arc<SuperBlock>, Arc<Dentry>) {
    let sb = SuperBlock::new("tmpfs");
    sb.pool.set_size(t.size_pages);
    sb.set_options(&tmpfs_show(t));
    let root = Inode::new(sb.next_ino(), uapi::S_IFDIR | t.mode, Cred { uid: t.uid, gid: t.gid }, Data::Dir);
    (sb.clone(), Dentry::new_root(sb, root))
}

/// devpts' options (`pts_mount_opts`).
pub struct Devpts {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: u32,
    pub ptmxmode: u32,
    pub max: Option<u64>,
}

/// `devpts_parse_param`.
pub fn devpts_opts(o: &Opts, who: &Mounter<'_>) -> Result<Devpts, Errno> {
    let mut d = Devpts { uid: None, gid: None, mode: 0o600, ptmxmode: 0, max: None };
    for (k, v) in o.rest.iter() {
        match k.as_slice() {
            b"uid" => d.uid = Some((who.uid_in)(num(v, 10)? as u32).ok_or(EINVAL)?),
            b"gid" => d.gid = Some((who.gid_in)(num(v, 10)? as u32).ok_or(EINVAL)?),
            b"mode" => d.mode = num(v, 8)? as u32 & 0o777,
            b"ptmxmode" => d.ptmxmode = num(v, 8)? as u32 & 0o777,
            b"max" => d.max = Some(num(v, 10)?.min(1 << 20)),
            b"newinstance" if v.is_none() => {}
            _ => return Err(EINVAL),
        }
    }
    Ok(d)
}

/// A new devpts instance: its root (0755) and `ptmx` (char 5:2, `ptmxmode`, owned by the namespace's root or
/// else the mounter; `devpts_fill_super`/`mknod_ptmx`), whose opens go to `ptmx` (the system's pty layer: the
/// slaves appear in the boot instance until the pty layer knows instances, SX-3a CR to IN).
pub fn devpts(d: &Devpts, who: &Mounter<'_>, ptmx: Option<Arc<dyn crate::File>>) -> (Arc<SuperBlock>, Arc<Dentry>) {
    let sb = SuperBlock::new("devpts");
    let mut s = String::new();
    if let Some(u) = d.uid {
        let _ = write!(s, ",uid={u}");
    }
    if let Some(g) = d.gid {
        let _ = write!(s, ",gid={g}");
    }
    let _ = write!(s, ",mode={:03o},ptmxmode={:03o}", d.mode, d.ptmxmode);
    if let Some(m) = d.max.filter(|m| *m < 1 << 20) {
        let _ = write!(s, ",max={m}");
    }
    sb.set_options(&s);
    let root = Inode::new(sb.next_ino(), uapi::S_IFDIR | 0o755, Cred::ROOT, Data::Dir);
    let top = Dentry::new_root(sb.clone(), root);
    if let Some(f) = ptmx {
        let (uid, gid) = who.ns_root.unwrap_or((who.fsuid, who.fsgid));
        let _g = sb.lock_names();
        if let Err(e) = top.create_child(b"ptmx", uapi::S_IFCHR | d.ptmxmode, Cred { uid, gid }, Data::Device(f)) {
            klog::event!(Warn, "vfs", "devpts.ptmx_failed", errno = e);
        }
    }
    (sb, top)
}

/// A new mqueue instance (`mqueue_fill_super`): an empty 1777 directory owned by the mounter; no options.
pub fn mqueue(o: &Opts, who: &Mounter<'_>) -> Result<(Arc<SuperBlock>, Arc<Dentry>), Errno> {
    if !o.rest.is_empty() {
        return Err(EINVAL);
    }
    let sb = SuperBlock::new("mqueue");
    let root = Inode::new(sb.next_ino(), uapi::S_IFDIR | 0o1777, Cred { uid: who.fsuid, gid: who.fsgid }, Data::Dir);
    Ok((sb.clone(), Dentry::new_root(sb, root)))
}

/// `proc_parse_param`: gid=, hidepid=, subset=pid, checked as Linux (EINVAL for other values) and not applied:
/// every reader sees every process of the instance's namespace. A Linux-valid option is never refused (systemd's
/// ProtectProc= passes them), so the deviation is a Warn event with the option names.
pub fn proc_opts(o: &Opts, who: &Mounter<'_>) -> Result<(), Errno> {
    for (k, v) in o.rest.iter() {
        match k.as_slice() {
            b"gid" => {
                (who.gid_in)(num(v, 10)? as u32).ok_or(EINVAL)?;
            }
            b"hidepid" => match v.as_deref() {
                Some(b"0" | b"1" | b"2" | b"4" | b"off" | b"noaccess" | b"invisible" | b"ptraceable") => {}
                _ => return Err(EINVAL),
            },
            b"subset" if v.as_deref() == Some(b"pid") => {}
            _ => return Err(EINVAL),
        }
    }
    if !o.rest.is_empty() {
        let mut names = String::new();
        for (k, _) in o.rest.iter() {
            if !names.is_empty() {
                names.push(',');
            }
            names.push_str(&String::from_utf8_lossy(k));
        }
        klog::event!(Warn, "vfs", "proc.options_ignored", options = names.as_str(); "proc mount options hidepid/subset/gid are not applied yet: every process of the namespace stays visible");
    }
    Ok(())
}

/// The boot instance of `fstype` (proc, sysfs): a whole mount of it in view `ns` (the mounter's view first,
/// then the boot view; a walk of those views, not of every mount).
pub fn boot_instance(ns: &crate::MountNs, fstype: &str) -> Option<(Arc<SuperBlock>, Arc<Dentry>)> {
    let boot = crate::root_ns();
    let t = crate::mount_ns::TOPO.lock();
    [ns.id, boot.id].iter().find_map(|&v| t.view(v).into_iter().find(|m| m.sb.fstype == fstype && m.root.parent().is_none()).map(|m| (m.sb.clone(), m.root.clone())))
}

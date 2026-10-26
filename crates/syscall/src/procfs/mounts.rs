//! `/proc/<pid>/mounts` and `/proc/<pid>/mountinfo` of the process's mount view (linux-6.6/fs/proc_namespace.c
//! `show_vfsmnt`, `show_mountinfo`, `show_mnt_opts`, `mangle`): mount id, parent id, the super block's device,
//! the mount's root in its file system, the mount point relative to the process's root, "rw"/"ro" with the
//! mount's flags (nosuid, nodev, noexec, noatime, nodiratime, relatime), the type, the source and the file
//! system's options (`show_options`).
//!
//! The list is the view's own (`MountNs::mounts`), the source the super block's (`SuperBlock::source`: the type's
//! name unless the mounter set one), the options the super block's (`SuperBlock::options`, AB-7c). V2 has no
//! mount propagation (no "shared:N" tags) and none of `show_sb_opts`' sync/dirsync/mand/lazytime.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use proc::Process;
use vfs::{Mount, MountNs, Path};

/// One mount as the two files show it.
pub(super) struct Row {
    pub id: u64,
    pub parent: u64,
    pub dev: u64,
    pub point: Vec<u8>,
    pub fstype: String,
    pub source: String,
    pub readonly: bool,
    /// `vfs::MNT_*`.
    pub flags: u32,
    /// `show_options`' text (leading comma per option, or empty).
    pub options: String,
}

/// `show_mnt_opts`' table, in its order.
const MNT_OPTS: [(u32, &str); 7] = [
    (vfs::MNT_NOSUID, ",nosuid"),
    (vfs::MNT_NODEV, ",nodev"),
    (vfs::MNT_NOEXEC, ",noexec"),
    (vfs::MNT_NOATIME, ",noatime"),
    (vfs::MNT_NODIRATIME, ",nodiratime"),
    (vfs::MNT_RELATIME, ",relatime"),
    (vfs::MNT_NOSYMFOLLOW, ",nosymfollow"),
];

fn mnt_opts(out: &mut String, flags: u32) {
    for (f, s) in MNT_OPTS {
        if flags & f != 0 {
            out.push_str(s);
        }
    }
}

/// `seq_path_root`'s escapes (space, tab, newline, backslash as octal); `hash` adds '#' as `mangle` does for
/// the type and the source.
pub(super) fn mangle_with(out: &mut String, s: &[u8], hash: bool) {
    for &c in s {
        match c {
            b' ' | b'\t' | b'\n' | b'\\' => {
                let _ = write!(out, "\\{c:03o}");
            }
            b'#' if hash => out.push_str("\\043"),
            _ => out.push(c as char),
        }
    }
}

/// The path escapes only (mount points).
pub(super) fn mangle(out: &mut String, s: &[u8]) {
    mangle_with(out, s, false)
}

pub(super) fn format_rows(rows: &[Row], info: bool) -> String {
    let mut s = String::new();
    for r in rows {
        // V2's super block is read-only exactly when its mount is (no bind mounts yet).
        let rw = if r.readonly { "ro" } else { "rw" };
        if info {
            let _ = write!(s, "{} {} {}:{} / ", r.id, r.parent, vfs::abi::dev_major(r.dev), vfs::abi::dev_minor(r.dev));
            mangle(&mut s, &r.point);
            let _ = write!(s, " {rw}");
            mnt_opts(&mut s, r.flags);
            s.push_str(" - ");
            mangle_with(&mut s, r.fstype.as_bytes(), true);
            s.push(' ');
            mangle_with(&mut s, r.source.as_bytes(), true);
            let _ = writeln!(s, " {rw}{}", r.options);
        } else {
            mangle_with(&mut s, r.source.as_bytes(), true);
            s.push(' ');
            mangle(&mut s, &r.point);
            s.push(' ');
            mangle_with(&mut s, r.fstype.as_bytes(), true);
            let _ = write!(s, " {rw}");
            mnt_opts(&mut s, r.flags);
            let _ = writeln!(s, "{} 0 0", r.options);
        }
    }
    s
}

/// The mounts of `ns` in mount order (AB-7a CR2: the view's own list, any depth).
fn mounts_of(ns: &MountNs) -> Vec<Arc<Mount>> {
    ns.mounts()
}

fn row(ns: &MountNs, root: &Path, m: &Arc<Mount>) -> Row {
    let (parent, point) = match &m.parent {
        Some((pm, at)) => (pm.id, ns.d_path(root, &Path { mnt: pm.clone(), dentry: at.clone() })),
        // The view's root: its own parent, as Linux shows a namespace root whose parent is outside the view.
        None => (m.id, b"/".to_vec()),
    };
    Row { id: m.id, parent, dev: m.sb.dev, point, source: m.sb.source(), fstype: String::from(m.sb.fstype), readonly: m.readonly, flags: m.flags(), options: m.sb.options() }
}

pub(super) fn render(p: &Process, info: bool) -> String {
    let ns = &p.ns;
    let root = ns.root_path();
    let rows: Vec<Row> = mounts_of(ns).iter().map(|m| row(ns, &root, m)).collect();
    format_rows(&rows, info)
}

//! SX-3a: `/proc/<pid>/mountinfo` and `/proc/<pid>/mounts` of a view seen from a root (linux-6.6/fs/
//! proc_namespace.c `show_mountinfo`, `show_vfsmnt`, `show_mnt_opts`, `mangle`; fs/pnode.c
//! `get_dominating_id`). Mounts the root does not reach (outside a chroot or a pivoted root) are left out,
//! as `seq_path_root` skips them. The per-mount part shows the mount's own read-only flag, the super block part
//! the file system's; propagation tags as Linux prints them ("shared:N", "master:N", "propagate_from:N",
//! "unbindable").

use crate::mount_ns::*;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

/// `show_mnt_opts`' table, in its order.
const MNT_OPTS: [(u32, &str); 7] = [
    (MNT_NOSUID, ",nosuid"),
    (MNT_NODEV, ",nodev"),
    (MNT_NOEXEC, ",noexec"),
    (MNT_NOATIME, ",noatime"),
    (MNT_NODIRATIME, ",nodiratime"),
    (MNT_RELATIME, ",relatime"),
    (MNT_NOSYMFOLLOW, ",nosymfollow"),
];

fn mnt_opts(out: &mut String, flags: u32) {
    for (f, s) in MNT_OPTS {
        if flags & f != 0 {
            out.push_str(s);
        }
    }
}

/// `seq_escape` with " \t\n\\" (paths), plus '#' for the type and the source (`mangle`).
fn mangle(out: &mut String, s: &[u8], hash: bool) {
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

/// `get_dominating_id`: the first group up `m`'s master chain with a peer in `m`'s view under `root` (each group
/// once; its members are an index range, not a scan of every mount).
fn dominating(t: &Topo, m: &Arc<Mount>, root: &Path) -> u64 {
    let ns = m.ns_id();
    let mut group = m.propagation().1;
    let mut seen = BTreeSet::new();
    while group != 0 && seen.insert(group) {
        let members = t.peers(group);
        if members.iter().any(|x| x.ns_id() == ns && reachable(x, &x.root, root)) {
            return group;
        }
        group = members.first().map_or(0, |x| x.propagation().1);
    }
    0
}

/// One line's facts, taken under the table's lock.
struct Row {
    id: u64,
    parent: u64,
    dev: u64,
    root: Vec<u8>,
    point: Vec<u8>,
    sb: Arc<crate::dentry::SuperBlock>,
    source: String,
    flags: u32,
    readonly: bool,
    group: u64,
    master: u64,
    dom: u64,
}

/// The file `info` (mountinfo) or mounts of view `ns` seen from `root`. The facts are taken under the mount
/// table's lock without references to the mounts (Linux holds `namespace_sem`), so a reader never makes an
/// unmount EBUSY; the text is made after the lock. The cost is the view's mounts, never the system's.
pub fn render(ns: &MountNs, root: &Path, info: bool) -> String {
    let rows: Vec<Row> = {
        let t = TOPO.lock();
        let mut list = t.view(ns.id);
        list.sort_by_key(|m| m.id);
        list.iter()
            .filter_map(|m| {
                let point = path_of(root, &Path { mnt: m.clone(), dentry: m.root.clone() })?;
                let (group, master) = m.propagation();
                Some(Row {
                    id: m.id,
                    parent: m.mountpoint().map_or(ns.base_id(), |(p, _)| p.id),
                    dev: m.sb.dev,
                    root: join(&m.root.path_in_fs()),
                    point,
                    sb: m.sb.clone(),
                    source: m.source(),
                    flags: m.flags(),
                    readonly: m.is_readonly(),
                    group,
                    master,
                    dom: if master != 0 { dominating(&t, m, root) } else { 0 },
                })
            })
            .collect()
    };
    let mut s = String::new();
    for r in rows.iter() {
        let fstype = r.sb.fstype.as_bytes();
        let sb_rw = if r.sb.is_ro() { "ro" } else { "rw" };
        let f = r.flags;
        if !info {
            mangle(&mut s, r.source.as_bytes(), true);
            s.push(' ');
            mangle(&mut s, &r.point, false);
            s.push(' ');
            mangle(&mut s, fstype, true);
            s.push_str(if r.readonly { " ro" } else { " rw" });
            mnt_opts(&mut s, f);
            let _ = writeln!(s, "{} 0 0", r.sb.options());
            continue;
        }
        let _ = write!(s, "{} {} {}:{} ", r.id, r.parent, crate::abi::dev_major(r.dev), crate::abi::dev_minor(r.dev));
        mangle(&mut s, &r.root, false);
        s.push(' ');
        mangle(&mut s, &r.point, false);
        s.push_str(if f & MNT_READONLY != 0 { " ro" } else { " rw" });
        mnt_opts(&mut s, f);
        if r.group != 0 {
            let _ = write!(s, " shared:{}", r.group);
        }
        if r.master != 0 {
            let _ = write!(s, " master:{}", r.master);
            if r.dom != 0 && r.dom != r.master {
                let _ = write!(s, " propagate_from:{}", r.dom);
            }
        }
        if f & MNT_UNBINDABLE != 0 {
            s.push_str(" unbindable");
        }
        s.push_str(" - ");
        mangle(&mut s, fstype, true);
        s.push(' ');
        mangle(&mut s, r.source.as_bytes(), true);
        let _ = writeln!(s, " {sb_rw}{}", r.sb.options());
    }
    s
}

#[cfg(test)]
mod tests {
    // INT-W26: the line pieces the old procfs formatter's host test checked (linux-6.6/fs/proc_namespace.c
    // `show_mnt_opts` :73, `mangle`), now that SX-3a renders the files here.
    use super::*;

    #[test]
    fn options_and_escapes() {
        let mut s = String::new();
        mnt_opts(&mut s, MNT_NOSUID | MNT_NODEV | MNT_NOEXEC | MNT_RELATIME);
        assert_eq!(s, ",nosuid,nodev,noexec,relatime");
        s.clear();
        mnt_opts(&mut s, MNT_RELATIME | MNT_NOSYMFOLLOW);
        assert_eq!(s, ",relatime,nosymfollow", "nosymfollow is the table's last entry");
        s.clear();
        mangle(&mut s, b"/mnt/a b#", false);
        assert_eq!(s, "/mnt/a\\040b#", "a path keeps '#'");
        s.clear();
        mangle(&mut s, b"/dev/x#1", true);
        assert_eq!(s, "/dev/x\\0431", "a source escapes '#'");
        s.clear();
        mangle(&mut s, b"a\tb\nc\\d", false);
        assert_eq!(s, "a\\011b\\012c\\134d");
    }
}

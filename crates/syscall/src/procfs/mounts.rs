//! `/proc/<pid>/mounts` and `/proc/<pid>/mountinfo` (linux-6.6/fs/proc_namespace.c `mounts_open_common`): the
//! process's mount namespace's view seen from the process's root; the text is vfs's (`vfs::mountinfo`, SX-3a:
//! per-mount and super block read-only apart, bind roots, propagation tags, mounts outside the root left out).

use alloc::string::String;
use proc::Process;

pub(super) fn render(p: &Process, info: bool) -> String {
    let ns = p.mount_ns();
    let root = p.fs().root().unwrap_or_else(|| ns.root_path());
    vfs::mountinfo::render(&ns, &root, info)
}

//! INT-CL3T (CL-S CR 2, MC-L CR 2): what a unix socket keeps of the program that connected it
//! (`net::sock::Program`, taken by connect, listen and socketpair next to the peer credentials).
//!
//! SO_PEERCRED's pid names a process, and a process may become another program (execve) or end and leave its
//! number to the next one before the listener looks. So the socket also keeps the program image that made the
//! call: BitsOS makes one `Process` per image, so the record names exactly the code that called connect.
//! Linux has the same place for it: the connecting task's security label (`security_unix_stream_connect`),
//! read with SO_PEERSEC. BitsOS's label is one line of text:
//!
//! `bitsos:program:1:<init|own>:<st_dev>:<st_ino>:<path>`
//!
//! - `init`: the image's credentials are in the initial user namespace and its files are the boot view's;
//!   `own`: it lives in a user or mount namespace somebody made (whoever made it reads and writes the image,
//!   or chose the files it finds under its fixed paths);
//! - `st_dev`, `st_ino`: the executable as stat(2) names it, decimal (`0:0`: an image without a file);
//! - `path`: where the boot view has that file now; empty when it has no name there (removed, or a file
//!   only another view reaches). The path is last: it may hold any byte but NUL.
//!
//! The kernel's own listeners compare the image itself (`image`), never the text.

use crate::Process;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Write as _;

/// What every label starts with; the number is the format's version.
pub const LABEL: &str = "bitsos:program:1";

/// Whether `p` lives where the system's own programs live: in the initial user namespace and the boot's
/// mount view.
pub fn native(p: &Process) -> bool {
    p.cred().user_ns.is_init() && Arc::ptr_eq(&p.mount_ns(), &vfs::root_ns())
}

/// The label's text for an executable `(st_dev, st_ino, path)`.
pub fn label(native: bool, exe: Option<(u64, u64, &[u8])>) -> Vec<u8> {
    let (dev, ino, path) = exe.unwrap_or((0, 0, b""));
    let mut head = alloc::string::String::new();
    let _ = write!(head, "{LABEL}:{}:{dev}:{ino}:", if native { "init" } else { "own" });
    let mut out = head.into_bytes();
    out.extend_from_slice(path);
    out
}

/// The record of `p` as it is now, for a socket to keep.
pub fn record(p: &Arc<Process>) -> Arc<net::sock::Program> {
    // Cloned out: the path walk below takes VFS locks, the image lock is a spin lock.
    let exe = p.image_info.lock().exe.clone();
    let named = exe.map(|e| {
        let a = e.attr();
        let ns = vfs::root_ns();
        let path = if e.dentry.is_unlinked() { None } else { ns.path_of(&ns.root_path(), &e) };
        (a.dev, a.ino, path.unwrap_or_default())
    });
    let label = label(native(p), named.as_ref().map(|(d, i, path)| (*d, *i, path.as_slice())));
    let image: Arc<dyn Any + Send + Sync> = p.clone();
    Arc::new(net::sock::Program { label, image: Arc::downgrade(&image) })
}

/// The image a socket recorded, while that image still lives.
pub fn image(prog: &net::sock::Program) -> Option<Arc<Process>> {
    prog.image.upgrade()?.downcast::<Process>().ok()
}

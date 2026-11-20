//! A plan as bubblewrap 0.13.1's arguments (SX-3b's /bin/bwrap). bwrap makes the user and mount namespace,
//! the base tmpfs, the binds (read-only as bind + MS_REMOUNT|MS_BIND|MS_RDONLY), pivot_root and the umount of
//! the old root: the same calls a direct executor would make (user/abi-tests/c/ns-mount.c's sequence). The
//! environment goes to bwrap's own process (it passes it on), so no `--setenv` is needed.

use crate::plan::{Op, Plan};
use std::ffi::OsString;

pub const BWRAP: &str = "/bin/bwrap";

/// `--die-with-parent`: killing the process the launcher started (bwrap) also ends the program. Its signal comes
/// when the launcher's *thread* that started bitsrun ends (PR_SET_PDEATHSIG), so a launcher must start programs
/// from a thread that lives as long as they do.
pub fn args(p: &Plan) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["--unshare-user", "--die-with-parent"].iter().map(OsString::from).collect();
    let mut push = |xs: &[&str]| a.extend(xs.iter().map(OsString::from));
    for op in &p.ops {
        match op {
            Op::RoBind { src, dst } => push(&["--ro-bind", src, dst]),
            Op::Bind { src, dst } => push(&["--bind", src, dst]),
            Op::DevBind { src, dst } => push(&["--dev-bind", src, dst]),
            Op::Symlink { target, dst } => push(&["--symlink", target, dst]),
            Op::Dir { dst, mode } => push(&["--perms", &format!("{mode:04o}"), "--dir", dst]),
            Op::Tmpfs { dst, mode } => push(&["--perms", &format!("{mode:04o}"), "--tmpfs", dst]),
            Op::Dev { dst } => push(&["--dev", dst]),
            Op::Proc { dst } => push(&["--proc", dst]),
            Op::RemountRo { dst } => push(&["--remount-ro", dst]),
            Op::UnshareNet => push(&["--unshare-net"]),
        }
    }
    push(&["--chdir", &p.chdir, "--"]);
    a.extend(p.argv.iter().cloned());
    a
}

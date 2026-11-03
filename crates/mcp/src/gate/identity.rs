//! MC-9a: who may claim the installer's role at the gate. The executable identity, measured at boot: when the
//! gate is armed (before init runs, right after the initramfs was copied into tmpfs) the kernel keeps the inode
//! of `/bin/bitsinstall` with its size and times. A caller passes only when it runs as root (euid 0) and its
//! image's executable (Linux `mm->exe_file`, set at execve and spawn) is that very inode, unchanged since: same
//! object (a file renamed over it or a copy elsewhere is another inode), same size, mtime and ctime (a write or
//! truncate moves them, and no call sets ctime from user space; linux-6.6/fs/attr.c `setattr_copy`). The
//! running program keeps it so: execve holds deny_write on its file (ETXTBSY for writers while it runs).
//! Linux has no such check (CAP_SYS_BOOT, linux-6.6/kernel/reboot.c, goes to any root process); it is
//! BitsOS's narrowing of root for the actions that change what boots.
//!
//! The process layer reports the caller (`set_caller`, from crates/proc/src/mcp_run/caller.rs): mcp sits below
//! proc, so it cannot ask proc itself.

use alloc::string::String;
use alloc::sync::Arc;
use vfs::abi::Time;
use vfs::{Inode, Path};

pub use mcp_wire::installer::EXE;

/// The calling process as the process layer sees it.
pub struct Caller {
    pub pid: u32,
    pub euid: u32,
    /// The image's executable (None for an image started from memory).
    pub exe: Option<Path>,
}

static CALLER: ksync::Once<fn() -> Option<Caller>> = ksync::Once::new();

/// Installed once by the process layer.
pub fn set_caller(f: fn() -> Option<Caller>) {
    CALLER.get_or_init(|| f);
}

/// The calling process (None in a kernel thread or before the hook is installed).
pub fn current() -> Option<Caller> {
    CALLER.get().and_then(|f| f())
}

/// The installer's executable as this boot brought it.
pub struct Measured {
    inode: Arc<Inode>,
    size: u64,
    mtime: Time,
    ctime: Time,
}

impl Measured {
    /// Looks `path` up in the root view as the kernel (None: missing or not a regular file).
    pub fn of(path: &str) -> Option<Measured> {
        let ns = vfs::root_ns();
        let p = vfs::Ctx::kernel(&ns).lookup(None, path.as_bytes(), true).ok()?;
        let inode = p.dentry.inode.clone();
        if !inode.is_reg() {
            return None;
        }
        let (_, mtime, ctime) = inode.meta.times();
        Some(Measured { size: inode.size(), mtime, ctime, inode })
    }
    fn unchanged(&self) -> bool {
        let (_, m, c) = self.inode.meta.times();
        self.inode.size() == self.size && m == self.mtime && c == self.ctime
    }
}

/// The caller's pid when it may hold the installer's role, else why not.
pub fn judge(m: Option<&Measured>, c: Option<Caller>) -> Result<u32, String> {
    let c = c.ok_or_else(|| String::from("not called from a program"))?;
    if c.euid != 0 {
        return Err(alloc::format!("pid {} runs as uid {}, not root", c.pid, c.euid));
    }
    let m = m.ok_or_else(|| alloc::format!("this boot brought no {}", mcp_wire::installer::EXE))?;
    let exe = c.exe.ok_or_else(|| alloc::format!("pid {} has no executable file", c.pid))?;
    if !Arc::ptr_eq(&exe.dentry.inode, &m.inode) {
        return Err(alloc::format!("pid {} does not run the boot's {}", c.pid, mcp_wire::installer::EXE));
    }
    if !m.unchanged() {
        return Err(alloc::format!("{} changed since the boot", mcp_wire::installer::EXE));
    }
    Ok(c.pid)
}

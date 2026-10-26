//! FS-2 ktests of the framework (the ones needing processes, PCI or the console are in `vfs-sys`). All Safe
//! and rerunnable in one boot: the boot view is only read, scratch views are fresh.

use crate::devfs;
use crate::inode::Cred;
use crate::mount_ns::MountNs;
use crate::ops::Ctx;
use crate::synth::{DirSource, SynthEntry, Text};
use crate::{abi, Errno, ProcDirEntry, ProcFileEntry};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};
use uapi::errno::*;

const USER: Cred = Cred { uid: 1000, gid: 1000 };
const SUBJECT: policy::Subject = policy::Subject { program: policy::ProgramId(1), pid: 0x7fff_0001, uid: 1000 };

fn errno<T>(r: Result<T, Errno>) -> Errno {
    match r {
        Ok(_) => 0,
        Err(e) => e,
    }
}

/// The boot view: devtmpfs at /dev, proc at /proc, sysfs at /sys; every built-in node with Linux's number,
/// mode and owner; the memory devices answer as drivers/char/mem.c.
fn nodes() -> TestResult {
    let ns = crate::root_ns();
    for (path, fs) in [("/dev", "devtmpfs"), ("/proc", "proc"), ("/sys", "sysfs")] {
        let p = ns.lookup_path(path.as_bytes(), true).map_err(|e| format!("{path}: {e}"))?;
        kassert!(p.mnt.sb.fstype == fs && p.is_mount_root(), "{path} is {} (mount root {})", p.mnt.sb.fstype, p.is_mount_root());
    }
    let want = [
        ("/dev/null", 1, 3, 0o666),
        ("/dev/zero", 1, 5, 0o666),
        ("/dev/full", 1, 7, 0o666),
        ("/dev/random", 1, 8, 0o666),
        ("/dev/urandom", 1, 9, 0o666),
        ("/dev/tty", 5, 0, 0o666),
        ("/dev/console", 5, 1, 0o620),
        ("/dev/tty0", 4, 0, 0o620),
    ];
    for (path, maj, min, perm) in want {
        let a = ns.lookup(path).map_err(|e| format!("{path}: {e}"))?.attr();
        kassert!(a.mode == uapi::S_IFCHR | perm && a.rdev == abi::encode_dev(maj, min), "{path}: mode {:o} rdev {:#x}", a.mode, a.rdev);
    }
    let o = |p: &str, fl: u64| ns.open_flags(p.as_bytes(), fl, 0).map_err(|e| format!("{p}: {e}"));
    let null = o("/dev/null", abi::O_RDWR)?;
    let mut b = [7u8; 16];
    kassert!(null.read(&mut b) == Ok(0) && null.write(b"gone") == Ok(4));
    kassert!(null.seek(100, 0) == Ok(0), "null_lseek");
    let zero = o("/dev/zero", abi::O_RDONLY)?;
    kassert!(zero.read(&mut b) == Ok(16) && b == [0; 16]);
    let full = o("/dev/full", abi::O_RDWR)?;
    kassert!(full.write(b"x") == Err(ENOSPC));
    let u = o("/dev/urandom", abi::O_RDONLY)?;
    let (mut x, mut y) = ([0u8; 64], [0u8; 64]);
    kassert!(u.read(&mut x) == Ok(64) && u.read(&mut y) == Ok(64) && x != y && x != [0; 64]);
    let r = o("/dev/random", abi::O_RDONLY | abi::O_NONBLOCK)?;
    kassert!(r.read(&mut x) == Ok(64) && x != y, "/dev/random never blocks once seeded");
    // FS's own computed directory.
    kassert!(ns.read_file("/proc/sys/fs/nr_open", 64).map_err(|e| format!("nr_open: {e}"))? == b"1048576\n");
    kassert!(ns.read_file("/proc/sys/fs/file-max", 64).map_err(|e| format!("file-max: {e}"))? == b"9223372036854775807\n");
    Ok(())
}
ktest!("fs.dev.nodes", Safe, nodes);

/// A program (uid 1000) in the boot view: the devices open, names in /dev, /proc and /sys cannot be made or
/// removed, with Linux's error for each.
fn program_rules() -> TestResult {
    let ns = crate::root_ns();
    let c = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: USER, umask: 0o022, subject: SUBJECT };
    for p in ["/dev/null", "/dev/zero", "/dev/urandom", "/dev/random", "/dev/full", "/dev/tty", "/dev/console"] {
        kassert!(c.open(None, p.as_bytes(), abi::O_RDWR, 0).is_ok(), "uid 1000 cannot open {p}");
    }
    kassert!(errno(c.open(None, b"/dev/fs-ktest", abi::O_CREAT | abi::O_WRONLY, 0o644)) == EACCES);
    kassert!(errno(c.open(None, b"/proc/fs-ktest", abi::O_CREAT | abi::O_WRONLY, 0o644)) == ENOENT);
    kassert!(errno(c.open(None, b"/sys/fs-ktest", abi::O_CREAT | abi::O_WRONLY, 0o644)) == EACCES);
    kassert!(errno(c.mkdir(None, b"/proc/fs-ktest", 0o755)) == ENOENT);
    kassert!(errno(c.mkdir(None, b"/sys/fs-ktest", 0o755)) == EACCES);
    kassert!(errno(c.unlink(None, b"/dev/null")) == EACCES);
    // The kernel (root) still gets Linux's answers in proc and sysfs: no names from outside the kernel.
    let k = Ctx { ns: &ns, root: ns.root_path(), cwd: ns.root_path(), cred: Cred::ROOT, umask: 0, subject: SUBJECT };
    kassert!(errno(k.mkdir(None, b"/sys/fs-ktest", 0o755)) == EPERM);
    kassert!(errno(k.open(None, b"/proc/fs-ktest", abi::O_CREAT | abi::O_WRONLY, 0o644)) == ENOENT);
    Ok(())
}
ktest!("fs.dev.program_rules", Safe, program_rules);

struct Numbers;
impl DirSource for Numbers {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok((0..300u64).map(|i| SynthEntry::file(&format!("n{i}"), 0o444, Arc::new(Text(format!("{i}\n")))).cookie(10 + i)).collect())
    }
}
fn numbers() -> Arc<dyn DirSource> {
    Arc::new(Numbers)
}
fn hello() -> String {
    String::from("hello\n")
}

/// The framework on a scratch view: a `procfile!`-style text and a 300-name computed directory under /proc
/// listed through getdents64 in small pieces (every name once, static names first), read and looked up.
fn framework() -> TestResult {
    let ns = Arc::new(MountNs::new_empty());
    let st = devfs::populate_from(
        &ns,
        &[],
        &[ProcFileEntry { path: "/proc/fs-ktest/hello", render: hello }],
        &[ProcDirEntry { path: "/proc/fs-ktest", make: numbers }],
    );
    kassert!(st.failed == 0 && st.procfiles == 1 && st.procdirs == 1, "{st:?}");
    let f = ns.open_flags(b"/proc/fs-ktest", abi::O_RDONLY | abi::O_DIRECTORY, 0).map_err(|e| format!("open: {e}"))?;
    let mut names: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut got = Vec::new();
        crate::ops::read_dir_into(&f, 128, |b| {
            got.extend_from_slice(b);
            Ok(())
        })
        .map_err(|e| format!("getdents: {e}"))?;
        if got.is_empty() {
            break;
        }
        let mut off = 0;
        while off < got.len() {
            let reclen = u16::from_le_bytes([got[off + 16], got[off + 17]]) as usize;
            let name = &got[off + 19..off + reclen];
            names.push(name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())].to_vec());
            off += reclen;
        }
    }
    kassert!(names.len() == 303, "{} names listed", names.len());
    kassert!(names[0] == b"." && names[1] == b".." && names[2] == b"hello", "static names first");
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    kassert!(sorted.len() == 303, "a name was listed twice");
    kassert!(ns.read_file("/proc/fs-ktest/n299", 10).map_err(|e| format!("n299: {e}"))? == b"299\n");
    kassert!(ns.read_file("/proc/fs-ktest/hello", 10).map_err(|e| format!("hello: {e}"))? == b"hello\n");
    kassert!(ns.lookup("/proc/fs-ktest/n300").err() == Some(ENOENT));
    let a = ns.lookup("/proc/fs-ktest/n7").map_err(|e| format!("n7: {e}"))?.attr();
    kassert!(a.mode == uapi::S_IFREG | 0o444 && a.ino == ns.lookup("/proc/fs-ktest/n7").map_err(|e| format!("{e}"))?.attr().ino);
    Ok(())
}
ktest!("fs.proc.framework", Safe, framework);

//! IN-7 ktests in the kernel: `/dev/ptmx` and `/dev/pts/N` through the VFS's open path (FS-3's `open_node`), the
//! lock, both directions, the hang-up when the master closes and the index going back. Everything opened is
//! closed again, so the test can run again in the same boot (Safe).

use crate::ktests_in4::{BASE, KMem};
use crate::pty;
use crate::termios::ioc::*;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

const O_RDWR: u64 = 2;
const O_NOCTTY: u64 = 0o400;

fn read_nb(f: &vfs::fd::OpenFile) -> Result<String, vfs::Errno> {
    let mut b = vec![0u8; 256];
    let n = f.file.read_stream(&mut b, true)?;
    Ok(String::from_utf8_lossy(&b[..n]).into_owned())
}

fn nodes() -> TestResult {
    let ns = vfs::root_ns();
    let ptmx = ns.lookup("/dev/ptmx").map_err(|e| format!("/dev/ptmx: errno {e}"))?;
    kassert!(ptmx.inode.meta.mode() == uapi::S_IFCHR | 0o666, "/dev/ptmx mode {:o}", ptmx.inode.meta.mode());
    kassert!(ns.lookup("/dev/pts").is_ok_and(|d| d.is_dir()), "no /dev/pts directory");
    let before = pty::count();
    let m = ns.open_flags(b"/dev/ptmx", O_RDWR | O_NOCTTY, 0).map_err(|e| format!("open /dev/ptmx: errno {e}"))?;
    let mem = KMem::new();
    kassert!(m.file.ioctl_user(TIOCGPTN, BASE, &mem) == Some(Ok(0)), "TIOCGPTN");
    let path = format!("/dev/pts/{}", mem.i32_at(0));
    let node = ns.lookup(&path).map_err(|e| format!("{path}: errno {e}"))?;
    let meta = &node.inode.meta;
    kassert!(meta.mode() == uapi::S_IFCHR | pty::SLAVE_PERM && meta.gid() == pty::TTY_GID, "{path}: mode {:o} gid {}", meta.mode(), meta.gid());
    kassert!(ns.open_flags(path.as_bytes(), O_RDWR | O_NOCTTY, 0).err() == Some(EIO), "a locked slave opened");
    mem.put(0, &0i32.to_le_bytes());
    kassert!(m.file.ioctl_user(TIOCSPTLCK, BASE, &mem) == Some(Ok(0)), "unlockpt");
    let s = ns.open_flags(path.as_bytes(), O_RDWR | O_NOCTTY, 0).map_err(|e| format!("open {path}: errno {e}"))?;
    kassert!(m.file.write_stream(b"hi\r", false) == Ok(3), "master write");
    let echo = read_nb(&m);
    kassert!(echo.as_deref() == Ok("hi\r\n"), "echo {echo:?}");
    let line = read_nb(&s);
    kassert!(line.as_deref() == Ok("hi\n"), "slave line {line:?}");
    kassert!(s.file.write_stream(b"x\n", false) == Ok(2), "slave write");
    let out = read_nb(&m);
    kassert!(out.as_deref() == Ok("x\r\n"), "slave output {out:?}");
    drop(m);
    kassert!(ns.lookup(&path).is_err(), "{path} still there after the master closed");
    kassert!(read_nb(&s) == Ok(String::new()), "a hung-up slave reads 0");
    kassert!(s.file.write_stream(b"y", false) == Err(EIO), "a hung-up slave writes EIO");
    drop(s);
    kassert!(pty::count() == before, "pairs {} after, {before} before", pty::count());
    Ok(())
}
ktest!("in.pty.nodes", Safe, nodes);

/// Many pairs at once (no count limit, R1), then all closed: every node is gone, the pair count is back, and the
/// next master gets the lowest index again (`devpts_new_index`).
fn many_pairs() -> TestResult {
    const PAIRS: usize = 300;
    let ns = vfs::root_ns();
    let before = pty::count();
    let mem = KMem::new();
    let mut open = alloc::vec::Vec::with_capacity(PAIRS);
    for i in 0..PAIRS {
        let m = ns.open_flags(b"/dev/ptmx", O_RDWR | O_NOCTTY, 0).map_err(|e| format!("open /dev/ptmx #{i}: errno {e}"))?;
        kassert!(m.file.ioctl_user(TIOCGPTN, BASE, &mem) == Some(Ok(0)), "TIOCGPTN #{i}");
        open.push((m, mem.i32_at(0)));
    }
    kassert!(pty::count() == before + PAIRS, "pairs {} with {PAIRS} open, {before} before", pty::count());
    let lowest = open.iter().map(|o| o.1).min().unwrap_or(0);
    let paths: alloc::vec::Vec<String> = open.iter().map(|o| format!("/dev/pts/{}", o.1)).collect();
    for p in &paths {
        kassert!(ns.lookup(p).is_ok(), "{p} missing while its master is open");
    }
    drop(open);
    for p in &paths {
        kassert!(ns.lookup(p).is_err(), "{p} still there after its master closed");
    }
    kassert!(pty::count() == before, "pairs {} after, {before} before", pty::count());
    let m = ns.open_flags(b"/dev/ptmx", O_RDWR | O_NOCTTY, 0).map_err(|e| format!("open /dev/ptmx again: errno {e}"))?;
    kassert!(m.file.ioctl_user(TIOCGPTN, BASE, &mem) == Some(Ok(0)), "TIOCGPTN again");
    kassert!(mem.i32_at(0) <= lowest, "index {} after the close, the lowest freed was {lowest}", mem.i32_at(0));
    Ok(())
}
ktest!("in.pty.many_pairs", Safe, many_pairs);

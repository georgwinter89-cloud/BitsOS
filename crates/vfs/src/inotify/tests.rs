//! FS-14 host tests of inotify through the syscalls (the answers checked on CachyOS with
//! host/tests/fs/progs/fs-inotify.c): the argument checks in Linux's order, wds, the events of every name
//! operation in Linux's order, merging, IN_ONESHOT, IN_EXCL_UNLINK, read's record layout and its EINVAL/EAGAIN,
//! FIONREAD, IN_Q_OVERFLOW, and that closing the instance ends its watches.

extern crate std;

use super::group::Group;
use super::*;
use crate::cases_test::{program_view, Mem, SUBJECT, USER};
use crate::node::File;
use crate::sys::{self, Sys};
use alloc::vec::Vec;
use uapi::errno::*;

const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_CREAT: u64 = 0o100;
const O_NONBLOCK: u64 = 0o4000;

fn call(s: &Sys<'_>, nr: usize, a: [u64; 6]) -> i64 {
    sys::call(s, nr, a)
}

macro_rules! env {
    ($ns:ident, $fds:ident, $mem:ident, $s:ident) => {
        let ($ns, $fds) = program_view();
        let $mem = Mem::new();
        let $s = Sys { mem: &$mem, fds: &$fds, ns: &$ns, cred: USER, subject: SUBJECT };
    };
}

fn path_call(s: &Sys<'_>, mem: &Mem, nr: usize, p: &str, a1: u64) -> i64 {
    let b = mem.cstr(p.as_bytes());
    call(s, nr, [b, a1, 0, 0, 0, 0])
}
fn add(s: &Sys<'_>, mem: &Mem, fd: i64, p: &str, mask: u32) -> i64 {
    let b = mem.cstr(p.as_bytes());
    call(s, 254, [fd as u64, b, mask as u64, 0, 0, 0])
}
fn open(s: &Sys<'_>, mem: &Mem, p: &str, flags: u64) -> i64 {
    let b = mem.cstr(p.as_bytes());
    call(s, 2, [b, flags, 0o644, 0, 0, 0])
}
fn close(s: &Sys<'_>, fd: i64) {
    assert_eq!(call(s, 3, [fd as u64, 0, 0, 0, 0, 0]), 0);
}
fn rename(s: &Sys<'_>, mem: &Mem, a: &str, b: &str) -> i64 {
    let (x, y) = (mem.cstr(a.as_bytes()), mem.cstr(b.as_bytes()));
    call(s, 82, [x, y, 0, 0, 0, 0])
}

/// One read of up to `len` bytes: the events as (wd, mask, cookie, name), or the errno.
fn read(s: &Sys<'_>, mem: &Mem, fd: i64, len: usize) -> Result<Vec<(i32, u32, u32, Vec<u8>)>, i64> {
    let buf = mem.alloc_zero(len);
    let n = call(s, 0, [fd as u64, buf, len as u64, 0, 0, 0]);
    if n < 0 {
        return Err(-n);
    }
    let b = mem.get(buf, n as usize);
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let w = |o: usize| u32::from_le_bytes(b[at + o..at + o + 4].try_into().unwrap());
        let len = w(12) as usize;
        assert_eq!(len % 16, 0, "name room is a multiple of 16");
        let name = &b[at + 16..at + 16 + len];
        let end = name.iter().position(|&c| c == 0).unwrap_or(len);
        assert!(name[end..].iter().all(|&c| c == 0), "padded with zeros");
        out.push((w(0) as i32, w(4), w(8), name[..end].to_vec()));
        at += 16 + len;
    }
    Ok(out)
}

fn ev(s: &Sys<'_>, mem: &Mem, fd: i64) -> Vec<(i32, u32, u32, Vec<u8>)> {
    read(s, mem, fd, 4096).unwrap_or_default()
}

/// Masks and names only (cookies checked where they matter).
fn mn(v: &[(i32, u32, u32, Vec<u8>)]) -> Vec<(i32, u32, &[u8])> {
    v.iter().map(|e| (e.0, e.1, e.3.as_slice())).collect()
}

#[test]
fn checks_in_linux_order_and_wds() {
    env!(ns, fds, mem, s);
    assert_eq!(call(&s, 294, [0o1, 0, 0, 0, 0, 0]), -EINVAL, "unknown init1 flag");
    let fd = call(&s, 294, [O_NONBLOCK, 0, 0, 0, 0, 0]);
    assert!(fd >= 0);
    assert_eq!(add(&s, &mem, fd, "/tmp", 0), -EINVAL, "no bit");
    assert_eq!(add(&s, &mem, fd, "/tmp", 0x0800_0000), -EINVAL, "an unknown bit");
    assert_eq!(add(&s, &mem, 99, "/tmp", IN_CREATE), -EBADF);
    assert_eq!(add(&s, &mem, fd, "/tmp", IN_CREATE | IN_MASK_ADD | IN_MASK_CREATE), -EINVAL);
    let plain = open(&s, &mem, "/tmp/plain", O_CREAT | O_WRONLY);
    assert_eq!(add(&s, &mem, plain, "/tmp", IN_CREATE), -EINVAL, "not an inotify fd");
    assert_eq!(add(&s, &mem, fd, "/tmp/plain", IN_CREATE | IN_ONLYDIR), -ENOTDIR);
    assert_eq!(add(&s, &mem, fd, "/tmp/nothing", IN_CREATE), -ENOENT);
    assert_eq!(add(&s, &mem, fd, "/tmp", IN_CREATE), 1);
    assert_eq!(add(&s, &mem, fd, "/tmp/plain", IN_MODIFY), 2);
    assert_eq!(add(&s, &mem, fd, "/tmp", IN_DELETE), 1, "the same inode: the same wd, mask replaced");
    assert_eq!(add(&s, &mem, fd, "/tmp", IN_CREATE | IN_MASK_CREATE), -EEXIST);
    assert_eq!(call(&s, 255, [fd as u64, 7, 0, 0, 0, 0]), -EINVAL, "no such wd");
    assert_eq!(call(&s, 255, [fd as u64, 2, 0, 0, 0, 0]), 0);
    assert_eq!(mn(&ev(&s, &mem, fd)), [(2, IN_IGNORED, &b""[..])]);
    assert_eq!(add(&s, &mem, fd, "/tmp/plain", IN_MODIFY), 3, "wds are not reused at once");
    assert_eq!(read(&s, &mem, fd, 64), Err(EAGAIN), "nothing queued, O_NONBLOCK");
    close(&s, plain);
    let mode = fds.get(fd as usize).unwrap().stat().mode;
    assert_eq!(mode, 0o600, "an anonymous inode");
}

#[test]
fn directory_events_in_linux_order() {
    env!(ns, fds, mem, s);
    assert_eq!(path_call(&s, &mem, 83, "/tmp/w", 0o755), 0);
    let fd = call(&s, 294, [O_NONBLOCK, 0, 0, 0, 0, 0]);
    assert_eq!(add(&s, &mem, fd, "/tmp/w", IN_ALL_EVENTS), 1);
    let f = open(&s, &mem, "/tmp/w/f", O_CREAT | O_WRONLY);
    let data = mem.alloc(b"abc");
    assert_eq!(call(&s, 1, [f as u64, data, 3, 0, 0, 0]), 3);
    assert_eq!(call(&s, 1, [f as u64, data, 3, 0, 0, 0]), 3);
    close(&s, f);
    assert_eq!(path_call(&s, &mem, 90, "/tmp/w/f", 0o600), 0);
    let r = open(&s, &mem, "/tmp/w/f", O_RDONLY);
    let buf = mem.alloc_zero(8);
    assert_eq!(call(&s, 0, [r as u64, buf, 8, 0, 0, 0]), 6);
    close(&s, r);
    let got = ev(&s, &mem, fd);
    let f = &b"f"[..];
    assert_eq!(mn(&got), [(1, IN_CREATE, f), (1, IN_OPEN, f), (1, IN_MODIFY, f), (1, IN_CLOSE_WRITE, f), (1, IN_ATTRIB, f),
        (1, IN_OPEN, f), (1, IN_ACCESS, f), (1, IN_CLOSE_NOWRITE, f)], "two writes merge into one IN_MODIFY");
    assert_eq!(rename(&s, &mem, "/tmp/w/f", "/tmp/w/g"), 0);
    let got = ev(&s, &mem, fd);
    assert_eq!(mn(&got), [(1, IN_MOVED_FROM, f), (1, IN_MOVED_TO, &b"g"[..])]);
    assert!(got[0].2 != 0 && got[0].2 == got[1].2, "one cookie for the pair");
    assert_eq!(path_call(&s, &mem, 87, "/tmp/w/g", 0), 0);
    assert_eq!(path_call(&s, &mem, 83, "/tmp/w/d", 0o755), 0);
    assert_eq!(path_call(&s, &mem, 84, "/tmp/w/d", 0), 0);
    let d = &b"d"[..];
    assert_eq!(mn(&ev(&s, &mem, fd)), [(1, IN_DELETE, &b"g"[..]), (1, IN_CREATE | IN_ISDIR, d), (1, IN_DELETE | IN_ISDIR, d)]);
    // The watched directory itself goes: IN_DELETE_SELF, IN_IGNORED (no IN_ISDIR with SELF events).
    assert_eq!(path_call(&s, &mem, 84, "/tmp/w", 0), 0);
    assert_eq!(mn(&ev(&s, &mem, fd)), [(1, IN_DELETE_SELF, &b""[..]), (1, IN_IGNORED, &b""[..])]);
    let g = fds.get(fd as usize).unwrap();
    assert_eq!(g.file.as_inotify().unwrap().watches(), 0);
    let _ = ns;
}

#[test]
fn self_watch_oneshot_and_hard_links() {
    env!(ns, fds, mem, s);
    let f = open(&s, &mem, "/tmp/s", O_CREAT | O_WRONLY);
    close(&s, f);
    let fd = call(&s, 294, [O_NONBLOCK, 0, 0, 0, 0, 0]);
    assert_eq!(add(&s, &mem, fd, "/tmp/s", IN_ALL_EVENTS), 1);
    let (a, b) = (mem.cstr(b"/tmp/s"), mem.cstr(b"/tmp/s2"));
    assert_eq!(call(&s, 86, [a, b, 0, 0, 0, 0]), 0);
    assert_eq!(path_call(&s, &mem, 87, "/tmp/s", 0), 0);
    let e = &b""[..];
    assert_eq!(mn(&ev(&s, &mem, fd)), [(1, IN_ATTRIB, e)], "link and unlink change the link count (merged)");
    assert_eq!(rename(&s, &mem, "/tmp/s2", "/tmp/s3"), 0);
    assert_eq!(mn(&ev(&s, &mem, fd)), [(1, IN_MOVE_SELF, e)]);
    // Held open, the last unlink defers IN_DELETE_SELF to the last close (`dentry_unlink_inode`).
    let h = open(&s, &mem, "/tmp/s3", O_RDONLY);
    assert_eq!(path_call(&s, &mem, 87, "/tmp/s3", 0), 0);
    assert_eq!(mn(&ev(&s, &mem, fd)), [(1, IN_OPEN, e), (1, IN_ATTRIB, e)]);
    close(&s, h);
    assert_eq!(mn(&ev(&s, &mem, fd)), [(1, IN_CLOSE_NOWRITE, e), (1, IN_DELETE_SELF, e), (1, IN_IGNORED, e)]);
    // IN_ONESHOT: one event, then the watch ends.
    let f = open(&s, &mem, "/tmp/o", O_CREAT | O_WRONLY);
    close(&s, f);
    ev(&s, &mem, fd);
    assert_eq!(add(&s, &mem, fd, "/tmp/o", IN_OPEN | IN_ONESHOT), 2);
    close(&s, open(&s, &mem, "/tmp/o", O_RDONLY));
    close(&s, open(&s, &mem, "/tmp/o", O_RDONLY));
    assert_eq!(mn(&ev(&s, &mem, fd)), [(2, IN_OPEN, e), (2, IN_IGNORED, e)]);
    let _ = ns;
}

#[test]
fn read_layout_einval_fionread() {
    env!(ns, fds, mem, s);
    let fd = call(&s, 294, [O_NONBLOCK, 0, 0, 0, 0, 0]);
    assert_eq!(add(&s, &mem, fd, "/tmp", IN_CREATE), 1);
    close(&s, open(&s, &mem, "/tmp/abcdefghijklmnop", O_CREAT | O_WRONLY));
    close(&s, open(&s, &mem, "/tmp/x", O_CREAT | O_WRONLY));
    let g = fds.get(fd as usize).unwrap();
    // 16 + 32 (16 letters + NUL rounded up) and 16 + 16.
    assert_eq!(g.file.ioctl(uapi::ioctl::FIONREAD, 0), Ok(80i32.to_le_bytes().to_vec()));
    assert_eq!(read(&s, &mem, fd, 47), Err(EINVAL), "the first event does not fit");
    assert_eq!(read(&s, &mem, fd, 0), Err(EINVAL));
    let one = read(&s, &mem, fd, 60).unwrap();
    assert_eq!(mn(&one), [(1, IN_CREATE, &b"abcdefghijklmnop"[..])], "only whole events");
    assert_eq!(read(&s, &mem, fd, 32).unwrap().len(), 1);
    assert_eq!(g.file.events().0 & crate::poll::Events::IN, 0, "empty: not readable");
    let _ = ns;
}

#[test]
fn overflow_and_exclusive_unlink() {
    env!(ns, fds, mem, s);
    let dir = ns.mkdir_p("/tmp/q").unwrap();
    dir.inode.meta.set_perm(0o777);
    let g = Group::new(USER.uid, 3);
    super::sys::take_instance_for_test(USER.uid);
    assert_eq!(g.update_watch(&dir.inode, IN_CREATE), Ok(1));
    for n in 0..6 {
        close(&s, open(&s, &mem, &alloc::format!("/tmp/q/{n}"), O_CREAT | O_WRONLY));
    }
    // Three events, then the one overflow event (wd -1), nothing more.
    assert_eq!(g.queued(), 4);
    let mut b = [0u8; 512];
    let n = g.read_stream(&mut b, true).unwrap();
    assert_eq!(n, 3 * 32 + 16);
    let last = &b[n - 16..n];
    assert_eq!(i32::from_le_bytes(last[0..4].try_into().unwrap()), -1);
    assert_eq!(u32::from_le_bytes(last[4..8].try_into().unwrap()), IN_Q_OVERFLOW);
    drop(g);
    // IN_EXCL_UNLINK: an unlinked file's events are not reported to that watch.
    let fd = call(&s, 294, [O_NONBLOCK, 0, 0, 0, 0, 0]);
    let w = open(&s, &mem, "/tmp/q/0", O_WRONLY);
    assert_eq!(add(&s, &mem, fd, "/tmp/q", IN_MODIFY | IN_EXCL_UNLINK), 1);
    assert_eq!(path_call(&s, &mem, 87, "/tmp/q/0", 0), 0);
    let data = mem.alloc(b"z");
    assert_eq!(call(&s, 1, [w as u64, data, 1, 0, 0, 0]), 1);
    assert_eq!(read(&s, &mem, fd, 64), Err(EAGAIN));
    close(&s, w);
    // Closing the instance ends its watches (none left on the inode).
    let inotify = fds.get(fd as usize).unwrap();
    assert_eq!(inotify.file.as_inotify().unwrap().watches(), 1);
    drop(inotify);
    close(&s, fd);
    assert!(dir.inode.marks.is_empty());
}

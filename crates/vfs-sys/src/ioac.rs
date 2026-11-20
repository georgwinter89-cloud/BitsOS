//! SYS-K: the read and write family counted for `/proc/<pid>/io` (`rchar`/`syscr`, `wchar`/`syscw`; Linux's
//! `vfs_read`/`vfs_write`, `do_sendfile` and `vfs_copy_file_range` count both sides). Each wrapper runs the call
//! and hands its result to `sched::ioac`.

use vfs::sys::{Args, Sys};

fn read(r: i64) -> i64 {
    sched::ioac::read_call(r);
    r
}

fn write(r: i64) -> i64 {
    sched::ioac::write_call(r);
    r
}

fn both(r: i64) -> i64 {
    sched::ioac::read_call(r);
    sched::ioac::write_call(r);
    r
}

pub(crate) fn read_any(s: &Sys<'_>, a: Args) -> i64 {
    read(super::read_any(s, a))
}
pub(crate) fn write_any(s: &Sys<'_>, a: Args) -> i64 {
    write(super::write_any(s, a))
}
pub(crate) fn readv_any(s: &Sys<'_>, a: Args) -> i64 {
    read(super::readv_any(s, a))
}
pub(crate) fn writev_any(s: &Sys<'_>, a: Args) -> i64 {
    write(super::writev_any(s, a))
}
pub(crate) fn pread64(s: &Sys<'_>, a: Args) -> i64 {
    read(vfs::sys::pread64(s, a))
}
pub(crate) fn pwrite64(s: &Sys<'_>, a: Args) -> i64 {
    write(vfs::sys::pwrite64(s, a))
}
pub(crate) fn preadv(s: &Sys<'_>, a: Args) -> i64 {
    read(vfs::sys_copy::preadv(s, a))
}
pub(crate) fn pwritev(s: &Sys<'_>, a: Args) -> i64 {
    write(vfs::sys_copy::pwritev(s, a))
}
pub(crate) fn preadv2_any(s: &Sys<'_>, a: Args) -> i64 {
    read(super::preadv2_any(s, a))
}
pub(crate) fn pwritev2_any(s: &Sys<'_>, a: Args) -> i64 {
    write(super::pwritev2_any(s, a))
}
pub(crate) fn sendfile(s: &Sys<'_>, a: Args) -> i64 {
    both(vfs::sys_copy::sendfile(s, a))
}
pub(crate) fn copy_file_range(s: &Sys<'_>, a: Args) -> i64 {
    both(vfs::sys_copy::copy_file_range(s, a))
}

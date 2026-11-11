//! SX-IPC glue: the twelve SysV IPC calls on x86-64 (linux-7.3.1/ipc/sem.c, shm.c, msg.c). The objects are
//! `proc::ipc`; here are the argument rules in Linux's order, the layouts of the structures and every user copy,
//! always with no IPC lock held.

mod msg;
mod sem;
mod shm;
mod sx_ipc_ktests;

use crate::SysCtx;
use proc::ipc::Caller;
use proc::ipc::ids::{Perm, UserPerm};
use proc::cred::Cred;
use uapi::ab::sysv::PERM64_SIZE;
use uapi::errno::*;
use vfs::Errno;

fn caller<'a>(c: &'a SysCtx<'_>) -> Caller<'a> {
    Caller::new(c.proc, Some(c.thread))
}

fn answer(r: Result<i64, Errno>) -> i64 {
    r.unwrap_or_else(|e| -e)
}

fn put(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_perm(b: &mut [u8], p: &Perm, c: &Cred) {
    b[..PERM64_SIZE].copy_from_slice(&p.to_user(c));
}

fn write_user(c: &SysCtx<'_>, addr: u64, b: &[u8]) -> Result<(), Errno> {
    c.proc.mm.copy_to_user(addr, b).map_err(|_| EFAULT)
}

fn read_user(c: &SysCtx<'_>, addr: u64, b: &mut [u8]) -> Result<(), Errno> {
    c.proc.mm.copy_from_user(b, addr).map_err(|_| EFAULT)
}

/// IPC_SET's argument: the whole structure must be readable (`copy_from_user(out, buf, sizeof(*out))`).
fn read_set<const N: usize>(c: &SysCtx<'_>, addr: u64) -> Result<(UserPerm, [u8; N]), Errno> {
    let mut b = [0u8; N];
    read_user(c, addr, &mut b)?;
    let mut p = [0u8; PERM64_SIZE];
    p.copy_from_slice(&b[..PERM64_SIZE]);
    Ok((UserPerm::from_user(&p), b))
}

//! semget, semop, semtimedop, semctl.

use super::*;
use alloc::vec::Vec;
use proc::ipc::{sem, sem_ctl};
use uapi::ab::sysv::*;

/// `timespec64_valid` on what `get_timespec64` read: its check comes after the array's (sem.c:2003).
fn timeout_ns(sec: i64, nsec: i64) -> Result<u64, Errno> {
    const NSEC: u64 = 1_000_000_000;
    if sec < 0 || !(0..NSEC as i64).contains(&nsec) {
        return Err(EINVAL);
    }
    Ok((sec as u64).saturating_mul(NSEC).saturating_add(nsec as u64))
}

/// `do_semtimedop` (sem.c:2243): the timeout's copy, the count, the array's copy.
fn semop_common(c: &SysCtx<'_>, semid: i32, tsops: u64, nsops: u32, timeout: u64) -> Result<(), Errno> {
    let who = caller(c);
    let ts = if timeout != 0 {
        let mut b = [0u8; 16];
        read_user(c, timeout, &mut b)?;
        Some((i64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8])), i64::from_le_bytes(b[8..].try_into().unwrap_or([0; 8]))))
    } else {
        None
    };
    if nsops as i64 > who.ns.tables.limits.sem[SC_SEMOPM].load(core::sync::atomic::Ordering::Relaxed) as i64 {
        return Err(E2BIG);
    }
    if nsops < 1 {
        return Err(EINVAL);
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(nsops as usize * 6).map_err(|_| ENOMEM)?;
    bytes.resize(nsops as usize * 6, 0u8);
    read_user(c, tsops, &mut bytes)?;
    let mut sops = Vec::new();
    sops.try_reserve_exact(nsops as usize).map_err(|_| ENOMEM)?;
    sops.extend(bytes.chunks_exact(6).map(|b| sem::SemBuf {
        num: u16::from_le_bytes([b[0], b[1]]),
        op: i16::from_le_bytes([b[2], b[3]]),
        flg: i16::from_le_bytes([b[4], b[5]]),
    }));
    if semid < 0 {
        return Err(EINVAL);
    }
    let ns = match ts {
        Some((s, n)) => Some(timeout_ns(s, n)?),
        None => None,
    };
    sem::semtimedop(&who, &c.thread.ipc().undo, semid, sops, ns)
}

fn sys_semget(c: &mut SysCtx<'_>) -> i64 {
    answer(sem::semget(&caller(c), c.args[0] as i32, c.args[1] as i32, c.args[2] as i32).map(|v| v as i64))
}
crate::syscall_nr!(NR_SEMGET, "semget", sys_semget);

fn sys_semop(c: &mut SysCtx<'_>) -> i64 {
    answer(semop_common(c, c.args[0] as i32, c.args[1], c.args[2] as u32, 0).map(|_| 0))
}
crate::syscall_nr!(NR_SEMOP, "semop", sys_semop);

fn sys_semtimedop(c: &mut SysCtx<'_>) -> i64 {
    answer(semop_common(c, c.args[0] as i32, c.args[1], c.args[2] as u32, c.args[3]).map(|_| 0))
}
crate::syscall_nr!(NR_SEMTIMEDOP, "semtimedop", sys_semtimedop);

fn semid64(st: &sem::SemStat, cred: &Cred) -> [u8; SEMID64_SIZE] {
    let mut b = [0u8; SEMID64_SIZE];
    put_perm(&mut b, &st.perm, cred);
    put(&mut b, 48, st.otime as u64);
    put(&mut b, 64, st.ctime as u64);
    put(&mut b, 80, st.nsems as u64);
    b
}

/// `ksys_semctl` (sem.c:1646).
fn semctl(c: &SysCtx<'_>) -> Result<i64, Errno> {
    let (semid, semnum, cmd, arg) = (c.args[0] as i32, c.args[1] as i32, c.args[2] as i32, c.args[3]);
    if semid < 0 {
        return Err(EINVAL);
    }
    let who = caller(c);
    match cmd {
        IPC_INFO | SEM_INFO => {
            let (v, max) = sem_ctl::info(&who, cmd);
            let mut b = [0u8; 40];
            for (i, x) in v.iter().enumerate() {
                put32(&mut b, i * 4, *x as u32);
            }
            write_user(c, arg, &b)?;
            Ok(max as i64)
        }
        IPC_STAT | SEM_STAT | SEM_STAT_ANY => {
            let (st, r) = sem_ctl::stat(&who, semid, cmd)?;
            write_user(c, arg, &semid64(&st, &who.cred))?;
            Ok(r as i64)
        }
        GETALL => {
            sem_ctl::getall(&who, semid, &mut |vals| {
                let mut b = Vec::new();
                b.try_reserve_exact(vals.len() * 2).map_err(|_| ENOMEM)?;
                b.extend(vals.iter().flat_map(|v| v.to_le_bytes()));
                write_user(c, arg, &b)
            })?;
            Ok(0)
        }
        SETALL => {
            sem_ctl::setall(&who, semid, &mut |vals| {
                let mut b = Vec::new();
                b.try_reserve_exact(vals.len() * 2).map_err(|_| ENOMEM)?;
                b.resize(vals.len() * 2, 0u8);
                read_user(c, arg, &mut b)?;
                for (v, x) in vals.iter_mut().zip(b.chunks_exact(2)) {
                    *v = u16::from_le_bytes([x[0], x[1]]);
                }
                Ok(())
            })?;
            Ok(0)
        }
        GETVAL | GETPID | GETNCNT | GETZCNT => sem_ctl::get_one(&who, semid, semnum, cmd).map(|v| v as i64),
        SETVAL => sem_ctl::setval(&who, semid, semnum, arg as i32).map(|_| 0),
        IPC_SET => {
            let (p, _) = read_set::<SEMID64_SIZE>(c, arg)?;
            sem_ctl::set(&who, semid, &p).map(|_| 0)
        }
        IPC_RMID => sem_ctl::rmid(&who, semid).map(|_| 0),
        _ => Err(EINVAL),
    }
}

fn sys_semctl(c: &mut SysCtx<'_>) -> i64 {
    answer(semctl(c))
}
crate::syscall_nr!(NR_SEMCTL, "semctl", sys_semctl);

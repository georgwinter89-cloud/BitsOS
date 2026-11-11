//! shmget, shmat, shmdt, shmctl.

use super::*;
use proc::ipc::{shm, shm_map};
use uapi::ab::sysv::*;

fn sys_shmget(c: &mut SysCtx<'_>) -> i64 {
    let who = caller(c);
    let r = shm::shmget(&who, c.args[0] as i32, c.args[1], c.args[2] as i32);
    if r.is_ok() {
        c.thread.ipc().made_segment_in(&who.ns);
    }
    answer(r.map(|v| v as i64))
}
crate::syscall_nr!(NR_SHMGET, "shmget", sys_shmget);

fn sys_shmat(c: &mut SysCtx<'_>) -> i64 {
    answer(shm_map::shmat(&caller(c), &c.proc.mm, c.args[0] as i32, c.args[1], c.args[2] as i32).map(|a| a as i64))
}
crate::syscall_nr!(NR_SHMAT, "shmat", sys_shmat);

fn sys_shmdt(c: &mut SysCtx<'_>) -> i64 {
    answer(shm_map::shmdt(&c.proc.mm, c.args[0]).map(|_| 0))
}
crate::syscall_nr!(NR_SHMDT, "shmdt", sys_shmdt);

fn shmid64(st: &shm::ShmStat, who: &Caller<'_>) -> [u8; SHMID64_SIZE] {
    let mut b = [0u8; SHMID64_SIZE];
    put_perm(&mut b, &st.perm, &who.cred);
    put(&mut b, 48, st.segsz);
    put(&mut b, 56, st.atim as u64);
    put(&mut b, 64, st.dtim as u64);
    put(&mut b, 72, st.ctim as u64);
    put32(&mut b, 80, who.vnr(st.cpid));
    put32(&mut b, 84, who.vnr(st.lpid));
    put(&mut b, 88, st.nattch);
    b
}

/// `ksys_shmctl` (shm.c:1241).
fn shmctl(c: &SysCtx<'_>) -> Result<i64, Errno> {
    let (id, cmd, buf) = (c.args[0] as i32, c.args[1] as i32, c.args[2]);
    if cmd < 0 || id < 0 {
        return Err(EINVAL);
    }
    let who = caller(c);
    match cmd {
        IPC_INFO => {
            let (v, max) = shm::ipc_info(&who);
            let mut b = [0u8; SHMINFO64_SIZE];
            for (i, x) in v.iter().enumerate() {
                put(&mut b, i * 8, *x);
            }
            write_user(c, buf, &b)?;
            Ok(max as i64)
        }
        SHM_INFO => {
            let (used, tot, rss, max) = shm::shm_info(&who);
            let mut b = [0u8; SHM_INFO_SIZE];
            put32(&mut b, 0, used as u32);
            put(&mut b, 8, tot);
            put(&mut b, 16, rss);
            write_user(c, buf, &b)?;
            Ok(max as i64)
        }
        IPC_STAT | SHM_STAT | SHM_STAT_ANY => {
            let (st, r) = shm::stat(&who, id, cmd)?;
            write_user(c, buf, &shmid64(&st, &who))?;
            Ok(r as i64)
        }
        IPC_SET => {
            let (p, _) = read_set::<SHMID64_SIZE>(c, buf)?;
            shm::set(&who, id, &p).map(|_| 0)
        }
        IPC_RMID => shm::rmid(&who, id).map(|_| 0),
        SHM_LOCK | SHM_UNLOCK => shm::lock(&who, id, cmd).map(|_| 0),
        _ => Err(EINVAL),
    }
}

fn sys_shmctl(c: &mut SysCtx<'_>) -> i64 {
    answer(shmctl(c))
}
crate::syscall_nr!(NR_SHMCTL, "shmctl", sys_shmctl);

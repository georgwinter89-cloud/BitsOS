//! msgget, msgsnd, msgrcv, msgctl.

use super::*;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use proc::ipc::msg::{self, Msg, RcvArgs};
use proc::ipc::msg_ctl;
use uapi::ab::sysv::*;

/// `load_msg`: the text from user memory, before any lock.
fn load(c: &SysCtx<'_>, addr: u64, len: u64) -> Result<Vec<u8>, Errno> {
    let mut text = Vec::new();
    text.try_reserve_exact(len as usize).map_err(|_| ENOMEM)?;
    text.resize(len as usize, 0u8);
    read_user(c, addr, &mut text)?;
    Ok(text)
}

fn sys_msgget(c: &mut SysCtx<'_>) -> i64 {
    answer(msg::msgget(&caller(c), c.args[0] as i32, c.args[1] as i32).map(|v| v as i64))
}
crate::syscall_nr!(NR_MSGGET, "msgget", sys_msgget);

/// `ksys_msgsnd`, `do_msgsnd` (msg.c:601): `get_user(mtype)` comes before everything.
fn msgsnd(c: &SysCtx<'_>) -> Result<(), Errno> {
    let (id, msgp, size, flg) = (c.args[0] as i32, c.args[1], c.args[2], c.args[3] as i32);
    let mut t = [0u8; 8];
    read_user(c, msgp, &mut t)?;
    let mtype = i64::from_le_bytes(t);
    let who = caller(c);
    msg::snd_check(&who, id, mtype, size)?;
    let text = load(c, msgp.wrapping_add(8), size)?;
    msg::msgsnd(&who, id, Msg { mtype, text }, flg)
}

fn sys_msgsnd(c: &mut SysCtx<'_>) -> i64 {
    answer(msgsnd(c).map(|_| 0))
}
crate::syscall_nr!(NR_MSGSND, "msgsnd", sys_msgsnd);

/// `do_msgrcv` (msg.c:805), `do_msg_fill`. MSG_COPY reads its buffer first (`prepare_copy` loads it, only for the
/// EFAULT and the size).
fn msgrcv(c: &SysCtx<'_>) -> Result<i64, Errno> {
    let (id, msgp, bufsz, mtype, flg) = (c.args[0] as i32, c.args[1], c.args[2], c.args[3] as i64, c.args[4] as i32);
    if id < 0 || (bufsz as i64) < 0 {
        return Err(EINVAL);
    }
    let who = caller(c);
    let copy_len = if flg & MSG_COPY != 0 {
        if flg & MSG_EXCEPT != 0 || flg & IPC_NOWAIT == 0 {
            return Err(EINVAL);
        }
        let room = bufsz.min(who.ns.tables.limits.msgmax.load(Ordering::Relaxed) as i64 as u64);
        load(c, msgp, room)?;
        Some(room)
    } else {
        None
    };
    let m = msg::msgrcv(&who, &RcvArgs { id, bufsz, mtype, flg }, copy_len)?;
    write_user(c, msgp, &m.mtype.to_le_bytes())?;
    let n = bufsz.min(m.text.len() as u64);
    write_user(c, msgp.wrapping_add(8), &m.text[..n as usize])?;
    Ok(n as i64)
}

fn sys_msgrcv(c: &mut SysCtx<'_>) -> i64 {
    answer(msgrcv(c))
}
crate::syscall_nr!(NR_MSGRCV, "msgrcv", sys_msgrcv);

fn msqid64(st: &msg_ctl::MsgStat, who: &Caller<'_>) -> [u8; MSQID64_SIZE] {
    let mut b = [0u8; MSQID64_SIZE];
    put_perm(&mut b, &st.perm, &who.cred);
    put(&mut b, 48, st.stime as u64);
    put(&mut b, 56, st.rtime as u64);
    put(&mut b, 64, st.ctime as u64);
    put(&mut b, 72, st.cbytes);
    put(&mut b, 80, st.qnum);
    put(&mut b, 88, st.qbytes);
    put32(&mut b, 96, who.vnr(st.lspid));
    put32(&mut b, 100, who.vnr(st.lrpid));
    b
}

/// `ksys_msgctl` (msg.c:485).
fn msgctl(c: &SysCtx<'_>) -> Result<i64, Errno> {
    let (id, cmd, buf) = (c.args[0] as i32, c.args[1] as i32, c.args[2]);
    if id < 0 || cmd < 0 {
        return Err(EINVAL);
    }
    let who = caller(c);
    match cmd {
        IPC_INFO | MSG_INFO => {
            let (v, seg, max) = msg_ctl::info(&who, cmd);
            let mut b = [0u8; MSGINFO_SIZE];
            for (i, x) in v.iter().enumerate() {
                put32(&mut b, i * 4, *x as u32);
            }
            b[28..30].copy_from_slice(&seg.to_le_bytes());
            write_user(c, buf, &b)?;
            Ok(max as i64)
        }
        IPC_STAT | MSG_STAT | MSG_STAT_ANY => {
            let (st, r) = msg_ctl::stat(&who, id, cmd)?;
            write_user(c, buf, &msqid64(&st, &who))?;
            Ok(r as i64)
        }
        IPC_SET => {
            let (p, raw) = read_set::<MSQID64_SIZE>(c, buf)?;
            let qbytes = u64::from_le_bytes(raw[88..96].try_into().unwrap_or([0; 8]));
            msg_ctl::set(&who, id, &p, qbytes).map(|_| 0)
        }
        IPC_RMID => msg_ctl::rmid(&who, id).map(|_| 0),
        _ => Err(EINVAL),
    }
}

fn sys_msgctl(c: &mut SysCtx<'_>) -> i64 {
    answer(msgctl(c))
}
crate::syscall_nr!(NR_MSGCTL, "msgctl", sys_msgctl);

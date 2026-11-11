//! The lines of `/proc/sysvipc/{sem,shm,msg}` (linux-7.3.1/ipc/sem.c `sysvipc_sem_proc_show`, shm.c
//! `sysvipc_shm_proc_show`, msg.c `sysvipc_msg_proc_show`), objects in index order, for the reader's namespaces.

use super::ids::{Ids, Perm};
use crate::ns::misc::IpcNs;
use crate::ns::user::UserNs;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

pub const SEM_HEADER: &str = "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n";
pub const SHM_HEADER: &str = "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap\n";
pub const MSG_HEADER: &str = "       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime\n";

/// Whose view: the namespace, the ids' user namespace and `pid_nr_ns` for the pid namespace of the reader.
pub struct View<'a> {
    pub ns: &'a IpcNs,
    pub user_ns: &'a UserNs,
    pub vnr: &'a dyn Fn(u32) -> u32,
}

/// The objects, taken out so nothing below runs under the table's lock.
fn all<T>(ids: &Ids<T>) -> Vec<Arc<T>> {
    let mut v = Vec::new();
    if v.try_reserve_exact(ids.in_use().max(0) as usize).is_ok() {
        v.extend(ids.iter().cloned());
    }
    v
}

fn owners(p: &Perm, c: &View<'_>) -> [u32; 4] {
    let ns = c.user_ns;
    [ns.uid(p.uid), ns.gid(p.gid), ns.uid(p.cuid), ns.gid(p.cgid)]
}

pub fn sem(c: &View<'_>) -> String {
    let mut s = String::from(SEM_HEADER);
    let objects = all(&c.ns.tables.sem.ids.read());
    for a in objects {
        let st = a.stat();
        let o = owners(&st.perm, c);
        let _ = writeln!(s, "{:10} {:10}  {:4o} {:10} {:5} {:5} {:5} {:5} {:10} {:10}", st.perm.key, st.perm.id, st.perm.mode, st.nsems, o[0], o[1], o[2], o[3], st.otime, st.ctime);
    }
    s
}

pub fn shm(c: &View<'_>) -> String {
    let mut s = String::from(SHM_HEADER);
    let objects = all(&c.ns.tables.shm.ids.read());
    for a in objects {
        let st = a.stat();
        let o = owners(&st.perm, c);
        let _ = writeln!(
            s,
            "{:10} {:10}  {:4o} {:21} {:5} {:5}  {:5} {:5} {:5} {:5} {:5} {:10} {:10} {:10} {:21} {:21}",
            st.perm.key, st.perm.id, st.perm.mode, st.segsz, (c.vnr)(st.cpid), (c.vnr)(st.lpid), st.nattch, o[0], o[1], o[2], o[3], st.atim, st.dtim, st.ctim, st.rss * 4096, 0
        );
    }
    s
}

pub fn msg(c: &View<'_>) -> String {
    let mut s = String::from(MSG_HEADER);
    let objects = all(&c.ns.tables.msg.ids.read());
    for a in objects {
        let st = a.stat();
        let o = owners(&st.perm, c);
        let _ = writeln!(
            s,
            "{:10} {:10}  {:4o}  {:10} {:10} {:5} {:5} {:5} {:5} {:5} {:5} {:10} {:10} {:10}",
            st.perm.key, st.perm.id, st.perm.mode, st.cbytes, st.qnum, (c.vnr)(st.lspid), (c.vnr)(st.lrpid), o[0], o[1], o[2], o[3], st.stime, st.rtime, st.ctime
        );
    }
    s
}

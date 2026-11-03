//! `capget` and `capset` (linux-6.6/kernel/capability.c :141, :220) over V2's capability model
//! (`proc::cred`): the permitted and effective sets follow from the ids until something stores them (a new
//! user namespace, or capset itself: SX-2), the inheritable set is stored. capget answers as Linux for any
//! task; capset follows `cap_capset` (EPERM for growing what Linux refuses) and stores the result, so a
//! program can drop capabilities (Chromium's sandbox keeps only CAP_SYS_ADMIN, then none).
//!
//! Approximation: Linux keeps the sets per thread; V2 per thread group, like the ids (AB-13). Chromium drops
//! them while single-threaded.

use crate::SysCtx;
use proc::cred::{self, Cred};
use uapi::errno::*;

const V1: u32 = 0x1998_0330;
const V2: u32 = 0x2007_1026;
const V3: u32 = 0x2008_0522;

/// `cap_validate_magic` (:75): how many 32-bit data blocks the version carries; an unknown version gets the
/// preferred one written back and EINVAL.
fn blocks(c: &SysCtx<'_>, header: u64) -> Result<usize, i64> {
    let mut v = [0u8; 4];
    c.proc.mm.copy_from_user(&mut v, header).map_err(|_| -EFAULT)?;
    match u32::from_le_bytes(v) {
        V1 => Ok(1),
        V2 | V3 => Ok(2),
        _ => match c.proc.mm.copy_to_user(header, &V3.to_le_bytes()) {
            Ok(()) => Err(-EINVAL),
            Err(_) => Err(-EFAULT),
        },
    }
}

fn header_pid(c: &SysCtx<'_>, header: u64) -> Result<i32, i64> {
    let mut p = [0u8; 4];
    c.proc.mm.copy_from_user(&mut p, header + 4).map_err(|_| -EFAULT)?;
    Ok(i32::from_le_bytes(p))
}

/// (effective, permitted, inheritable) of credentials `k` under bounding set `bset`.
fn sets(k: &Cred, bset: u64) -> [u64; 3] {
    let (prm, eff) = cred::cap_sets(k, bset);
    [eff, prm, k.cap_inh]
}

/// `struct __user_cap_data_struct[blocks]`: {effective, permitted, inheritable} per 32 bits.
fn encode(s: [u64; 3], blocks: usize) -> alloc::vec::Vec<u8> {
    (0..blocks).flat_map(|i| s.into_iter().flat_map(move |x| ((x >> (32 * i)) as u32).to_le_bytes())).collect()
}

fn sys_capget(c: &mut SysCtx<'_>) -> i64 {
    let (header, data) = (c.args[0], c.args[1]);
    let n = match blocks(c, header) {
        Ok(n) => n,
        // A NULL data pointer asks for the preferred version only (`capget` :150).
        Err(e) if data == 0 && e == -EINVAL => return 0,
        Err(e) => return e,
    };
    if data == 0 {
        return 0;
    }
    let pid = match header_pid(c, header) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if pid < 0 {
        return -EINVAL;
    }
    // `cap_get_target_pid` (:109): pid 0 is the caller; any other task id may be read (no LSM).
    let s = if pid == 0 || pid as u32 == c.thread.vtid() {
        sets(&c.proc.cred(), c.thread.prctl().cap_bset())
    } else {
        match crate::ns_sys::find_thread(c, pid as u32) {
            Some(t) => sets(&t.group().cred(), t.prctl().cap_bset()),
            None => return -ESRCH,
        }
    };
    match c.proc.mm.copy_to_user(data, &encode(s, n)) {
        Ok(()) => 0,
        Err(_) => -EFAULT,
    }
}
crate::syscall_nr!(uapi::ab::nr_more::CAPGET, "capget", sys_capget);

fn sys_capset(c: &mut SysCtx<'_>) -> i64 {
    let (header, data) = (c.args[0], c.args[1]);
    let n = match blocks(c, header) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let pid = match header_pid(c, header) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if pid != 0 && pid as u32 != c.thread.vtid() {
        return -EPERM;
    }
    let mut raw = [0u8; 24];
    if c.proc.mm.copy_from_user(&mut raw[..n * 12], data).is_err() {
        return -EFAULT;
    }
    let word = |i: usize| u32::from_le_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]) as u64;
    let valid = proc::prctl_state::CAP_FULL_SET;
    let want = [0, 4, 8].map(|o| (word(o) | word(12 + o) << 32) & valid);
    let bset = c.thread.prctl().cap_bset();
    if want == sets(&c.proc.cred(), bset) {
        return 0;
    }
    // security/commoncap.c `cap_capset` (`proc::cred::capset`), then `commit_creds`.
    let [e, p, i] = want;
    match c.proc.change_cred(c.thread, |o| cred::capset(o, bset, want)) {
        Ok(_) => {
            klog::event!(Info, "syscall", "capset", pid = c.proc.pid, eff = klog::Hex(e), prm = klog::Hex(p), inh = klog::Hex(i));
            0
        }
        Err(err) => {
            klog::event!(Info, "syscall", "capset.refused", pid = c.proc.pid, eff = klog::Hex(e), prm = klog::Hex(p), inh = klog::Hex(i));
            -err
        }
    }
}
crate::syscall_nr!(uapi::ab::nr_more::CAPSET, "capset", sys_capset);

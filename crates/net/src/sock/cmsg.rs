//! Control messages of sendmsg (linux-6.6/net/core/scm.c `__scm_send`, `scm_fp_copy`, `scm_check_creds`;
//! include/linux/socket.h `CMSG_OK`, `__cmsg_nxthdr`). The control buffer is a kernel copy; descriptors are
//! looked up through the caller's fd table as they are met (EBADF stops the parse).

use super::abi::*;
use super::{Caller, Ucred};
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::{Errno, OpenFile};

/// What a sendmsg's control data carries.
#[derive(Default)]
pub struct SendCtl {
    /// SCM_RIGHTS: the open files in order (all SCM_RIGHTS headers together, at most SCM_MAX_FD).
    pub files: Vec<Arc<OpenFile>>,
    /// SCM_CREDENTIALS as given (checked against the caller).
    pub creds: Option<Ucred>,
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(a)
}

/// The headers of `ctl` in Linux's walk: `(level, type, data)`; EINVAL for a header whose length is below
/// `sizeof(cmsghdr)` or reaches past the buffer (`CMSG_OK`).
pub fn headers(ctl: &[u8]) -> Result<Vec<(u32, u32, &[u8])>, Errno> {
    let mut v = Vec::new();
    let mut at = 0usize;
    // __CMSG_FIRSTHDR: a buffer shorter than one header has none; __cmsg_nxthdr: the next header must fit.
    while at + CMSGHDR <= ctl.len() {
        let len = u64_at(ctl, at);
        if len < CMSGHDR as u64 || len > (ctl.len() - at) as u64 {
            return Err(EINVAL);
        }
        let len = len as usize;
        v.push((u32_at(ctl, at + 8), u32_at(ctl, at + 12), &ctl[at + CMSGHDR..at + len]));
        at += cmsg_align(len);
    }
    Ok(v)
}

/// `__scm_send` for an AF_UNIX socket. `fd` looks a descriptor up (`fget_raw`: O_PATH files count).
pub fn parse_unix(ctl: &[u8], caller: &Caller, mut fd: impl FnMut(i32) -> Option<Arc<OpenFile>>) -> Result<SendCtl, Errno> {
    let mut out = SendCtl::default();
    for (level, ty, data) in headers(ctl)? {
        if level != SOL_SOCKET {
            continue;
        }
        match ty {
            SCM_RIGHTS => {
                let num = data.len() / 4;
                if num == 0 {
                    continue;
                }
                if num > SCM_MAX_FD || out.files.len() + num > SCM_MAX_FD {
                    return Err(EINVAL);
                }
                out.files.try_reserve(num).map_err(|_| ENOMEM)?;
                for i in 0..num {
                    let n = u32_at(data, i * 4) as i32;
                    let f = if n < 0 { None } else { fd(n) };
                    out.files.push(f.ok_or(EBADF)?);
                }
            }
            SCM_CREDENTIALS => {
                if data.len() != 12 {
                    return Err(EINVAL);
                }
                let c = Ucred { pid: u32_at(data, 0), uid: u32_at(data, 4), gid: u32_at(data, 8) };
                check_creds(&c, caller)?;
                out.creds = Some(c);
            }
            _ => return Err(EINVAL),
        }
    }
    Ok(out)
}

/// `scm_check_creds`: a program may send its own pid, uid and gid only (root: anything, but the pid must
/// exist, ESRCH); -1 as an id is EINVAL.
fn check_creds(c: &Ucred, caller: &Caller) -> Result<(), Errno> {
    if c.uid == u32::MAX || c.gid == u32::MAX {
        return Err(EINVAL);
    }
    let root = caller.uid == 0;
    let ok = (c.pid == caller.pid || root) && (c.uid == caller.uid || root) && (c.gid == caller.gid || root);
    if !ok {
        return Err(EPERM);
    }
    if c.pid != caller.pid && !super::pid_exists(c.pid) {
        return Err(ESRCH);
    }
    Ok(())
}

/// TCP and UDP (`sock_cmsg_send`, `ip_cmsg_send`): headers are checked; SOL_SOCKET ones this kernel does not
/// support (SO_MARK, timestamps; SCM_RIGHTS is AF_UNIX only) are EINVAL; IP-level ones are accepted and ignored.
pub fn check_inet(ctl: &[u8]) -> Result<(), Errno> {
    for (level, _, _) in headers(ctl)? {
        if level == SOL_SOCKET {
            return Err(EINVAL);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(level: u32, ty: u32, data: &[u8]) -> Vec<u8> {
        let mut v = (cmsg_len(data.len()) as u64).to_le_bytes().to_vec();
        v.extend_from_slice(&level.to_le_bytes());
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(data);
        v.resize(cmsg_space(data.len()), 0);
        v
    }

    const ME: Caller = Caller { pid: 7, uid: 1000, gid: 1000, subject: policy::Subject::KERNEL };

    #[test]
    fn walk_and_checks() {
        assert!(headers(&[0u8; 8]).unwrap().is_empty(), "shorter than one header: none");
        let mut bad = hdr(SOL_SOCKET, SCM_RIGHTS, &[0; 4]);
        bad[0] = 15;
        assert_eq!(headers(&bad).err(), Some(EINVAL));
        bad[0] = 40;
        assert_eq!(headers(&bad).err(), Some(EINVAL), "past the end");
        let mut two = hdr(1234, 1, &[1, 2, 3]);
        two.extend(hdr(SOL_SOCKET, SCM_CREDENTIALS, &Ucred { pid: 7, uid: 1000, gid: 1000 }.bytes()));
        let h = headers(&two).unwrap();
        assert_eq!(h.len(), 2);
        let c = parse_unix(&two, &ME, |_| None).unwrap();
        assert_eq!(c.creds, Some(Ucred { pid: 7, uid: 1000, gid: 1000 }));
        let other = hdr(SOL_SOCKET, SCM_CREDENTIALS, &Ucred { pid: 7, uid: 0, gid: 1000 }.bytes());
        assert_eq!(parse_unix(&other, &ME, |_| None).err(), Some(EPERM));
        let short = hdr(SOL_SOCKET, SCM_CREDENTIALS, &[0; 8]);
        assert_eq!(parse_unix(&short, &ME, |_| None).err(), Some(EINVAL));
        let unknown = hdr(SOL_SOCKET, 99, &[]);
        assert_eq!(parse_unix(&unknown, &ME, |_| None).err(), Some(EINVAL));
        let badfd = hdr(SOL_SOCKET, SCM_RIGHTS, &5i32.to_le_bytes());
        assert_eq!(parse_unix(&badfd, &ME, |_| None).err(), Some(EBADF));
        let many: Vec<u8> = (0..254).flat_map(|_| 0i32.to_le_bytes()).collect();
        assert_eq!(parse_unix(&hdr(SOL_SOCKET, SCM_RIGHTS, &many), &ME, |_| None).err(), Some(EINVAL));
        assert_eq!(check_inet(&hdr(SOL_IP, 8, &[0; 12])), Ok(()));
        assert_eq!(check_inet(&hdr(SOL_SOCKET, SCM_RIGHTS, &[0; 4])), Err(EINVAL));
    }
}

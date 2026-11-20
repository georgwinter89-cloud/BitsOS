//! CL-3b: who may answer on the session's assistant socket. The socket's folder is the user's own, so any of
//! his programs can put a listener at the path; what such a program says would be shown as the assistant's, and
//! it would get every question and paste. bitscomp therefore talks only to a peer the kernel vouches for:
//! - the helper bitscomp itself started (`SO_PEERCRED` on the connected socket names the process that called
//!   `listen`: its pid is the keeper's child, its uid bitscomp's own), or
//! - a program the kernel recorded as /bin/assist when it called `listen` (BitsOS: `SO_PEERSEC`, the label of
//!   `proc::peer`; settingsd reads the same record). That is the helper a bitscomp before this one started:
//!   it still answers after bitscomp came back, and holds the conversation.
//!
//! Linux keeps no such record: there only bitscomp's own child counts (/proc/<pid>/exe would name what a pid
//! runs now, not who listens: a program can listen, hand the socket to a child and become /bin/assist).

use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// What every label of BitsOS's kernel starts with, with its version (kernel: `proc::peer::LABEL`).
const LABEL: &str = "bitsos:program:1:";

pub struct Guard {
    /// The helper's program and bitscomp's own uid.
    exe: PathBuf,
    uid: u32,
    /// The pid of the helper the keeper started; 0: none runs.
    helper: AtomicU32,
    /// An accepted peer is connected.
    served: AtomicBool,
}

impl Guard {
    pub fn new(exe: &Path, uid: u32) -> Guard {
        Guard { exe: exe.to_path_buf(), uid, helper: AtomicU32::new(0), served: AtomicBool::new(false) }
    }

    pub fn set_helper(&self, pid: u32) {
        self.helper.store(pid, Ordering::SeqCst);
    }

    pub fn set_served(&self, on: bool) {
        self.served.store(on, Ordering::SeqCst);
    }

    pub fn served(&self) -> bool {
        self.served.load(Ordering::SeqCst)
    }

    /// Ok(how the peer was accepted) or Err(why not), for the log.
    pub fn check(&self, s: &UnixStream) -> Result<String, String> {
        let cred = crate::clients::peer_cred(s);
        let label = crate::clients::peer_label(s);
        self.judge(cred, label.as_deref(), |p| std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino())))
    }

    /// `check` with the kernel's answers given: the peer's (pid, uid), its label, and what file lies at a path.
    pub fn judge(&self, cred: Option<(i32, u32)>, label: Option<&[u8]>, at: impl FnOnce(&Path) -> Option<(u64, u64)>) -> Result<String, String> {
        let (pid, uid) = cred.ok_or("the socket names no peer (SO_PEERCRED)")?;
        if uid != self.uid {
            return Err(format!("pid {pid} runs as uid {uid}, not as this session's uid {}", self.uid));
        }
        let helper = self.helper.load(Ordering::SeqCst);
        if helper != 0 && i64::from(pid) == i64::from(helper) {
            return Ok(format!("pid {pid} is the helper bitscomp started"));
        }
        match label.and_then(recorded) {
            Some(Some((path, file))) if Path::new(&path) == self.exe && at(&self.exe) == Some(file) => Ok(format!("pid {pid}: the kernel recorded {path} as the listening program")),
            Some(Some((path, _))) => Err(format!("pid {pid}: the kernel recorded {path:?} as the listening program, not {}", self.exe.display())),
            Some(None) => Err(format!("pid {pid}: the kernel's record names no program of this system (a namespace of its own, or no file)")),
            None => Err(format!("pid {pid} is not the helper bitscomp started (pid {helper}), and this kernel keeps no record of the listening program")),
        }
    }
}

/// The kernel's record `bitsos:program:1:<init|own>:<st_dev>:<st_ino>:<path>` as (path, (dev, ino)). None: not
/// such a label. Some(None): a label that names nobody to trust (a program in a user or mount namespace of its
/// own, one without a file). The path is last and may hold any byte.
fn recorded(label: &[u8]) -> Option<Option<(String, (u64, u64))>> {
    let rest = label.strip_prefix(LABEL.as_bytes())?;
    let parts = || {
        let mut f = rest.splitn(4, |&b| b == b':');
        let (ns, dev, ino, path) = (f.next()?, f.next()?, f.next()?, f.next()?);
        let num = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u64>().ok();
        let path = std::str::from_utf8(path).ok()?;
        (ns == b"init" && path.starts_with('/')).then_some((path.to_string(), (num(dev)?, num(ino)?)))
    };
    Some(parts())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> Guard {
        Guard::new(Path::new("/bin/assist"), 1000)
    }

    #[test]
    fn only_bitscomps_own_helper_or_the_kernels_record_of_the_assistant_is_accepted() {
        let g = guard();
        let file = |_: &Path| Some((8, 77));
        // No helper runs and the kernel says nothing: whoever listens is a stranger.
        assert!(g.judge(Some((500, 1000)), None, file).is_err());
        g.set_helper(500);
        assert!(g.judge(Some((500, 1000)), None, file).is_ok(), "the keeper's child");
        assert!(g.judge(Some((501, 1000)), None, file).is_err(), "another pid of the same user");
        assert!(g.judge(Some((500, 0)), None, file).is_err(), "another user, whatever its pid");
        assert!(g.judge(None, None, file).is_err());
        // The helper of a bitscomp before this one: the kernel's record of who called listen decides.
        g.set_helper(0);
        let label = |s: &str| s.as_bytes().to_vec();
        assert!(g.judge(Some((300, 1000)), Some(&label("bitsos:program:1:init:8:77:/bin/assist")), file).is_ok());
        assert!(g.judge(Some((300, 1000)), Some(&label("bitsos:program:1:init:8:78:/bin/assist")), file).is_err(), "another file under the path");
        assert!(g.judge(Some((300, 1000)), Some(&label("bitsos:program:1:own:8:77:/bin/assist")), file).is_err(), "a namespace of its own");
        assert!(g.judge(Some((300, 1000)), Some(&label("bitsos:program:1:init:8:77:/home/user/assist")), file).is_err());
        assert!(g.judge(Some((300, 1000)), Some(&label("bitsos:program:2:init:8:77:/bin/assist")), file).is_err(), "a version this bitscomp does not read");
        assert!(g.judge(Some((300, 1000)), Some(&label("unconfined")), file).is_err(), "another kernel's label");
        assert!(g.judge(Some((300, 1000)), Some(&label("bitsos:program:1:init:8:77:/bin/assist")), |_| None).is_err(), "no file at the path here");
    }

    #[test]
    fn a_socket_pair_names_this_process() {
        let (a, _b) = UnixStream::pair().unwrap();
        let g = Guard::new(Path::new("/bin/assist"), crate::drop_root::euid());
        assert!(g.check(&a).is_err(), "no helper was started");
        g.set_helper(std::process::id());
        assert!(g.check(&a).is_ok());
    }
}

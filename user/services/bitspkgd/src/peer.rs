//! Who may change (plan 1.10): root (its own `bitspkg` command line could do the same) and bitscomp as uid 1000
//! while it holds fb0's compositor grant, as the kernel names it (`/proc/bitsos/compositor`): MC-9a's check of
//! install.sock's confirmer, `bitsinstall::judge_peer`, reused. Everyone else reads. Claude's changing tools come
//! through the kernel's installer gate instead (`mcp`), never through this socket: a process MCP's `run` started
//! (root too) only reads. The kernel names those pids in `/proc/bitsos/power` (`mcp_run=`, kept over fork and
//! exec), the list init judges power-offs by (VB-1 change request: `update_confirm` and the other update ops).

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub enum Right {
    /// May change; who it is (for the report and the events).
    Change(String),
    /// Reads only; why it may not change (German, for the refusal).
    Read(String),
}

impl Right {
    pub fn may_change(&self) -> bool {
        matches!(self, Right::Change(_))
    }
    pub fn who(&self) -> &str {
        match self {
            Right::Change(w) | Right::Read(w) => w,
        }
    }
}

/// The file whose line `mcp_run=` names the processes of MCP's `run`.
pub const MCP_RUN_FILE: &str = "/proc/bitsos/power";

/// The pids of `/proc/bitsos/power`'s line `mcp_run=1,2` (none when the line is missing or empty).
pub fn mcp_run_pids(text: &str) -> Vec<u32> {
    text.lines()
        .find_map(|l| l.strip_prefix("mcp_run="))
        .map(|l| l.split(',').filter_map(|p| p.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// The decision for a peer: its pid and uid (SO_PEERCRED), its executable (None: not readable), the kernel's
/// holder file (None: this kernel has none) and whether the kernel names it a process of MCP's `run`.
pub fn judge(pid: u32, uid: u32, exe: Option<&Path>, holder: Option<&str>, mcp_run: bool) -> Right {
    if mcp_run {
        return Right::Read(format!("pid {pid} (uid {uid}) wurde über MCP gestartet; Änderungen nur über den Installer mit Bestätigung am Gerät"));
    }
    if uid == 0 {
        return Right::Change(format!("root (pid {pid})"));
    }
    if !bitsinstall::CONFIRMER_UIDS.contains(&uid) {
        return Right::Read(format!("pid {pid} (uid {uid}) darf nur lesen"));
    }
    let Some(exe) = exe else {
        return Right::Read(format!("pid {pid} (uid {uid}): Programm nicht feststellbar; nur lesen"));
    };
    match bitsinstall::judge_peer(&bitsinstall::CONFIRMERS, pid, uid, exe, holder) {
        Ok(who) => Right::Change(who),
        Err(why) => Right::Read(format!("nur der Compositor mit der Bildschirm-Freigabe darf Programme ändern ({why})")),
    }
}

/// The right of the process at the other end of `s`.
pub fn of(s: &UnixStream) -> Right {
    let (pid, uid, _) = match usys::power::peer_cred(s.as_raw_fd() as usize) {
        Ok(c) => c,
        Err(e) => return Right::Read(format!("SO_PEERCRED: Fehler {e}; nur lesen")),
    };
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let holder = match std::fs::read_to_string(bitsinstall::COMPOSITOR_HOLDER) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Right::Read(format!("{}: {e}; nur lesen", bitsinstall::COMPOSITOR_HOLDER)),
    };
    let mcp_run = match std::fs::read_to_string(MCP_RUN_FILE) {
        Ok(t) => mcp_run_pids(&t).contains(&pid),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Right::Read(format!("{MCP_RUN_FILE}: {e}; nur lesen")),
    };
    judge(pid, uid, exe.as_deref(), holder.as_deref(), mcp_run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_the_grant_holder_change_others_read() {
        assert!(judge(7, 0, None, None, false).may_change());
        let comp = Path::new("/boot/BITSOS2/bin/bitscomp");
        assert!(judge(412, 1000, Some(comp), Some("holder=412\n"), false).may_change());
        assert!(judge(412, 1000, Some(comp), None, false).may_change(), "a kernel without the file: uid and program decide");
        for (pid, uid, exe, holder) in [
            (413, 1000, Some(comp), Some("holder=412\n")),
            (412, 1000, Some(comp), Some("holder=none\n")),
            (412, 1000, Some(Path::new("/bin/bitspkgd")), Some("holder=412\n")),
            (412, 1000, None, Some("holder=412\n")),
            (412, 1001, Some(comp), Some("holder=412\n")),
            (412, 1000, Some(comp), Some("holder=x\n")),
        ] {
            let r = judge(pid, uid, exe, holder, false);
            assert!(!r.may_change(), "{pid} {uid} {exe:?} {holder:?}: {r:?}");
        }
    }

    #[test]
    fn a_process_of_mcps_run_only_reads_root_too() {
        let t = "poweroff=refused why=stale\nold\nmcp_run=7,412\n";
        assert_eq!(mcp_run_pids(t), vec![7, 412]);
        assert_eq!(mcp_run_pids("poweroff=allowed\nx\nmcp_run=\n"), Vec::<u32>::new());
        assert_eq!(mcp_run_pids("poweroff=allowed\nx\n"), Vec::<u32>::new(), "an older kernel without the line");
        let pids = mcp_run_pids(t);
        let r = judge(7, 0, None, None, pids.contains(&7));
        assert!(!r.may_change() && r.who().contains("MCP"), "{r:?}");
        assert!(judge(8, 0, None, None, pids.contains(&8)).may_change(), "root not started by MCP still changes");
        let comp = Path::new("/boot/BITSOS2/bin/bitscomp");
        assert!(!judge(412, 1000, Some(comp), Some("holder=412\n"), true).may_change(), "even the holder's pid");
    }
}

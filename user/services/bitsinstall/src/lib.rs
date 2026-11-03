//! bitsinstall (MC-9a): the installer service. The kernel lets exactly this program (the boot's
//! /bin/bitsinstall, root) take bitsmcp's install requests at the gate (`mcp_wire::installer`): installing a
//! kernel or system generation, pinning a generation, Reload, reboot, guarded settings (settingsd's changing
//! tools) and, once it has a tool, opening the NVMe gate. For each one it decides (`decide`): no desktop session
//! means no; a standing grant set at the physical keyboard (`grant`, kept on the stick by `store`) means yes
//! without a dialog; otherwise bitscomp shows a dialog on top (`confirm` is the protocol to MC-9b, `hub` the
//! place the confirmer connects to; `text` what the dialog says) and only a yes given with real input counts.
//! Every decision and grant change goes into the journal (`log`, kernel events). The kernel runs the tool only
//! after the yes; this service never acts by itself.
//!
//! `link`: the gate session; `filter`: the seccomp deny list it runs under after its setup; `console` (MC-11): the
//! text console asks when no confirmer is connected (real keyboard only).
#![forbid(unsafe_code)]

pub mod access;
pub mod confirm;
pub mod console;
pub mod decide;
pub mod filter;
pub mod gate_loop;
pub mod grant;
pub mod hub;
pub mod link;
pub mod log;
pub mod store;
pub mod text;

/// The confirmer the session runs (CONFIG.TXT `mcp_confirmer=test`: the test confirmer below, QEMU only).
/// INT-W24 (WY-14 CR 3): bitscomp's rescue build and its gpu build on the stick (CONFIG bitscomp=gpu hands over to
/// it, so the session's compositor runs that file); both must hold fb0's compositor grant.
pub const CONFIRMERS: [&str; 2] = ["/bin/bitscomp", "/boot/BITSOS2/bin/bitscomp-gpu"];
pub const TEST_CONFIRMER: &str = "/bin/bitsinstall";
/// The users a confirmer may run as: root and the desktop user (bitscomp gives root up for 1000).
pub const CONFIRMER_UIDS: [u32; 2] = [0, 1000];
/// INT-W17 (MC-9b CR 2): the kernel names the holder of fb0's compositor grant here (gopfb
/// fbdev/compositor.rs), so only the session's real compositor is taken as the confirmer.
pub const COMPOSITOR_HOLDER: &str = "/proc/bitsos/compositor";

/// The peer check after SO_PEERCRED: `uid` must be a confirmer's, `exe` (the peer's /proc/PID/exe) one of `allowed`,
/// and for a compositor build (`CONFIRMERS`) the process `holder` names (the text of `COMPOSITOR_HOLDER`, None when
/// the kernel has no such file) must be `pid`. Ok: the line logged for the accepted peer; Err: why it was refused.
pub fn judge_peer(allowed: &[&str], pid: u32, uid: u32, exe: &std::path::Path, holder: Option<&str>) -> Result<String, String> {
    if !CONFIRMER_UIDS.contains(&uid) {
        return Err(format!("pid {pid} runs as uid {uid}"));
    }
    let Some(name) = allowed.iter().find(|a| exe == std::path::Path::new(a)) else {
        return Err(format!("pid {pid} runs {}, not {}", exe.display(), allowed.join(" or ")));
    };
    // INT-W17 (MC-9b CR 2): the real confirmer is the process holding fb0's compositor grant, as the kernel names
    // it; a second bitscomp started while the session's is down is not. A kernel without the file leaves the
    // decision to the uid and the executable.
    if CONFIRMERS.contains(name) {
        if let Some(t) = holder {
            return match holder_of(t) {
                Some(Some(h)) if h == pid => Ok(format!("{name} (pid {pid}, uid {uid}, holds the compositor grant)")),
                Some(Some(h)) => Err(format!("pid {pid} runs {name}, but pid {h} holds the compositor grant")),
                Some(None) => Err(format!("pid {pid} runs {name}, but no process holds the compositor grant")),
                None => Err(format!("{COMPOSITOR_HOLDER}: not understood: {:?}", t.trim())),
            };
        }
    }
    Ok(format!("{name} (pid {pid}, uid {uid})"))
}

/// What a kernel `holder=` file says: `Some(Some(pid))`, `Some(None)` for `holder=none`, None for anything else.
pub fn holder_of(text: &str) -> Option<Option<u32>> {
    let v = text.trim().strip_prefix("holder=")?;
    if v == "none" {
        return Some(None);
    }
    v.parse::<u32>().ok().filter(|&p| p != 0).map(Some)
}

#[cfg(test)]
mod holder_tests {
    #[test]
    fn holder_lines() {
        assert_eq!(super::holder_of("holder=412\n"), Some(Some(412)));
        assert_eq!(super::holder_of("holder=none\n"), Some(None));
        for bad in ["", "holder=", "holder=0", "holder=-1", "holder=12 13", "pid=12", "holder=x"] {
            assert_eq!(super::holder_of(bad), None, "{bad:?}");
        }
    }
}

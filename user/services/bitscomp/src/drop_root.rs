//! WY-7: bitscomp started as root (Georg's console shell is init's, uid 0) runs as the desktop user. It opens
//! what needs root first (the framebuffer, the input devices, the console's terminal, the compositor grant),
//! then gives root up for good (setgroups, setresgid, setresuid, in that order as chpst does; linux-6.6/
//! kernel/sys.c `__sys_setresuid` :667, `__sys_setresgid` :769, kernel/groups.c `setgroups` :198), before any
//! thread, socket or file of the session exists: the sockets, the session file, the foot colours and the font
//! cache are the user's, `bitscomp ctl` and MCP's programs (uid 1000) reach the sockets, and every program it
//! starts (foot, Cobalt's launcher) runs as that user too. File descriptors opened before stay open (the kernel
//! checks access at open); hot-plugged keyboards and mice are opened after the drop, which works because V2's
//! /dev/input/event* belong to uid 1000 (console evdevfs NODE_UID, mode 0660), as /dev/fb0 does.
//!
//! A drop that fails ends bitscomp (it never runs the desktop as root by accident); `--user 0` keeps root on
//! purpose. Only the fbdev back end drops: headless and nested runs are a developer's own uid already.

/// The desktop user V2 has (MCP's programs, the stick's home, the device nodes): uid and gid 1000.
pub const DESKTOP: (u32, u32) = (1000, 1000);

/// `--user UID[:GID]` (GID defaults to UID).
pub fn parse_user(s: &str) -> Result<(u32, u32), String> {
    let num = |t: &str| t.parse::<u32>().map_err(|_| format!("--user {s}: want UID or UID:GID (numbers)"));
    match s.split_once(':') {
        Some((u, g)) => Ok((num(u)?, num(g)?)),
        None => num(s).map(|u| (u, u)),
    }
}

/// Whether to drop and to whom: only a root bitscomp on fbdev with a non-root target.
pub fn plan(euid: u32, backend: &str, target: (u32, u32)) -> Option<(u32, u32)> {
    (euid == 0 && backend == "fbdev" && target.0 != 0).then_some(target)
}

/// HOME and the user name of `uid` from passwd text (`name:pw:uid:gid:gecos:home:shell` lines).
pub fn passwd_entry(text: &str, uid: u32) -> Option<(String, String)> {
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 7 && f[2].parse::<u32>().ok() == Some(uid) && !f[5].is_empty()).then(|| (f[0].to_string(), f[5].to_string()))
    })
}

pub fn euid() -> u32 {
    // SAFETY: geteuid has no failure and touches no memory.
    unsafe { libc::geteuid() }
}

fn ids() -> ([u32; 3], [u32; 3]) {
    let (mut r, mut e, mut s) = (0u32, 0u32, 0u32);
    let (mut rg, mut eg, mut sg) = (0u32, 0u32, 0u32);
    // SAFETY: each call writes three u32s into locals that outlive it.
    unsafe {
        libc::getresuid(&mut r, &mut e, &mut s);
        libc::getresgid(&mut rg, &mut eg, &mut sg);
    }
    ([r, e, s], [rg, eg, sg])
}

/// Gives root up for `(uid, gid)`: supplementary groups = [gid] (as `chpst -u uid:gid`), then all three gids,
/// then all three uids; checked afterwards, and that root cannot come back. Called while bitscomp has one thread.
pub fn drop_to(uid: u32, gid: u32) -> Result<(), String> {
    let err = |what: &str| format!("{what}: {}", std::io::Error::last_os_error());
    let groups = [gid as libc::gid_t];
    // SAFETY: setgroups reads one gid from `groups`, which outlives the call.
    if unsafe { libc::setgroups(1, groups.as_ptr()) } != 0 {
        return Err(err("setgroups"));
    }
    // SAFETY: plain id changes, no memory is passed.
    if unsafe { libc::setresgid(gid, gid, gid) } != 0 {
        return Err(err("setresgid"));
    }
    // SAFETY: as above.
    if unsafe { libc::setresuid(uid, uid, uid) } != 0 {
        return Err(err("setresuid"));
    }
    let (u, g) = ids();
    if u != [uid; 3] || g != [gid; 3] {
        return Err(format!("ids after the drop are uid {u:?} gid {g:?}"));
    }
    // SAFETY: as above; this must fail now (no saved root id, no capability).
    if uid != 0 && unsafe { libc::setuid(0) } == 0 {
        return Err("setuid(0) still works after the drop".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_argument() {
        assert_eq!(parse_user("1000"), Ok((1000, 1000)));
        assert_eq!(parse_user("1000:100"), Ok((1000, 100)));
        assert_eq!(parse_user("0"), Ok((0, 0)));
        for bad in ["", "x", "1000:", ":5", "-1", "1:2:3"] {
            assert!(parse_user(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_root_on_fbdev_drops() {
        assert_eq!(plan(0, "fbdev", DESKTOP), Some(DESKTOP));
        assert_eq!(plan(0, "fbdev", (0, 0)), None, "--user 0 keeps root");
        assert_eq!(plan(1000, "fbdev", DESKTOP), None, "already the user (MCP's start)");
        assert_eq!(plan(0, "headless", DESKTOP), None);
        assert_eq!(plan(0, "winit", DESKTOP), None);
    }

    #[test]
    fn a_drop_that_cannot_happen_is_an_error() {
        // Not root (host tests run as a developer): setgroups is refused, and drop_to says so instead of going on.
        if euid() == 0 {
            return;
        }
        let e = drop_to(euid() + 1, euid() + 1).unwrap_err();
        assert!(e.starts_with("setgroups:"), "{e}");
        assert_eq!(ids().0, [euid(); 3], "nothing changed");
    }

    #[test]
    fn home_from_passwd() {
        let t = "root:x:0:0:root:/root:/bin/sh\nbits:x:1000:1000:Georg:/:/bin/sh\nbroken:x:7\n";
        assert_eq!(passwd_entry(t, 1000), Some(("bits".into(), "/".into())));
        assert_eq!(passwd_entry(t, 0), Some(("root".into(), "/root".into())));
        assert_eq!(passwd_entry(t, 7), None);
        assert_eq!(passwd_entry("", 1000), None);
    }
}

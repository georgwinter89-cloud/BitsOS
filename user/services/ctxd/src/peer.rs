//! VORRANG: whether a connection on ctxd's socket is the desktop (bitscomp, either build). The socket takes
//! any program of the session's uid; most habit events only teach what Georg did and may come from anyone,
//! but the Vorrang ones stand for his own input in the task manager and his own "Ja" to a high priority, which
//! only the desktop sees (`habits_vorrang::screened`). The answer is the kernel's, never the message's:
//! SO_PEERCRED's uid and BitsOS's record of the program that called `connect` (SO_PEERSEC
//! `bitsos:program:1:<init|own>:<dev>:<ino>:<path>`, checked against the file at that path, as atspid and
//! settingsd do). A kernel without that record (Linux, the host tests) is asked through `/proc/<pid>/exe`.
//! bitscomp is not dumpable on BitsOS, so its exe link is closed to uid 1000 there: the record decides.

use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;

pub const DESKTOP: &[&str] = &["/boot/BITSOS2/bin/bitscomp"];
const LABEL: &str = "bitsos:program:";

/// The program a BitsOS label names, when that is the very file at its path. Some(None): a BitsOS label that
/// names nobody here (another namespace, a removed or replaced file, an unknown version). None: not BitsOS's.
pub fn labelled(label: &[u8], at: impl FnOnce(&str) -> Option<(u64, u64)>) -> Option<Option<String>> {
    let rest = label.strip_prefix(LABEL.as_bytes())?;
    let runs = || {
        let mut f = rest.splitn(5, |&b| b == b':');
        let (version, ns, dev, ino, path) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
        let num = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u64>().ok();
        let path = std::str::from_utf8(path).ok()?;
        let file = (num(dev)?, num(ino)?);
        (version == b"1" && ns == b"init" && path.starts_with('/') && at(path)? == file).then(|| path.to_string())
    };
    Some(runs())
}

fn on_disk(path: &str) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino()))
}

/// The desktop's program files (the test build also takes `CTXD_TEST_DESKTOP`, paths split by `:`).
pub fn is_desktop(exe: &str) -> bool {
    DESKTOP.contains(&exe) || crate::knobs::var("CTXD_TEST_DESKTOP").is_some_and(|v| v.split(':').any(|p| p == exe))
}

/// Whether the peer of `s` is the desktop of ctxd's own user.
pub fn desktop(s: &UnixStream) -> bool {
    let Ok(fd) = usize::try_from(s.as_raw_fd()) else { return false };
    let Ok((pid, uid, _)) = usys::peer_cred(fd) else { return false };
    let me = usys::geteuid().ok().map(|u| u as u32);
    if pid == 0 || Some(uid) != me {
        return false;
    }
    let exe = match usys::peer_label(fd).ok().and_then(|l| labelled(&l, on_disk)) {
        Some(told) => told,
        None => std::fs::read_link(format!("/proc/{pid}/exe")).ok().and_then(|p| p.into_os_string().into_string().ok()),
    };
    exe.is_some_and(|e| is_desktop(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bitscomp_s_files_and_only_the_very_file() {
        assert!(is_desktop("/boot/BITSOS2/bin/bitscomp") && !is_desktop("/bin/bitscomp"));
        assert!(!is_desktop("/bin/ctxd") && !is_desktop("/data/home/1000/boot/BITSOS2/bin/bitscomp") && !is_desktop(""));
        let at = |p: &str| (p == "/boot/BITSOS2/bin/bitscomp").then_some((3, 42));
        assert_eq!(labelled(b"bitsos:program:1:init:3:42:/boot/BITSOS2/bin/bitscomp", at), Some(Some("/boot/BITSOS2/bin/bitscomp".into())));
        assert_eq!(labelled(b"bitsos:program:1:init:3:43:/boot/BITSOS2/bin/bitscomp", at), Some(None), "another file under the path");
        assert_eq!(labelled(b"bitsos:program:1:own:3:42:/boot/BITSOS2/bin/bitscomp", at), Some(None), "a namespace of its own");
        assert_eq!(labelled(b"unconfined", at), None, "not BitsOS's: ask /proc");
    }

    #[test]
    fn a_socket_pair_of_this_test_is_not_the_desktop() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(!desktop(&a), "the test binary is not bitscomp");
    }
}

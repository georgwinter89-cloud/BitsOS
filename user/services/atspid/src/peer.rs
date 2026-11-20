//! Who may ask C3: root (the checks' command line) and, of the session's user, only the desktop (bitscomp, either
//! build). Durchblick's rule: the element list goes to the desktop; agents get it from bitscomp only while Georg's
//! keyboard release holds, and no other program of the session reads Brave's page or acts in it through here.
//! The program comes from the kernel's record at `connect` (BitsOS's SO_PEERSEC label
//! `bitsos:program:1:<init|own>:<dev>:<ino>:<path>`, checked against the file at that path, as settingsd does);
//! a kernel without that record (Linux, the host tests) is asked through `/proc/<pid>/exe`. bitscomp gave root up
//! and is not dumpable, so its exe link is closed to uid 1000: on BitsOS the label is what decides.

use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

pub const DESKTOP: &[&str] = &["/boot/BITSOS2/bin/bitscomp"];
const LABEL: &str = "bitsos:program:";

/// The program a BitsOS label names, when that is the very file at its path in atspid's view. Some(None): a label
/// of BitsOS that names nobody here (another namespace, a removed file, an unknown version).
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

/// The program at the other end of `s`, connected by process `pid`.
pub fn program(s: &impl AsFd, pid: i32) -> Option<String> {
    let fd = usize::try_from(s.as_fd().as_raw_fd()).ok()?;
    if let Ok(label) = usys::peer_label(fd) {
        if let Some(told) = labelled(&label, on_disk) {
            return told;
        }
    }
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    exe.into_os_string().into_string().ok()
}

/// Root, or the session's user through the desktop or a program `extra` names (`serve --peer`, the host tests).
pub fn allowed(uid: u32, session: u32, exe: Option<&str>, extra: &[PathBuf]) -> Result<(), String> {
    let known = |e: &str| DESKTOP.contains(&e) || extra.iter().any(|x| x.as_os_str() == e);
    match (uid, exe) {
        (0, _) => Ok(()),
        (u, Some(e)) if u == session && known(e) => Ok(()),
        (u, e) if u == session => Err(format!("Fremde Programme liest Durchblick nur für den Schreibtisch ({} fragt)", e.unwrap_or("ein unbekanntes Programm"))),
        (u, _) => Err(format!("Benutzer {u} darf fremde Programme nicht lesen")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_and_the_desktop() {
        assert!(allowed(0, 1000, None, &[]).is_ok());
        assert!(allowed(1000, 1000, Some("/boot/BITSOS2/bin/bitscomp"), &[]).is_ok());
        assert!(allowed(1000, 1000, Some("/bin/bitscomp"), &[]).is_err(), "not the desktop's place");
        assert!(allowed(1000, 1000, Some("/data/home/1000/.steam/game"), &[]).is_err(), "a game of the session");
        assert!(allowed(1000, 1000, None, &[]).is_err(), "a program nobody can name");
        assert!(allowed(1001, 1000, Some("/boot/BITSOS2/bin/bitscomp"), &[]).is_err(), "another user");
        assert!(allowed(1000, 1000, Some("/t/bridge"), &[PathBuf::from("/t/bridge")]).is_ok());
    }

    #[test]
    fn a_label_names_the_file_only_when_it_is_that_file() {
        let at = |p: &str| (p == "/boot/BITSOS2/bin/bitscomp").then_some((3, 42));
        assert_eq!(labelled(b"bitsos:program:1:init:3:42:/boot/BITSOS2/bin/bitscomp", at), Some(Some("/boot/BITSOS2/bin/bitscomp".into())));
        assert_eq!(labelled(b"bitsos:program:1:init:3:43:/boot/BITSOS2/bin/bitscomp", at), Some(None), "another file under the path");
        assert_eq!(labelled(b"bitsos:program:1:own:3:42:/boot/BITSOS2/bin/bitscomp", at), Some(None), "a namespace of its own");
        assert_eq!(labelled(b"unconfined", at), None, "not BitsOS's: ask /proc");
    }
}

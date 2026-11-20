//! The socket file. One assistant per user: a second start finds the first one answering and gives up (the
//! pattern of bitsmusik and bitspkgd); a socket left behind by an assistant that is gone is replaced.
//!
//! "Answering" is the kernel's word, not the path's: the socket's folder is writable by every program of the
//! user, so any of them can put a listener there. The sidebar does not talk to such a listener, and if this
//! program gave up before it, the squatter would keep the assistant away for as long as it lives. So only a
//! listener the kernel names as this user's assistant counts as taken ([`assistant`]); anybody else's socket
//! file is replaced.
//!
//! The listener is made by the process that was started (no fork before `listen`): the sidebar accepts its
//! helper by the pid it spawned.

use settingsd::peer::{Peer, Source};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// Where the assistant is installed: the one program the sidebar accepts as a listener.
pub const PROGRAM: &str = "/bin/assist";

#[derive(Debug, PartialEq, Eq)]
pub enum Fail {
    /// Another assistant answers there.
    Taken,
    /// German, for stderr.
    Other(String),
}

/// The listener, and whose socket file was replaced to make it (a listener that is not the assistant).
pub type Bound = (UnixListener, Option<Peer>);

/// Is `peer`, the listener a probing connection reached, the assistant of user `uid`?
///
/// Where the kernel keeps the record of the program that called `listen` (BitsOS), that record must name
/// `/bin/assist`. A kernel without it (a host, where the tests run) is asked what the listening pid runs
/// now: the file this very process runs (`me`).
pub fn assistant(uid: u32, peer: &Peer, me: Option<&str>) -> bool {
    peer.uid == Some(uid)
        && match (peer.by, peer.exe.as_deref()) {
            (Source::Kernel, Some(exe)) => exe == PROGRAM,
            (Source::Proc, Some(exe)) => Some(exe) == me,
            _ => false,
        }
}

/// The socket of user `uid` at `path`, asking the kernel who listens there already.
pub fn bind(path: &Path, uid: u32) -> Result<Bound, Fail> {
    let me = std::env::current_exe().ok().and_then(|p| p.into_os_string().into_string().ok());
    bind_by(path, &|s| settingsd::peer::of(s), &|p| assistant(uid, p, me.as_deref()))
}

/// [`bind`] with the kernel's answers given: who listens behind a connection, and whether that is the
/// assistant.
pub fn bind_by(path: &Path, seen: &dyn Fn(&UnixStream) -> Peer, assistant: &dyn Fn(&Peer) -> bool) -> Result<Bound, Fail> {
    let other = |e: std::io::Error| Fail::Other(format!("{}: {e}", path.display()));
    let mut replaced = None;
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            return Err(Fail::Other(format!("{} ist kein Socket und wird nicht ersetzt.", path.display())));
        }
        if let Ok(probe) = UnixStream::connect(path) {
            let peer = seen(&probe);
            if assistant(&peer) {
                return Err(Fail::Taken);
            }
            replaced = Some(peer);
        }
        std::fs::remove_file(path).map_err(other)?;
    }
    let l = UnixListener::bind(path).map_err(other)?;
    // The folder is the user's alone already (init: /run/user/<uid> 0700); the mode says the same of the
    // socket. Who is served is the gate's question either way, never the file mode's.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(other)?;
    Ok((l, replaced))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(uid: u32, exe: Option<&str>, by: Source) -> Peer {
        Peer { pid: 7, uid: Some(uid), exe: exe.map(str::to_string), by }
    }

    #[test]
    fn only_the_kernels_record_of_this_users_assistant_counts_as_taken() {
        let me = Some("/work/target/debug/assist");
        assert!(assistant(1000, &peer(1000, Some("/bin/assist"), Source::Kernel), me));
        let strangers = [
            (peer(1000, Some("/home/user/assist"), Source::Kernel), "a copy somewhere else"),
            (peer(1000, Some("/bin/sh"), Source::Kernel), "another program"),
            (peer(1000, Some("/work/target/debug/assist"), Source::Kernel), "where the kernel keeps the record, only /bin/assist"),
            (peer(1000, None, Source::Kernel), "a program in a namespace of its own"),
            (peer(0, Some("/bin/assist"), Source::Kernel), "root's assistant is not this user's"),
            (peer(1001, Some("/bin/assist"), Source::Kernel), "another user's"),
            (peer(1000, Some("/bin/assist"), Source::Proc), "told by /proc only: not the file this process runs"),
            (peer(1000, Some("/bin/assist"), Source::Given), "nobody the kernel named"),
        ];
        for (p, what) in strangers {
            assert!(!assistant(1000, &p, me), "{what}");
        }
        // A host without the record: the listening pid runs this very file.
        assert!(assistant(1000, &peer(1000, me, Source::Proc), me));
        assert!(!assistant(1000, &peer(1000, me, Source::Proc), None));
        let nobody = Peer { pid: 0, uid: None, exe: None, by: Source::Given };
        assert!(!assistant(1000, &nobody, me));
    }

    #[test]
    fn a_strangers_listener_is_replaced_and_an_assistants_is_not() {
        let dir = std::env::temp_dir().join(format!("assist-bind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.sock");
        let seen = |_: &UnixStream| peer(1000, Some("/bin/sh"), Source::Kernel);
        let squatter = UnixListener::bind(&path).unwrap();
        // The squatter answers, and is told as an assistant: this start gives up and leaves the file alone.
        assert_eq!(bind_by(&path, &seen, &|_| true).err(), Some(Fail::Taken));
        assert!(UnixStream::connect(&path).is_ok());
        // Told as what it is: its socket file goes, the new listener is at the path.
        let (l, replaced) = bind_by(&path, &seen, &|p| assistant(1000, p, None)).unwrap();
        assert_eq!(replaced.and_then(|p| p.exe), Some("/bin/sh".to_string()));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        l.set_nonblocking(true).unwrap();
        squatter.set_nonblocking(true).unwrap();
        // Two probes reached the squatter; what connects now reaches the new listener only.
        while squatter.accept().is_ok() {}
        let _c = UnixStream::connect(&path).unwrap();
        assert!(l.accept().is_ok());
        assert!(squatter.accept().is_err());
        // A socket nobody answers on is replaced without a word, a file that is no socket never.
        drop(l);
        let (_l, replaced) = bind_by(&path, &|_| unreachable!(), &|_| unreachable!()).unwrap();
        assert!(replaced.is_none());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "x").unwrap();
        assert!(matches!(bind_by(&path, &seen, &|_| false), Err(Fail::Other(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

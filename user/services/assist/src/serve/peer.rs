//! Who is served: the compositor of this user's session and nobody else. `answer` with yes lets a tool
//! run, so a program of the same user that finds the socket must not be able to say it.
//!
//! Three answers of the kernel decide, none of them the caller's word:
//! - who connected: pid and uid (`SO_PEERCRED`) and the program that called `connect` (BitsOS:
//!   `SO_PEERSEC`), read once after `accept` by settingsd's `peer` module, which also turns away a program in
//!   a user or mount namespace of its own;
//! - which process holds the screen's compositor grant now (`/proc/bitsos/compositor`), read again for every
//!   request: a compositor that lost the grant since it connected is not served any more;
//! - the rule itself is the installer's and bitspkgd's (`bitsinstall::judge_peer`).
//!
//! Unlike bitspkgd, root gets nothing extra here: the service belongs to one user's session.

use crate::tools::local::Place;
use settingsd::peer::{Peer, Source};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Who connected, as the kernel recorded it.
pub type Seen = dyn Fn(&UnixStream) -> Peer + Send + Sync;
/// The text of the kernel's holder file now; None on a kernel without one, Err when it could not be read.
pub type Holder = dyn Fn() -> Result<Option<String>, String> + Send + Sync;

pub struct Gate {
    /// The user this service runs as.
    pub uid: u32,
    pub seen: Box<Seen>,
    pub holder: Box<Holder>,
}

impl Gate {
    pub fn system(uid: u32) -> Gate {
        Gate { uid, seen: Box::new(settingsd::peer::of), holder: Box::new(holder_file) }
    }

    /// May `peer` be served now? Ok: who it is, for the events; Err: why not (for the events, never told to
    /// the caller).
    pub fn admit(&self, peer: &Peer) -> Result<String, String> {
        judge(self.uid, peer, (self.holder)()?.as_deref())
    }
}

fn holder_file() -> Result<Option<String>, String> {
    match std::fs::read_to_string(bitsinstall::COMPOSITOR_HOLDER) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", bitsinstall::COMPOSITOR_HOLDER)),
    }
}

/// May a service whose tools are asked at `place` listen on this machine?
///
/// The sidebar cannot trust the socket's path (every program of the user can put a socket there); it judges
/// who listens from the kernel's record: this user's `/bin/assist`. That record names the program, not the
/// world it reads. A program of Georg's user could start the real assistant with a settings service,
/// certificates or tools of its own (`--socket`, `SETTINGS_SOCKET`, `SSL_CERT_FILE`, `TOOLS_SOCKET`) and
/// move its socket to where the sidebar looks: name, provider and every answer in the trusted sidebar would
/// then be that program's. So on BitsOS (a kernel that names the screen's holder) a service whose world was
/// named from outside does not start, and "`/bin/assist` listens" means "the system's settings decide".
/// A host without the holder file is a test's world.
pub fn may_listen(place: &Place) -> bool {
    own_world(place, &holder_file())
}

fn own_world(place: &Place, holder: &Result<Option<String>, String>) -> bool {
    *place == Place::Door || matches!(holder, Ok(None))
}

pub fn judge(own: u32, peer: &Peer, holder: Option<&str>) -> Result<String, String> {
    let Some(uid) = peer.uid else { return Err("the kernel names no peer for the connection".into()) };
    if uid != own {
        return Err(format!("pid {} runs as uid {uid}, this assistant serves uid {own}", peer.pid));
    }
    let Some(exe) = peer.exe.as_deref() else {
        return Err(format!("pid {}: the program could not be told (gone, or in a namespace of its own)", peer.pid));
    };
    // A kernel that names the grant's holder also records who connected. A program told by a later look at
    // /proc is then not taken: between connect and the look a pid can come to run something else.
    if holder.is_some() && peer.by != Source::Kernel {
        return Err(format!("pid {}: the kernel has no record of the program that connected", peer.pid));
    }
    bitsinstall::judge_peer(&bitsinstall::CONFIRMERS, peer.pid, uid, Path::new(exe), holder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(pid: u32, uid: u32, exe: Option<&str>, by: Source) -> Peer {
        Peer { pid, uid: Some(uid), exe: exe.map(str::to_string), by }
    }

    #[test]
    fn only_the_grant_holding_compositor_of_this_user() {
        let comp = Some("/boot/BITSOS2/bin/bitscomp");
        assert!(judge(1000, &peer(412, 1000, comp, Source::Kernel), Some("holder=412\n")).is_ok());
        assert!(judge(1000, &peer(412, 1000, Some("/bin/bitscomp"), Source::Kernel), Some("holder=412\n")).is_err(), "not the desktop's place");
        assert!(judge(1000, &peer(412, 1000, comp, Source::Proc), None).is_ok(), "a kernel without the holder file (the host): uid and program decide");
        let refused = [
            (1000, peer(413, 1000, comp, Source::Kernel), Some("holder=412\n"), "another bitscomp than the screen's"),
            (1000, peer(412, 1000, comp, Source::Kernel), Some("holder=none\n"), "nobody holds the grant"),
            (1000, peer(412, 1000, comp, Source::Kernel), Some("holder=x\n"), "a holder file that is not understood"),
            (1000, peer(412, 1000, Some("/bin/assist"), Source::Kernel), Some("holder=412\n"), "another program with the holder's pid"),
            (1000, peer(412, 1000, Some("/bin/sh"), Source::Kernel), None, "another program"),
            (1000, peer(412, 1000, None, Source::Kernel), Some("holder=412\n"), "a program in a namespace of its own"),
            (1000, peer(412, 1000, comp, Source::Proc), Some("holder=412\n"), "told by /proc where the kernel keeps the record"),
            (1000, peer(412, 0, comp, Source::Kernel), Some("holder=412\n"), "root's compositor is not this user's"),
            (1000, peer(412, 1001, comp, Source::Kernel), Some("holder=412\n"), "another user"),
            (0, peer(412, 1000, comp, Source::Kernel), Some("holder=412\n"), "an assistant of root serves no desktop user"),
            (1001, peer(412, 1001, comp, Source::Kernel), Some("holder=412\n"), "a user who has no compositor"),
        ];
        for (own, p, holder, what) in refused {
            assert!(judge(own, &p, holder).is_err(), "{what}");
        }
        let nobody = Peer { pid: 0, uid: None, exe: None, by: Source::Given };
        assert!(judge(1000, &nobody, None).is_err());
    }

    #[test]
    fn on_bitsos_only_the_systems_own_world_listens() {
        let bitsos = [Ok(Some("holder=412\n".to_string())), Ok(Some("holder=none\n".to_string())), Err("EIO".to_string())];
        let named = [Place::Nowhere("the settings socket was named from outside"), Place::Named("/tmp/tools.sock".into())];
        for holder in &bitsos {
            assert!(own_world(&Place::Door, holder));
            for place in &named {
                assert!(!own_world(place, holder), "{place:?} {holder:?}");
            }
        }
        // A host: tests name their settings and tools.
        for place in named.iter().chain([&Place::Door]) {
            assert!(own_world(place, &Ok(None)));
        }
    }

    #[test]
    fn a_holder_file_that_cannot_be_read_admits_nobody() {
        let gate = Gate { uid: 1000, seen: Box::new(|_| unreachable!()), holder: Box::new(|| Err("EIO".into())) };
        assert!(gate.admit(&peer(412, 1000, Some("/boot/BITSOS2/bin/bitscomp"), Source::Kernel)).is_err());
    }
}
